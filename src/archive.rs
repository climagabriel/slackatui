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
    /// Distinct message timestamps, the same figure `slack cache list` shows.
    pub msgs: i64,
    /// Messages written by the archive's owner, replies included.
    pub mine: i64,
    pub first_id: i64,
    pub last_id: i64,
}

pub struct Corpus {
    pub archives: Vec<Archive>,
    pub convs: Vec<Conv>,
    pub workspace_url: String,
    /// The archive owner's user id: `$SLACK_SELF_USER_ID`, else the user
    /// present in every direct message of the DM archive.
    pub me: Option<String>,
    /// Channel id -> name across every archive: a mention of a channel
    /// archived elsewhere still gets its name.
    pub channel_names: HashMap<String, String>,
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
}

pub struct FileInfo {
    pub name: String,
    pub filetype: String,
    pub size: Option<i64>,
    pub mode: String,
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
                out.push(FileInfo {
                    name,
                    filetype: s("filetype"),
                    size: f.get("size").and_then(Value::as_i64),
                    mode: s("mode"),
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

fn ts_to_id(ts: &str) -> Option<i64> {
    let (secs, frac) = ts.split_once('.')?;
    let secs: i64 = secs.parse().ok()?;
    let frac: i64 = format!("{frac:0<6}").get(..6)?.parse().ok()?;
    Some(secs * 1_000_000 + frac)
}

impl Corpus {
    pub fn open(root: &Path) -> Result<Corpus, String> {
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
                .collect();
            subs.sort();
            for sub in subs {
                let db = sub.join("slackdump.sqlite");
                let rel = format!(
                    "{}/{}",
                    set,
                    sub.file_name().unwrap_or_default().to_string_lossy()
                );
                match open_ro(&db) {
                    Ok(conn) => archives.push(Archive {
                        rel,
                        conn,
                        users: RefCell::new(None),
                        channel_names: HashMap::new(),
                    }),
                    Err(e) => eprintln!("slack-tui: {rel}: {e}"),
                }
            }
        }
        if archives.is_empty() {
            return Err(format!(
                "no slackdump.sqlite under {}/{{{}}}",
                root.display(),
                ARCHIVE_SETS.join(",")
            ));
        }
        let me = std::env::var("SLACK_SELF_USER_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| archives.iter().find_map(|a| a.self_user()));
        let workspace_url = archives
            .iter()
            .find_map(|a| a.workspace_url())
            .unwrap_or_else(|| "https://slack.com".to_string());
        let mut convs = Vec::new();
        for (ai, a) in archives.iter_mut().enumerate() {
            match a.scan_convs(ai, me.as_deref()) {
                Ok(mut c) => convs.append(&mut c),
                Err(e) => eprintln!("slack-tui: {}: {e}", a.rel),
            }
        }
        let mut channel_names = HashMap::new();
        for a in &archives {
            for (id, name) in &a.channel_names {
                if !name.is_empty() {
                    channel_names
                        .entry(id.clone())
                        .or_insert_with(|| name.clone());
                }
            }
        }
        Ok(Corpus {
            archives,
            convs,
            workspace_url,
            channel_names,
            me,
        })
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
            conn: Connection::open_in_memory().expect("in-memory sqlite"),
            users: RefCell::new(Some(map)),
            channel_names: channels
                .iter()
                .map(|(id, name)| (id.to_string(), name.to_string()))
                .collect(),
        }
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

    fn load_users(&self) -> rusqlite::Result<HashMap<String, User>> {
        let mut map = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT ID, USERNAME, json_extract(DATA, '$.real_name'), \
             json_extract(DATA, '$.profile.display_name'), \
             json_extract(DATA, '$.deleted'), json_extract(DATA, '$.is_bot') \
             FROM S_USER ORDER BY CHUNK_ID",
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

    pub fn user_name(&self, uid: &str) -> String {
        if uid == "USLACKBOT" {
            return "Slackbot".to_string();
        }
        self.users()
            .get(uid)
            .map(|u| u.name.clone())
            .unwrap_or_else(|| uid.to_string())
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

    /// Who wrote it, as a reader would name them.
    pub fn author(&self, m: &Msg) -> String {
        if let Some(uid) = &m.user {
            return self.user_name(uid);
        }
        let d = &m.data;
        if let Some(u) = d
            .get("username")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return u.to_string();
        }
        if let Some(u) = d
            .pointer("/bot_profile/name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return u.to_string();
        }
        if let Some(u) = d
            .get("attachments")
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter()
                    .find_map(|x| x.get("author_name").and_then(Value::as_str))
            })
            .filter(|s| !s.is_empty())
        {
            return u.to_string();
        }
        "bot".to_string()
    }

    fn scan_convs(&mut self, ai: usize, me: Option<&str>) -> rusqlite::Result<Vec<Conv>> {
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
        // The owner's messages per channel. The LIKE prefilter keeps the JSON
        // parse to rows that can match: 197 ms -> 56 ms on the largest archive.
        let mut mine: HashMap<String, i64> = HashMap::new();
        if let Some(me) = me {
            let like = format!("%\"user\":\"{}\"%", me.replace(['%', '_'], ""));
            let mut stmt = self.conn.prepare(
                "SELECT CHANNEL_ID, COUNT(DISTINCT TS) FROM MESSAGE \
                 WHERE DATA LIKE ?1 AND json_extract(DATA, '$.user') = ?2 GROUP BY CHANNEL_ID",
            )?;
            let rows = stmt.query_map(params![like, me], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            mine = rows.flatten().collect();
        }
        let mut convs = Vec::new();
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
        for (cid, msgs, first_id, last_id) in rows.flatten() {
            let (name, kind, archived) = match meta.get(&cid) {
                Some(m) => (
                    self.display_name(&cid, m.kind, &m.name, m.im_user.as_deref(), &m.members, me),
                    m.kind,
                    m.archived,
                ),
                None => (cid.clone(), Kind::Channel, false),
            };
            let mine = mine.get(&cid).copied().unwrap_or(0);
            convs.push(Conv {
                archive: ai,
                id: cid,
                name,
                kind,
                archived,
                msgs,
                mine,
                first_id,
                last_id,
            });
        }
        Ok(convs)
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
            data: r.data,
        }
    }
}
