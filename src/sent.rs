//! The signed-in user's messages, newest first, using Slack search cursors.
use crate::{api::Client, archive::Msg};
use std::collections::HashSet;

pub struct Page {
    pub messages: Vec<Msg>,
    pub next_cursor: Option<String>,
}

pub fn fetch(client: &Client, cursor: &str) -> Result<Page, String> {
    search(client, cursor, "from:me", "SENT")
}

pub fn mentions(client: &Client, cursor: &str, me: &str) -> Result<Page, String> {
    if me.is_empty() || !me.chars().all(|c| c.is_ascii_alphanumeric()) { return Err("MENTIONS: own Slack user ID unavailable".into()); }
    let mut page = search(client, cursor, &format!("<@{me}>"), "MENTIONS")?;
    // Slack also matches links to messages authored by this user. Keep actual mentions.
    page.messages.retain(|message| mentions_user(&message.data, me));
    Ok(page)
}

fn mentions_user(value: &serde_json::Value, me: &str) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains(&format!("<@{me}>")) || text.contains(&format!("<@{me}|")),
        serde_json::Value::Array(values) => values.iter().any(|value| mentions_user(value, me)),
        serde_json::Value::Object(fields) => {
            if matches!(value["type"].as_str(), Some("plain_text" | "text" | "rich_text_preformatted")) { return false; }
            let blocks = value["blocks"].as_array().is_some_and(|blocks| !blocks.is_empty());
            (value["type"] == "user" && value["user_id"].as_str() == Some(me))
                || fields.iter().filter(|(key,_)| matches!(key.as_str(), "text" | "blocks" | "attachments" | "elements" | "fields" | "pretext" | "fallback" | "title" | "value")
                    && !(blocks && matches!(key.as_str(), "text" | "fallback")))
                    .any(|(_,value)| mentions_user(value, me))
        }
        _ => false,
    }
}

fn search(client: &Client, cursor: &str, query: &str, label: &str) -> Result<Page, String> {
    let response = client.call("search.messages", &[
        ("query", query), ("count", "100"), ("sort", "timestamp"),
        ("sort_dir", "desc"), ("cursor", cursor), ("highlight", "false"),
    ])?;
    let matches = response.pointer("/messages/matches").and_then(|v| v.as_array())
        .ok_or_else(||format!("{label}: Slack returned no message list"))?;
    let next_cursor = response.pointer("/messages/pagination/next_cursor")
        .or_else(|| response.pointer("/messages/paging/next_cursor"))
        .and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
    if next_cursor.as_deref() == Some(cursor) { return Err(format!("{label}: Slack repeated the pagination cursor; r refreshes")); }
    let mut seen = HashSet::new();
    let mut messages = Vec::new();
    for value in matches {
        let cid = value.pointer("/channel/id").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
            .ok_or_else(||format!("{label}: message missing channel ID"))?;
        let mut message = Msg::from_api(cid.into(), value.clone()).ok_or_else(||format!("{label}: invalid message timestamp"))?;
        message.channel_name = value.pointer("/channel/name").and_then(|v| v.as_str()).map(str::to_string);
        if seen.insert((message.channel_id.clone(), message.id)) { messages.push(message); }
    }
    messages.sort_by_key(|message| std::cmp::Reverse(message.id));
    Ok(Page { messages, next_cursor })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mentions_match_personal_tags_and_rich_text_not_authors_or_message_links() {
        let client = Client::for_test(|method, params| {
            assert_eq!(method, "search.messages");
            assert!(params.contains(&("query", "<@U1>")));
            let messages = vec![
                json!({"ts":"5.000000","text":"hi <@U1>"}),
                json!({"ts":"4.000000","text":"hi <@U1|name>"}),
                json!({"ts":"3.000000","blocks":[{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"user","user_id":"U1"}]}]}]}),
                json!({"ts":"2.000000","user":"U1","text":"own message <@U10>","blocks":[{"type":"message_mention","author_id":"U1"}]}),
                json!({"ts":"1.000000","text":"hi @name <!here>"}),
                json!({"ts":"6.000000","text":"<@U1>","blocks":[{"type":"rich_text","elements":[{"type":"text","text":"<@U1>"}]}]}),
                json!({"ts":"7.000000","blocks":[{"type":"section","text":{"type":"plain_text","text":"<@U1>"}}]})
            ].into_iter().map(|mut message| { message["channel"] = json!({"id":"C1"}); message }).collect::<Vec<_>>();
            Ok(json!({"messages":{"matches":messages,"pagination":{"next_cursor":"older"}}}))
        });
        let page = mentions(&client, "*", "U1").unwrap();
        assert_eq!(page.messages.iter().map(|m|m.id).collect::<Vec<_>>(), [5_000_000,4_000_000,3_000_000]);
        assert_eq!(page.next_cursor.as_deref(),Some("older"));
        assert!(mentions(&client,"*","").is_err());
        let page = mentions(&Client::for_test(|_,_|Ok(json!({"messages":{"matches":[],"pagination":{"next_cursor":"older"}}}))),"*","U1").unwrap();
        assert!(page.messages.is_empty()); assert_eq!(page.next_cursor.as_deref(),Some("older"));
    }

    #[test]
    fn sent_pages_keep_reply_targets_sort_and_deduplicate() {
        let client = Client::for_test(|method, params| {
            assert_eq!(method,"search.messages");
            for param in [("query","from:me"),("sort","timestamp"),("sort_dir","desc"),("count","100")] { assert!(params.contains(&param)); }
            if params.contains(&("cursor","*")) {
                let reply=json!({"channel":{"id":"C1","name":"general"},"ts":"3.000001","text":"reply","permalink":"https://test.slack.com/archives/C1/p3000001?thread_ts=1.000001"});
                Ok(json!({"messages":{"matches":[{"channel":{"id":"D1"},"ts":"2.000000"},reply.clone(),reply],"pagination":{"next_cursor":"next"}}}))
            } else {
                assert!(params.contains(&("cursor","next")));
                Ok(json!({"messages":{"matches":[],"paging":{"next_cursor":""}}}))
            }
        });
        let page=fetch(&client,"*").unwrap();
        assert_eq!(page.messages.len(),2);assert_eq!(page.messages[0].id,3_000_001);
        assert_eq!(page.messages[0].thread_root(),1_000_001);
        assert!(fetch(&client,page.next_cursor.as_deref().unwrap()).unwrap().next_cursor.is_none());
    }

    #[test]
    fn invalid_pages_are_errors_and_empty_results_are_valid() {
        assert!(fetch(&Client::for_test(|_,_|Ok(json!({}))),"*").is_err());
        assert!(fetch(&Client::for_test(|_,_|Ok(json!({"messages":{"matches":[],"pagination":{"next_cursor":"same"}}}))),"same").is_err());
        assert!(fetch(&Client::for_test(|_,_|Ok(json!({"messages":{"matches":[]}}))),"*").unwrap().messages.is_empty());
        assert!(fetch(&Client::for_test(|_,_|Err("denied".into())),"*").is_err());
    }
}
