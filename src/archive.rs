//! The slackdump archives on disk: discovery, conversation inventory, and
//! the message queries the views run. Everything is read-only; a resume may
//! be writing the same database (WAL), so connections open with a busy
//! timeout and never touch the schema.

use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use serde_json::Value;

/// Archive sets under the root, in listing order.
pub const ARCHIVE_SETS: [&str; 2] = ["full", "dms"];
/// Messages fetched per timeline page.
pub const PAGE: usize = 200;
/// Cap on search candidates fetched from SQL.
pub const SEARCH_CAP: usize = 500;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    Channel,
    Private,
    Mpim,
    Im,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Channel => "channel",
            Kind::Private => "private channel",
            Kind::Mpim => "group message",
            Kind::Im => "direct message",
        }
    }
}

pub struct User {
    pub name: String,
    pub is_bot: bool,
}

pub struct Archive {
    /// `full/team-alpha_20260428`: the directory relative to the root.
    pub rel: String,
    pub dir: PathBuf,
    pub conn: Connection,
    users: RefCell<Option<HashMap<String, User>>>,
    pub channel_names: HashMap<String, String>,
}

pub struct Conv {
    pub archive: usize,
    pub id: String,
    /// Display name: `#team-alpha`, `@oliver.hendricks`, `@a,b` for group DMs.
    pub name: String,
    pub kind: Kind,
    pub archived: bool,
    /// Distinct message timestamps, the same figure `slq cache list` shows.
    pub msgs: i64,
    /// Messages written by the archive's owner, replies included.
    pub mine: i64,
    /// The same messages weighted by recency: each counts 2^(-age / half-life).
    pub score: f64,
    pub first_id: i64,
    pub last_id: i64,
    /// Known from Slack only: a member conversation no archive holds.
    pub live_only: bool,
    /// `/leave` succeeded: no longer a member, the archive stays readable.
    pub left: bool,
    /// Muted in Slack, from the last confirmed notification-preference snapshot.
    pub muted: bool,
    pub unread: bool,
    pub mentions: i64,
    /// Slack's read marker for the owner, as a message id; 0 when unknown.
    pub last_read: i64,
}

pub struct Corpus {
    pub root: PathBuf,
    pub archives: Vec<Archive>,
    pub convs: Vec<Conv>,
    pub workspace_url: String,
    /// The archive owner's user id: `$SLACK_SELF_USER_ID`, else the user
    /// present in every direct message of the DM archive.
    pub me: Option<String>,
    /// Channel id -> name across every archive: a mention of a channel
    /// archived elsewhere still gets its name.
    pub channel_names: HashMap<String, String>,
    pub half_life_days: f64,
    pub usergroups: HashMap<String, String>,
    /// The workspace's users, from the archive with the most of them: every
    /// archive stores the whole list, so one table answers for all.
    users: HashMap<String, User>,
}

/// One message, with the copies slackdump keeps of a thread parent folded in.
#[derive(Clone, Debug)]
pub struct Msg {
    /// Numeric timestamp: `1782927057.158059` -> 1782927057158059. Orders and pages.
    pub id: i64,
    pub ts: String,
    pub channel_id: String,
    /// The thread this message belongs to when it is a reply (or a reply
    /// broadcast to the channel); None for top-level messages.
    pub parent_id: Option<i64>,
    pub thread_ts: Option<String>,
    /// A thread-fetch copy exists: the archive walked this thread.
    pub is_parent: bool,
    /// Slack's own reply count from the JSON, the largest across copies.
    pub reply_count: i64,
    /// Replies present in the archive, filled by `reply_stats`.
    pub archived_replies: i64,
    pub latest_reply_id: Option<i64>,
    pub user: Option<String>,
    pub subtype: Option<String>,
    pub text: String,
    pub data: Value,
    pub edited: bool,
    /// `thread_broadcast`: a reply that was also sent to the channel.
    pub broadcast: bool,
    /// Set on a live search hit: the conversation it came from.
    pub channel_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct FileInfo {
    pub id: String,
    /// The conversation the message carrying the file belongs to.
    pub channel: String,
    pub name: String,
    pub filetype: String,
    pub size: Option<i64>,
    pub mode: String,
    pub mimetype: String,
    pub width: u32,
    pub height: u32,
    /// A small rendition and the original, for a download.
    pub thumb: Option<String>,
    pub url: Option<String>,
}

impl FileInfo {
    pub fn is_image(&self) -> bool {
        self.mimetype.starts_with("image/") && !self.mimetype.contains("svg")
    }

    /// The extension the cached copy gets.
    pub fn ext(&self) -> String {
        self.name
            .rsplit('.')
            .next()
            .filter(|e| e.len() <= 5)
            .unwrap_or("bin")
            .to_lowercase()
    }
}

impl Msg {
    pub fn secs(&self) -> i64 {
        self.id / 1_000_000
    }

    pub fn thread_root(&self) -> i64 {
        self.parent_id.unwrap_or(self.id)
    }

    pub fn has_thread(&self) -> bool {
        self.is_parent || self.reply_count > 0 || self.archived_replies > 0
    }

