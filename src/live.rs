//! Going to Slack when the cache cannot answer. Two engines: the Web API
//! through the desktop app's session (fast, the default once signed in),
//! and `slackdump` runs (the fallback, and the only writer of archives).
//! Everything runs in a background thread and reports through a channel;
//! nothing here touches an archive database or prints a credential.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::api::{self, Client};
use crate::archive::Msg;
use crate::auth;

pub enum JobKind {
    OpenBrowser,
    ConversationRefresh { gen: u64 },
    SentContext { generation: u64, focus: i64, channel: String },
    Sent { generation: u64, append: bool },
    Saved,
    MessageLink { raw_id: u64, link: crate::raw::Link },
    /// Resume one archive directory; `conv` is the conversation to reload.
    Refresh {
        conv: usize,
        before: i64,
    },
    /// Fetch one thread.
    Thread {
        cid: String,
        root: i64,
        focus: i64,
    },
    /// Workspace-wide message search.
    Search {
        query: String,
    },
    /// The cross-conversation scan of the archives themselves, behind
    /// `/find message:` and `/find from:@name` from the conversation list.
    ArchiveScan {
        query: String,
    },
    /// Archive a conversation not in the cache yet.
    ArchiveNew {
        spec: String,
    },
    /// Sign in through the environment, the cached token, or the desktop app.
    Auth,
    /// Messages newer than what is loaded for `conv`.
    Tail {
        conv: usize,
    },
    /// Messages older than what is loaded for `conv` (API-only conversations).
    Older {
        conv: usize,
    },
    Newer { conv: usize },
    /// The conversations the user is a member of.
    Conversations,
    Profiles,
    Usergroups,
    /// Unread state per conversation, tagged with the generation it was asked for.
    Counts {
        gen: u64,
    },
    /// One file into the file cache.
    File {
        id: String,
    },
    /// The read marker of `conv` moved to message `id`.
    Mark {
        conv: usize,
        id: i64,
    },
    /// A message posted to `conv`, into the thread rooted at `thread` when given.
    Send {
        conv: usize,
        thread: Option<i64>,
    },
    Upload {
        conv: usize,
        thread: Option<i64>,
    },
    /// One of the owner's own messages withdrawn from Slack. The conversation
    /// and the thread it was a reply in travel with it: a THREADS card counts
    /// replies it does not draw, and the message is gone from every list by
    /// the time Slack answers.
    Delete {
        id: i64,
        cid: String,
        root: Option<i64>,
    },
    /// Membership of `conv` given up.
    Leave {
        conv: usize,
    },
    /// The unread messages of one conversation, for an UNREADS card the
    /// archive could not fill.
    UnreadHistory {
        cid: String,
    },
    /// The owner's recent threads, as `search.messages` names them: the
    /// search half of the THREADS live phase.
    ThreadSearch,
    /// One thread's root and newest reply, for a THREADS card the archive
    /// does not hold.
    ThreadCard {
        cid: String,
        root: i64,
    },
    /// The channels muted in Slack itself.
    MutedChannels {
        gen: u64,
    },
    SetMuted,
    StarredChannels { gen: u64 },
    SetStarred,
}

impl JobKind {
    fn log_name(&self) -> &'static str {
        match self {
            Self::OpenBrowser => "open_browser",
            Self::ConversationRefresh { .. } => "conversation_refresh",
            Self::SentContext {..} => "sent_context",
            Self::MessageLink {..} => "message_link",
            Self::Saved => "saved_messages",
            Self::Sent { .. } => "sent_messages",
            Self::Refresh {..} => "refresh", Self::Thread {..} => "thread", Self::Search {..} => "search",
            Self::ArchiveScan {..} => "archive_scan",
            Self::ArchiveNew {..} => "archive", Self::Auth => "auth", Self::Tail {..} => "tail",
            Self::Older {..} => "older", Self::Newer {..} => "newer", Self::Conversations => "conversations",
            Self::Profiles => "profiles", Self::Usergroups => "usergroups", Self::Counts {..} => "counts",
            Self::File {..} => "file", Self::Mark {..} => "mark", Self::Send {..} => "send", Self::Upload {..} => "upload",
            Self::Delete {..} => "delete", Self::Leave {..} => "leave",
            Self::UnreadHistory {..} => "unread_history",
            Self::ThreadSearch => "thread_search", Self::ThreadCard {..} => "thread_card",
            Self::MutedChannels {..} => "muted_channels", Self::SetMuted => "set_muted",
            Self::StarredChannels {..} => "starred_channels", Self::SetStarred => "set_starred",
        }
    }
}

pub enum Done {
    BrowserOpened,
    ConversationSnapshot(Vec<Value>, Value),
    MessageContext(crate::file_message::Location),
    SentPage(crate::sent::Page),
    Saved(Vec<Msg>),
    Refreshed,
    Thread(PathBuf),
    ThreadMsgs(Vec<Msg>),
    Search(PathBuf),
    /// What Slack returned for a search, and whether that is everything it
    /// holds for the query or only as far as the cap reached.
    SearchHits { hits: Vec<Msg>, complete: bool },
    /// What the archive scan found: the hit list the view shows, whether the
    /// cap truncated it, and the user maps the worker read, by archive index,
    /// so the next scan and the UI's own rendering do not read them again.
    ArchiveHits {
        hits: Vec<Msg>,
        capped: bool,
        users: Vec<(usize, std::sync::Arc<std::collections::HashMap<String, crate::archive::User>>)>,
    },
    /// The unread messages of one conversation, straight from Slack, oldest
    /// first. They go on the view's card and nowhere else: slackdump stays
    /// the only writer of an archive. Which conversation they belong to
    /// travels on the job's own `UnreadHistory` kind.
    UnreadHistory {
        msgs: Vec<Msg>,
        /// The walk reached the read marker, so `msgs` is the whole unread
        /// run and its first message is the first unread one. False when the
        /// page cap or Slack's own count stopped the walk short: the oldest
        /// message fetched is then only the oldest of a window, and the card
        /// must not present it as the first unread message.
        complete: bool,
    },
    /// The threads `search.messages` named, newest match first, deduplicated.
    /// What the archive already holds is filtered out on this side, where the
    /// view's own cards say what that is.
    ThreadCandidates(Vec<ThreadTarget>),
    /// One thread from Slack: its root, and the newest reply under it. `root`
    /// is None for a thread Slack no longer has — the phase skips it rather
    /// than stopping. Which thread it is travels on the job's `ThreadCard`.
    ThreadCard {
        root: Option<Box<Msg>>,
        last: Option<Msg>,
    },
    Archived(PathBuf),
    Auth(Arc<Client>, String),
    Messages(Vec<Msg>),
    NewerMessages(Vec<Msg>, bool),
    OlderMessages(Vec<Msg>, bool),
    Conversations(Vec<Value>),
    Profiles(Vec<Value>, Option<String>),
    Usergroups(Vec<Value>, Option<String>),
    Counts(Value),
    File(PathBuf),
    Marked,
    Sent(Box<Msg>),
    /// A file reached Slack; the name it went up under.
    Uploaded(String),
    /// A message Slack no longer holds.
    Deleted,
    Left,
    MutedChannels(Vec<String>),
    StarredChannels(Vec<String>),
    StarChanged { cid: String, starred: bool, ids: Vec<String> },
    MuteChanged {
        cid: String,
        muted: bool,
        ids: Vec<String>,
    },
}

