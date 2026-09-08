//! Slack Later message membership. Never use conversation stars for this.
use crate::{
    api::Client,
    archive::{Archive, Msg},
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

pub type Sources = HashMap<String, Vec<PathBuf>>;

fn active(item: &Value) -> bool {
    item["item_type"] == "message"
        && item["is_archived"] != true
        && item["date_completed"].as_i64().unwrap_or(0) == 0
        && !matches!(
            item["state"].as_str(),
            Some("archived" | "completed" | "deleted")
        )
}

fn identity(item: &Value) -> Result<(String, String), String> {
    let cid = item["item_id"]
        .as_str()
        .ok_or("saved.list: missing item_id")?;
    let ts = item["ts"].as_str().ok_or("saved.list: missing ts")?;
    let url = format!(
        "https://saved.slack.com/archives/{cid}/p{}",
        ts.replace('.', "")
    );
    if crate::raw::Link::parse(&url).is_none() {
        return Err("saved.list: invalid message identity".into());
    }
    Ok((cid.into(), ts.into()))
}

pub fn list(client: &Client) -> Result<Vec<Value>, String> {
    let mut items = Vec::new();
    let mut cursor = String::new();
    let mut cursors = HashSet::new();
    let mut seen = HashSet::new();
    loop {
        // filter=saved is rejected by Slack; the default includes active and archived items.
        let response = client.call("saved.list", &[("limit", "50"), ("cursor", &cursor)])?;
        let page = response["saved_items"]
            .as_array()
            .ok_or("saved.list: missing saved_items")?;
        for item in page.iter().filter(|item| active(item)) {
            if seen.insert(identity(item)?) {
                items.push(item.clone());
            }
        }
        cursor = response
            .pointer("/response_metadata/next_cursor")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into();
        if cursor.is_empty() {
            break;
        }
        if !cursors.insert(cursor.clone()) {
            return Err("saved.list: repeated cursor".into());
        }
    }
    items.sort_by_key(|item| std::cmp::Reverse(item["date_created"].as_i64().unwrap_or(0)));
    Ok(items)
}

pub fn change(client: &Client, message: &Msg, save: bool) -> Result<(), String> {
    let method = if save { "saved.add" } else { "saved.delete" };
    client.call(
        method,
        &[
            ("item_type", "message"),
            ("item_id", &message.channel_id),
            ("ts", &message.ts),
        ],
    )?;
    let requested = json!([{"item_type":"message", "item_id":message.channel_id, "ts":message.ts, "item_detail":""}]).to_string();
    let response = client
        .call("saved.get", &[("items", &requested)])
        .map_err(|e| {
            format!("Save change accepted, but verification failed: {e}; refresh SAVED")
        })?;
    let items = response["saved_items"]
        .as_array()
        .ok_or("Save change accepted, but verification returned no saved_items; refresh SAVED")?;
    let present = items.iter().any(|item| {
        active(item) && item["item_id"] == message.channel_id && item["ts"] == message.ts
    });
    if present != save {
        return Err("Slack has not confirmed the requested saved state; refresh SAVED".into());
    }
    Ok(())
}

pub fn fetch(
    client: &Client,
    known: Vec<Msg>,
    sources: Sources,
    cache: PathBuf,
) -> Result<Vec<Msg>, String> {
    let items = list(client)?;
    let mut result = Vec::new();
    for item in items {
        let (cid, ts) = identity(&item)?;
        let id = crate::archive::ts_to_id(&ts).ok_or("Invalid saved timestamp")?;
        let mut message = known
            .iter().rev()
            .find(|m| m.channel_id == cid && m.id == id && m.data["saved_unavailable"] != true)
            .cloned();
        if message.is_none() {
            for dir in sources.get(&cid).into_iter().flatten() {
                if let Ok(archive) = Archive::open("saved".into(), dir) {
                    message = archive
                        .thread(&cid, id)
                        .ok()
                        .and_then(|msgs| msgs.into_iter().find(|m| m.id == id));
                    if message.is_some() {
                        break;
                    }
                }
            }
        }
        if message.is_none() {
            message = crate::raw::cached_reply(&cache, &cid, id)
                .and_then(|msgs| msgs.into_iter().find(|m| m.id == id));
        }
        if message.is_none() {
            let fetched = client.call(
                "conversations.history",
                &[
                    ("channel", &cid),
                    ("oldest", &ts),
                    ("latest", &ts),
                    ("inclusive", "true"),
                    ("limit", "1"),
                ],
            );
            if let Ok(response) = fetched {
                message = response["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| Msg::from_api(cid.clone(), value.clone()))
                    .find(|m| m.id == id);
            }
        }
        let mut message = message.unwrap_or_else(|| Msg::from_api(cid.clone(), json!({"ts":ts,
            "text":"[Saved message unavailable in the archive or Slack history; /unsave removes it from Later]",
            "saved_unavailable":true,"user":"unknown"})).expect("validated identity"));
        message.data["saved_date_created"] = item["date_created"].clone();
        result.push(message);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    fn item(cid: &str, ts: &str, saved: i64) -> Value {
        json!({"item_type":"message","item_id":cid,"ts":ts,"date_created":saved,"state":"in_progress","is_archived":false})
    }
    #[test]
    fn saved_list_paginates_deduplicates_and_omits_completed_archived_and_files() {
        let client = Client::for_test(|method, params| {
            assert_eq!(method, "saved.list");
            assert!(!params.iter().any(|(key, _)| *key == "filter"));
            Ok(if params.contains(&("cursor", "")) {
                let mut archived = item("C2", "2.000000", 30);
                archived["is_archived"] = true.into();
                let mut completed = item("C3", "3.000000", 40);
                completed["date_completed"] = 50.into();
                json!({"saved_items":[item("C1","1.000000",10),archived,completed,{"item_type":"file"}],"response_metadata":{"next_cursor":"next"}})
            } else {
                json!({"saved_items":[item("C1","1.000000",10),item("D1","1.000000",20)]})
            })
        });
        let items = list(&client).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["item_id"], "D1");
        assert!(list(&Client::for_test(|_, _| Ok(json!({})))).is_err());
        assert!(list(&Client::for_test(|_, _| Ok(
            json!({"saved_items":[],"response_metadata":{"next_cursor":"loop"}})
        )))
        .is_err());
    }
    #[test]
    fn save_and_unsave_target_exact_message_then_verify_membership() {
        for save in [true, false] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let recorded = calls.clone();
            let client = Client::for_test(move |method, params| {
                recorded.lock().unwrap().push(method.to_string());
                if method == "saved.get" {
                    let requested: Value = serde_json::from_str(params[0].1).unwrap();
                    assert_eq!(
                        requested,
                        json!([{"item_type":"message","item_id":"D1","ts":"2.000001","item_detail":""}])
                    );
                    Ok(json!({"saved_items":if save {vec![item("D1","2.000001",10)]}else{vec![]}}))
                } else {
                    assert_eq!(method, if save { "saved.add" } else { "saved.delete" });
                    assert_eq!(
                        params,
                        &[
                            ("item_type", "message"),
                            ("item_id", "D1"),
                            ("ts", "2.000001")
                        ]
                    );
                    Ok(json!({"ok":true}))
                }
            });
            let message = Msg::from_api(
                "D1".into(),
                json!({"ts":"2.000001","thread_ts":"1.000000","text":"reply"}),
            )
            .unwrap();
            change(&client, &message, save).unwrap();
            assert_eq!(calls.lock().unwrap().len(), 2);
            let denied = Client::for_test(|_, _| Err("denied".into()));
            assert_eq!(change(&denied, &message, save).unwrap_err(), "denied");
            let unverified = Client::for_test(|method, _| {
                if method == "saved.get" {
                    Err("offline".into())
                } else {
                    Ok(json!({}))
                }
            });
            assert!(change(&unverified, &message, save)
                .unwrap_err()
                .contains("verification failed"));
        }
    }
    #[test]
    fn hydration_uses_exact_ids_and_keeps_unavailable_items_removable() {
        let client = Client::for_test(|method, params| {
            if method == "saved.list" {
                return Ok(
                    json!({"saved_items":[item("C1","1.000000",10),item("D1","2.000000",20)]}),
                );
            }
            assert_eq!(method, "conversations.history");
            assert!(params.contains(&("channel", "D1")));
            Ok(json!({"messages":[{"ts":"3.000000","text":"wrong nearby message"}]}))
        });
        let cached = Msg::from_api(
            "C1".into(),
            json!({"ts":"1.000000","text":"cached reply","thread_ts":"0.000001"}),
        )
        .unwrap();
        let messages = fetch(&client, vec![cached], Sources::new(), PathBuf::new()).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].data["saved_unavailable"], true);
        assert_eq!(messages[0].channel_id, "D1");
        assert_eq!(messages[0].id, 2_000_000);
        assert_eq!(messages[1].text, "cached reply");
        assert_eq!(messages[1].thread_root(), 1);
    }
}
