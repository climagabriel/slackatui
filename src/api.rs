//! A small blocking Slack Web API client, used from background threads. A
//! session token needs its cookie on every call; rate limits are honoured
//! once per call.

use std::time::Duration;

use serde_json::Value;

use crate::auth::Auth;

pub struct Client {
    agent: ureq::Agent,
    auth: Auth,
    unread_rotation: std::sync::atomic::AtomicUsize,
    unread_counts: std::sync::Mutex<std::collections::HashMap<String, (String, i64)>>,
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
            unread_counts: Default::default(),
            unread_rotation: Default::default(),
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

    /// Every match Slack will give for `query`, up to `cap`, following the
    /// pagination cursor. The flag says whether Slack ran out first: only
    /// then is the answer the whole of what Slack holds, which is what lets
    /// a caller call the difference against it "only in cache".
    pub fn search_all(&self, query: &str, cap: usize) -> Result<(Vec<Value>, bool), String> {
        let mut out: Vec<Value> = Vec::new();
        let mut cursor = "*".to_string();
        loop {
            let response = self.call(
                "search.messages",
                &[
                    ("query", query),
                    ("count", "100"),
                    ("sort", "timestamp"),
                    ("sort_dir", "desc"),
                    ("cursor", cursor.as_str()),
                    ("highlight", "false"),
                ],
            )?;
            if let Some(matches) = response.pointer("/messages/matches").and_then(Value::as_array) {
                out.extend(matches.iter().cloned());
            }
            let next = response
                .pointer("/messages/pagination/next_cursor")
                .or_else(|| response.pointer("/messages/paging/next_cursor"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let Some(next) = next else {
                out.truncate(cap);
                return Ok((out, true));
            };
            // A cursor Slack hands back unchanged would page for ever.
            if next == cursor {
                out.truncate(cap);
                return Ok((out, true));
            }
            if out.len() >= cap {
                out.truncate(cap);
                return Ok((out, false));
            }
            cursor = next;
        }
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

    /// Leave a channel or group DM.
    pub fn leave(&self, cid: &str) -> Result<(), String> {
        self.call("conversations.leave", &[("channel", cid)])
            .map(|_| ())
    }

    /// Conversation stars only; saved messages and files do not star their channels.
    pub fn starred_channels(&self) -> Result<Vec<String>, String> {
        let mut ids = Vec::new();
        let mut cursor = String::new();
        let mut seen = std::collections::HashSet::new();
        loop {
            let response = self.call("stars.list", &[("limit", "100"), ("cursor", &cursor)])?;
            let items = response.get("items").and_then(Value::as_array).ok_or("stars.list: missing items")?;
            for item in items {
                if matches!(item.get("type").and_then(Value::as_str), Some("channel" | "group" | "im" | "mpim")) {
                    let id = item.get("channel").or_else(|| item.get("group")).and_then(Value::as_str).ok_or("stars.list: missing conversation ID")?;
                    ids.push(id.to_string());
                }
            }
            cursor = response.pointer("/response_metadata/next_cursor").and_then(Value::as_str).unwrap_or("").to_string();
            if cursor.is_empty() { break; }
            if !seen.insert(cursor.clone()) { return Err("stars.list: repeated cursor".into()); }
        }
        Ok(ids)
    }

    pub fn set_starred(&self, cid: &str, starred: bool) -> Result<Vec<String>, String> {
        let method = if starred { "stars.add" } else { "stars.remove" };
        if let Err(error) = self.call(method, &[("channel", cid)]) {
            let harmless = format!("{method}: {}", if starred { "already_starred" } else { "not_starred" });
            if error != harmless { return Err(error); }
        }
        let ids = self.starred_channels()?;
        if ids.iter().any(|id| id == cid) != starred {
            return Err("Star change was not confirmed by Slack".into());
        }
        Ok(ids)
    }

    /// Server notification preferences; malformed responses must not clear the UI.
    pub fn muted_channels(&self) -> Result<Vec<String>, String> {
        muted_ids(&self.call("users.prefs.get", &[("prefs", "all_notifications_prefs")])?)
    }

    pub fn set_muted(&self, cid: &str, muted: bool) -> Result<Vec<String>, String> {
        update_mute(cid, muted, |method, params| self.call(method, params))
    }

    /// Fetch at most ten new counts per poll; unchanged snapshots reuse counts.
    pub fn enrich_unread_counts(&self, mut snapshot: Value, targets: &[String]) -> Result<Value, String> {
        let mut cache = self.unread_counts.lock().unwrap_or_else(|error| error.into_inner());
        let mut budget = 10;
        let mut entries: Vec<&mut Value> = snapshot.as_object_mut().into_iter()
            .flat_map(|object| object.iter_mut())
            .filter(|(key, _)| ["channels", "ims", "mpims"].contains(&key.as_str()))
            .flat_map(|(_, value)| value.as_array_mut().into_iter().flatten())
            .filter(|conversation| conversation["id"].as_str().is_some_and(|id| targets.iter().any(|target| target == id)))
            .collect();
        if !entries.is_empty() {
            let offset = self.unread_rotation.fetch_add(10, std::sync::atomic::Ordering::Relaxed) % entries.len();
            entries.rotate_left(offset);
        }
        for conversation in entries {
            let Some(id) = conversation["id"].as_str().map(str::to_owned) else { continue };
            if conversation["has_unreads"].as_bool() != Some(true) {
                cache.remove(&id);
                conversation["unread_count"] = 0.into();
                continue;
            }
            let Some(marker) = conversation["last_read"].as_str() else { continue };
            let Some(fingerprint) = unread_fingerprint(conversation) else { continue };
            if let Some((_, count)) = cache.get(&id).filter(|(old, _)| *old == fingerprint) {
                conversation["unread_count"] = (*count).into();
                continue;
            }
            if budget == 0 { continue; }
            budget -= 1;
            let response = self.call("conversations.history", &[
                ("channel", &id), ("oldest", marker), ("inclusive", "false"), ("limit", "10"),
            ]);
            let Ok(response) = response else { continue };
            let Some(messages) = response["messages"].as_array() else { continue };
            let count = messages.len().min(10) as i64;
            // A short, incomplete page cannot prove an exact unread count.
            if count < 10 && (response["has_more"].as_bool() == Some(true)
                || response.pointer("/response_metadata/next_cursor").and_then(Value::as_str).is_some_and(|cursor| !cursor.is_empty())) { continue; }
            cache.insert(id, (fingerprint, count));
            conversation["unread_count"] = count.into();
        }
        Ok(snapshot)
    }

    /// Unread state per conversation, as the web client fetches it.
    pub fn counts(&self) -> Result<Value, String> {
        self.call("client.counts", &[("thread_counts_by_channel", "false")])
    }
}

pub(crate) fn unread_fingerprint(conversation: &Value) -> Option<String> {
    let marker = conversation["last_read"].as_str()?;
    let latest = conversation["latest"].as_str()?;
    crate::archive::ts_to_id(marker)?;
    crate::archive::ts_to_id(latest)?;
    Some(format!("{marker}|{latest}|{}", conversation["history_invalid"]))
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
    #[test]
    fn unread_lookups_have_a_budget_and_cover_more_than_one_batch() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let client = super::Client::for_test(move |method, params| {
            assert_eq!(method, "conversations.history");
            seen.lock().unwrap().push(params.iter().find(|(key,_)| *key == "channel").unwrap().1.to_string());
            Ok(serde_json::json!({"messages":[{"ts":"2.000000"}],"has_more":false}))
        });
        let targets: Vec<String> = (0..25).map(|n| format!("C{n}")).collect();
        let snapshot = serde_json::json!({"channels": targets.iter().map(|id| serde_json::json!({"id":id,"has_unreads":true,"last_read":"1.000000","latest":"2.000000"})).collect::<Vec<_>>()});
        for _ in 0..3 {
            let before = calls.lock().unwrap().len();
            client.enrich_unread_counts(snapshot.clone(), &targets).unwrap();
            assert!(calls.lock().unwrap().len() - before <= 10);
        }
        assert_eq!(calls.lock().unwrap().len(), 25);
        client.enrich_unread_counts(snapshot.clone(), &targets).unwrap();
        assert_eq!(calls.lock().unwrap().len(), 25);
        let mut changed = snapshot;
        changed["channels"][0]["history_invalid"] = serde_json::json!("changed");
        client.enrich_unread_counts(changed, &targets).unwrap();
        assert_eq!(calls.lock().unwrap().len(), 26);
    }

    #[test]
    fn unread_counts_are_bounded_cached_and_do_not_guess_incomplete_pages() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let client = super::Client::for_test(move |method, params| {
            if method == "client.counts" {
                return Ok(serde_json::json!({"channels": [
                    {"id":"C1","has_unreads":true,"last_read":"1.000000","latest":"20.000000"},
                    {"id":"C2","has_unreads":true,"last_read":"1.000000","latest":"20.000000"},
                    {"id":"C3","has_unreads":false}
                ]}));
            }
            assert_eq!(method, "conversations.history");
            assert!(params.contains(&("limit", "10")));
            assert!(params.contains(&("oldest", "1.000000")));
            assert!(params.contains(&("inclusive", "false")));
            seen.fetch_add(1, Ordering::SeqCst);
            if params.contains(&("channel", "C2")) {
                Ok(serde_json::json!({"messages":[],"has_more":true}))
            } else {
                Ok(serde_json::json!({"messages":vec![serde_json::json!({"ts":"2.000000"});10],"has_more":true}))
            }
        });
        let targets = vec!["C1".into(),"C2".into(),"C3".into()];
        for _ in 0..2 {
            let snapshot = client.enrich_unread_counts(client.counts().unwrap(), &targets).unwrap();
            assert_eq!(snapshot["channels"][0]["unread_count"], 10);
            assert!(snapshot["channels"][1]["unread_count"].is_null());
            assert_eq!(snapshot["channels"][2]["unread_count"], 0);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        client.enrich_unread_counts(client.counts().unwrap(), &[]).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    use super::parse_permalink;

    #[test]
    fn stars_paginate_ignore_messages_and_verify_channel_only_writes() {
        let client = super::Client::for_test(|method, params| {
            assert_eq!(method, "stars.list");
            if params.contains(&("cursor", "")) {
                Ok(serde_json::json!({"items":[{"type":"channel","channel":"C1"},{"type":"message","channel":"C2"}],"response_metadata":{"next_cursor":"page2"}}))
            } else {
                assert!(params.contains(&("cursor", "page2")));
                Ok(serde_json::json!({"items":[{"type":"im","channel":"D1"},{"type":"group","group":"G1"}],"response_metadata":{"next_cursor":""}}))
            }
        });
        assert_eq!(client.starred_channels().unwrap(), vec!["C1", "D1", "G1"]);
        for starred in [true, false] {
            let client = super::Client::for_test(move |method, params| {
                if method == "stars.list" {
                    Ok(serde_json::json!({"items":if starred {vec![serde_json::json!({"type":"channel","channel":"C1"})]}else{vec![]}}))
                } else {
                    assert_eq!(method, if starred {"stars.add"} else {"stars.remove"});
                    assert_eq!(params, &[("channel", "C1")]);
                    Ok(serde_json::json!({"ok":true}))
                }
            });
            assert_eq!(client.set_starred("C1", starred).unwrap().contains(&"C1".into()), starred);
        }
        let client = super::Client::for_test(|method, _| {
            if method == "stars.add" { Ok(serde_json::json!({"ok":true})) }
            else { Ok(serde_json::json!({"items":[]})) }
        });
        assert!(client.set_starred("C1", true).is_err());
        let client = super::Client::for_test(|_, _| Err("write denied".into()));
        assert!(client.set_starred("C1", true).is_err());
    }

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