pub struct Job {
    pub navigate_on_completion: bool,
    pub kind: JobKind,
    pub label: String,
    pub started: Instant,
    rx: Receiver<Result<Done, String>>,
}

impl Job {
    #[cfg(test)]
    pub(crate) fn wait_for_test(&self) -> Result<Done, String> {
        self.rx.recv_timeout(std::time::Duration::from_secs(2)).expect("job timed out")
    }

    #[cfg(test)]
    pub(crate) fn completed_for_test(kind: JobKind, outcome: Result<Done, String>) -> Self {
        let (sender, rx) = mpsc::channel();
        sender.send(outcome).unwrap();
        Self { navigate_on_completion: true, kind, label: "test".into(), started: Instant::now(), rx }
    }

    /// The outcome, once; None while the job is still running.
    pub fn poll(&self) -> Option<Result<Done, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("worker thread died".to_string())),
        }
    }
}

pub(crate) fn spawn(
    kind: JobKind,
    label: String,
    work: impl FnOnce() -> Result<Done, String> + Send + 'static,
) -> Job {
    let (tx, rx) = mpsc::channel();
    let log = crate::session_log::JobLog::start(kind.log_name());
    std::thread::spawn(move || {
        let result = work();
        let id = log.complete(result.as_ref().err().map(String::as_str));
        if tx.send(result).is_err() {
            crate::session_log::record("job_delivery_dropped", serde_json::json!({"id":id}));
        }
    });
    Job {
        navigate_on_completion: true,
        kind,
        label,
        started: Instant::now(),
        rx,
    }
}

/// `1782927057158059` -> `1782927057.158059`, the form the API takes.
pub fn id_to_ts(id: i64) -> String {
    format!("{}.{:06}", id / 1_000_000, id % 1_000_000)
}

// ------------------------------------------------------------------- auth