    pub fn is_bot(&self) -> bool {
        self.user.is_none()
            || self.subtype.as_deref() == Some("bot_message")
            || self.data.get("bot_id").and_then(Value::as_str).is_some()
    }

    pub fn files(&self) -> Vec<FileInfo> {
        let mut out = Vec::new();
        if let Some(files) = self.data.get("files").and_then(Value::as_array) {
            for f in files {
                let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let mut name = s("title");
                if name.is_empty() {
                    name = s("name");
                }
                if name.is_empty() {
                    name = s("id");
                }
                let n = |k: &str| {
                    f.get(k)
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        .min(u32::MAX as u64) as u32
                };
                let opt = |k: &str| {
                    f.get(k)
                        .and_then(Value::as_str)
                        .filter(|u| !u.is_empty())
                        .map(str::to_string)
                };
                // A width and a height from the same rendition, or none.
                let (width, height) = if n("original_w") > 0 && n("original_h") > 0 {
                    (n("original_w"), n("original_h"))
                } else if n("thumb_360_w") > 0 && n("thumb_360_h") > 0 {
                    (n("thumb_360_w"), n("thumb_360_h"))
                } else {
                    (0, 0)
                };
                out.push(FileInfo {
                    id: s("id"),
                    channel: self.channel_id.clone(),
                    name,
                    filetype: s("filetype"),
                    size: f.get("size").and_then(Value::as_i64),
                    mode: s("mode"),
                    mimetype: s("mimetype"),
                    width,
                    height,
                    thumb: opt("thumb_720")
                        .or_else(|| opt("thumb_480"))
                        .or_else(|| opt("thumb_360")),
                    url: opt("url_private_download").or_else(|| opt("url_private")),
                });
            }
        }
        out
    }

    pub fn reactions(&self) -> Vec<(String, i64)> {
        let mut out = Vec::new();
        if let Some(rs) = self.data.get("reactions").and_then(Value::as_array) {
            for r in rs {
                let name = r
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let count = r.get("count").and_then(Value::as_i64).unwrap_or(0);
                out.push((name, count));
            }
        }
        out
    }

    pub fn permalink(&self, workspace_url: &str) -> String {
        let base = workspace_url.trim_end_matches('/');
        let p = self.ts.replace('.', "");
        match (&self.thread_ts, self.parent_id) {
            (Some(t), Some(_)) => format!(
                "{base}/archives/{}/p{p}?thread_ts={t}&cid={}",
                self.channel_id, self.channel_id
            ),
            _ => format!("{base}/archives/{}/p{p}", self.channel_id),
        }
    }
}

fn open_ro(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

pub(crate) fn ts_to_id(ts: &str) -> Option<i64> {
    let (secs, frac) = ts.split_once('.')?;
    let secs: i64 = secs.parse().ok()?;
    let frac: i64 = format!("{frac:0<6}").get(..6)?.parse().ok()?;
    Some(secs * 1_000_000 + frac)
}

impl Corpus {
    pub fn merge_profiles(&mut self, users: Vec<Value>) {
        for u in users {
            if let (Some(id), Some(name)) = (u["id"].as_str(), u["name"].as_str()) {
                self.users.insert(
                    id.to_string(),
                    User {
                        name: name.to_string(),
                        is_bot: u["is_bot"].as_bool().unwrap_or(false),
                    },
                );
            }
        }
    }
    /// Live-only conversations have no archive; their numeric index is a placeholder.
    pub fn conv_archive(&self, conv: &Conv) -> Option<&Archive> {
        if conv.live_only {
            None
        } else {
            self.archives.get(conv.archive)
        }
    }
    pub fn open(
        root: &Path,
        half_life_days: f64,
        stats_cache: Option<PathBuf>,
    ) -> Result<Corpus, String> {
        let mut archives = Vec::new();
        for set in ARCHIVE_SETS {
            let dir = root.join(set);
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut subs: Vec<PathBuf> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.join("slackdump.sqlite").is_file())
                // A hidden directory is an archive still being written.
                .filter(|p| {
                    !p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .starts_with('.')
                })
                .collect();
            subs.sort();
            for sub in subs {
                let rel = format!(
                    "{}/{}",
                    set,
                    sub.file_name().unwrap_or_default().to_string_lossy()
                );
                match Archive::open(rel.clone(), &sub) {
                    Ok(a) => archives.push(a),
                    Err(e) => eprintln!("slack-tui: {rel}: {e}"),
                }
            }
        }
        crate::trace("open: archives opened");
        let me = std::env::var("SLACK_SELF_USER_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                // Only a DM archive can answer; ask those first.
                let mut order: Vec<&Archive> = archives.iter().collect();
                order.sort_by_key(|a| !a.rel.starts_with("dms/"));
                order.into_iter().find_map(|a| a.self_user())
            });
        let workspace_url = archives
            .iter()
            .find_map(|a| a.workspace_url())
            .or_else(crate::auth::selected_workspace_url)
            .unwrap_or_else(|| "https://slack.com".to_string());
        crate::trace("open: me and workspace url");
        let mut cache = stats_cache.map(StatsCache::load);
        let mut convs = Vec::new();
        for (ai, a) in archives.iter_mut().enumerate() {
            match a.scan_convs(ai, me.as_deref(), half_life_days, cache.as_mut()) {
                Ok(mut c) => convs.append(&mut c),
                Err(e) => eprintln!("slack-tui: {}: {e}", a.rel),
            }
        }
        let mut corpus = Corpus {
            root: root.to_path_buf(),
            archives: Vec::new(),
            convs,
            workspace_url,
            channel_names: HashMap::new(),
            me,
            half_life_days,
            users: HashMap::new(),
            usergroups: HashMap::new(),
        };
        if let Some(c) = &cache {
            c.save();
        }
        crate::trace("open: conversations scanned");
        if let Some(best) = archives.iter().max_by_key(|a| a.user_count()) {
            crate::trace("open: user counts done");
            corpus.users = best.load_users().unwrap_or_default();
        }
        crate::trace("open: users loaded");
        for a in archives {
            corpus.learn_channels(&a);
            corpus.archives.push(a);
        }
        Ok(corpus)
    }

