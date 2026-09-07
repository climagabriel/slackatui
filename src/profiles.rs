//! Workspace-scoped, minimal user directory cache. No tokens or emails on disk.
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const TTL: u64 = 24 * 60 * 60;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn minimal_groups_include_disabled_and_prefer_handles() {
        let groups = fetch_groups(json!({"usergroups":[
            {"id":"S1", "handle":" @oncall ", "name":"On Call", "users":["U1"], "description":"private"},
            {"id":"S2", "handle":"", "name":" Legacy ", "date_delete":42},
            {"id":"S3"}, {"id":""}, {}
        ]})).unwrap();
        assert_eq!(
            groups,
            vec![
                json!({"id":"S1", "name":"oncall"}),
                json!({"id":"S2", "name":"Legacy"}),
                json!({"id":"S3", "name":"S3"})
            ]
        );
        assert!(fetch_groups(json!({})).is_err());
        assert!(fetch_groups(json!({"usergroups":[]})).unwrap().is_empty());
    }

    #[test]
    fn pagination_and_minimal_profiles() {
        let mut cursors = Vec::new();
        let users = fetch(|cursor| {
            cursors.push(cursor.to_string());
            Ok(if cursor.is_empty() { json!({"members": [{"id":"U1", "name":"handle", "profile":{"display_name":" Ada ", "email":"private"}}], "response_metadata":{"next_cursor":"next"}}) }
                else { json!({"members":[{"id":"U2", "name":"bot", "deleted":true, "is_bot":true}]}) })
        }).unwrap();
        assert_eq!(cursors, ["", "next"]);
        assert_eq!(users[0], json!({"id":"U1", "name":"Ada", "is_bot":false}));
        assert_eq!(users[1]["name"], "bot (gone)");
        assert_eq!(users[1]["is_bot"], true);
        assert!(
            fetch(|_| Ok(json!({"members":[], "response_metadata":{"next_cursor":"loop"}})))
                .is_err()
        );
        assert!(fetch(|_| Ok(json!({}))).is_err());
    }

    #[test]
    fn cache_reuse_isolation_stale_failure_and_permissions() {
        let root = std::env::temp_dir().join(format!(
            "slack-profiles-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let users = vec![json!({"id":"U1", "name":"Ada", "is_bot":false})];
        assert_eq!(
            load_or_fetch(&root, "T1", || Ok(users.clone())).unwrap().0,
            users
        );
        let groups = vec![json!({"id":"S1", "name":"oncall"})];
        assert_eq!(
            load_groups(&root, "T1", || Ok(groups.clone())).unwrap().0,
            groups
        );
        assert_eq!(
            load_groups(&root, "T1", || panic!("fresh group cache fetched"))
                .unwrap()
                .0,
            groups
        );
        assert!(load_groups(&root, "T2", || Err("missing_scope".into())).is_err());
        assert!(load_groups(&root, "../escape", || panic!()).is_err());
        let group_path = root.join("usergroups/T1.json");
        assert_eq!(
            std::fs::metadata(&group_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        save(
            &group_path,
            &json!({"team":"T1", "fetched":0, "groups":groups}),
        )
        .unwrap();
        let group_before = std::fs::read(&group_path).unwrap();
        let (cached, warning) = load_groups(&root, "T1", || Err("missing_scope".into())).unwrap();
        assert_eq!(cached, groups);
        assert!(warning.unwrap().contains("using stale cache"));
        assert_eq!(std::fs::read(&group_path).unwrap(), group_before);
        std::fs::write(&group_path, "broken").unwrap();
        assert_eq!(
            load_groups(&root, "T1", || Ok(groups.clone())).unwrap().0,
            groups
        );
        assert_eq!(
            load_or_fetch(&root, "T1", || panic!("fresh cache fetched"))
                .unwrap()
                .0,
            users
        );
        assert!(load_or_fetch(&root, "T2", || Err("denied".into())).is_err());
        assert!(load_or_fetch(&root, "../escape", || panic!()).is_err());
        let path = root.join("profiles/T1.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        save(&path, &json!({"team":"T1", "fetched":0, "users":users})).unwrap();
        let before = std::fs::read(&path).unwrap();
        let (cached, warning) = load_or_fetch(&root, "T1", || Err("rate limited".into())).unwrap();
        assert_eq!(cached, users);
        assert!(warning.unwrap().contains("using stale cache"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::write(&path, "broken").unwrap();
        assert_eq!(
            load_or_fetch(&root, "T1", || Ok(users.clone())).unwrap().0,
            users
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

pub fn normalize(v: &Value) -> Option<Value> {
    let id = v.get("id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    let name = [
        v.pointer("/profile/display_name"),
        v.pointer("/profile/real_name"),
        v.get("real_name"),
        v.get("name"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .map(str::trim)
    .find(|s| !s.is_empty())
    .unwrap_or(id);
    let name = if v["deleted"] == true {
        format!("{name} (gone)")
    } else {
        name.to_string()
    };
    Some(json!({"id": id, "name": name, "is_bot": v["is_bot"].as_bool().unwrap_or(false)}))
}

pub fn fetch(mut call: impl FnMut(&str) -> Result<Value, String>) -> Result<Vec<Value>, String> {
    let mut users = Vec::new();
    let mut cursor = String::new();
    let mut seen = HashSet::new();
    loop {
        let page = call(&cursor)?;
        let members = page["members"]
            .as_array()
            .ok_or("users.list: missing members")?;
        users.extend(members.iter().filter_map(normalize));
        cursor = page
            .pointer("/response_metadata/next_cursor")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if cursor.is_empty() {
            return Ok(users);
        }
        if !seen.insert(cursor.clone()) {
            return Err("users.list: repeated pagination cursor".into());
        }
    }
}

pub fn load_or_fetch(
    root: &Path,
    team: &str,
    fetch: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Result<(Vec<Value>, Option<String>), String> {
    load_directory(root, team, "profiles", "users", fetch)
}

pub fn fetch_groups(response: Value) -> Result<Vec<Value>, String> {
    let groups = response["usergroups"]
        .as_array()
        .ok_or("usergroups.list: missing usergroups")?;
    Ok(groups
        .iter()
        .filter_map(|v| {
            let id = v["id"].as_str()?.trim();
            if id.is_empty() {
                return None;
            }
            let name = [v.get("handle"), v.get("name")]
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::trim)
                .map(|s| s.trim_start_matches('@'))
                .find(|s| !s.is_empty())
                .unwrap_or(id);
            Some(json!({"id":id, "name":name}))
        })
        .collect())
}

pub fn load_groups(
    root: &Path,
    team: &str,
    fetch: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Result<(Vec<Value>, Option<String>), String> {
    load_directory(root, team, "usergroups", "groups", fetch)
}

fn load_directory(
    root: &Path,
    team: &str,
    directory: &str,
    key: &str,
    fetch: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Result<(Vec<Value>, Option<String>), String> {
    if team.is_empty() || !team.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!("{directory}: invalid workspace id"));
    }
    let path = root.join(directory).join(format!("{team}.json"));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cached = std::fs::read(&path)
        .ok()
        .and_then(|s| serde_json::from_slice::<Value>(&s).ok())
        .filter(|v| {
            v["team"] == team
                && v[key].as_array().is_some_and(|a| {
                    a.iter()
                        .all(|u| u["id"].is_string() && u["name"].is_string())
                })
        });
    if let Some(v) = &cached {
        if v["fetched"]
            .as_u64()
            .is_some_and(|t| t <= now && now - t < TTL)
        {
            return Ok((v[key].as_array().unwrap().clone(), None));
        }
    }
    let users = match fetch() {
        Ok(users) => users,
        Err(e) => {
            return cached
                .map(|v| {
                    (
                        v[key].as_array().unwrap().clone(),
                        Some(format!("{directory}: {e}; using stale cache")),
                    )
                })
                .ok_or(e)
        }
    };
    let warning = save(&path, &json!({"team": team, "fetched": now, (key): users}))
        .err()
        .map(|e| format!("{directory}: cannot save cache: {e}"));
    Ok((users, warning))
}

fn save(path: &Path, value: &Value) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path.parent().unwrap())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let part = path.with_extension(format!("{}.{}.tmp", std::process::id(), stamp));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&part)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        std::fs::rename(&part, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}