pub fn sign_in(workspace_url: String, scratch: PathBuf) -> Job {
    spawn(JobKind::Auth, "signing in".to_string(), move || {
        let agent = api::agent();
        let mut errors = Vec::new();
        let mut candidates: Vec<(auth::Auth, bool)> = Vec::new();
        if let Some(a) = auth::from_env() {
            candidates.push((a, false));
        }
        if let Some(a) = auth::load_cached() {
            candidates.push((a, false));
        }
        for (a, save) in candidates {
            let client = Client::new(a.clone());
            match client.auth_test() {
                Ok((_, user, team)) => {
                    if save {
                        let _ = auth::save_cached(&a);
                    }
                    return Ok(Done::Auth(
                        Arc::new(client),
                        format!("{user} at {team} via {}", a.source),
                    ));
                }
                Err(e) => errors.push(e),
            }
        }
        match auth::from_desktop(&agent, &workspace_url, &scratch) {
            Ok(a) => {
                let client = Client::new(a.clone());
                match client.auth_test() {
                    Ok((_, user, team)) => {
                        let _ = auth::save_cached(&a);
                        Ok(Done::Auth(
                            Arc::new(client),
                            format!("{user} at {team} via the desktop app"),
                        ))
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => {
                errors.push(e);
                Err(errors.join("; "))
            }
        }
    })
}

// -------------------------------------------------------------------- api

fn msgs_from_values(cid: &str, values: Vec<Value>) -> Vec<Msg> {
    values
        .into_iter()
        .filter_map(|v| Msg::from_api(cid.to_string(), v))
        .collect()
}

/// Where a fetched thread lives: `<cache>/threads/<channel>-<root id>.json`
/// from the API, or a slackdump directory of the same stem.
pub fn thread_dir(cache: &Path, cid: &str, root: i64) -> PathBuf {
    cache.join("threads").join(format!("{cid}-{root}"))
}

pub fn thread_file(cache: &Path, cid: &str, root: i64) -> PathBuf {
    thread_dir(cache, cid, root).with_extension("json")
}

pub fn cached_thread(cache: &Path, cid: &str, root: i64) -> Option<Vec<Msg>> {
    let text = std::fs::read_to_string(thread_file(cache, cid, root)).ok()?;
    let values: Vec<Value> = serde_json::from_str(&text).ok()?;
    let msgs = msgs_from_values(cid, values);
    (!msgs.is_empty()).then_some(msgs)
}

pub fn api_message_link(client: Arc<Client>, raw_id: u64, link: crate::raw::Link) -> Job {
    let target = link.clone();
    spawn(JobKind::MessageLink { raw_id, link }, "fetching linked message".into(), move || {
        crate::raw::fetch(&client, &target).map(Done::ThreadMsgs)
    })
}

pub fn api_thread(client: Arc<Client>, cache: &Path, cid: String, root: i64, focus: i64) -> Job {
    let file = thread_file(cache, &cid, root);
    let c = cid.clone();
    spawn(
        JobKind::Thread { cid, root, focus },
        "fetching the thread".to_string(),
        move || {
            let values = client.replies(&c, &id_to_ts(root))?;
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&file, serde_json::to_string(&values).unwrap_or_default());
            Ok(Done::ThreadMsgs(msgs_from_values(&c, values)))
        },
    )
}

pub fn api_search(client: Arc<Client>, query: String) -> Job {
    api_search_labeled(client, query.clone(), query)
}

pub fn api_search_labeled(client: Arc<Client>, query: String, label: String) -> Job {
    let q = query;
    spawn(
        JobKind::Search { query: label },
        format!("searching Slack for '{q}'"),
        move || {
            // Paged to the same cap the archive scan uses, so the two hit
            // lists are comparable and the difference between them means
            // something.
            let (matches, complete) = client.search_all(&q, crate::archive::SEARCH_CAP)?;
            let mut out = Vec::new();
            for m in matches {
                let cid = m
                    .pointer("/channel/id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = m
                    .pointer("/channel/name")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(mut msg) = Msg::from_api(cid, m) {
                    msg.channel_name = name.filter(|n| !n.is_empty()).map(|n| format!("#{n}"));
                    out.push(msg);
                }
            }
            Ok(Done::SearchHits { hits: out, complete })
        },
    )
}

// ----------------------------------------------------------- archive scan

/// One conversation the archive scan visits.
pub struct ScanTarget {
    pub cid: String,
    /// The conversation as the list names it, for the progress line.
    pub name: String,
    /// Index into the request's `archives`.
    pub archive: usize,
}

/// Everything the scan worker needs, by value: it opens its own read-only
/// connections and renders message text against its own name snapshot, so it
/// never borrows `App` or the live `Corpus`.
pub struct ScanRequest {
    pub targets: Vec<ScanTarget>,
    pub archives: Vec<crate::archive::ArchiveHandle>,
    /// The needle as typed; empty for an author-only search.
    pub needle: String,
    pub author: Option<String>,
    pub cap: usize,
    /// "message" or "author": how a failure names the search that failed.
    pub kind: &'static str,
    /// Channel id -> the name a hit carries, as the conversation list has it.
    pub conv_names: std::collections::HashMap<String, String>,
    pub names: crate::archive::Corpus,
    pub palette: crate::palette::Palette,
    pub tz: crate::render::Tz,
    pub image_font: Option<(u16, u16)>,
    /// Held closed, the worker waits before touching an archive, so a test
    /// can decide what lands first.
    #[cfg(test)]
    pub gate: Option<mpsc::Receiver<()>>,
}

/// A line the scan wants shown in the progress box, and whether it is
/// incidental detail rather than the scan's own narration.
pub struct ScanLine {
    pub text: String,
    pub dim: bool,
}

impl ScanLine {
    pub fn plain(text: impl Into<String>) -> ScanLine {
        ScanLine { text: text.into(), dim: false }
    }
    pub fn dim(text: impl Into<String>) -> ScanLine {
        ScanLine { text: text.into(), dim: true }
    }
}

/// Scan every conversation in `request`, narrating into `progress`. Dropping
/// the progress receiver ends the scan at the next conversation boundary,
/// which is how Esc cancels it.
pub fn archive_scan(
    query: String,
    request: ScanRequest,
    progress: mpsc::Sender<ScanLine>,
) -> Job {
    let label = format!("searching the archives for '{query}'");
    spawn(JobKind::ArchiveScan { query }, label, move || {
        use crate::archive::Archive;
        use crate::render::{self, Ctx};
        use std::collections::{HashMap, HashSet};

        #[cfg(test)]
        let gate = request.gate;
        #[cfg(test)]
        if let Some(gate) = gate { let _ = gate.recv(); }
        let ScanRequest {
            targets, archives, needle, author, cap, kind, conv_names, names, palette, tz, image_font, ..
        } = request;
        let author = author.as_deref();
        let say = |line: ScanLine| progress.send(line).map_err(|_| "search cancelled".to_string());
        say(ScanLine::dim(format!(
            "sqlite: {}",
            crate::archive::search_filter_sql(&needle, author, cap)
        )))?;
        let mut opened: HashMap<usize, Archive> = HashMap::new();
        let mut hits: Vec<Msg> = Vec::new();
        let mut seen: HashSet<(String, i64)> = HashSet::new();
        let mut capped = false;
        let lowered = needle.to_lowercase();
        for target in &targets {
            let archive = match opened.entry(target.archive) {
                std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    let handle = archives
                        .get(target.archive)
                        .ok_or_else(|| format!("{kind} search: {} has no archive", target.name))?;
                    slot.insert(handle.open().map_err(|error| format!("{kind} search: {error}"))?)
                }
            };
            let candidates = archive
                .search_filtered(Some(&target.cid), &needle, author, cap)
                .map_err(|error| format!("{kind} search: {error}"))?;
            capped |= candidates.len() >= cap;
            let ctx = Ctx {
                archive: Some(archive), corpus: &names, tz, image_font,
                last_read: None, palette: &palette,
            };
            let mut found = 0usize;
            for mut message in candidates {
                if !lowered.is_empty()
                    && !render::plain(&render::body(&message, &ctx)).to_lowercase().contains(&lowered)
                    && !render::message_urls(&message).iter().any(|url| url.to_lowercase().contains(&lowered))
                {
                    continue;
                }
                message.channel_name = conv_names.get(&message.channel_id).cloned();
                if seen.insert((message.channel_id.clone(), message.id)) {
                    hits.push(message);
                    found += 1;
                }
            }
            say(ScanLine::plain(format!(
                "searching {} … {}",
                target.name,
                match found {
                    0 => "none".to_string(),
                    1 => "1 hit".to_string(),
                    n => format!("{n} hits"),
                }
            )))?;
        }
        hits.sort_by_key(|message| std::cmp::Reverse(message.id));
        capped |= hits.len() > cap;
        hits.truncate(cap);
        say(ScanLine::plain(format!(
            "{} conversation{} scanned · {} hit{}{}",
            targets.len(),
            if targets.len() == 1 { "" } else { "s" },
            hits.len(),
            if hits.len() == 1 { "" } else { "s" },
            if capped { format!(" (capped at {cap})") } else { String::new() },
        )))?;
        let users = opened
            .iter()
            .filter_map(|(index, archive)| Some((*index, archive.loaded_users()?)))
            .collect();
        Ok(Done::ArchiveHits { hits, capped, users })
    })
}

// ---------------------------------------------------------- unread fetch

/// One conversation the UNREADS live phase asks Slack about.
pub struct UnreadTarget {
    pub cid: String,
    /// The conversation as the list names it, for the progress line.
    pub name: String,
    /// The read marker, which the call passes as `oldest`, exclusive.
    pub oldest: i64,
    /// What Slack last said is unread there, where it said anything. Only a
    /// count can say that a full page is not the whole run.
    pub unread_count: Option<i64>,
}

/// Messages per `conversations.history` call.
const UNREAD_PAGE: usize = 100;
/// Calls per conversation. Three pages is three hundred unread messages, and
/// a conversation past that is not one anybody reads off a card.
const UNREAD_PAGES: usize = 3;

/// The unread messages of one conversation from Slack: `conversations.history`
/// from the read marker forward, newest first, a page at a time, walking
/// `latest` back towards the marker. A page goes out only while Slack says
/// there is more behind the one before it and its own count says the run is
/// not covered yet.
///
/// Paging is driven by `has_more` and the cursor, never by a page coming back
/// full: Slack is free to return fewer rows than the limit and still have
/// more, and a fetch that stopped on a short page would report that short
/// page as the whole unread run.
///
/// One HTTP request per turn of the loop, so `UNREAD_PAGES` caps the requests
/// and not merely the pages that carried something. `history_page` would read
/// through empty pages on its own, a hundred requests deep, with no way for
/// this side to give up in the middle of them.
///
/// Narrates one line per request into `progress`, and that send is the
/// cancellation check: the box holds the other end, so Esc stops the walk at
/// the next request boundary rather than at the next conversation.
pub fn api_unread_history(
    client: Arc<Client>,
    target: UnreadTarget,
    progress: mpsc::Sender<ScanLine>,
) -> Job {
    let UnreadTarget { cid, name, oldest, unread_count } = target;
    let label = format!("fetching the unread messages of {name}");
    let channel = cid.clone();
    spawn(JobKind::UnreadHistory { cid }, label, move || {
        let marker = id_to_ts(oldest);
        let mut collected: Vec<Msg> = Vec::new();
        let mut complete = false;
        // `latest` walks the pages back towards the marker; `oldest` stays
        // where the reader left off, so every call names the same marker.
        let mut latest: Option<String> = None;
        // Set only while reading through a page that came back empty; a page
        // with messages in it is walked from with `latest` instead.
        let mut cursor: Option<String> = None;
        for _ in 0..UNREAD_PAGES {
            // `inclusive` is false, so the message the marker names — the
            // last one the reader has read — is never part of the answer.
            let (page, more, next) = crate::file_message::history_request(
                &client,
                &channel,
                latest.as_deref(),
                Some(&marker),
                false,
                UNREAD_PAGE,
                cursor.as_deref(),
            )?;
            let got = page.len();
            progress
                .send(ScanLine::plain(format!(
                    "conversations.history {name} oldest={marker} → {got} message{}",
                    if got == 1 { "" } else { "s" }
                )))
                .map_err(|_| "unread fetch cancelled".to_string())?;
            let edge = page.first().map(|message| id_to_ts(message.id));
            // Each page is older than the one before it.
            collected.splice(0..0, page);
            if !more {
                // Slack has nothing left between the marker and this page:
                // the walk reached the marker and the run is whole.
                complete = true;
                break;
            }
            // Enough: Slack's count is covered by what came back, or, with no
            // count to go by, a page carried something and that is as far as
            // one card is worth reading. An empty page has collected nothing,
            // so it is never enough — it is the case the cursor is for.
            if !collected.is_empty()
                && !unread_count.is_some_and(|count| (collected.len() as i64) < count)
            {
                break;
            }
            match edge {
                // An empty page carries no bound to walk from, so the cursor
                // is the only way on — and it costs one of the three
                // requests, not a hundred.
                None if !next.is_empty() => cursor = Some(next),
                None => break,
                Some(edge) => {
                    cursor = None;
                    latest = Some(edge);
                }
            }
        }
        collected.dedup_by_key(|message| message.id);
        Ok(Done::UnreadHistory { msgs: collected, complete })
    })
}

// ---------------------------------------------------------- thread fetch

/// One thread the THREADS live phase asks Slack about. All that is known of
/// it before the first call is where it lives and what its root is: the
/// search answers with a reply, not with the thread.
#[derive(Clone)]
pub struct ThreadTarget {
    pub cid: String,
    /// The conversation as the search named it, for the card's header and
    /// the progress line; the channel id where Slack sent no name.
    pub name: String,
    /// The thread root, as a message id.
    pub root: i64,
    /// The newest match that named this thread, across both queries. The
    /// per-run cap orders by it: the queries are asked one after the other,
    /// so collection order is `from:me`'s week and then the mentions' week,
    /// and a cap taken in that order would drop today's mention for a
    /// six-day-old message of the owner's.
    pub newest: i64,
}

/// Matches per `search.messages` page.
const THREAD_SEARCH_PAGE: usize = 100;
/// Pages per query. Five pages of a hundred is five hundred of the owner's
/// own messages in a week, which no week has; the window is what normally
/// ends the walk, and this ends it when Slack answers with something else.
const THREAD_SEARCH_PAGES: usize = 5;

/// The thread ids the owner's recent messages and mentions name, newest
/// first: `search.messages` for `from:me` and then for the literal `<@ME>`,
/// paged while the matches stay inside the window.
///
/// A match names its thread only in its permalink — `?thread_ts=…` — which is
/// the extraction `slackdump-my-threads` does in jq, and a match
/// without one is a message in no thread and brings nothing in.
///
/// `since` is the window's start in whole seconds. Slack sorts the matches by
/// timestamp descending, so a page whose last match is older than that is the
/// page the window ends on: nothing behind it can be inside.
///
/// Narrates one line per request into `progress`, and that send is the
/// cancellation check: the box holds the other end, so Esc stops the walk at
/// the next request boundary.
pub fn api_thread_search(
    client: Arc<Client>,
    me: String,
    since: i64,
    progress: mpsc::Sender<ScanLine>,
) -> Job {
    spawn(
        JobKind::ThreadSearch,
        "looking for your recent threads".to_string(),
        move || {
            let mut out: Vec<ThreadTarget> = Vec::new();
            let mut seen: std::collections::HashMap<(String, i64), usize> =
                std::collections::HashMap::new();
            // The owner's own messages first: a thread they wrote in is a
            // thread of theirs whether or not anyone named them in it.
            for query in ["from:me".to_string(), format!("<@{me}>")] {
                let mut cursor = "*".to_string();
                let mut visited: std::collections::HashSet<String> =
                    std::iter::once(cursor.clone()).collect();
                for page in 0..THREAD_SEARCH_PAGES {
                    let count = THREAD_SEARCH_PAGE.to_string();
                    let response = client.call(
                        "search.messages",
                        &[
                            ("query", query.as_str()),
                            ("count", count.as_str()),
                            ("sort", "timestamp"),
                            ("sort_dir", "desc"),
                            ("cursor", cursor.as_str()),
                            ("highlight", "false"),
                        ],
                    )?;
                    let matches = response
                        .pointer("/messages/matches")
                        .and_then(Value::as_array)
                        .ok_or("Slack returned no message list")?;
                    let mut oldest: Option<i64> = None;
                    let mut found = 0usize;
                    for value in matches {
                        let Some(id) = value
                            .get("ts")
                            .and_then(Value::as_str)
                            .and_then(crate::archive::ts_to_id)
                        else {
                            continue;
                        };
                        let secs = id / 1_000_000;
                        oldest = Some(oldest.map_or(secs, |old: i64| old.min(secs)));
                        if secs < since {
                            continue;
                        }
                        let cid = value
                            .pointer("/channel/id")
                            .and_then(Value::as_str)
                            .filter(|cid| !cid.is_empty());
                        let root = value
                            .get("permalink")
                            .and_then(Value::as_str)
                            .and_then(crate::archive::thread_ts_in_permalink)
                            .and_then(crate::archive::ts_to_id);
                        let (Some(cid), Some(root)) = (cid, root) else {
                            continue;
                        };
                        // The same thread named again — by the other query,
                        // or by an older message of the owner's in it — keeps
                        // the newest of the matches that named it.
                        if let Some(&at) = seen.get(&(cid.to_string(), root)) {
                            out[at].newest = out[at].newest.max(id);
                            continue;
                        }
                        let name = value
                            .pointer("/channel/name")
                            .and_then(Value::as_str)
                            .filter(|name| !name.is_empty())
                            .map(|name| format!("#{name}"))
                            .unwrap_or_else(|| cid.to_string());
                        seen.insert((cid.to_string(), root), out.len());
                        out.push(ThreadTarget { cid: cid.to_string(), name, root, newest: id });
                        found += 1;
                    }
                    progress
                        .send(ScanLine::plain(format!(
                            "search.messages {query:?} page {} → {} match{} · {found} thread{}",
                            page + 1,
                            matches.len(),
                            if matches.len() == 1 { "" } else { "es" },
                            if found == 1 { "" } else { "s" },
                        )))
                        .map_err(|_| "thread fetch cancelled".to_string())?;
                    // The window ended inside this page, so the next one is
                    // wholly behind it.
                    if oldest.is_some_and(|oldest| oldest < since) {
                        break;
                    }
                    let next = response
                        .pointer("/messages/pagination/next_cursor")
                        .or_else(|| response.pointer("/messages/paging/next_cursor"))
                        .and_then(Value::as_str)
                        .filter(|cursor| !cursor.is_empty())
                        .map(str::to_string);
                    // Slack ran out, or repeated a cursor it had already
                    // given, which would page for ever.
                    let Some(next) = next.filter(|next| visited.insert(next.clone())) else {
                        break;
                    };
                    cursor = next;
                }
            }
            // Newest match first, across both queries: the per-run cap then
            // keeps the most recent threads whichever query named them, and a
            // phase stopped halfway has fetched the ones worth having most.
            out.sort_by_key(|target| std::cmp::Reverse(target.newest));
            Ok(Done::ThreadCandidates(out))
        },
    )
}

/// Replies a page of the fallback walk asks for, and pages of it. Five
/// hundred replies is deeper than a thread a card summarises ever goes; the
/// walk exists for an answer Slack should not give at all.
const THREAD_REPLY_PAGE: i64 = 100;
const THREAD_REPLY_PAGES: usize = 5;

/// The two Slack errors that mean the thread the search named is not there to
/// be read: it was deleted, or the owner has left the conversation since the
/// search answered. Neither says anything about the threads behind it, so the
/// candidate is skipped and the phase goes on; every other error stops it.
fn thread_is_gone(error: &str) -> bool {
    error.contains("thread_not_found") || error.contains("channel_not_found")
}

/// The newest message in a `conversations.replies` answer that is not the
/// thread's root. Slack prepends the parent to every answer, and a reply
/// written since the request before can land in a window too.
fn newest_reply(channel: &str, root: i64, response: &Value) -> Option<Msg> {
    response
        .get("messages")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|value| Msg::from_api(channel.to_string(), value.clone()))
        .filter(|message| message.id != root)
        .max_by_key(|message| message.id)
}

/// One thread's card from Slack: the root, and the newest reply under it.
///
/// Two requests, because `conversations.replies` pages a thread forward from
/// its oldest reply: the newest one is reachable only by naming a bound, and
/// the bound — the root's `latest_reply` — is what the first request is for.
/// The second names it as `oldest`, inclusive, so the window holds that reply
/// and nothing before it. `limit` is two rather than one: Slack returns the
/// parent alongside the window whether or not it counts toward the limit, and
/// a limit of one would hand back the parent alone wherever it does.
///
/// A thread with no reply costs one request: there is no bound to ask from,
/// and the card is the root by itself. A root that says it has replies and
/// carries no `latest_reply` costs more: the thread is walked forward, a
/// hundred replies a request, and the newest of what comes back is the
/// card's. Nothing else would find that reply, and a card sorted by its root
/// while every other card sorts by its newest reply is a card in the wrong
/// place in the view.
///
/// A thread Slack no longer has is skipped rather than fetched: `root` comes
/// back None, and the phase goes on to the next thread. Slack says so with an
/// error, which would otherwise stop the phase, and with an empty message
/// list where it answers `ok` for a thread it cannot show.
pub fn api_thread_card(
    client: Arc<Client>,
    target: ThreadTarget,
    progress: mpsc::Sender<ScanLine>,
) -> Job {
    let ThreadTarget { cid, name, root, .. } = target;
    let label = format!("fetching a thread in {name}");
    let channel = cid.clone();
    spawn(JobKind::ThreadCard { cid, root }, label, move || {
        let ts = id_to_ts(root);
        let say = |line: String| {
            progress
                .send(ScanLine::plain(line))
                .map_err(|_| "thread fetch cancelled".to_string())
        };
        let ask = |params: &[(&str, &str)]| -> Result<Option<Value>, String> {
            match client.call("conversations.replies", params) {
                Ok(response) => Ok(Some(response)),
                Err(error) if thread_is_gone(&error) => Ok(None),
                Err(error) => Err(error),
            }
        };
        let gone = |say: &dyn Fn(String) -> Result<(), String>| -> Result<Done, String> {
            say(format!("conversations.replies {name} ts={ts} → no thread"))?;
            Ok(Done::ThreadCard { root: None, last: None })
        };
        let Some(response) = ask(&[
            ("channel", channel.as_str()),
            ("ts", ts.as_str()),
            ("limit", "1"),
        ])?
        else {
            return gone(&say);
        };
        let head = response
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.first())
            .cloned();
        let head = head.and_then(|value| Msg::from_api(channel.clone(), value));
        let Some(head) = head.filter(|message| message.id == root) else {
            return gone(&say);
        };
        say(format!(
            "conversations.replies {name} ts={ts} → the root · {} repl{}",
            head.reply_count,
            if head.reply_count == 1 { "y" } else { "ies" }
        ))?;
        let last = match head.latest_reply_id.filter(|latest| *latest > root) {
            // The bound Slack gave: one request for a window holding that
            // reply and nothing before it.
            Some(latest) => {
                let bound = id_to_ts(latest);
                let Some(response) = ask(&[
                    ("channel", channel.as_str()),
                    ("ts", ts.as_str()),
                    ("oldest", bound.as_str()),
                    ("inclusive", "true"),
                    ("limit", "2"),
                ])?
                else {
                    return gone(&say);
                };
                let last = newest_reply(&channel, root, &response);
                say(format!(
                    "conversations.replies {name} oldest={bound} → {}",
                    if last.is_some() { "the newest reply" } else { "no reply" }
                ))?;
                last
            }
            // Replies, and no bound to ask from. Forward from the oldest is
            // the only order the call pages in, so the whole thread is read
            // and the newest kept. The count is what sizes the page and not
            // what ends the walk: it counts replies Slack will not return,
            // and the cursor is what says whether more are behind the page.
            None if head.reply_count > 0 => {
                let page = (head.reply_count + 1).clamp(1, THREAD_REPLY_PAGE).to_string();
                let mut newest: Option<Msg> = None;
                let mut cursor = String::new();
                for request in 0..THREAD_REPLY_PAGES {
                    let mut params = vec![
                        ("channel", channel.as_str()),
                        ("ts", ts.as_str()),
                        ("limit", page.as_str()),
                    ];
                    if !cursor.is_empty() {
                        params.push(("cursor", cursor.as_str()));
                    }
                    let Some(response) = ask(&params)? else {
                        return gone(&say);
                    };
                    let got = response
                        .get("messages")
                        .and_then(Value::as_array)
                        .map_or(0, |messages| messages.len());
                    let here = newest_reply(&channel, root, &response);
                    if here.as_ref().map(|reply| reply.id) > newest.as_ref().map(|reply| reply.id) {
                        newest = here;
                    }
                    say(format!(
                        "conversations.replies {name} ts={ts} page {} → {got} message{}",
                        request + 1,
                        if got == 1 { "" } else { "s" }
                    ))?;
                    let next = response
                        .pointer("/response_metadata/next_cursor")
                        .and_then(Value::as_str)
                        .filter(|next| !next.is_empty() && *next != cursor)
                        .map(str::to_string);
                    let Some(next) = next else { break };
                    cursor = next;
                }
                newest
            }
            None => None,
        };
        // The root as the archive's own roots come out: knowing its newest
        // reply, whichever request found it. The view sorts every card by
        // that reply, so a root that came back without a `latest_reply` and
        // kept it would sort at its own ts, below cards answered before it.
        let mut head = head;
        if last.as_ref().map(|reply| reply.id) > head.latest_reply_id {
            head.latest_reply_id = last.as_ref().map(|reply| reply.id);
        }
        Ok(Done::ThreadCard { root: Some(Box::new(head)), last })
    })
}