    fn learn_channels(&mut self, a: &Archive) {
        for (id, name) in &a.channel_names {
            if !name.is_empty() {
                self.channel_names
                    .entry(id.clone())
                    .or_insert_with(|| name.clone());
            }
        }
    }

    /// Register an archive directory created after startup; returns the
    /// indices of its conversations.
    pub fn add_archive(&mut self, dir: &Path) -> Result<Vec<usize>, String> {
        let rel = dir
            .strip_prefix(&self.root)
            .unwrap_or(dir)
            .to_string_lossy()
            .to_string();
        let mut a = Archive::open(rel, dir).map_err(|e| e.to_string())?;
        let ai = self.archives.len();
        let convs = a
            .scan_convs(ai, self.me.as_deref(), self.half_life_days, None)
            .map_err(|e| e.to_string())?;
        self.learn_channels(&a);
        self.archives.push(a);
        let first = self.convs.len();
        let n = convs.len();
        self.convs.extend(convs);
        Ok((first..first + n).collect())
    }

    /// A user's name from the workspace list.
    pub fn user_name(&self, uid: &str) -> Option<String> {
        self.users.get(uid).map(|u| u.name.clone())
    }

    /// The id behind a handle, for `@name` in an outgoing message.
    pub fn user_id(&self, handle: &str) -> Option<String> {
        self.users
            .iter()
            .find(|(_, u)| u.name.eq_ignore_ascii_case(handle))
            .map(|(id, _)| id.clone())
    }

    pub fn user_is_bot(&self, uid: &str) -> bool {
        self.users.get(uid).is_some_and(|u| u.is_bot)
    }

    /// The archived entry wins when a conversation is listed twice, once
    /// from an archive and once from Slack's membership list.
    pub fn conv_by_channel(&self, cid: &str) -> Option<usize> {
        let mut hits = self.convs.iter().enumerate().filter(|(_, c)| c.id == cid);
        let first = hits.next()?;
        Some(hits.find(|(_, c)| !c.live_only).unwrap_or(first).0)
    }

    /// A conversation by display name, `#kudos` or `kudos`, case-insensitive;
    /// the archived entry wins over a live-only twin.
    pub fn conv_by_name(&self, name: &str) -> Option<usize> {
        let want = name.trim().trim_start_matches(['#', '@']).to_lowercase();
        if want.is_empty() {
            return None;
        }
        let mut hits = self
            .convs
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name.trim_start_matches(['#', '@']).to_lowercase() == want);
        let first = hits.next()?;
        Some(hits.find(|(_, c)| !c.live_only).unwrap_or(first).0)
    }

    #[cfg(test)]
    pub fn stub(channels: &[(&str, &str)]) -> Corpus {
        Corpus {
            root: PathBuf::new(),
            archives: Vec::new(),
            convs: Vec::new(),
            workspace_url: String::new(),
            channel_names: channels
                .iter()
                .map(|(id, name)| (id.to_string(), name.to_string()))
                .collect(),
            me: None,
            half_life_days: 30.0,
            users: HashMap::new(),
            usergroups: HashMap::new(),
        }
    }

    /// Find a conversation by its display name, with or without the `#`/`@`.
    pub fn find_conv(&self, name: &str) -> Option<usize> {
        let want = name.trim_start_matches(['#', '@']).to_lowercase();
        self.convs
            .iter()
            .position(|c| c.name.trim_start_matches(['#', '@']).to_lowercase() == want)
    }
}

impl Archive {
    /// An archive with no database behind it, for tests of the renderer.
    #[cfg(test)]
    pub fn stub(users: &[(&str, &str)], channels: &[(&str, &str)]) -> Archive {
        let map = users
            .iter()
            .map(|(id, name)| {
                (
                    id.to_string(),
                    User {
                        name: name.to_string(),
                        is_bot: false,
                    },
                )
            })
            .collect();
        Archive {
            rel: "test".to_string(),
            dir: PathBuf::new(),
            conn: Connection::open_in_memory().expect("in-memory sqlite"),
            users: RefCell::new(Some(map)),
            channel_names: channels
                .iter()
                .map(|(id, name)| (id.to_string(), name.to_string()))
                .collect(),
        }
    }

