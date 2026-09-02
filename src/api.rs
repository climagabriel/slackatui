//! A small blocking Slack Web API client, used from background threads. A
//! session token needs its cookie on every call; rate limits are honoured
//! once per call.

use std::time::Duration;

use serde_json::Value;

use crate::auth::Auth;

pub struct Client {
    agent: ureq::Agent,
    auth: Auth,
}

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into()
}

impl Client {
    pub fn new(auth: Auth) -> Client {
        Client {
            agent: agent(),
            auth,
        }
    }

    /// One Web API call; the JSON on `ok`, the `error` field otherwise.
    pub fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value, String> {
        let url = format!("https://slack.com/api/{method}");
        for attempt in 0..2 {
            let mut resp = self
                .agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", self.auth.token))
                .header("Cookie", &format!("d={}", self.auth.cookie))
                .send_form(params.iter().copied())
                .map_err(|e| format!("{method}: {e}"))?;
            if resp.status().as_u16() == 429 && attempt == 0 {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(5)
                    .min(30);
                std::thread::sleep(Duration::from_secs(wait));
                continue;
            }
            let text = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("{method}: {e}"))?;
            let v: Value = serde_json::from_str(&text).map_err(|e| format!("{method}: {e}"))?;
            if v.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(v);
            }
            let err = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!("{method}: {err}"));
        }
        Err(format!("{method}: rate limited"))
    }

    /// Who the token is: (user id, user name, team name).
    pub fn auth_test(&self) -> Result<(String, String, String), String> {
        let v = self.call("auth.test", &[])?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Ok((s("user_id"), s("user"), s("team")))
    }

    /// Top-level messages, newest first, at most `limit`; `latest` walks
    /// older (exclusive), `oldest` walks newer (exclusive).
    pub fn history(
        &self,
        cid: &str,
        latest: Option<&str>,
        oldest: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>, String> {
        let limit = limit.to_string();
        let mut params = vec![
            ("channel", cid),
            ("limit", limit.as_str()),
            ("inclusive", "false"),
        ];
        if let Some(l) = latest {
            params.push(("latest", l));
        }
        if let Some(o) = oldest {
            params.push(("oldest", o));
        }
        let v = self.call("conversations.history", &params)?;
        Ok(v.get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// A whole thread, root first, following the cursor.
    pub fn replies(&self, cid: &str, ts: &str) -> Result<Vec<Value>, String> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut params = vec![("channel", cid), ("ts", ts), ("limit", "200")];
            if let Some(c) = cursor.as_deref() {
                params.push(("cursor", c));
            }
            let v = self.call("conversations.replies", &params)?;
            if let Some(m) = v.get("messages").and_then(Value::as_array) {
                out.extend(m.iter().cloned());
            }
            cursor = v
                .pointer("/response_metadata/next_cursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Search matches across the workspace, newest first, at most `count`.
    pub fn search(&self, query: &str, count: usize) -> Result<Vec<Value>, String> {
        let count = count.min(100).to_string();
        let v = self.call(
            "search.messages",
            &[
                ("query", query),
                ("count", count.as_str()),
                ("sort", "timestamp"),
                ("sort_dir", "desc"),
            ],
        )?;
        Ok(v.pointer("/messages/matches")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Every conversation the user is a member of.
    pub fn my_conversations(&self) -> Result<Vec<Value>, String> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut params = vec![
                ("types", "public_channel,private_channel,mpim,im"),
                ("exclude_archived", "true"),
                ("limit", "1000"),
            ];
            if let Some(c) = cursor.as_deref() {
                params.push(("cursor", c));
            }
            let v = self.call("users.conversations", &params)?;
            if let Some(c) = v.get("channels").and_then(Value::as_array) {
                out.extend(c.iter().cloned());
            }
            cursor = v
                .pointer("/response_metadata/next_cursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Unread state per conversation, as the web client fetches it.
    pub fn counts(&self) -> Result<Value, String> {
        self.call("client.counts", &[("thread_counts_by_channel", "false")])
    }
}