/// Messages after `since` (a message id), ascending.
pub fn api_tail(client: Arc<Client>, conv: usize, cid: String, since: i64, quiet: bool) -> Job {
    let label = if quiet {
        String::new()
    } else {
        "fetching newer messages".to_string()
    };
    spawn(JobKind::Tail { conv }, label, move || {
        let values = client.history(&cid, None, Some(&id_to_ts(since)), 200)?;
        let mut msgs = msgs_from_values(&cid, values);
        msgs.sort_by_key(|m| m.id);
        Ok(Done::Messages(msgs))
    })
}

/// Messages before `before` (a message id, or the newest when 0), ascending.
pub fn api_older(client: Arc<Client>, conv: usize, cid: String, before: i64) -> Job {
    spawn(
        JobKind::Older { conv },
        "fetching older messages".to_string(),
        move || {
            let latest = (before > 0).then(|| id_to_ts(before));
            let (messages, more) = crate::file_message::history_page(&client, &cid, latest.as_deref(), None, false, 200)?;
            Ok(Done::OlderMessages(messages, more))
        },
    )
}

pub fn api_newer(client: Arc<Client>, conv: usize, cid: String, since: i64) -> Job {
    spawn(JobKind::Newer { conv }, "fetching newer messages".into(), move || {
        let (messages, more) = crate::file_message::newer(&client, &cid, &id_to_ts(since), 200)?;
        Ok(Done::NewerMessages(messages, more))
    })
}