    /// Open an archive directory read-only.
    pub fn open(rel: String, dir: &Path) -> rusqlite::Result<Archive> {
        let conn = open_ro(&dir.join("slackdump.sqlite"))?;
        Ok(Archive {
            rel,
            dir: dir.to_path_buf(),
            conn,
            users: RefCell::new(None),
            channel_names: HashMap::new(),
        })
    }

    /// Fresh message stats for one channel: (distinct messages, first id, last id).
    pub fn channel_stats(&self, cid: &str) -> rusqlite::Result<(i64, i64, i64)> {
        self.conn.query_row(
            "SELECT COUNT(DISTINCT TS), IFNULL(MIN(ID), 0), IFNULL(MAX(ID), 0) FROM MESSAGE WHERE CHANNEL_ID = ?1",
            params![cid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    }

    /// The rows a `slackdump search` run wrote: hits across the workspace.
    pub fn search_hits(&self) -> rusqlite::Result<Vec<Msg>> {
        let mut stmt = self.conn.prepare(
            "SELECT CHANNEL_ID, CHANNEL_NAME, TS, DATA FROM SEARCH_MESSAGE ORDER BY CAST(TS AS REAL) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for (cid, name, ts, blob) in rows.flatten() {
            let mut data: Value = serde_json::from_slice(&blob).unwrap_or(Value::Null);
            if data.get("ts").is_none() {
                if let Some(obj) = data.as_object_mut() {
                    obj.insert("ts".to_string(), Value::String(ts.clone()));
                }
            }
            if let Some(mut m) = Msg::from_api(cid, data) {
                m.channel_name = name.filter(|n| !n.is_empty()).map(|n| format!("#{n}"));
                out.push(m);
            }
        }
        Ok(out)
    }

    fn self_user(&self) -> Option<String> {
        // The one user present in every direct-message conversation is us.
        self.conn
            .query_row(
                "SELECT cu.USER_ID FROM CHANNEL_USER cu \
                 JOIN CHANNEL c ON c.ID = cu.CHANNEL_ID \
                 WHERE json_extract(c.DATA, '$.is_im') = 1 \
                 GROUP BY cu.USER_ID ORDER BY COUNT(DISTINCT cu.CHANNEL_ID) DESC LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    fn workspace_url(&self) -> Option<String> {
        self.conn
            .query_row(
                "SELECT URL FROM WORKSPACE ORDER BY CHUNK_ID DESC LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .filter(|u| !u.is_empty())
    }

    fn user_count(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(DISTINCT ID) FROM S_USER", [], |r| r.get(0))
            .unwrap_or(0)
    }

    fn load_users(&self) -> rusqlite::Result<HashMap<String, User>> {
        let mut map = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT ID, USERNAME, json_extract(DATA, '$.real_name'), \
             json_extract(DATA, '$.profile.display_name'), \
             json_extract(DATA, '$.deleted'), json_extract(DATA, '$.is_bot') \
             FROM S_USER WHERE rowid IN (SELECT MAX(rowid) FROM S_USER GROUP BY ID)",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<i64>>(5)?,
            ))
        })?;
        for row in rows.flatten() {
            let (id, username, real, display, deleted, is_bot) = row;
            let mut name = [display, real, username]
                .into_iter()
                .flatten()
                .find(|s| !s.is_empty())
                .unwrap_or_else(|| id.clone());
            if deleted.unwrap_or(0) != 0 {
                name.push_str(" (gone)");
            }
            map.insert(
                id,
                User {
                    name,
                    is_bot: is_bot.unwrap_or(0) != 0,
                },
            );
        }
        Ok(map)
    }

