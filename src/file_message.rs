//! Read-only navigation from a file to its latest share in the current channel.
use crate::{
    api::Client,
    archive::{ts_to_id, Msg},
};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Metadata {
    pub id: String,
    pub created: Option<i64>,
}
impl Metadata {
    pub fn from_file(file: &Value) -> Self {
        Self {
            id: file["id"].as_str().unwrap_or("").into(),
            created: file["created"]
                .as_i64()
                .or_else(|| file["timestamp"].as_i64()),
        }
    }
    pub fn date(&self) -> String {
        self.created
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
            .map(|date| date.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "date unknown".into())
    }
}
pub struct Location {
    pub channel: String,
    pub focus: i64,
    pub root: i64,
    pub timeline: Vec<Msg>,
    pub replies: Vec<Msg>,
    pub has_older: bool,
    pub has_newer: bool,
}
fn share(file: &Value, channel: &str) -> Result<(String, String), String> {
    ["public", "private"]
        .iter()
        .filter_map(|kind| file["shares"][kind][channel].as_array())
        .flatten()
        .filter_map(|row| {
            let ts = row["ts"].as_str()?;
            let id = ts_to_id(ts)?;
            let root = row["thread_ts"]
                .as_str()
                .filter(|s| ts_to_id(s).is_some())
                .unwrap_or(ts);
            Some((id, ts.to_string(), root.to_string()))
        })
        .max_by_key(|(id, _, _)| *id)
        .map(|(_, ts, root)| (ts, root))
        .ok_or_else(|| "Slack exposes no sharing message for this file in this channel.".into())
}
pub fn newer(
    client: &Client,
    channel: &str,
    since: &str,
    limit: usize,
) -> Result<(Vec<Msg>, bool), String> {
    history_page(client, channel, None, Some(since), false, limit)
}

/// One page of `conversations.history`, sorted oldest first, with whether
/// Slack says there is more behind it and the cursor that would ask for the
/// next one. Exactly one HTTP request: a caller that needs the cursor
/// followed does so itself, so it stays in charge of how many requests go out
/// and can give up between them.
pub fn history_request(
    client: &Client,
    channel: &str,
    latest: Option<&str>,
    oldest: Option<&str>,
    inclusive: bool,
    limit: usize,
    cursor: Option<&str>,
) -> Result<(Vec<Msg>, bool, String), String> {
    let count = limit.to_string();
    let mut params = vec![
        ("channel", channel),
        ("inclusive", if inclusive { "true" } else { "false" }),
        ("limit", count.as_str()),
    ];
    if let Some(latest) = latest {
        params.push(("latest", latest));
    }
    if let Some(oldest) = oldest {
        params.push(("oldest", oldest));
    }
    if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
        params.push(("cursor", cursor));
    }
    let response = client.call("conversations.history", &params)?;
    let rows = response["messages"]
        .as_array()
        .ok_or("Slack returned no message list")?;
    let mut messages: Vec<Msg> = rows
        .iter()
        .filter_map(|row| Msg::from_api(channel.into(), row.clone()))
        .collect();
    let next = response
        .pointer("/response_metadata/next_cursor")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let more = response["has_more"].as_bool().unwrap_or(false) || !next.is_empty();
    messages.sort_by_key(|message| message.id);
    Ok((messages, more, next))
}

/// A page with messages in it: empty pages that say there is more are read
/// through, following the cursor, until one carries something. Unbounded in
/// requests as far as its caller is concerned, so it is not for a path that
/// has to stay interruptible.
pub fn history_page(
    client: &Client,
    channel: &str,
    latest: Option<&str>,
    oldest: Option<&str>,
    inclusive: bool,
    limit: usize,
) -> Result<(Vec<Msg>, bool), String> {
    let mut cursor = String::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        let (messages, more, next) = history_request(
            client, channel, latest, oldest, inclusive, limit, Some(cursor.as_str()),
        )?;
        if !messages.is_empty() || !more {
            return Ok((messages, more));
        }
        if next.is_empty() || !seen.insert(next.clone()) {
            return Err("Slack history pagination did not advance".into());
        }
        cursor = next;
    }
    Err("Slack returned too many empty history pages; retry the lookup".into())
}

pub fn load(client: &Client, channel: &str, file: &str) -> Result<Location, String> {
    let info = client.call("files.info", &[("file", file)])?;
    let (ts, root_ts) = share(&info["file"], channel)?;
    let focus = ts_to_id(&ts).ok_or("Invalid sharing timestamp")?;
    let root = ts_to_id(&root_ts).ok_or("Invalid thread timestamp")?;
    let location = message_context(client, channel, focus, root)?;
    let timeline = &location.timeline;
    let replies = &location.replies;
    let target = if focus == root { &timeline } else { &replies };
    if !target.iter().any(|m| {
        m.id == focus
            && m.data["files"]
                .as_array()
                .is_some_and(|files| files.iter().any(|f| f["id"].as_str() == Some(file)))
    }) {
        return Err("The sharing message no longer contains this file in Slack.".into());
    }
    Ok(location)
}