pub fn api_conversations(client: Arc<Client>) -> Job {
    spawn(JobKind::Conversations, String::new(), move || {
        Ok(Done::Conversations(client.my_conversations()?))
    })
}

pub fn api_profiles(client: Arc<Client>, cache: PathBuf) -> Job {
    spawn(
        JobKind::Profiles,
        "loading user profiles".into(),
        move || {
            let identity = client.call("auth.test", &[])?;
            let team = identity["team_id"]
                .as_str()
                .ok_or("profiles: auth.test missing team_id")?;
            let (users, warning) = crate::profiles::load_or_fetch(&cache, team, || {
                crate::profiles::fetch(|cursor| {
                    client.call("users.list", &[("limit", "200"), ("cursor", cursor)])
                })
            })?;
            Ok(Done::Profiles(users, warning))
        },
    )
}

pub fn api_usergroups(client: Arc<Client>, cache: PathBuf) -> Job {
    spawn(
        JobKind::Usergroups,
        "loading user groups".into(),
        move || {
            let identity = client.call("auth.test", &[])?;
            let team = identity["team_id"]
                .as_str()
                .ok_or("usergroups: auth.test missing team_id")?;
            let (groups, warning) = crate::profiles::load_groups(&cache, team, || {
                crate::profiles::fetch_groups(client.call(
                    "usergroups.list",
                    &[
                        ("team_id", team),
                        ("include_disabled", "true"),
                        ("include_users", "false"),
                    ],
                )?)
            })?;
            Ok(Done::Usergroups(groups, warning))
        },
    )
}

