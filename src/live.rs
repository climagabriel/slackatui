//! Going to Slack when the cache cannot answer. Every fetch is a `slackdump`
//! run in a background thread: it owns the credentials and the write path,
//! so this file never touches a token or an archive database. Threads land
//! in an on-disk cache under the tool's cache directory; search results are
//! read once and discarded; a refresh resumes the archive in place under
//! the same lock the hourly job takes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

pub enum JobKind {
    /// Resume one archive directory; `conv` is the conversation to reload.
    Refresh { conv: usize, before: i64 },
    /// Fetch one thread into the thread cache.
    Thread { cid: String, root: i64, focus: i64 },
    /// Workspace-wide message search.
    Search { query: String },
    /// Archive a conversation not in the cache yet.
    ArchiveNew { spec: String },
}

pub enum Done {
    Refreshed,
    Thread(PathBuf),
    Search(PathBuf),
    Archived(PathBuf),
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

pub fn slackdump_bin() -> String {
    std::env::var("SLACKDUMP")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "slackdump".to_string())
}

/// Whether a `slackdump` binary answers at all; the live features hide
/// themselves otherwise.
pub fn available() -> bool {
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
    let label = format!("refreshing from Slack, {lookback} back");
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

/// Where a fetched thread lives: `<cache>/threads/<channel>-<root id>/`.
pub fn thread_dir(cache: &Path, cid: &str, root: i64) -> PathBuf {
    cache.join("threads").join(format!("{cid}-{root}"))
}

pub fn fetch_thread(cache: &Path, workspace_url: &str, cid: String, root: i64, focus: i64) -> Job {
    let dir = thread_dir(cache, &cid, root);
    let url = format!(
        "{}/archives/{cid}/p{root}",
        workspace_url.trim_end_matches('/')
    );
    let label = "fetching the thread from Slack".to_string();
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
    let label = format!("searching Slack for '{query}'");
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
}