    pub fn users(&self) -> Ref<'_, HashMap<String, User>> {
        if self.users.borrow().is_none() {
            let map = self.load_users().unwrap_or_default();
            *self.users.borrow_mut() = Some(map);
        }
        Ref::map(self.users.borrow(), |o| o.as_ref().expect("users loaded"))
    }

    /// A user's name when this archive knows the user.
    pub fn user(&self, uid: &str) -> Option<String> {
        self.users().get(uid).map(|u| u.name.clone())
    }

    pub fn user_name(&self, uid: &str) -> String {
        if uid == "USLACKBOT" {
            return "Slackbot".to_string();
        }
        self.user(uid).unwrap_or_else(|| uid.to_string())
    }

    /// A bot user (PagerDuty, Jira, ...) posts with a user id whose S_USER row says so.
    pub fn user_is_bot(&self, uid: &str) -> bool {
        self.users().get(uid).is_some_and(|u| u.is_bot)
    }

    pub fn channel_name(&self, cid: &str) -> Option<String> {
        self.channel_names
            .get(cid)
            .filter(|n| !n.is_empty())
            .cloned()
    }

    pub(crate) fn scan_convs(
        &mut self,
        ai: usize,
        me: Option<&str>,
        half_life_days: f64,
        cache: Option<&mut StatsCache>,
    ) -> rusqlite::Result<Vec<Conv>> {
        struct Meta {
            name: String,
            kind: Kind,
            archived: bool,
            im_user: Option<String>,
            members: Vec<String>,
        }
        let mut meta: HashMap<String, Meta> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT ID, NAME, json_extract(DATA, '$.is_im'), json_extract(DATA, '$.is_mpim'), \
                 json_extract(DATA, '$.is_private'), json_extract(DATA, '$.is_archived'), \
                 json_extract(DATA, '$.user') FROM CHANNEL ORDER BY CHUNK_ID",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            })?;
            for (id, name, is_im, is_mpim, is_private, is_archived, im_user) in rows.flatten() {
                let kind = if is_im.unwrap_or(0) != 0 {
                    Kind::Im
                } else if is_mpim.unwrap_or(0) != 0 {
                    Kind::Mpim
                } else if is_private.unwrap_or(0) != 0 {
                    Kind::Private
                } else {
                    Kind::Channel
                };
                let name = name.unwrap_or_default();
                self.channel_names.insert(id.clone(), name.clone());
                meta.insert(
                    id,
                    Meta {
                        name,
                        kind,
                        archived: is_archived.unwrap_or(0) != 0,
                        im_user,
                        members: Vec::new(),
                    },
                );
            }
        }
        {
            let mut stmt = self
                .conn
                .prepare("SELECT DISTINCT CHANNEL_ID, USER_ID FROM CHANNEL_USER")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for (cid, uid) in rows.flatten() {
                if let Some(m) = meta.get_mut(&cid) {
                    m.members.push(uid);
                }
            }
        }
        // The owner's messages per channel: how many, and a recency-weighted
        // score in which a message counts 2^(-age / half-life). The LIKE
        // prefilter keeps the JSON parse to rows that can match: 197 ms -> 56 ms
        // on the largest archive.
        // Message stats and the owner's message times, cached per archive
        // and keyed by the database's size and mtime: only what a refresh
        // touched is recounted.
        let key = self.stat_key();
        let stats = match cache.as_ref().and_then(|c| c.get(&self.rel, &key)) {
            Some(st) => st,
            None => {
                let st = self.compute_stats(me)?;
                if let Some(c) = cache {
                    c.put(&self.rel, &key, &st);
                }
                st
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let half_life_secs = half_life_days.max(0.01) * 86_400.0;
        let mut convs = Vec::new();
        for (cid, st) in stats.channels {
            let (name, kind, archived) = match meta.get(&cid) {
                Some(m) => (
                    self.display_name(&cid, m.kind, &m.name, m.im_user.as_deref(), &m.members, me),
                    m.kind,
                    m.archived,
                ),
                None => (cid.clone(), Kind::Channel, false),
            };
            let score: f64 = st
                .mine
                .iter()
                .map(|&t| 0.5f64.powf((now - t as f64).max(0.0) / half_life_secs))
                .sum();
            convs.push(Conv {
                archive: ai,
                id: cid,
                name,
                kind,
                archived,
                msgs: st.msgs,
                mine: st.mine.len() as i64,
                score,
                first_id: st.first,
                last_id: st.last,
                live_only: false,
                left: false,
                muted: false,
                unread: false,
                mentions: 0,
                last_read: 0,
            });
        }
        convs.sort_by(|x, y| x.id.cmp(&y.id));
        Ok(convs)
    }

    /// Size and mtime of the database and its WAL: what a resume changes.
    fn stat_key(&self) -> String {
        let mut key = String::new();
        for name in ["slackdump.sqlite", "slackdump.sqlite-wal"] {
            match std::fs::metadata(self.dir.join(name)) {
                Ok(md) => {
                    let mtime = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    key.push_str(&format!("{}:{}:", md.len(), mtime));
                }
                Err(_) => key.push_str("-:"),
            }
        }
        key
    }

    /// Per channel: distinct messages, first and last id, and the times of
    /// the owner's messages. The LIKE prefilter keeps the JSON parse to rows
    /// that can match: 197 ms -> 56 ms on the largest archive.
    fn compute_stats(&self, me: Option<&str>) -> rusqlite::Result<ArchiveStats> {
        let mut channels: HashMap<String, ChannelStats> = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT CHANNEL_ID, COUNT(DISTINCT TS), MIN(ID), MAX(ID) FROM MESSAGE GROUP BY CHANNEL_ID",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for (cid, msgs, first, last) in rows.flatten() {
            channels.insert(
                cid,
                ChannelStats {
                    msgs,
                    first,
                    last,
                    mine: Vec::new(),
                },
            );
        }
        if let Some(me) = me {
            let like = format!("%\"user\":\"{}\"%", me.replace(['%', '_'], ""));
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT CHANNEL_ID, TS FROM MESSAGE \
                 WHERE DATA LIKE ?1 AND json_extract(DATA, '$.user') = ?2",
            )?;
            let rows = stmt.query_map(params![like, me], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for (cid, ts) in rows.flatten() {
                if let Some(st) = channels.get_mut(&cid) {
                    if let Some(secs) = ts.split('.').next().and_then(|s| s.parse::<i64>().ok()) {
                        st.mine.push(secs);
                    }
                }
            }
        }
        Ok(ArchiveStats { channels })
    }

    fn display_name(
        &self,
        cid: &str,
        kind: Kind,
        name: &str,
        im_user: Option<&str>,
        members: &[String],
        me: Option<&str>,
    ) -> String {
        match kind {
            Kind::Im => {
                let other = im_user
                    .map(str::to_string)
                    .or_else(|| members.iter().find(|u| Some(u.as_str()) != me).cloned());
                match other {
                    Some(u) if Some(u.as_str()) == me => "@me (self)".to_string(),
                    Some(u) => format!("@{}", self.user_name(&u)),
                    None => format!("@{cid}"),
                }
            }
            Kind::Mpim => {
                let mut others: Vec<String> = members
                    .iter()
                    .filter(|u| Some(u.as_str()) != me)
                    .map(|u| self.user_name(u))
                    .collect();
                if others.is_empty() {
                    // `mpdm-a--b--c-1` without a member list: read the names off the name.
                    let stripped = name.trim_start_matches("mpdm-");
                    let stripped = stripped
                        .rsplit_once("-")
                        .map(|(a, _)| a)
                        .unwrap_or(stripped);
                    others = stripped.split("--").map(str::to_string).collect();
                }
                others.sort();
                if others.is_empty() {
                    format!("@{name}")
                } else {
                    format!("@{}", others.join(","))
                }
            }
            _ => {
                if name.is_empty() {
                    cid.to_string()
                } else {
                    format!("#{name}")
                }
            }
        }
    }

    // ------------------------------------------------------------ messages

    const COLS: &'static str =
        "m.ID, m.CHUNK_ID, m.CHANNEL_ID, m.TS, m.PARENT_ID, m.THREAD_TS, m.IS_PARENT, m.LATEST_REPLY, m.TXT, m.DATA";

    /// Top-level messages: what the channel view shows. A thread parent is
    /// usually stored only as its thread-fetch copy (`PARENT_ID = ID`), and a
    /// reply broadcast to the channel belongs in the channel too.
    const TOP_LEVEL: &'static str =
        "(PARENT_ID IS NULL OR PARENT_ID = ID OR json_extract(DATA, '$.subtype') = 'thread_broadcast')";

    fn rows_to_msgs(rows: Vec<RawRow>) -> Vec<Msg> {
        // Rows arrive grouped by ID with the newest chunk first; fold the
        // channel copy and the thread copy (and exact resume duplicates) of
        // one message into one.
        let mut out: Vec<Msg> = Vec::new();
        for r in rows {
            if let Some(last) = out.last_mut() {
                if last.id == r.id {
                    last.is_parent |= r.is_parent;
                    if r.parent_id.is_some()
                        && r.parent_id != Some(r.id)
                        && last.parent_id.is_none()
                    {
                        last.parent_id = r.parent_id;
                        last.thread_ts = r.thread_ts.clone();
                    }
                    if last.thread_ts.is_none() {
                        last.thread_ts = r.thread_ts.clone();
                    }
                    let rc = r
                        .data
                        .get("reply_count")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    last.reply_count = last.reply_count.max(rc);
                    let lr = r
                        .latest_reply
                        .as_deref()
                        .and_then(ts_to_id)
                        .filter(|v| *v > 0);
                    if lr > last.latest_reply_id {
                        last.latest_reply_id = lr;
                    }
                    continue;
                }
            }
            out.push(Msg::from_raw(r));
        }
        out
    }

    fn query_msgs(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> rusqlite::Result<Vec<Msg>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(p, RawRow::from_row)?;
        let mut raw = Vec::new();
        for r in rows {
            raw.push(r?);
        }
        Ok(Self::rows_to_msgs(raw))
    }

    /// One page of top-level messages, ascending by ID. `before` walks
    /// older (`ID < before`), `after` walks newer (`ID >= after`); neither
    /// means the newest page.
    pub fn timeline_page(
        &self,
        cid: &str,
        before: Option<i64>,
        after: Option<i64>,
        limit: usize,
    ) -> rusqlite::Result<Vec<Msg>> {
        let dir = if after.is_some() && before.is_none() {
            "ASC"
        } else {
            "DESC"
        };
        let sql = format!(
            "WITH ids AS (SELECT DISTINCT ID FROM MESSAGE \
               WHERE CHANNEL_ID = ?1 AND (?2 IS NULL OR ID < ?2) AND (?3 IS NULL OR ID >= ?3) \
               AND {top} ORDER BY ID {dir} LIMIT ?4) \
             SELECT {cols} FROM MESSAGE m JOIN ids ON ids.ID = m.ID \
             WHERE m.CHANNEL_ID = ?1 ORDER BY m.ID ASC, m.CHUNK_ID DESC",
            top = Self::TOP_LEVEL,
            cols = Self::COLS,
        );
        let mut msgs = self.query_msgs(&sql, &[&cid, &before, &after, &(limit as i64)])?;
        self.reply_stats(cid, &mut msgs)?;
        Ok(msgs)
    }

    pub fn timeline_count(&self, cid: &str) -> rusqlite::Result<i64> {
        self.conn.query_row(
            &format!(
                "SELECT COUNT(DISTINCT ID) FROM MESSAGE WHERE CHANNEL_ID = ?1 AND {}",
                Self::TOP_LEVEL
            ),
            params![cid],
            |r| r.get(0),
        )
    }

    /// The thread: root first, then its replies, ascending.
    pub fn thread(&self, cid: &str, root: i64) -> rusqlite::Result<Vec<Msg>> {
        let sql = format!(
            "SELECT {cols} FROM MESSAGE m WHERE m.CHANNEL_ID = ?1 AND (m.ID = ?2 OR m.PARENT_ID = ?2) \
             ORDER BY m.ID ASC, m.CHUNK_ID DESC",
            cols = Self::COLS
        );
        let mut msgs = self.query_msgs(&sql, &[&cid, &root])?;
        self.reply_stats(cid, &mut msgs)?;
        Ok(msgs)
    }

    /// Roots of every thread the owner wrote in.
    pub fn my_threads(&self, me: &str) -> rusqlite::Result<Vec<Msg>> {
        let like = format!("%\"user\":\"{}\"%", me.replace(['%', '_'], ""));
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT CHANNEL_ID, THREAD_TS FROM MESSAGE \
             WHERE THREAD_TS IS NOT NULL AND DATA LIKE ?1 AND json_extract(DATA, '$.user') = ?2",
        )?;
        let roots: Vec<(String, String)> = stmt
            .query_map(params![like, me], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .flatten()
            .collect();
        let sql = format!(
            "SELECT {cols} FROM MESSAGE m WHERE m.CHANNEL_ID = ?1 AND m.ID = ?2 ORDER BY m.ID ASC, m.CHUNK_ID DESC",
            cols = Self::COLS
        );
        let mut out = Vec::new();
        for (cid, ts) in roots {
            let Some(root) = ts_to_id(&ts) else {
                continue;
            };
            let mut msgs = self.query_msgs(&sql, &[&cid, &root])?;
            self.reply_stats(&cid, &mut msgs)?;
            out.extend(msgs);
        }
        Ok(out)
    }

    /// Fill `archived_replies` / `latest_reply_id` for the parents in `msgs`.
    fn reply_stats(&self, cid: &str, msgs: &mut [Msg]) -> rusqlite::Result<()> {
        let (Some(lo), Some(hi)) = (
            msgs.iter().map(|m| m.id).min(),
            msgs.iter().map(|m| m.id).max(),
        ) else {
            return Ok(());
        };
        let mut stmt = self.conn.prepare(
            "SELECT PARENT_ID, COUNT(DISTINCT ID), MAX(ID) FROM MESSAGE \
             WHERE CHANNEL_ID = ?1 AND PARENT_ID BETWEEN ?2 AND ?3 AND ID <> PARENT_ID GROUP BY PARENT_ID",
        )?;
        let rows = stmt.query_map(params![cid, lo, hi], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let stats: HashMap<i64, (i64, i64)> =
            rows.flatten().map(|(p, n, last)| (p, (n, last))).collect();
        for m in msgs.iter_mut() {
            if let Some((n, last)) = stats.get(&m.id) {
                m.archived_replies = *n;
                if Some(*last) > m.latest_reply_id {
                    m.latest_reply_id = Some(*last);
                }
            }
        }
        Ok(())
    }

    /// Candidate matches for a substring, newest first, any depth (replies
    /// included). SQL narrows on the text column and the raw JSON; the
    /// caller confirms against the rendered text.
    pub fn search(&self, cid: &str, needle: &str, limit: usize) -> rusqlite::Result<Vec<Msg>> {
        let esc = |s: &str| {
            s.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        };
        let like = format!("%{}%", esc(needle));
        // Slack stores & < > as entities; a needle typed as displayed must
        // also be tried in its stored form.
        let stored = needle
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let like_stored = format!("%{}%", esc(&stored));
        let sql = format!(
            "SELECT {cols} FROM MESSAGE m WHERE m.CHANNEL_ID = ?1 AND \
             (m.TXT LIKE ?2 ESCAPE '\\' OR m.TXT LIKE ?3 ESCAPE '\\' \
              OR CAST(m.DATA AS TEXT) LIKE ?2 ESCAPE '\\' OR CAST(m.DATA AS TEXT) LIKE ?3 ESCAPE '\\') \
             ORDER BY m.ID DESC, m.CHUNK_ID DESC LIMIT ?4",
            cols = Self::COLS
        );
        let mut msgs = self.query_msgs(&sql, &[&cid, &like, &like_stored, &(limit as i64)])?;
        self.reply_stats(cid, &mut msgs)?;
        Ok(msgs)
    }
}