pub fn fetch_file(client: Arc<Client>, id: String, url: String, dest: PathBuf) -> Job {
    spawn(JobKind::File { id }, String::new(), move || {
        let bytes = client.download(&url)?;
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        // Whole file or nothing: a reader must never open a truncated copy.
        let part = dest.with_extension("part");
        std::fs::write(&part, &bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
        Ok(Done::File(dest))
    })
}

pub fn api_mark(client: Arc<Client>, conv: usize, cid: String, id: i64) -> Job {
    spawn(
        JobKind::Mark { conv, id },
        "marking read".to_string(),
        move || {
            client.mark(&cid, &id_to_ts(id))?;
            Ok(Done::Marked)
        },
    )
}

pub fn api_send(
    client: Arc<Client>,
    conv: usize,
    cid: String,
    thread: Option<i64>,
    text: String,
) -> Job {
    spawn(
        JobKind::Send { conv, thread },
        "sending".to_string(),
        move || {
            let ts = thread.map(id_to_ts);
            let data = client.post_message(&cid, &text, ts.as_deref())?;
            Msg::from_api(cid, data)
                .map(|m| Done::Sent(Box::new(m)))
                .ok_or_else(|| "sent, but Slack's answer carried no timestamp".to_string())
        },
    )
}

/// Upload `path` into a conversation, with `comment` as its message.
pub fn api_upload(
    client: Arc<Client>,
    conv: usize,
    cid: String,
    thread: Option<i64>,
    path: std::path::PathBuf,
    comment: String,
    scratch: &Path,
) -> Job {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    // A clipboard capture is this tool's own temporary file; it goes once
    // Slack has the bytes.
    let temporary = path.starts_with(scratch);
    spawn(
        JobKind::Upload { conv, thread },
        format!("uploading {name}"),
        move || {
            let ts = thread.map(id_to_ts);
            client.upload_file(&cid, ts.as_deref(), &path, &comment)?;
            if temporary {
                let _ = std::fs::remove_file(&path);
            }
            Ok(Done::Uploaded(name))
        },
    )
}

/// Withdraw one of the owner's own messages.
pub fn api_delete(client: Arc<Client>, cid: String, id: i64, root: Option<i64>) -> Job {
    let channel = cid.clone();
    spawn(
        JobKind::Delete { id, cid, root },
        "deleting".to_string(),
        move || {
            client.delete_message(&channel, &id_to_ts(id))?;
            Ok(Done::Deleted)
        },
    )
}

pub fn api_leave(client: Arc<Client>, conv: usize, cid: String) -> Job {
    spawn(JobKind::Leave { conv }, "leaving".to_string(), move || {
        client.leave(&cid)?;
        Ok(Done::Left)
    })
}

pub fn api_muted_channels(client: Arc<Client>, gen: u64) -> Job {
    spawn(JobKind::MutedChannels { gen }, String::new(), move || {
        Ok(Done::MutedChannels(client.muted_channels()?))
    })
}

pub fn api_set_muted(client: Arc<Client>, cid: String, muted: bool) -> Job {
    let label = if muted {
        "muting in Slack"
    } else {
        "unmuting in Slack"
    };
    spawn(JobKind::SetMuted, label.into(), move || {
        let ids = client.set_muted(&cid, muted)?;
        Ok(Done::MuteChanged { cid, muted, ids })
    })
}

pub fn api_counts(client: Arc<Client>, gen: u64) -> Job {
    spawn(JobKind::Counts { gen }, String::new(), move || {
        Ok(Done::Counts(client.counts()?))
    })
}

pub fn api_unread_counts(client: Arc<Client>, gen: u64, snapshot: Value, targets: Vec<String>) -> Job {
    spawn(JobKind::Counts { gen }, String::new(), move || {
        Ok(Done::Counts(client.enrich_unread_counts(snapshot, &targets)?))
    })
}

// -------------------------------------------------------------- slackdump

pub fn slackdump_bin() -> String {
    std::env::var("SLACKDUMP")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "slackdump".to_string())
}

