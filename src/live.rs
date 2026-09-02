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
    /// Resume one archive directory; `conv` is the conversation to reload.
    Refresh { conv: usize, before: i64 },
    /// Fetch one thread.
    Thread { cid: String, root: i64, focus: i64 },
    /// Workspace-wide message search.
    Search { query: String },
    /// Archive a conversation not in the cache yet.
    ArchiveNew { spec: String },
    /// Sign in through the environment, the cached token, or the desktop app.
    Auth,
    /// Messages newer than what is loaded for `conv`.
    Tail { conv: usize },
    /// Messages older than what is loaded for `conv` (API-only conversations).
    Older { conv: usize },
    /// The conversations the user is a member of.
    Conversations,
    /// Unread state per conversation.
    Counts,
    /// One file into the file cache.
    File { id: String },
    /// The read marker of `conv` moved to message `id`.
    Mark { conv: usize, id: i64 },
}

pub enum Done {
    Refreshed,
    Thread(PathBuf),
    ThreadMsgs(Vec<Msg>),
    Search(PathBuf),
    SearchHits(Vec<Msg>),
    Archived(PathBuf),
    Auth(Arc<Client>, String),
    Messages(Vec<Msg>),
    Conversations(Vec<Value>),
    Counts(Value),
    File(PathBuf),
    Marked,
}

pub struct Job {
    pub kind: JobKind,
    pub label: String,
    pub started: Instant,
    rx: Receiver<Result<Done, String>>,
}

impl Job {
    /// The outcome, once; None while the job is still running.
    pub fn poll(&self) -> Option<Result<Done, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("worker thread died".to_string())),
        }
    }
}

fn spawn(
    kind: JobKind,
    label: String,
    work: impl FnOnce() -> Result<Done, String> + Send + 'static,
) -> Job {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    Job {
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
    let q = query.clone();
    spawn(
        JobKind::Search { query },
        format!("searching Slack for '{q}'"),
        move || {
            let matches = client.search(&q, 100)?;
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
            Ok(Done::SearchHits(out))
        },
    )
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
            let values = client.history(&cid, latest.as_deref(), None, 200)?;
            let mut msgs = msgs_from_values(&cid, values);
            msgs.sort_by_key(|m| m.id);
            Ok(Done::Messages(msgs))
        },
    )
}

pub fn api_conversations(client: Arc<Client>) -> Job {
    spawn(JobKind::Conversations, String::new(), move || {
        Ok(Done::Conversations(client.my_conversations()?))
    })
}

pub fn fetch_file(client: Arc<Client>, id: String, url: String, dest: PathBuf) -> Job {
    spawn(JobKind::File { id }, String::new(), move || {
        let bytes = client.download(&url)?;
        if let Some(d) = dest.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        std::fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
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

pub fn api_counts(client: Arc<Client>) -> Job {
    spawn(JobKind::Counts, String::new(), move || {
        Ok(Done::Counts(client.counts()?))
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