#[derive(Clone)]
pub struct ChannelStats {
    pub msgs: i64,
    pub first: i64,
    pub last: i64,
    /// Unix seconds of the owner's messages, for the recency score.
    pub mine: Vec<i64>,
}

#[derive(Clone, Default)]
pub struct ArchiveStats {
    pub channels: HashMap<String, ChannelStats>,
}

/// Per-archive stats on disk, so a start recounts only what changed.
pub struct StatsCache {
    path: PathBuf,
    entries: serde_json::Map<String, Value>,
    dirty: bool,
}

impl StatsCache {
    pub fn load(path: PathBuf) -> StatsCache {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        StatsCache {
            path,
            entries,
            dirty: false,
        }
    }

    fn get(&self, rel: &str, key: &str) -> Option<ArchiveStats> {
        let e = self.entries.get(rel)?;
        if e.get("key").and_then(Value::as_str) != Some(key) {
            return None;
        }
        let mut channels = HashMap::new();
        for (cid, c) in e.get("channels")?.as_object()? {
            let n = |k: &str| c.get(k).and_then(Value::as_i64).unwrap_or(0);
            let mine = c
                .get("mine")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            channels.insert(
                cid.clone(),
                ChannelStats {
                    msgs: n("msgs"),
                    first: n("first"),
                    last: n("last"),
                    mine,
                },
            );
        }
        Some(ArchiveStats { channels })
    }