/// Whether a `slackdump` binary answers at all.
pub fn slackdump_available() -> bool {
    Command::new(slackdump_bin())
        .arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The last informative stderr line: slackdump's progress spinner and its
/// running-as-root warning are noise.
fn last_error_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let stripped = strip_ansi(&text);
    stripped
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            !l.contains("courageously")
                && !l.contains("hope you know")
                && !l.contains("what you're doing")
        })
        .filter(|l| {
            l.contains("ERR")
                || l.contains("error")
                || l.contains("Error")
                || l.starts_with("slackdump")
        })
        .next_back()
        .unwrap_or("slackdump failed")
        .chars()
        .take(200)
        .collect()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn run(mut cmd: Command) -> Result<(), String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", cmd.get_program().to_string_lossy()))?;
    if out.status.success() {
        return Ok(());
    }
    if out.status.code() == Some(75) {
        return Err(
            "the hourly refresh holds the slackdump lock; try again in a minute".to_string(),
        );
    }
    Err(last_error_line(&out.stderr))
}

/// ISO-8601 duration for `-lookback`, in whole hours, 2 h to 14 d.
pub fn lookback_hours(since_secs: i64, now_secs: i64) -> String {
    let hours = (now_secs - since_secs).max(0) / 3600 + 1;
    format!("PT{}H", hours.clamp(2, 14 * 24))
}

