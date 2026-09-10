//! The slackdump archives on disk: discovery, conversation inventory, and
//! the message queries the views run. Everything is read-only; a resume may
//! be writing the same database (WAL), so connections open with a busy
//! timeout and never touch the schema.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use serde_json::Value;

/// Archive sets under the root, in listing order. `threads/` holds archives
/// of individual threads from channels `full/` does not cache; the order
/// matters, because the first archive holding a conversation is the one the
/// others are unioned into.
pub const ARCHIVE_SETS: [&str; 3] = ["full", "dms", "threads"];
/// Messages fetched per timeline page.
pub const PAGE: usize = 200;
/// Cap on search candidates fetched from SQL.
pub const SEARCH_CAP: usize = 500;
/// Slackbot's user id. Its `S_USER` row carries no `is_bot`, so anything
/// asking whether a user is an app has to name it.
pub const SLACKBOT: &str = "USLACKBOT";

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

/// What the archive can say about the other side of a direct message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Counterpart {
    User(String),
    /// The membership does not settle it: this many members, none of them
    /// singled out once the owner is removed.
    Ambiguous(usize),
    /// The archive owner is unknown, so no member can be ruled out.
    OwnerUnknown(usize),
}

impl Counterpart {
    /// The rule `im_counterpart` applies, over what the caller already holds:
    /// the channel object's own `user` field, the channel's members, and the
    /// archive owner. Kept apart from the queries so that `scan_convs`, which
    /// has both in memory for every channel at once, settles the counterpart
    /// the same way without a query per conversation.
    pub fn resolve(im_user: Option<&str>, members: &[String], me: Option<&str>) -> Counterpart {
        if let Some(user) = im_user.filter(|user| !user.is_empty()) {
            return Counterpart::User(user.to_string());
        }
        let Some(me) = me else {
            return Counterpart::OwnerUnknown(members.len());
        };
        let mut others = members.iter().filter(|user| user.as_str() != me);
        match (others.next(), others.next()) {
            (Some(user), None) => Counterpart::User(user.clone()),
            _ => Counterpart::Ambiguous(members.len()),
        }
    }