    fn put(&mut self, rel: &str, key: &str, stats: &ArchiveStats) {
        let mut channels = serde_json::Map::new();
        for (cid, c) in &stats.channels {
            channels.insert(
                cid.clone(),
                serde_json::json!({ "msgs": c.msgs, "first": c.first, "last": c.last, "mine": c.mine }),
            );
        }
        self.entries.insert(
            rel.to_string(),
            serde_json::json!({ "key": key, "channels": channels }),
        );
        self.dirty = true;
    }

    pub fn save(&self) {
        if !self.dirty {
            return;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.path, Value::Object(self.entries.clone()).to_string());
    }
}

struct RawRow {
    id: i64,
    channel_id: String,
    ts: String,
    parent_id: Option<i64>,
    thread_ts: Option<String>,
    is_parent: bool,
    latest_reply: Option<String>,
    txt: Option<String>,
    data: Value,
}

impl RawRow {
    fn from_row(r: &Row) -> rusqlite::Result<RawRow> {
        let blob: Vec<u8> = r.get(9)?;
        let data = serde_json::from_slice(&blob).unwrap_or(Value::Null);
        Ok(RawRow {
            id: r.get(0)?,
            channel_id: r.get(2)?,
            ts: r.get(3)?,
            parent_id: r.get(4)?,
            thread_ts: r.get(5)?,
            is_parent: r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
            latest_reply: r.get(7)?,
            txt: r.get(8)?,
            data,
        })
    }
}

