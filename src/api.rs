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

/// What this client will carry in one upload; Slack's own limit is larger,
/// but the bytes go through memory.
pub const MAX_UPLOAD: u64 = 256 * 1024 * 1024;

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

    /// A file's bytes, with the session's credentials as files.slack.com wants them.
    pub fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        // The session credentials go to Slack's own file hosts only.
        let host = url
            .strip_prefix("https://")
            .and_then(|r| r.split('/').next())
            .ok_or_else(|| format!("GET file: {url}: not an https URL"))?;
        if host != "slack.com" && !host.ends_with(".slack.com") {
            return Err(format!("GET file: refusing to send the session to {host}"));
        }
        let mut resp = self
            .agent
            .get(url)
            .header("Authorization", &format!("Bearer {}", self.auth.token))
            .header("Cookie", &format!("d={}", self.auth.cookie))
            .call()
            .map_err(|e| format!("GET file: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GET file: HTTP {}", resp.status().as_u16()));
        }
        let html = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|t| t.starts_with("text/html"))
            .unwrap_or(false);
        if html {
            return Err(
                "GET file: Slack answered with a page, not the file (session expired?)".to_string(),
            );
        }
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_vec()
            .map_err(|e| format!("GET file: {e}"))?;
        if bytes.starts_with(b"<") {
            return Err("Slack answered with a page, not the file (session expired?)".to_string());
        }
        Ok(bytes)
    }

    /// Upload one file into `cid` (into the thread when `thread_ts` is
    /// given), with `comment` as the message that carries it. Slack wants
    /// three calls: a URL, the bytes, then the message.
    pub fn upload_file(
        &self,
        cid: &str,
        thread_ts: Option<&str>,
        path: &std::path::Path,
        comment: &str,
    ) -> Result<Value, String> {
        let size = std::fs::metadata(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        if size == 0 {
            return Err(format!("{}: the file is empty", path.display()));
        }
        if size > MAX_UPLOAD {
            return Err(format!(
                "{}: {size} bytes is past the {MAX_UPLOAD}-byte limit this client sets",
                path.display()
            ));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("upload")
            .to_string();
        let length = bytes.len().to_string();
        let ticket = self.call(
            "files.getUploadURLExternal",
            &[("filename", &name), ("length", &length)],
        )?;
        let url = ticket
            .get("upload_url")
            .and_then(Value::as_str)
            .ok_or("files.getUploadURLExternal: no upload_url")?;
        let file_id = ticket
            .get("file_id")
            .and_then(Value::as_str)
            .ok_or("files.getUploadURLExternal: no file_id")?
            .to_string();
        // The upload URL is Slack's own, single-use, and carries no session.
        let host = url
            .strip_prefix("https://")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default();
        if host != "slack.com" && !host.ends_with(".slack.com") {
            return Err(format!("upload: Slack handed out a URL on {host}"));
        }
        // Its own agent: the shared one gives every call a minute, which a
        // large file over a slow link does not fit into.
        let uploader: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(600)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = uploader
            .post(url)
            .header("Content-Type", "application/octet-stream")
            .send(&bytes[..])
            .map_err(|e| format!("upload {name}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("upload {name}: HTTP {}", resp.status().as_u16()));
        }
        let _ = resp.body_mut().read_to_string();
        let files = serde_json::json!([{ "id": file_id, "title": name }]).to_string();
        let mut params = vec![("files", files.as_str()), ("channel_id", cid)];
        if let Some(ts) = thread_ts {
            params.push(("thread_ts", ts));
        }
        if !comment.is_empty() {
            params.push(("initial_comment", comment));
        }
        self.call("files.completeUploadExternal", &params)
    }

    /// Move the read marker of a conversation to a message: the one write
    /// this client makes.
    pub fn mark(&self, cid: &str, ts: &str) -> Result<(), String> {
        self.call("conversations.mark", &[("channel", cid), ("ts", ts)])
            .map(|_| ())
    }

    /// Post `text` to `cid`, into the thread `thread_ts` when given; the
    /// message as Slack stored it comes back.
    pub fn post_message(
        &self,
        cid: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<Value, String> {
        let mut params = vec![("channel", cid), ("text", text)];
        if let Some(t) = thread_ts {
            params.push(("thread_ts", t));
        }
        let v = self.call("chat.postMessage", &params)?;
        v.get("message")
            .cloned()
            .ok_or_else(|| "chat.postMessage: no message in the answer".to_string())
    }

    /// Delete one of your own messages.
    pub fn delete_message(&self, cid: &str, ts: &str) -> Result<(), String> {
        self.call("chat.delete", &[("channel", cid), ("ts", ts)])
            .map(|_| ())
    }

    /// Add or remove your reaction `name` on the message `ts` in `cid`.
    pub fn react(&self, cid: &str, ts: &str, name: &str, add: bool) -> Result<(), String> {
        let method = if add {
            "reactions.add"
        } else {
            "reactions.remove"
        };
        self.call(
            method,
            &[("channel", cid), ("timestamp", ts), ("name", name)],
        )
        .map(|_| ())
    }

    /// Leave a channel or group DM.
    pub fn leave(&self, cid: &str) -> Result<(), String> {
        self.call("conversations.leave", &[("channel", cid)])
            .map(|_| ())
    }

    /// Channel ids muted in Slack itself, from the notification preferences.
    pub fn muted_channels(&self) -> Result<Vec<String>, String> {
        let v = self.call("users.prefs.get", &[])?;
        let raw = v
            .pointer("/prefs/all_notifications_prefs")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let prefs: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
        Ok(prefs
            .pointer("/channels")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| v.get("muted").and_then(Value::as_bool).unwrap_or(false))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The workspace's custom emoji image URLs and aliases.
    pub fn emoji_list(&self) -> Result<crate::custom_emoji::Catalog, String> {
        let v = self.call("emoji.list", &[])?;
        Ok(v.get("emoji")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(name, value)| value.as_str().map(|value| (name.clone(), value.to_string()))).collect())
            .unwrap_or_default())
    }

    /// Unread state per conversation, as the web client fetches it.
    pub fn counts(&self) -> Result<Value, String> {
        self.call("client.counts", &[("thread_counts_by_channel", "false")])
    }
}

/// `https://x.slack.com/archives/C123/p1788423554556689` -> (C123, 1788423554.556689).
pub fn parse_permalink(link: &str) -> Option<(String, String)> {
    let rest = link.split("/archives/").nth(1)?;
    let mut it = rest.split('/');
    let cid = it.next()?.to_string();
    let p = it.next()?.split(['?', '#']).next()?.strip_prefix('p')?;
    if p.len() < 11 || !p.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (secs, micros) = p.split_at(p.len() - 6);
    Some((cid, format!("{secs}.{micros}")))
}

#[cfg(test)]
mod tests {
    use super::parse_permalink;

    #[test]
    fn permalinks_split_into_channel_and_timestamp() {
        assert_eq!(
            parse_permalink("https://myorg.slack.com/archives/C0EXAMPLE01/p1788423810534599"),
            Some(("C0EXAMPLE01".to_string(), "1788423810.534599".to_string()))
        );
        assert_eq!(
            parse_permalink("https://myorg.slack.com/archives/D0EXAMPLE02/p1788423562398749?thread_ts=1788423554.556689&cid=D0EXAMPLE02"),
            Some(("D0EXAMPLE02".to_string(), "1788423562.398749".to_string()))
        );
        assert_eq!(
            parse_permalink("https://myorg.slack.com/archives/C1/p12"),
            None
        );
        assert_eq!(parse_permalink("https://example.com/x"), None);
    }
}