    /// The user this named, when it named one.
    pub fn user(&self) -> Option<&str> {
        match self {
            Counterpart::User(user) => Some(user),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct User {
    pub name: String,
    pub is_bot: bool,
}

/// Everything a second, independent read-only connection to one archive
/// needs: the directory, the channel names `scan_convs` learned, and the
/// extra archives folded into its `MESSAGE` view. Owned, so a worker thread
/// can open its own `Archive` over the same files without borrowing the
/// `Corpus`.
#[derive(Clone)]
pub struct ArchiveHandle {
    rel: String,
    dir: PathBuf,
    channel_names: HashMap<String, String>,
    combined: Vec<(PathBuf, Vec<String>)>,
    users: Option<Arc<HashMap<String, User>>>,
}

impl ArchiveHandle {
    /// A fresh read-only `Archive` that answers exactly as the one this
    /// handle came from: same union of sources, same names.
    pub fn open(&self) -> rusqlite::Result<Archive> {
        let mut archive = Archive::open(self.rel.clone(), &self.dir)?;
        archive.channel_names = self.channel_names.clone();
        if let Some(users) = &self.users {
            archive.adopt_users(users.clone());
        }
        if !self.combined.is_empty() {
            archive.combine_sources(&self.combined)?;
        }
        Ok(archive)
    }
}

pub struct Archive {
    /// `full/team-alpha_20260428`: the directory relative to the root.
    pub rel: String,
    pub dir: PathBuf,
    pub source_dirs: Vec<PathBuf>,
    combined_sources: Vec<(PathBuf, Vec<String>)>,
    source_fingerprints: RefCell<Vec<String>>,
    pub conn: Connection,
    /// The workspace's users as this archive stores them, loaded once.
    /// Shared rather than copied: the `/find` scan renders on a worker
    /// thread with its own connections but the same maps.
    users: RefCell<Option<Arc<HashMap<String, User>>>>,
    pub channel_names: HashMap<String, String>,
}

pub struct Conv {
    pub archive: usize,
    pub id: String,
    /// Display name: `#team-alpha`, `@oliver.hendricks`, `@a,b` for group DMs.
    pub name: String,
    pub kind: Kind,
    pub archived: bool,
    /// `is_shared` or `is_ext_shared` on the channel object: the conversation
    /// is shared with another organization, which is what makes it a Slack
    /// Connect conversation. The flags are authoritative; the name is not.
    pub shared: bool,
    /// The other side of a direct message, as `im_counterpart` settles it, and
    /// `None` for every other kind and for a direct message whose counterpart
    /// the archive cannot name. Kept rather than the answer to "is this an
    /// app", so that a user record learned later — from a live profile fetch
    /// after the archive was scanned — decides it on the next sort.
    pub im_counterpart: Option<String>,
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
    /// Last confirmed live count, capped at ten; None when unavailable.
    pub unread_count: Option<i64>,
    pub unread_snapshot: Option<String>,
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
    pub fn from_slack(f: &Value, channel: &str) -> Self {
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
        Self {
            id: s("id"),
            channel: channel.to_string(),
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
        }
    }

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
                out.push(FileInfo::from_slack(f, &self.channel_id));
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

/// The two LIKE patterns `search_filtered` binds: the needle as typed, and
/// the same needle with `& < >` in the entity form Slack stores.
fn search_patterns(needle: &str) -> (String, String) {
    let esc = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    };
    let stored = needle
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    (format!("%{}%", esc(needle)), format!("%{}%", esc(&stored)))
}

/// The filter `search_filtered` runs, with its parameters resolved, for
/// showing a reader what the scan is asking of SQLite. Display only: the
/// query itself still binds its parameters, so the doubling of `'` here is
/// about rendering valid SQL on screen, not about what runs. The identical
/// entity form of a needle without `& < >` is folded into one pair of terms.
pub fn search_filter_sql(needle: &str, author: Option<&str>, limit: usize) -> String {
    let quoted = |text: &str| format!("'{}'", text.replace('\'', "''"));
    let (like, like_stored) = search_patterns(needle);
    let mut terms = vec![like.clone()];
    if like_stored != like {
        terms.push(like_stored);
    }
    let matches: Vec<String> = terms
        .iter()
        .flat_map(|pattern| {
            [
                format!("TXT LIKE {} ESCAPE '\\'", quoted(pattern)),
                format!("CAST(DATA AS TEXT) LIKE {} ESCAPE '\\'", quoted(pattern)),
            ]
        })
        .collect();
    let who = author.map_or_else(|| "NULL".to_string(), quoted);
    format!(
        "… WHERE ({}) AND ({who} IS NULL OR json_extract(DATA,'$.user') = {who}) LIMIT {limit}",
        matches.join(" OR ")
    )
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
    /// An owned `Corpus` carrying only what `render::Ctx` reads out of one:
    /// user names, channel names, user groups and the owner's id. Archives
    /// and conversations are left empty — a worker thread renders message
    /// text with this and its own `Archive`, and never borrows the real
    /// corpus the UI thread is drawing from.
    pub fn names_snapshot(&self) -> Corpus {
        Corpus {
            root: PathBuf::new(),
            archives: Vec::new(),
            convs: Vec::new(),
            workspace_url: self.workspace_url.clone(),
            me: self.me.clone(),
            channel_names: self.channel_names.clone(),
            half_life_days: self.half_life_days,
            usergroups: self.usergroups.clone(),
            users: self.users.clone(),
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
        corpus.combine_archived_conversations(cache.as_mut())?;
        if let Some(cache) = &cache { cache.save(); }
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

    /// Where an archive's `rel` puts it among `ARCHIVE_SETS`. A directory
    /// under no known set sorts last, so one dropped in by hand never
    /// displaces a real archive.
    fn set_rank(rel: &str) -> usize {
        let set = rel.split('/').next().unwrap_or("");
        ARCHIVE_SETS
            .iter()
            .position(|known| *known == set)
            .unwrap_or(ARCHIVE_SETS.len())
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
        let mut indices = Vec::new();
        for conv in convs {
            if let Some(index) = self.conv_by_channel(&conv.id) {
                if self.convs[index].live_only {
                    self.convs[index] = conv;
                } else if Self::set_rank(&self.archives[ai].rel)
                    < Self::set_rank(&self.archives[self.convs[index].archive].rel)
                {
                    // The new archive comes from an earlier set than the one
                    // holding this conversation, so it takes over as primary,
                    // exactly as ARCHIVE_SETS order would decide at a restart.
                    // Without this the conversation keeps reading its name,
                    // kind and membership out of the lesser archive — a
                    // `threads/` archive has no members at all — until the
                    // next start silently changes them.
                    let old = self.convs[index].archive;
                    let mut dirs = vec![self.archives[old].dir.clone()];
                    // Whatever the old primary had already folded in for this
                    // channel has to come along, or it is lost.
                    dirs.extend(self.archives[old].combined_dirs_for(&conv.id));
                    let sources: Vec<(PathBuf, Vec<String>)> = dirs
                        .into_iter()
                        .map(|source| (source, vec![conv.id.clone()]))
                        .collect();
                    self.archives[ai]
                        .combine_sources(&sources)
                        .map_err(|e| e.to_string())?;
                    let updated = self.archives[ai]
                        .scan_convs(ai, self.me.as_deref(), self.half_life_days, None)
                        .map_err(|e| e.to_string())?;
                    if let Some(mut fresh) = updated.into_iter().find(|c| c.id == conv.id) {
                        // The name, kind and stats are the new primary's; what
                        // Slack told this session about the conversation is not
                        // in any archive and has to survive the swap. `muted`
                        // is copied for the window before the next
                        // `sync_muted`, which re-derives it from the confirmed
                        // snapshot; the rest has no other source.
                        let existing = &self.convs[index];
                        fresh.left = existing.left;
                        fresh.muted = existing.muted;
                        fresh.unread = existing.unread;
                        fresh.unread_count = existing.unread_count;
                        fresh.unread_snapshot = existing.unread_snapshot.clone();
                        fresh.mentions = existing.mentions;
                        fresh.last_read = existing.last_read;
                        self.convs[index] = fresh;
                    }
                } else {
                    let primary = self.convs[index].archive;
                    self.archives[primary].combine_sources(&[(dir.to_path_buf(), vec![conv.id.clone()])]).map_err(|e| e.to_string())?;
                    let updated = self.archives[primary].scan_convs(primary, self.me.as_deref(), self.half_life_days, None).map_err(|e| e.to_string())?;
                    if let Some(stats) = updated.into_iter().find(|c| c.id == conv.id) {
                        let existing = &mut self.convs[index];
                        existing.msgs = stats.msgs;
                        existing.first_id = stats.first_id;
                        existing.last_id = stats.last_id;
                        existing.mine = stats.mine;
                        existing.score = stats.score;
                    }
                }
                indices.push(index);
            } else {
                indices.push(self.convs.len());
                self.convs.push(conv);
            }
        }
        Ok(indices)
    }

    /// A user's name from the workspace list.
    pub fn author_names(&self) -> Vec<(String,String)> {
        let mut users: Vec<_> = self.users.iter().map(|(id,user)|(id.clone(),user.name.clone())).collect();
        users.sort_by_key(|(id,name)|(name.to_lowercase(),id.clone()));
        users
    }

    pub fn user_name(&self, uid: &str) -> Option<String> {
        self.users.get(uid).map(|u| u.name.clone())
    }

    fn combine_archived_conversations(&mut self, mut cache: Option<&mut StatsCache>) -> Result<(), String> {
        let mut first = HashMap::new();
        let mut overlaps: HashMap<usize, HashMap<usize, Vec<String>>> = HashMap::new();
        for conv in &self.convs {
            if let Some(&primary) = first.get(&conv.id) {
                if primary != conv.archive {
                    overlaps.entry(primary).or_default().entry(conv.archive).or_default().push(conv.id.clone());
                }
            } else { first.insert(conv.id.clone(), conv.archive); }
        }
        for (primary, others) in overlaps {
            let mut sources: Vec<_> = others.into_iter().map(|(index, ids)| (self.archives[index].dir.clone(), ids)).collect();
            sources.sort_by(|a, b| a.0.cmp(&b.0));
            self.archives[primary].combine_sources(&sources).map_err(|error| format!("Combining {}: {error}", self.archives[primary].rel))?;
            let updated = self.archives[primary].scan_convs(primary, self.me.as_deref(), self.half_life_days, cache.as_deref_mut())
                .map_err(|error| format!("Combined archive stats: {error}"))?;
            for conv in updated {
                if let Some(existing) = self.convs.iter_mut().find(|c| c.archive == primary && c.id == conv.id) {
                    *existing = conv;
                }
            }
        }
        self.convs.retain(|conv| first.get(&conv.id) == Some(&conv.archive));
        Ok(())
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
            source_dirs: vec![],
            combined_sources: vec![],
            source_fingerprints: RefCell::new(vec![]),
            conn: Connection::open_in_memory().expect("in-memory sqlite"),
            users: RefCell::new(Some(Arc::new(map))),
            channel_names: channels
                .iter()
                .map(|(id, name)| (id.to_string(), name.to_string()))
                .collect(),
        }
    }

    /// What another thread needs to open this archive for itself.
    pub fn handle(&self) -> ArchiveHandle {
        ArchiveHandle {
            rel: self.rel.clone(),
            dir: self.dir.clone(),
            channel_names: self.channel_names.clone(),
            combined: self.combined_sources.clone(),
            users: self.loaded_users(),
        }
    }

    /// Open an archive directory read-only.
    pub fn open(rel: String, dir: &Path) -> rusqlite::Result<Archive> {
        let conn = open_ro(&dir.join("slackdump.sqlite"))?;
        Ok(Archive {
            rel,
            dir: dir.to_path_buf(),
            source_dirs: vec![dir.to_path_buf()],
            combined_sources: vec![],
            source_fingerprints: RefCell::new(vec![]),
            conn,
            users: RefCell::new(None),
            channel_names: HashMap::new(),
        })
    }

    /// Fresh message stats for one channel: (distinct messages, first id, last id).
    pub fn channel_stats(&self, cid: &str) -> rusqlite::Result<(i64, i64, i64)> {
        self.ensure_combined_fresh()?;
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

    pub fn users(&self) -> Arc<HashMap<String, User>> {
        if self.users.borrow().is_none() {
            let map = Arc::new(self.load_users().unwrap_or_default());
            *self.users.borrow_mut() = Some(map);
        }
        self.users.borrow().clone().expect("users loaded")
    }

    /// The user map only if it has already been read, for handing to a
    /// worker without making it read the table again.
    pub fn loaded_users(&self) -> Option<Arc<HashMap<String, User>>> {
        self.users.borrow().clone()
    }

    /// Adopt a map another connection to the same archive already read.
    pub fn adopt_users(&self, users: Arc<HashMap<String, User>>) {
        *self.users.borrow_mut() = Some(users);
    }

    /// A user's name when this archive knows the user.
    pub fn user(&self, uid: &str) -> Option<String> {
        self.users().get(uid).map(|u| u.name.clone())
    }

    pub fn user_name(&self, uid: &str) -> String {
        if uid == SLACKBOT {
            return "Slackbot".to_string();
        }
        self.user(uid).unwrap_or_else(|| uid.to_string())
    }

    /// A bot user (PagerDuty, Jira, ...) posts with a user id whose S_USER row says so.
    pub fn user_is_bot(&self, uid: &str) -> bool {
        self.users().get(uid).is_some_and(|u| u.is_bot)
    }

    /// One row's stored Slack object, as JSON. `DATA` is a blob of UTF-8
    /// JSON, so the cast is how every other reader here reaches its text.
    /// `Ok(None)` is a row that is not there; `Err` is a row that is there
    /// and cannot be decoded. Folding the two together makes a corrupt row
    /// report as a missing one, and sends `im_counterpart` down its
    /// fallback as if the channel object carried no `user`.
    fn row_json(&self, sql: &str, id: &str) -> Result<Option<Value>, String> {
        let text: Option<String> = self
            .conn
            .query_row(sql, params![id], |r| r.get(0))
            .optional()
            .map_err(|error| error.to_string())?;
        let Some(text) = text else {
            return Ok(None);
        };
        match serde_json::from_str(&text) {
            Ok(value @ Value::Object(_)) => Ok(Some(value)),
            Ok(_) => Err("stored row is JSON but not an object".to_string()),
            Err(error) => Err(error.to_string()),
        }
    }

    /// The whole `CHANNEL` row of one conversation, not the handful of
    /// fields `Conv` keeps. A refresh appends a new chunk rather than
    /// rewriting the old one, so the newest chunk answers.
    pub fn channel_json(&self, cid: &str) -> Result<Option<Value>, String> {
        self.row_json(
            "SELECT CAST(DATA AS TEXT) FROM CHANNEL WHERE ID = ?1 \
             ORDER BY CHUNK_ID DESC, rowid DESC LIMIT 1",
            cid,
        )
    }

    /// The whole `S_USER` row of one user. `MAX(rowid)` per id is what
    /// `load_users` takes; for a single id that is the last row.
    pub fn user_json(&self, uid: &str) -> Result<Option<Value>, String> {
        self.row_json(
            "SELECT CAST(DATA AS TEXT) FROM S_USER WHERE ID = ?1 ORDER BY rowid DESC LIMIT 1",
            uid,
        )
    }

    /// The other side of a direct message. The channel object's own `user`
    /// field answers on its own, self-DMs included. Without one, the
    /// membership answers only when it settles the question: a known owner
    /// and exactly one member who is not the owner. An owner-only or
    /// wider membership, or an unknown owner, names nobody rather than
    /// picking a member — a group conversation misfiled as an IM would
    /// otherwise open an arbitrary person's user object.
    pub fn im_counterpart(&self, cid: &str, me: Option<&str>) -> Result<Counterpart, String> {
        let im_user = self
            .channel_json(cid)?
            .and_then(|channel| channel["user"].as_str().map(str::to_string));
        if let Some(user) = im_user.as_deref().filter(|user| !user.is_empty()) {
            return Ok(Counterpart::User(user.to_string()));
        }
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT USER_ID FROM CHANNEL_USER WHERE CHANNEL_ID = ?1")
            .map_err(|error| error.to_string())?;
        // A member row that cannot be read is an error, not a member to
        // skip: dropping it could leave exactly one other member and settle
        // the counterpart on the wrong person.
        let members: Vec<String> = stmt
            .query_map(params![cid], |r| r.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        Ok(Counterpart::resolve(im_user.as_deref(), &members, me))
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
            shared: bool,
            im_user: Option<String>,
            members: Vec<String>,
            /// A `CHANNEL_USER` row of this channel whose user id could not be
            /// read — a NULL among them, say. The membership is then not a
            /// membership this can reason about.
            members_unreadable: bool,
        }
        let mut meta: HashMap<String, Meta> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT ID, NAME, json_extract(DATA, '$.is_im'), json_extract(DATA, '$.is_mpim'), \
                 json_extract(DATA, '$.is_private'), json_extract(DATA, '$.is_archived'), \
                 json_extract(DATA, '$.user'), json_extract(DATA, '$.is_shared'), \
                 json_extract(DATA, '$.is_ext_shared') FROM CHANNEL ORDER BY CHUNK_ID",
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
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                ))
            })?;
            for (id, name, is_im, is_mpim, is_private, is_archived, im_user, is_shared, is_ext_shared) in
                rows.flatten()
            {
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
                        // Either flag makes it a Slack Connect conversation:
                        // `is_shared` is set on a channel shared at all,
                        // `is_ext_shared` on one shared outside the workspace,
                        // and a channel can carry one without the other.
                        shared: is_shared.unwrap_or(0) != 0 || is_ext_shared.unwrap_or(0) != 0,
                        im_user,
                        members: Vec::new(),
                        members_unreadable: false,
                    },
                );
            }
        }
        {
            let mut stmt = self
                .conn
                .prepare("SELECT DISTINCT CHANNEL_ID, USER_ID FROM CHANNEL_USER")?;
            // The user id is read inside the row rather than as part of it, so
            // a row that cannot be read still says which channel it belonged
            // to. Dropping it silently is what `im_counterpart` refuses to do:
            // one member fewer can leave exactly one other member and settle a
            // direct message's counterpart on the wrong person.
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1))))?;
            for (cid, uid) in rows.flatten() {
                if let Some(m) = meta.get_mut(&cid) {
                    match uid {
                        Ok(uid) => m.members.push(uid),
                        Err(_) => m.members_unreadable = true,
                    }
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
        let cache_name = if self.combined_sources.is_empty() { self.rel.clone() } else { format!("{}#combined",self.rel) };
        let stats = match cache.as_ref().and_then(|c| c.get(&cache_name, &key)) {
            Some(st) => st,
            None => {
                let st = self.compute_stats(me)?;
                if let Some(c) = cache {
                    c.put(&cache_name, &key, &st);
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
            // A conversation with messages but no channel row is a public
            // channel with nothing known about it: not shared, and with no
            // counterpart to name.
            let (name, kind, archived, shared, im_counterpart) = match meta.get(&cid) {
                Some(m) => (
                    self.display_name(&cid, m.kind, &m.name, m.im_user.as_deref(), &m.members, me),
                    m.kind,
                    m.archived,
                    m.shared,
                    (m.kind == Kind::Im)
                        .then(|| {
                            // Where the channel object names the counterpart
                            // the membership is not consulted at all, exactly
                            // as `im_counterpart` returns before querying it.
                            // Without one, a membership carrying a row that
                            // could not be read names nobody: `im_counterpart`
                            // fails on the same archive, and a counterpart
                            // that cannot be resolved counts as a person.
                            let named = m.im_user.as_deref().filter(|user| !user.is_empty());
                            if named.is_none() && m.members_unreadable {
                                return None;
                            }
                            Counterpart::resolve(named, &m.members, me)
                                .user()
                                .map(str::to_string)
                        })
                        .flatten(),
                ),
                None => (cid.clone(), Kind::Channel, false, false, None),
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
                shared,
                im_counterpart,
                msgs: st.msgs,
                mine: st.mine.len() as i64,
                score,
                first_id: st.first,
                last_id: st.last,
                live_only: false,
                left: false,
                muted: false,
                unread: false,
                unread_count: None,
                unread_snapshot: None,
                mentions: 0,
                last_read: 0,
            });
        }
        convs.sort_by(|x, y| x.id.cmp(&y.id));
        Ok(convs)
    }

    /// The directories this archive has already folded in that carry `cid`.
    fn combined_dirs_for(&self, cid: &str) -> Vec<PathBuf> {
        self.combined_sources
            .iter()
            .filter(|(_, ids)| ids.iter().any(|id| id == cid))
            .map(|(dir, _)| dir.clone())
            .collect()
    }

    fn combine_sources(&mut self, sources: &[(PathBuf, Vec<String>)]) -> rusqlite::Result<()> {
        if self.combined_sources.is_empty() { self.conn.execute_batch("PRAGMA temp_store=MEMORY;")?; }
        for (dir, channels) in sources {
            if let Some((_, ids)) = self.combined_sources.iter_mut().find(|(path, _)| path == dir) {
                ids.extend(channels.iter().cloned());
                ids.sort();
                ids.dedup();
            } else {
                self.combined_sources.push((dir.clone(), channels.clone()));
                self.source_dirs.push(dir.clone());
            }
        }
        self.conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS extra_messages (
            ID INTEGER,CHUNK_ID INTEGER,CHANNEL_ID TEXT,TS TEXT,PARENT_ID INTEGER,THREAD_TS TEXT,
            IS_PARENT INTEGER,LATEST_REPLY TEXT,TXT TEXT,DATA BLOB,SOURCE_TIME INTEGER);
            CREATE INDEX IF NOT EXISTS temp.extra_messages_channel ON extra_messages(CHANNEL_ID,ID);
            CREATE INDEX IF NOT EXISTS temp.extra_messages_parent ON extra_messages(CHANNEL_ID,PARENT_ID);
            DROP VIEW IF EXISTS temp.MESSAGE;
            CREATE TEMP VIEW MESSAGE AS
            SELECT m.ID,m.CHUNK_ID,m.CHANNEL_ID,m.TS,m.PARENT_ID,m.THREAD_TS,m.IS_PARENT,m.LATEST_REPLY,m.TXT,m.DATA,
                COALESCE((SELECT UNIX_TS FROM main.CHUNK WHERE ID=m.CHUNK_ID),0) AS SOURCE_TIME FROM main.MESSAGE m
            UNION ALL SELECT * FROM extra_messages;")?;
        self.source_fingerprints.borrow_mut().clear();
        self.ensure_combined_fresh()
    }

    fn ensure_combined_fresh(&self) -> rusqlite::Result<()> {
        if self.combined_sources.is_empty() { return Ok(()); }
        let fingerprints: Vec<_> = self.combined_sources.iter().map(|(dir,_)| Self::directory_key(dir)).collect();
        if *self.source_fingerprints.borrow() == fingerprints { return Ok(()); }
        self.conn.execute_batch("SAVEPOINT refresh_union; DELETE FROM temp.extra_messages;")?;
        let result = (|| -> rusqlite::Result<()> {
            let mut insert = self.conn.prepare("INSERT INTO temp.extra_messages VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?;
            for (dir, channels) in &self.combined_sources {
                let source = open_ro(&dir.join("slackdump.sqlite"))?;
                let mut statement = source.prepare("SELECT m.ID,m.CHUNK_ID,m.CHANNEL_ID,m.TS,m.PARENT_ID,m.THREAD_TS,m.IS_PARENT,m.LATEST_REPLY,m.TXT,m.DATA,
                    COALESCE((SELECT UNIX_TS FROM CHUNK WHERE ID=m.CHUNK_ID),0) FROM MESSAGE m WHERE m.CHANNEL_ID=?1")?;
                for channel in channels {
                    let mut rows = statement.query([channel])?;
                    while let Some(row) = rows.next()? {
                        let values: Vec<rusqlite::types::Value> = (0..11).map(|index| row.get(index)).collect::<rusqlite::Result<_>>()?;
                        insert.execute(rusqlite::params_from_iter(values))?;
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.conn.execute_batch("ROLLBACK TO refresh_union; RELEASE refresh_union;")?;
            return Err(error);
        }
        self.conn.execute_batch("RELEASE refresh_union;")?;
        *self.source_fingerprints.borrow_mut() = fingerprints;
        Ok(())
    }

    fn message_order(&self) -> &'static str {
        if self.source_dirs.len() > 1 { "m.SOURCE_TIME DESC, m.CHUNK_ID DESC" }
        else { "m.CHUNK_ID DESC" }
    }

    /// Size and mtime of the database and its WAL: what a resume changes.
    fn stat_key(&self) -> String {
        self.source_dirs.iter().map(|dir| format!("{}:{}",dir.display(),Self::directory_key(dir))).collect::<Vec<_>>().join("|")
    }
    fn directory_key(dir: &Path) -> String {
        let mut key = String::new();
        for name in ["slackdump.sqlite", "slackdump.sqlite-wal"] {
            match std::fs::metadata(dir.join(name)) {
                Ok(md) => {
                    let mtime = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
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
        self.ensure_combined_fresh()?;
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
                if last.id == r.id && last.channel_id == r.channel_id {
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
        self.ensure_combined_fresh()?;
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
             WHERE m.CHANNEL_ID = ?1 ORDER BY m.ID ASC, {order}",
            top = Self::TOP_LEVEL,
            cols = Self::COLS,
            order = self.message_order(),
        );
        let mut msgs = self.query_msgs(&sql, &[&cid, &before, &after, &(limit as i64)])?;
        self.reply_stats(cid, &mut msgs)?;
        Ok(msgs)
    }

    pub fn timeline_count(&self, cid: &str) -> rusqlite::Result<i64> {
        self.ensure_combined_fresh()?;
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
             ORDER BY m.ID ASC, {order}",
            cols = Self::COLS,
            order = self.message_order()
        );
        let mut msgs = self.query_msgs(&sql, &[&cid, &root])?;
        self.reply_stats(cid, &mut msgs)?;
        Ok(msgs)
    }

    /// Roots of every thread the owner wrote a message in, or was mentioned in
    /// directly. Root and replies count alike for both. A direct mention is the
    /// literal `<@ME>`, so `<!here>`, `<!channel>` and `<!subteam^…>` do not
    /// bring a thread in.
    pub fn my_threads(&self, me: &str) -> rusqlite::Result<Vec<Msg>> {
        self.ensure_combined_fresh()?;
        // `%` and `_` are LIKE wildcards; a user id carries neither, so
        // dropping them keeps a crafted id from widening the prefilter.
        let safe = me.replace(['%', '_'], "");
        let author_like = format!("%\"user\":\"{safe}\"%");
        let mention_like = format!("%<@{safe}>%");
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT CHANNEL_ID, THREAD_TS FROM MESSAGE \
               WHERE THREAD_TS IS NOT NULL AND DATA LIKE ?1 AND json_extract(DATA, '$.user') = ?2 \
             UNION \
             SELECT DISTINCT CHANNEL_ID, THREAD_TS FROM MESSAGE \
               WHERE THREAD_TS IS NOT NULL AND DATA LIKE ?3",
        )?;
        let roots: Vec<(String, String)> = stmt
            .query_map(params![author_like, me, mention_like], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .flatten()
            .collect();
        let sql = format!(
            "SELECT {cols} FROM MESSAGE m WHERE m.CHANNEL_ID = ?1 AND m.ID = ?2 ORDER BY m.ID ASC, {order}",
            cols = Self::COLS,
            order = self.message_order()
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

    /// The newest archived reply in one thread: what a THREADS card draws
    /// under its root. None when the archive holds the root alone.
    ///
    /// Addressed through `PARENT_ID` rather than through the root's
    /// `latest_reply_id`, which is the larger of Slack's `latest_reply` and
    /// what the archive holds: when a refresh has not caught up, that id
    /// names a reply no archive has, and a lookup by it finds nothing.
    /// `ID <> PARENT_ID` drops the copy of the root that slackdump stores
    /// with the replies.
    pub fn last_reply(&self, cid: &str, root: i64) -> rusqlite::Result<Option<Msg>> {
        let sql = format!(
            "SELECT {cols} FROM MESSAGE m WHERE m.CHANNEL_ID = ?1 AND m.ID = \
               (SELECT MAX(ID) FROM MESSAGE WHERE CHANNEL_ID = ?1 AND PARENT_ID = ?2 AND ID <> PARENT_ID) \
             ORDER BY m.ID ASC, {order}",
            cols = Self::COLS,
            order = self.message_order()
        );
        Ok(self.query_msgs(&sql, &[&cid, &root])?.pop())
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
        self.search_filtered(Some(cid), needle, None, limit)
    }

    pub fn search_filtered(&self, cid: Option<&str>, needle: &str, author: Option<&str>, limit: usize) -> rusqlite::Result<Vec<Msg>> {
        let (like, like_stored) = search_patterns(needle);
        let sql = format!(
            "WITH ranked AS (SELECT m.*, ROW_NUMBER() OVER (PARTITION BY m.CHANNEL_ID, m.ID ORDER BY {order}) AS position \
             FROM MESSAGE m WHERE (?1 IS NULL OR m.CHANNEL_ID = ?1)) \
             SELECT {cols} FROM ranked m WHERE m.position = 1 AND (?5 IS NULL OR json_extract(CAST(m.DATA AS TEXT), '$.user') = ?5) AND \
             (m.TXT LIKE ?2 ESCAPE '\\' OR m.TXT LIKE ?3 ESCAPE '\\' \
              OR CAST(m.DATA AS TEXT) LIKE ?2 ESCAPE '\\' OR CAST(m.DATA AS TEXT) LIKE ?3 ESCAPE '\\') \
             ORDER BY m.ID DESC, {order} LIMIT ?4",
            cols = Self::COLS,
            order = self.message_order()
        );
        let mut msgs = self.query_msgs(&sql, &[&cid, &like, &like_stored, &(limit as i64), &author])?;
        if let Some(cid) = cid { self.reply_stats(cid, &mut msgs)?; }
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

/// One channel's worth of threads for the THREADS tests, as
/// `(id, root, user, text)`. `root == id` marks a thread root, `root == 0` a
/// message outside any thread. The owner is `U1`.
#[cfg(test)]
pub(crate) const THREAD_FIXTURE: &[(i64, i64, &str, &str)] = &[
    // Started by the owner; only somebody else replied.
    (1, 1, "U1", "root I started"),
    (2, 1, "U2", "someone else replies"),
    // Somebody else's thread the owner replied in.
    (3, 3, "U2", "their root"),
    (4, 3, "U1", "my reply"),
    // The owner wrote nothing here; a reply mentions them directly.
    (5, 5, "U2", "quiet root"),
    (6, 5, "U2", "hey <@U1> take a look"),
    // Broadcasts, a longer user id and another user: none of these count.
    (7, 7, "U2", "<!here> and <!channel>, plus <@U12>"),
    (8, 7, "U2", "<!subteam^S1> with <@U2>"),
    // Outside any thread, so out of the list however it reads.
    (9, 0, "U1", "not in a thread"),
    (10, 0, "U2", "<@U1> outside a thread"),
];

/// A single-channel archive on disk holding `messages`, for tests.
#[cfg(test)]
pub(crate) fn thread_database(dir: &Path, messages: &[(i64, i64, &str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    let conn = Connection::open(dir.join("slackdump.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE CHUNK(ID INTEGER, UNIX_TS INTEGER);
         CREATE TABLE CHANNEL(ID TEXT, NAME TEXT, DATA BLOB, CHUNK_ID INTEGER);
         CREATE TABLE CHANNEL_USER(CHANNEL_ID TEXT, USER_ID TEXT);
         CREATE TABLE MESSAGE(ID INTEGER, CHUNK_ID INTEGER, CHANNEL_ID TEXT, TS TEXT,
             PARENT_ID INTEGER, THREAD_TS TEXT, IS_PARENT INTEGER, LATEST_REPLY TEXT, TXT TEXT, DATA BLOB);
         INSERT INTO CHUNK VALUES(1,100);
         INSERT INTO CHANNEL VALUES ('C1','one',CAST('{}' AS BLOB),1);",
    )
    .unwrap();
    for &(id, root, user, text) in messages {
        let ts = format!("{id}.000000");
        let data = serde_json::json!({"text":text,"ts":ts,"user":user})
            .to_string()
            .into_bytes();
        conn.execute(
            "INSERT INTO MESSAGE VALUES(?1,1,'C1',?2,?3,?4,?5,NULL,?6,?7)",
            params![
                id * 1_000_000,
                ts,
                (root != 0).then_some(root * 1_000_000),
                (root != 0).then(|| format!("{root}.000000")),
                i64::from(root == id),
                text,
                data
            ],
        )
        .unwrap();
    }
}

/// A slackdump archive holding several channels, for the tests of the
/// `threads/` set. `channels` are `(id, name, kind)` and get the `CHANNEL`
/// row a real archive carries, flags included; `messages` are
/// `(channel, id, root, user, text)` in `thread_database`'s shape. `S_USER`
/// gets one row per author, named after the id in lower case. No
/// `CHANNEL_USER` rows are written: that is what a `threads/` archive looks
/// like, and `add_members` puts a membership in where a test wants one.
#[cfg(test)]
pub(crate) fn channel_database(
    dir: &Path,
    channels: &[(&str, &str, Kind)],
    messages: &[(&str, i64, i64, &str, &str)],
) {
    std::fs::create_dir_all(dir).unwrap();
    let conn = Connection::open(dir.join("slackdump.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE CHUNK(ID INTEGER, UNIX_TS INTEGER);
         CREATE TABLE CHANNEL(ID TEXT, NAME TEXT, DATA BLOB, CHUNK_ID INTEGER);
         CREATE TABLE CHANNEL_USER(CHANNEL_ID TEXT, USER_ID TEXT);
         CREATE TABLE S_USER(ID TEXT, USERNAME TEXT, DATA BLOB);
         CREATE TABLE MESSAGE(ID INTEGER, CHUNK_ID INTEGER, CHANNEL_ID TEXT, TS TEXT,
             PARENT_ID INTEGER, THREAD_TS TEXT, IS_PARENT INTEGER, LATEST_REPLY TEXT, TXT TEXT, DATA BLOB);
         INSERT INTO CHUNK VALUES(1,100);",
    )
    .unwrap();
    for &(id, name, kind) in channels {
        let data = serde_json::json!({
            "id": id,
            "name": name,
            "is_im": kind == Kind::Im,
            "is_mpim": kind == Kind::Mpim,
            "is_private": matches!(kind, Kind::Private | Kind::Mpim),
            "is_archived": false,
        })
        .to_string()
        .into_bytes();
        conn.execute(
            "INSERT INTO CHANNEL VALUES(?1,?2,?3,1)",
            params![id, name, data],
        )
        .unwrap();
    }
    let mut authors: Vec<&str> = messages.iter().map(|&(_, _, _, user, _)| user).collect();
    authors.sort_unstable();
    authors.dedup();
    for user in authors {
        let data = serde_json::json!({"id": user, "name": user.to_lowercase()})
            .to_string()
            .into_bytes();
        conn.execute(
            "INSERT INTO S_USER VALUES(?1,?2,?3)",
            params![user, user.to_lowercase(), data],
        )
        .unwrap();
    }
    for &(channel, id, root, user, text) in messages {
        let ts = format!("{id}.000000");
        let data = serde_json::json!({"text":text,"ts":ts,"user":user})
            .to_string()
            .into_bytes();
        conn.execute(
            "INSERT INTO MESSAGE VALUES(?1,1,?2,?3,?4,?5,?6,NULL,?7,?8)",
            params![
                id * 1_000_000,
                channel,
                ts,
                (root != 0).then_some(root * 1_000_000),
                (root != 0).then(|| format!("{root}.000000")),
                i64::from(root == id),
                text,
                data
            ],
        )
        .unwrap();
    }
}

/// `CHANNEL_USER` rows for a fixture archive, as `(channel, user)`.
#[cfg(test)]
pub(crate) fn add_members(dir: &Path, rows: &[(&str, &str)]) {
    let conn = Connection::open(dir.join("slackdump.sqlite")).unwrap();
    for &(channel, user) in rows {
        conn.execute(
            "INSERT INTO CHANNEL_USER VALUES(?1,?2)",
            params![channel, user],
        )
        .unwrap();
    }
}

/// Merge `extra` into one fixture message's stored JSON, so a root can carry
/// what a Slack root carries: `reply_count`, `reply_users`,
/// `reply_users_count`. `id` is the fixture's short id, not the message id.
#[cfg(test)]
pub(crate) fn set_message_data(dir: &Path, id: i64, extra: Value) {
    let conn = Connection::open(dir.join("slackdump.sqlite")).unwrap();
    let merged = extra.to_string();
    conn.execute(
        "UPDATE MESSAGE SET DATA = CAST(json_patch(CAST(DATA AS TEXT), ?1) AS BLOB) WHERE ID = ?2",
        params![merged, id * 1_000_000],
    )
    .unwrap();
}

/// A scratch directory this test alone owns.
#[cfg(test)]
pub(crate) fn test_dir(slug: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "slack-{slug}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[cfg(test)]
mod union_tests {
    use super::*;

    fn database(dir: &Path, chunk_time: i64, messages: &[(i64, Option<i64>, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let conn = Connection::open(dir.join("slackdump.sqlite")).unwrap();
        conn.execute_batch("CREATE TABLE CHUNK(ID INTEGER, UNIX_TS INTEGER);
            CREATE TABLE CHANNEL(ID TEXT, NAME TEXT, DATA BLOB, CHUNK_ID INTEGER);
            CREATE TABLE CHANNEL_USER(CHANNEL_ID TEXT, USER_ID TEXT);
            CREATE TABLE MESSAGE(ID INTEGER, CHUNK_ID INTEGER, CHANNEL_ID TEXT, TS TEXT,
                PARENT_ID INTEGER, THREAD_TS TEXT, IS_PARENT INTEGER, LATEST_REPLY TEXT, TXT TEXT, DATA BLOB);
            INSERT INTO CHANNEL VALUES ('C1','same',CAST('{}' AS BLOB),1);").unwrap();
        conn.execute("INSERT INTO CHUNK VALUES(1,?1)", [chunk_time]).unwrap();
        for &(id, parent, text) in messages {
            let ts = format!("{id}.000000");
            let data = serde_json::json!({"text":text,"ts":ts,"user":"U1"}).to_string().into_bytes();
            conn.execute("INSERT INTO MESSAGE VALUES(?1,1,'C1',?2,?3,?4,0,NULL,?5,?6)",
                params![id*1_000_000, ts, parent.map(|p|p*1_000_000), parent.map(|p|format!("{p}.000000")),text,data]).unwrap();
        }
    }

    #[test]
    fn author_search_filters_before_limit_and_keeps_replies_and_channels() {
        let root=std::env::temp_dir().join(format!("slack-author-test-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        database(&root,100,&[(1,None,"nginx old"),(2,Some(1),"nginx reply"),(3,None,"U1 mentioned by somebody else")]);
        let connection=Connection::open(root.join("slackdump.sqlite")).unwrap();
        connection.execute("UPDATE MESSAGE SET DATA=CAST(json_set(CAST(DATA AS TEXT),'$.user','U2') AS BLOB) WHERE ID=3000000",[]).unwrap();
        connection.execute("INSERT INTO MESSAGE SELECT ID,CHUNK_ID,'D1',TS,PARENT_ID,THREAD_TS,IS_PARENT,LATEST_REPLY,TXT,DATA FROM MESSAGE WHERE ID=2000000",[]).unwrap();
        drop(connection);
        let archive=Archive::open("test".into(),&root).unwrap();
        let messages=archive.search_filtered(Some("C1"),"",Some("U1"),1).unwrap();
        assert_eq!(messages.len(),1);assert_eq!(messages[0].id,2_000_000);assert_eq!(messages[0].parent_id,Some(1_000_000));
        assert_eq!(archive.search_filtered(None,"nginx",Some("U1"),10).unwrap().len(),3);
        assert!(archive.search_filtered(None,"somebody",Some("U1"),10).unwrap().is_empty());
        assert_eq!(archive.search("C1","somebody",10).unwrap().len(),1);
        drop(archive);std::fs::remove_dir_all(root).unwrap();
    }

    /// What `/find message:` asks the archive: text alone, every author, every
    /// channel, replies included, newest first, cut at the limit.
    #[test]
    fn text_search_without_an_author_spans_authors_channels_and_replies() {
        let root=std::env::temp_dir().join(format!("slack-message-test-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        database(&root,100,&[(1,None,"nginx old"),(2,Some(1),"nginx reply"),(3,None,"nginx by somebody else")]);
        let connection=Connection::open(root.join("slackdump.sqlite")).unwrap();
        connection.execute("UPDATE MESSAGE SET DATA=CAST(json_set(CAST(DATA AS TEXT),'$.user','U2') AS BLOB) WHERE ID=3000000",[]).unwrap();
        connection.execute("INSERT INTO MESSAGE SELECT ID,CHUNK_ID,'D1',TS,PARENT_ID,THREAD_TS,IS_PARENT,LATEST_REPLY,TXT,DATA FROM MESSAGE WHERE ID=2000000",[]).unwrap();
        drop(connection);
        let archive=Archive::open("test".into(),&root).unwrap();
        let channel=archive.search_filtered(Some("C1"),"nginx",None,10).unwrap();
        assert_eq!(channel.iter().map(|message|message.id).collect::<Vec<_>>(),[3_000_000,2_000_000,1_000_000]);
        assert_eq!(channel[1].parent_id,Some(1_000_000));
        assert_eq!(channel.iter().filter(|message|message.user.as_deref()==Some("U2")).count(),1);
        let everywhere=archive.search_filtered(None,"nginx",None,10).unwrap();
        assert_eq!(everywhere.len(),4);
        assert!(everywhere.iter().any(|message|message.channel_id=="D1"));
        assert_eq!(archive.search_filtered(None,"nginx",None,2).unwrap().len(),2);
        assert!(archive.search_filtered(Some("C1"),"absent",None,10).unwrap().is_empty());
        drop(archive);std::fs::remove_dir_all(root).unwrap();
    }

    /// Membership of the THREADS list: a thread counts when the owner wrote
    /// any message in it, or when any message in it mentions them as `<@ME>`.
    /// Broadcasts, subteams and another user's mention do not bring one in,
    /// and neither does a message outside a thread.
    #[test]
    fn my_threads_takes_authored_and_directly_mentioned_threads_only() {
        let root = test_dir("my-threads");
        thread_database(&root, THREAD_FIXTURE);
        let archive = Archive::open("test".into(), &root).unwrap();
        let mut mine = archive.my_threads("U1").unwrap();
        mine.sort_by_key(|message| message.id);
        assert_eq!(
            mine.iter().map(|message| message.id).collect::<Vec<_>>(),
            [1_000_000, 3_000_000, 5_000_000]
        );
        // Each root's newest reply, which is what the caller sorts on.
        assert_eq!(
            mine.iter().map(|message| message.latest_reply_id).collect::<Vec<_>>(),
            [Some(2_000_000), Some(4_000_000), Some(6_000_000)]
        );
        // The third is the mention-only thread: the owner wrote nothing in it.
        assert_eq!(mine[2].text, "quiet root");
        assert!(archive
            .thread("C1", 5_000_000)
            .unwrap()
            .iter()
            .all(|message| message.user.as_deref() != Some("U1")));
        // `<@U1>` is not a prefix of `<@U12>`, in either direction.
        assert_eq!(
            archive.my_threads("U12").unwrap().iter().map(|m| m.id).collect::<Vec<_>>(),
            [7_000_000]
        );
        drop(archive);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// What a THREADS card draws under its root: the newest reply the archive
    /// holds, addressed through `PARENT_ID`. A root that Slack says has newer
    /// replies than the last archive run fetched still gets the newest one
    /// there is, and a root nobody answered gets none.
    #[test]
    fn last_reply_returns_the_newest_archived_reply_or_none() {
        let root = test_dir("last-reply");
        thread_database(&root, THREAD_FIXTURE);
        // Slack has three more replies than the archive walked.
        set_message_data(&root, 1, serde_json::json!({"reply_count": 4, "latest_reply": "99.000000"}));
        let archive = Archive::open("test".into(), &root).unwrap();
        let mine = archive.my_threads("U1").unwrap();
        let ahead = mine.iter().find(|m| m.id == 1_000_000).expect("the owner's root");
        assert_eq!(ahead.reply_count, 4);
        assert_eq!(ahead.archived_replies, 1);
        assert_eq!(ahead.latest_reply_id, Some(99_000_000), "a reply no archive holds");
        let last = archive.last_reply("C1", 1_000_000).unwrap().expect("the newest archived reply");
        assert_eq!((last.id, last.text.as_str()), (2_000_000, "someone else replies"));
        // The thread whose newest reply is the mention, and one that is not a
        // thread at all.
        assert_eq!(archive.last_reply("C1", 5_000_000).unwrap().map(|m| m.id), Some(6_000_000));
        assert!(archive.last_reply("C1", 9_000_000).unwrap().is_none());
        // The root's own copy stored beside the replies is not a reply.
        assert!(archive.last_reply("C1", 3_000_000).unwrap().is_some_and(|m| m.id == 4_000_000));
        assert!(archive.last_reply("D1", 1_000_000).unwrap().is_none(), "another channel");
        drop(archive);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overlapping_archives_preserve_union_pages_threads_and_refreshes() {
        let root = std::env::temp_dir().join(format!("slack-archive-union-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let first = root.join("full/first");
        let second = root.join("dms/second");
        database(&first, 100, &[(1,None,"old root"),(2,None,"only first")]);
        database(&second, 200, &[(1,None,"new root"),(3,Some(1),"only second reply")]);
        let original = std::fs::read(first.join("slackdump.sqlite")).unwrap();
        let mut corpus = Corpus::open(&root,30.0,None).unwrap();
        assert_eq!(corpus.convs.len(),1);
        assert_eq!(corpus.convs[0].msgs,3);
        let archive = corpus.conv_archive(&corpus.convs[0]).unwrap();
        assert_eq!(archive.timeline_count("C1").unwrap(),2);
        let newest = archive.timeline_page("C1",None,None,1).unwrap();
        assert_eq!(newest[0].text,"only first");
        let older = archive.timeline_page("C1",Some(newest[0].id),None,1).unwrap();
        assert_eq!(older[0].text,"new root");
        assert_eq!(older[0].archived_replies,1);
        let thread = archive.thread("C1",1_000_000).unwrap();
        assert_eq!(thread.len(),2);
        assert_eq!(thread[1].text,"only second reply");
        assert_eq!(archive.search("C1","only second",10).unwrap().len(),1);
        assert!(archive.search("C1","old root",10).unwrap().is_empty());
        assert!(archive.search_filtered(Some("C1"),"old root",Some("U1"),10).unwrap().is_empty());
        assert_eq!(archive.search_filtered(Some("C1"),"new root",Some("U1"),10).unwrap().len(),1);
        assert_eq!(archive.source_dirs.len(),2);
        assert!(open_ro(&second.join("slackdump.sqlite")).unwrap().execute("DELETE FROM MESSAGE",[]).is_err());
        assert_eq!(std::fs::read(first.join("slackdump.sqlite")).unwrap(),original);
        let writer = Connection::open(second.join("slackdump.sqlite")).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        writer.execute("INSERT INTO MESSAGE SELECT 4000000,CHUNK_ID,CHANNEL_ID,'4.000000',NULL,NULL,0,NULL,'refreshed',DATA FROM MESSAGE LIMIT 1",[]).unwrap();
        assert_eq!(archive.channel_stats("C1").unwrap().0,4);
        let third = root.join("full/third");
        database(&third,300,&[(5,None,"added while running")]);
        corpus.me = Some("U1".into());
        assert_eq!(corpus.add_archive(&third).unwrap(),vec![0]);
        assert_eq!(corpus.convs.len(),1);
        assert_eq!(corpus.convs[0].msgs,5);
        assert_eq!(corpus.convs[0].mine,5);
        for id in 6..18 {
            let path = root.join(format!("full/source{id}"));
            database(&path,id*100,&[(5,None,"new overlapping message"),(id,None,"another unique message")]);
            assert_eq!(corpus.add_archive(&path).unwrap(),vec![0]);
        }
        assert_eq!(corpus.convs.len(),1);
        assert_eq!(corpus.convs[0].msgs,17);
        let archive = corpus.conv_archive(&corpus.convs[0]).unwrap();
        assert_eq!(archive.search("C1","message",14).unwrap().len(),13);
        drop(corpus);
        drop(writer);
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// What `scan_convs` reads off a real `CHANNEL` row for the `Type` sort: the
/// shared flags and the counterpart of a direct message.
#[cfg(test)]
mod conv_type_scan_tests {
    use super::*;

    /// An archive whose channels are the ones given as `(id, name, flags)`,
    /// each with one message so it reaches the conversation list, and whose
    /// `CHANNEL_USER` rows are the memberships given.
    fn database(dir: &Path, channels: &[(&str, &str, Value)], members: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let conn = Connection::open(dir.join("slackdump.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE CHUNK(ID INTEGER, UNIX_TS INTEGER);
             CREATE TABLE CHANNEL(ID TEXT, NAME TEXT, DATA BLOB, CHUNK_ID INTEGER);
             CREATE TABLE CHANNEL_USER(CHANNEL_ID TEXT, USER_ID TEXT);
             CREATE TABLE MESSAGE(ID INTEGER, CHUNK_ID INTEGER, CHANNEL_ID TEXT, TS TEXT,
                 PARENT_ID INTEGER, THREAD_TS TEXT, IS_PARENT INTEGER, LATEST_REPLY TEXT,
                 TXT TEXT, DATA BLOB);
             INSERT INTO CHUNK VALUES(1,100);",
        )
        .unwrap();
        for (index, (id, name, flags)) in channels.iter().enumerate() {
            let mut data = flags.clone();
            data["id"] = serde_json::json!(id);
            data["name"] = serde_json::json!(name);
            conn.execute(
                "INSERT INTO CHANNEL VALUES(?1,?2,?3,1)",
                params![id, name, data.to_string().into_bytes()],
            )
            .unwrap();
            let ts = format!("{}.000000", index + 1);
            let data = serde_json::json!({"text":"hi","ts":ts,"user":"U1"})
                .to_string()
                .into_bytes();
            conn.execute(
                "INSERT INTO MESSAGE VALUES(?1,1,?2,?3,NULL,NULL,0,NULL,'hi',?4)",
                params![(index as i64 + 1) * 1_000_000, id, ts, data],
            )
            .unwrap();
        }
        for (cid, uid) in members {
            conn.execute("INSERT INTO CHANNEL_USER VALUES(?1,?2)", params![cid, uid])
                .unwrap();
        }
    }

    /// Either shared flag lands on `Conv::shared`, a channel with neither is
    /// unshared, and a direct message's counterpart is the channel object's
    /// own `user` where there is one and the single other member where the
    /// membership settles it — and nobody where it does not.
    #[test]
    fn the_shared_flags_and_the_counterpart_come_off_the_channel_row() {
        let root = std::env::temp_dir().join(format!(
            "slack-conv-type-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        database(
            &root,
            &[
                ("C1", "public", serde_json::json!({})),
                ("C2", "connect", serde_json::json!({"is_shared":true})),
                (
                    "C3",
                    "ext",
                    serde_json::json!({"is_private":true,"is_ext_shared":true}),
                ),
                ("D1", "", serde_json::json!({"is_im":true,"user":"UBOT"})),
                ("D2", "", serde_json::json!({"is_im":true})),
                ("D3", "", serde_json::json!({"is_im":true})),
            ],
            // D2's membership settles its counterpart; D3's does not.
            &[("D2", "U1"), ("D2", "U9"), ("D3", "U1"), ("D3", "U8"), ("D3", "U7")],
        );
        let mut archive = Archive::open("test".into(), &root).unwrap();
        let convs = archive.scan_convs(0, Some("U1"), 30.0, None).unwrap();
        let conv = |id: &str| {
            convs
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no {id} in the scan"))
        };
        assert!(!conv("C1").shared);
        assert!(conv("C2").shared);
        assert!(conv("C3").shared);
        assert_eq!(conv("C3").kind, Kind::Private);
        assert_eq!(conv("D1").im_counterpart.as_deref(), Some("UBOT"));
        assert_eq!(conv("D2").im_counterpart.as_deref(), Some("U9"));
        assert_eq!(conv("D3").im_counterpart, None);
        // A channel is not a direct message, whatever its members.
        assert_eq!(conv("C1").im_counterpart, None);
        drop(archive);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A membership row whose user id cannot be read is not a member to skip.
    /// Dropping it would leave exactly one other member and settle the
    /// counterpart on that one — typing a direct message as one with an app on
    /// the strength of a row the scan could not read — while
    /// `Archive::im_counterpart` fails on the same archive. The scan names
    /// nobody instead, and a counterpart that cannot be resolved is a person.
    /// The channel object's own `user` still answers where there is one: that
    /// resolution never reaches the membership.
    #[test]
    fn an_unreadable_membership_row_leaves_a_direct_message_without_a_counterpart() {
        let root = std::env::temp_dir().join(format!(
            "slack-conv-type-null-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        database(
            &root,
            &[
                ("D1", "", serde_json::json!({"is_im":true})),
                ("D2", "", serde_json::json!({"is_im":true,"user":"UBOT"})),
                ("D3", "", serde_json::json!({"is_im":true})),
            ],
            &[
                ("D1", "U1"),
                ("D1", "UBOT"),
                ("D2", "U1"),
                ("D2", "UBOT"),
                ("D3", "U1"),
                ("D3", "UBOT"),
            ],
        );
        {
            let conn = Connection::open(root.join("slackdump.sqlite")).unwrap();
            // The row the scan used to drop: it belongs to D1 and D2, so both
            // memberships carry one, and D3 keeps a readable membership.
            for cid in ["D1", "D2"] {
                conn.execute("INSERT INTO CHANNEL_USER VALUES(?1,NULL)", params![cid])
                    .unwrap();
            }
        }
        let mut archive = Archive::open("test".into(), &root).unwrap();
        let convs = archive.scan_convs(0, Some("U1"), 30.0, None).unwrap();
        let conv = |id: &str| {
            convs
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no {id} in the scan"))
        };
        assert_eq!(conv("D1").im_counterpart, None);
        // The channel object names D2's counterpart, so its membership, NULL
        // row and all, is never read.
        assert_eq!(conv("D2").im_counterpart.as_deref(), Some("UBOT"));
        assert_eq!(conv("D3").im_counterpart.as_deref(), Some("UBOT"));
        // The two answers agree: where the scan names nobody the method fails,
        // and where the scan names somebody the method names the same user.
        assert!(archive.im_counterpart("D1", Some("U1")).is_err());
        assert_eq!(
            archive.im_counterpart("D2", Some("U1")).unwrap(),
            Counterpart::User("UBOT".to_string())
        );
        assert_eq!(
            archive.im_counterpart("D3", Some("U1")).unwrap(),
            Counterpart::User("UBOT".to_string())
        );
        drop(archive);
        std::fs::remove_dir_all(root).unwrap();
    }
}