impl Msg {
    /// A message as the API returns it: a search hit, a dumped thread.
    pub fn from_api(channel_id: String, data: Value) -> Option<Msg> {
        let ts = data.get("ts").and_then(Value::as_str)?.to_string();
        let id = ts_to_id(&ts)?;
        let subtype = data
            .get("subtype")
            .and_then(Value::as_str)
            .map(str::to_string);
        let broadcast = subtype.as_deref() == Some("thread_broadcast");
        let mut thread_ts = data
            .get("thread_ts")
            .and_then(Value::as_str)
            .map(str::to_string);
        if thread_ts.is_none() {
            // A search hit carries its thread only in the permalink.
            thread_ts = data
                .get("permalink")
                .and_then(Value::as_str)
                .and_then(|p| p.split_once("thread_ts="))
                .map(|(_, r)| r.split('&').next().unwrap_or("").to_string())
                .filter(|s| !s.is_empty());
        }
        let parent_id = thread_ts.as_deref().and_then(ts_to_id).filter(|p| *p != id);
        let reply_count = data.get("reply_count").and_then(Value::as_i64).unwrap_or(0);
        let latest_reply_id = data
            .get("latest_reply")
            .and_then(Value::as_str)
            .and_then(ts_to_id);
        Some(Msg {
            id,
            ts,
            channel_id,
            parent_id,
            thread_ts,
            is_parent: reply_count > 0,
            reply_count,
            archived_replies: 0,
            latest_reply_id,
            user: data.get("user").and_then(Value::as_str).map(str::to_string),
            subtype,
            text: data
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            edited: data.get("edited").is_some(),
            broadcast,
            channel_name: None,
            data,
        })
    }

    fn from_raw(r: RawRow) -> Msg {
        let d = &r.data;
        let subtype = d.get("subtype").and_then(Value::as_str).map(str::to_string);
        let broadcast = subtype.as_deref() == Some("thread_broadcast");
        let parent_id = r.parent_id.filter(|p| *p != r.id);
        let thread_ts = r.thread_ts.clone().or_else(|| {
            d.get("thread_ts")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let reply_count = d.get("reply_count").and_then(Value::as_i64).unwrap_or(0);
        let latest_reply_id = r
            .latest_reply
            .as_deref()
            .and_then(ts_to_id)
            .filter(|v| *v > 0)
            .or_else(|| {
                d.get("latest_reply")
                    .and_then(Value::as_str)
                    .and_then(ts_to_id)
            });
        let text = r
            .txt
            .clone()
            .or_else(|| d.get("text").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        Msg {
            id: r.id,
            ts: r.ts,
            channel_id: r.channel_id,
            parent_id,
            thread_ts,
            is_parent: r.is_parent,
            reply_count,
            archived_replies: 0,
            latest_reply_id,
            user: d.get("user").and_then(Value::as_str).map(str::to_string),
            subtype,
            text,
            edited: d.get("edited").is_some(),
            broadcast,
            channel_name: None,
            data: r.data,
        }
    }
}
