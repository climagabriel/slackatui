//! A small blocking Slack Web API client, used from background threads. A
//! session token needs its cookie on every call; rate limits are honoured
//! once per call.

use std::time::Duration;

use serde_json::Value;

use crate::auth::Auth;

pub struct Client {
    agent: ureq::Agent,
    auth: Auth,
    #[cfg(test)]
    mock: Option<Box<dyn Fn(&str, &[(&str, &str)]) -> Result<Value, String> + Send + Sync>>,
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
            #[cfg(test)]
            mock: None,
        }
    }

    #[cfg(test)]
    pub fn for_test(mock: impl Fn(&str, &[(&str, &str)]) -> Result<Value, String> + Send + Sync + 'static) -> Self {
        let mut client = Self::new(Auth { token: String::new(), cookie: String::new(), source: "test".into() });
        client.mock = Some(Box::new(mock));
        client
    }

    /// One Web API call; the JSON on `ok`, the `error` field otherwise.
    pub fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value, String> {
        #[cfg(test)]
        if let Some(mock) = &self.mock { return mock(method, params); }
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

    /// Canvas export is HTML; keep it separate from binary file downloads.
    pub fn download_canvas(&self, url: &str) -> Result<String, String> {
        let host = url.strip_prefix("https://").and_then(|s| s.split('/').next()).unwrap_or("");
        if host != "files.slack.com" { return Err("canvas export must be on files.slack.com".into()); }
        let mut response = self.agent.get(url)
            .config().max_redirects(0).build()
            .header("Authorization", &format!("Bearer {}", self.auth.token))
            .header("Cookie", &format!("d={}", self.auth.cookie))
            .call().map_err(|e| e.to_string())?;
        if !response.status().is_success() { return Err(format!("canvas export: HTTP {}", response.status())); }
        response.body_mut().with_config().limit(4 * 1024 * 1024).read_to_string().map_err(|e| e.to_string())
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

    /// Server notification preferences; malformed responses must not clear the UI.
    pub fn muted_channels(&self) -> Result<Vec<String>, String> {
        muted_ids(&self.call("users.prefs.get", &[("prefs", "all_notifications_prefs")])?)
    }

    pub fn set_muted(&self, cid: &str, muted: bool) -> Result<Vec<String>, String> {
        update_mute(cid, muted, |method, params| self.call(method, params))
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

fn muted_ids(response: &Value) -> Result<Vec<String>, String> {
    let raw = response
        .pointer("/prefs/all_notifications_prefs")
        .ok_or("users.prefs.get: missing notification preferences")?;
    let parsed;
    let prefs = if let Some(text) = raw.as_str() {
        parsed = serde_json::from_str::<Value>(text)
            .map_err(|_| "users.prefs.get: invalid notification preferences JSON")?;
        &parsed
    } else {
        raw
    };
    let channels = prefs
        .get("channels")
        .and_then(Value::as_object)
        .ok_or("users.prefs.get: missing channels in notification preferences")?;
    let mut ids = Vec::new();
    for (id, settings) in channels {
        if !settings.is_object() {
            return Err("users.prefs.get: invalid conversation preference".into());
        }
        match settings.get("muted") {
            Some(Value::Bool(true)) => ids.push(id.clone()),
            Some(Value::Bool(false)) | None => {}
            _ => return Err("users.prefs.get: invalid muted flag".into()),
        }
    }
    Ok(ids)
}

/// Set one override, then independently read it back. Never upload the whole prefs object.
fn update_mute(
    cid: &str,
    muted: bool,
    mut call: impl FnMut(&str, &[(&str, &str)]) -> Result<Value, String>,
) -> Result<Vec<String>, String> {
    if cid.len() < 2
        || !cid.starts_with(['C', 'D', 'G'])
        || !cid
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return Err("mute: invalid conversation ID".into());
    }
    call(
        "users.prefs.setNotifications",
        &[
            ("channel_id", cid),
            ("name", "muted"),
            ("value", if muted { "true" } else { "false" }),
            ("global", "false"),
            ("sync", "false"),
        ],
    )?;
    let response = call("users.prefs.get", &[("prefs", "all_notifications_prefs")])
        .map_err(|error| format!("mute update accepted, but verification failed: {error}"))?;
    let ids = muted_ids(&response)
        .map_err(|error| format!("mute update accepted, but verification failed: {error}"))?;
    if ids.iter().any(|id| id == cid) != muted {
        return Err("Slack has not confirmed the requested mute state; preferences will be rechecked".into());
    }
    Ok(ids)
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
    #[test]
    fn mute_writes_one_override_and_checks_the_server() {
        use super::update_mute;
        use serde_json::json;
        for (cid, muted) in [("C123", true), ("D123", false), ("G123", true)] {
            let mut calls = 0;
            let ids=update_mute(cid,muted,|method,params|{
                calls+=1;
                if calls==1 {
                    assert_eq!(method,"users.prefs.setNotifications");
                    assert_eq!(params,&[("channel_id",cid),("name","muted"),("value",if muted{"true"}else{"false"}),("global","false"),("sync","false")]);
                    Ok(json!({"ok":true}))
                } else {
                    assert_eq!(method,"users.prefs.get");
                    assert_eq!(params,&[("prefs","all_notifications_prefs")]);
                    Ok(json!({"prefs":{"all_notifications_prefs":json!({"channels":{cid:{"muted":muted},"COTHER":{"muted":true,"desktop":"all"}}}).to_string()}}))
                }
            }).unwrap();
            assert_eq!(calls, 2);
            assert!(ids.contains(&"COTHER".into()));
            assert_eq!(ids.iter().any(|id| id == cid), muted);
        }
    }
    #[test]
    fn mute_rejects_failed_writes_unverified_updates_and_invalid_preferences() {
        use super::{muted_ids, update_mute};
        use serde_json::json;
        assert!(update_mute("bad/id", true, |_, _| panic!(
            "invalid id must not reach Slack"
        ))
        .is_err());
        let mut calls = 0;
        let result = update_mute("C1", true, |_, _| {
            calls += 1;
            Err("write denied".into())
        });
        assert_eq!(calls, 1);
        assert_eq!(result.unwrap_err(), "write denied");
        let result = update_mute("C1", true, |method, _| {
            if method == "users.prefs.get" {
                Err("offline".into())
            } else {
                Ok(json!({"ok":true}))
            }
        });
        assert!(result.unwrap_err().contains("verification failed"));
        let result = update_mute("C1", true, |_, _| {
            Ok(json!({"prefs":{"all_notifications_prefs":{"channels":{}}}}))
        });
        assert!(result.unwrap_err().contains("not confirmed"));
        for bad in [
            json!({}),
            json!({"prefs":{"all_notifications_prefs":"not json"}}),
            json!({"prefs":{"all_notifications_prefs":{"channels":{"C1":{"muted":"true"}}}}}),
        ] {
            assert!(muted_ids(&bad).is_err());
        }
        assert_eq!(
            muted_ids(
                &json!({"prefs":{"all_notifications_prefs":{"channels":{"C1":{"desktop":"all"}}}}})
            )
            .unwrap(),
            Vec::<String>::new()
        );
    }
}