pub fn refresh(
    conv: usize,
    before: i64,
    archive_dir: PathBuf,
    lock: PathBuf,
    lookback: String,
) -> Job {
    let label = format!("refreshing through slackdump, {lookback} back");
    spawn(JobKind::Refresh { conv, before }, label, move || {
        let mut cmd = Command::new("flock");
        cmd.args(["--nonblock", "--conflict-exit-code", "75"])
            .arg(&lock)
            .arg(slackdump_bin())
            .args([
                "resume",
                "-channel-users",
                "-files=false",
                "-lookback",
                &lookback,
            ])
            .arg(&archive_dir);
        run(cmd)?;
        Ok(Done::Refreshed)
    })
}

pub fn fetch_thread(cache: &Path, workspace_url: &str, cid: String, root: i64, focus: i64) -> Job {
    let dir = thread_dir(cache, &cid, root);
    let url = format!(
        "{}/archives/{cid}/p{root}",
        workspace_url.trim_end_matches('/')
    );
    let label = "fetching the thread through slackdump".to_string();
    spawn(JobKind::Thread { cid, root, focus }, label, move || {
        let tmp = dir.with_extension("part");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.parent().unwrap_or(Path::new(".")))
            .map_err(|e| e.to_string())?;
        let mut cmd = Command::new(slackdump_bin());
        cmd.args(["archive", "-channel-users", "-files=false", "-o"])
            .arg(&tmp)
            .arg(&url);
        run(cmd)?;
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&tmp, &dir).map_err(|e| e.to_string())?;
        Ok(Done::Thread(dir))
    })
}

pub fn search(cache: &Path, query: String) -> Job {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = cache.join("search").join(stamp.to_string());
    let label = format!("searching through slackdump for '{query}'");
    let q = query.clone();
    spawn(JobKind::Search { query }, label, move || {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.parent().unwrap_or(Path::new(".")))
            .map_err(|e| e.to_string())?;
        let mut cmd = Command::new(slackdump_bin());
        cmd.args(["search", "messages", "-no-channel-users", "-o"])
            .arg(&dir)
            .arg(&q);
        run(cmd)?;
        Ok(Done::Search(dir))
    })
}

/// Archive a conversation (URL or id) into a hidden directory under
/// `<root>/full/`; the caller names it once the channel is known.
pub fn archive_new(root: &Path, spec: String, days: i64) -> Job {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = root.join("full").join(format!(".new-{stamp}"));
    let from = chrono::Utc::now() - chrono::Duration::days(days);
    let from = from.format("%Y-%m-%dT%H:%M:%S").to_string();
    let label = format!("archiving {spec}, last {days} days");
    let s = spec.clone();
    spawn(JobKind::ArchiveNew { spec }, label, move || {
        let _ = std::fs::remove_dir_all(&dir);
        let mut cmd = Command::new(slackdump_bin());
        cmd.args([
            "archive",
            "-channel-users",
            "-files=false",
            "-time-from",
            &from,
            "-o",
        ])
        .arg(&dir)
        .arg(&s);
        run(cmd)?;
        Ok(Done::Archived(dir))
    })
}

pub const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookback_is_whole_hours_within_bounds() {
        assert_eq!(lookback_hours(1000, 1000), "PT2H");
        assert_eq!(lookback_hours(0, 5 * 3600), "PT6H");
        assert_eq!(lookback_hours(0, 400 * 24 * 3600), "PT336H");
    }

    #[test]
    fn error_line_skips_noise_and_ansi() {
        let err = b"\x1b[33mWARN\x1b[0m slackdump: courageously running as root, hope you know\n  o (0/-) [0s]\n\x1b[31mERROR\x1b[0m slackdump: not found: C123\n";
        assert_eq!(last_error_line(err), "ERROR slackdump: not found: C123");
    }

    #[test]
    fn ids_round_trip_to_timestamps() {
        assert_eq!(id_to_ts(1782927057158059), "1782927057.158059");
        assert_eq!(id_to_ts(1782927057000009), "1782927057.000009");
    }
}

#[cfg(test)]
pub fn completed_job(kind: JobKind, result: Result<Done, String>) -> Job {
    let (tx, rx) = mpsc::channel();
    tx.send(result).unwrap();
    Job {
        navigate_on_completion: true,
        kind,
        label: String::new(),
        started: Instant::now(),
        rx,
    }
}

#[cfg(test)]
pub fn pending_job(kind: JobKind) -> (Job, mpsc::Sender<Result<Done, String>>) {
    let (sender, rx) = mpsc::channel();
    (Job { navigate_on_completion: true, kind, label: String::new(), started: Instant::now(), rx }, sender)
}

pub fn api_starred_channels(client: Arc<Client>, gen: u64) -> Job {
    spawn(JobKind::StarredChannels { gen }, String::new(), move || {
        Ok(Done::StarredChannels(client.starred_channels()?))
    })
}
pub fn api_set_starred(client: Arc<Client>, cid: String, starred: bool) -> Job {
    spawn(JobKind::SetStarred, "Updating starred conversation".into(), move || {
        let ids = client.set_starred(&cid, starred)?;
        Ok(Done::StarChanged { cid, starred, ids })
    })
}