pub fn message_context(client: &Client, channel: &str, focus: i64, root: i64) -> Result<Location, String> {
    let root_ts = crate::live::id_to_ts(root);
    let (mut timeline, has_older) = history_page(client, channel, Some(&root_ts), None, true, 40)?;
    if !timeline.iter().any(|m| m.id == root) {
        return Err(
            "The message or its thread root is no longer available in Slack.".into(),
        );
    }
    let (newer, has_newer) = newer(client, channel, &root_ts, 40)?;
    timeline.extend(newer);
    timeline.sort_by_key(|m| m.id);
    timeline.dedup_by_key(|m| m.id);
    let replies: Vec<Msg> = if focus != root {
        client
            .replies(channel, &root_ts)?
            .into_iter()
            .filter_map(|row| Msg::from_api(channel.into(), row))
            .collect()
    } else {
        vec![]
    };
    if !(if focus == root { &timeline } else { &replies }).iter().any(|message| message.id == focus) {
        return Err("Message is no longer available in Slack".into());
    }
    Ok(Location {
        channel: channel.into(),
        focus,
        root,
        timeline,
        replies,
        has_older,
        has_newer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn empty_pages_follow_cursors_in_both_directions_and_short_pages_keep_more() {
        for older in [true, false] {
            let client = Client::for_test(move |_, params| {
                assert!(params
                    .iter()
                    .any(|(key, _)| *key == if older { "latest" } else { "oldest" }));
                if params.contains(&("cursor", "second")) {
                    Ok(json!({"messages":[{"ts":"2.000000"}],"has_more":true}))
                } else {
                    Ok(json!({"messages":[],"response_metadata":{"next_cursor":"second"}}))
                }
            });
            let (messages, more) = history_page(
                &client,
                "C1",
                older.then_some("3.000000"),
                (!older).then_some("1.000000"),
                false,
                200,
            )
            .unwrap();
            assert_eq!(messages.len(), 1);
            assert!(more);
        }
        let client = Client::for_test(|_, _| {
            Ok(json!({"messages":[],"response_metadata":{"next_cursor":"same"}}))
        });
        assert!(newer(&client, "C1", "1.000000", 40).is_err());
    }
    #[test]
    fn newer_page_preserves_server_more_flag_for_short_pages() {
        let client = Client::for_test(|method, params| {
            assert_eq!(method, "conversations.history");
            assert!(params.contains(&("oldest", "1.000000")));
            assert!(!params.iter().any(|(key, _)| *key == "latest"));
            Ok(json!({"messages":[{"ts":"3.000000"},{"ts":"2.000000"}],"has_more":true}))
        });
        let (messages, more) = newer(&client, "C1", "1.000000", 40).unwrap();
        assert!(more);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            vec![2000000, 3000000]
        );
    }
    #[test]
    fn selects_latest_share_in_current_channel_and_formats_date() {
        let file = json!({"created":0,"shares":{"public":{"C2":[{"ts":"900.000001"}],"C1":[{"ts":"10.000001"}]},"private":{"C1":[{"ts":"20.000002","thread_ts":"15.000001"}]}}});
        assert_eq!(
            share(&file, "C1").unwrap(),
            ("20.000002".into(), "15.000001".into())
        );
        assert!(share(&file, "C3").is_err());
        assert_eq!(Metadata::from_file(&file).date(), "1970-01-01 00:00 UTC");
        assert_eq!(Metadata::from_file(&json!({})).date(), "date unknown");
    }
    #[test]
    fn loads_thread_share_without_writes_and_rejects_missing_target() {
        let client = Client::for_test(|method, params| match method {
            "files.info" => Ok(
                json!({"file":{"shares":{"private":{"C1":[{"ts":"20.000002","thread_ts":"15.000001"}]}}}}),
            ),
            "conversations.history" if params.contains(&("inclusive", "true")) => {
                Ok(json!({"messages":[{"ts":"15.000001"}],"has_more":false}))
            }
            "conversations.history" => Ok(json!({"messages":[]})),
            "conversations.replies" => Ok(
                json!({"messages":[{"ts":"15.000001"},{"ts":"20.000002","thread_ts":"15.000001","files":[{"id":"F1"}]}]}),
            ),
            _ => panic!("Unexpected request {method}"),
        });
        let location = load(&client, "C1", "F1").unwrap();
        assert_eq!(location.focus, 20000002);
        assert_eq!(location.root, 15000001);
        assert_eq!(location.replies.len(), 2);
        assert!(load(&client, "C1", "F2").is_err());
    }
}
