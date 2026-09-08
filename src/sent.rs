//! The signed-in user's messages, newest first, using Slack search cursors.
use crate::{api::Client, archive::Msg};
use std::collections::HashSet;

pub struct Page {
    pub messages: Vec<Msg>,
    pub next_cursor: Option<String>,
}

pub fn fetch(client: &Client, cursor: &str) -> Result<Page, String> {
    let response = client.call("search.messages", &[
        ("query", "from:me"), ("count", "100"), ("sort", "timestamp"),
        ("sort_dir", "desc"), ("cursor", cursor), ("highlight", "false"),
    ])?;
    let matches = response.pointer("/messages/matches").and_then(|v| v.as_array())
        .ok_or("SENT: Slack returned no message list")?;
    let next_cursor = response.pointer("/messages/pagination/next_cursor")
        .or_else(|| response.pointer("/messages/paging/next_cursor"))
        .and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
    if next_cursor.as_deref() == Some(cursor) { return Err("SENT: Slack repeated the pagination cursor; r refreshes".into()); }
    let mut seen = HashSet::new();
    let mut messages = Vec::new();
    for value in matches {
        let cid = value.pointer("/channel/id").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
            .ok_or("SENT: message missing channel ID")?;
        let mut message = Msg::from_api(cid.into(), value.clone()).ok_or("SENT: invalid message timestamp")?;
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
