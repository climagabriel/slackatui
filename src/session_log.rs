//! Owner-only, append-only diagnostics for one interactive session.
use crate::app::{App, Focus, Mode, View};
use ratatui::crossterm::event::Event;
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static LOG: OnceLock<Mutex<Writer>> = OnceLock::new();
static JOB_ID: AtomicU64 = AtomicU64::new(1);

struct Writer {
    file: Option<File>,
    started: Instant,
    sequence: u64,
    failure: Option<String>,
    failure_reported: bool,
}

impl Writer {
    fn create(directory: &Path, stamp: &str) -> io::Result<(Self, PathBuf)> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(directory)?.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "session log directory must be owner-only (mode 0700)",
                ));
            }
        }
        for suffix in 0..1000 {
            let name = if suffix == 0 {
                format!("{stamp}.log")
            } else {
                format!("{stamp}_{suffix}.log")
            };
            let path = directory.join(name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    return Ok((
                        Self {
                            file: Some(file),
                            started: Instant::now(),
                            sequence: 0,
                            failure: None,
                            failure_reported: false,
                        },
                        path,
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "session log filename collisions",
        ))
    }

    fn write(&mut self, event: &str, data: Value) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        self.sequence += 1;
        let entry = json!({"time":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),
            "elapsed_ms": self.started.elapsed().as_millis(), "sequence":self.sequence, "event":event, "data":data});
        let mut bytes = serde_json::to_vec(&entry).expect("JSON value serializes");
        bytes.push(b'\n');
        // No userspace buffering: every received key reaches the file before dispatch.
        if let Err(error) = file.write_all(&bytes) {
            self.failure = Some(format!("session logging stopped: {error}"));
            self.file = None;
        }
    }
}

pub fn start() -> io::Result<PathBuf> {
    let directory = std::env::var_os("SLACK_TUI_SESSION_LOG_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .map(|p| p.join("slack-tui/sessions"))
        })
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .map(|p| PathBuf::from(p).join(".cache/slack-tui/sessions"))
        })
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "HOME and XDG_CACHE_HOME are unset")
        })?;
    let stamp = chrono::Utc::now()
        .format("%Y-%m-%d_%H-%M-%S%.6f")
        .to_string();
    let (writer, path) = Writer::create(&directory, &stamp).map_err(|error| {
        io::Error::new(error.kind(), format!("{}: {error}", directory.display()))
    })?;
    LOG.set(Mutex::new(writer)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "session logger already started",
        )
    })?;
    record(
        "session_start",
        json!({"version":crate::version(), "pid":std::process::id(), "format":1}),
    );
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Panic payloads can contain response bodies or credentials; record location only.
        if let Some(log) = LOG.get() {
            if let Ok(mut writer) = log.try_lock() {
                writer.write("panic", json!({"location":info.location().map(|l| format!("{}:{}:{}",l.file(),l.line(),l.column()))}));
            }
        }
        previous(info);
    }));
    Ok(path)
}

pub fn record(event: &str, data: Value) {
    if let Some(log) = LOG.get() {
        if let Ok(mut writer) = log.lock() {
            writer.write(event, data);
        }
    }
}

pub fn take_failure() -> Option<String> {
    let mut writer = LOG.get()?.lock().ok()?;
    if writer.failure_reported {
        return None;
    }
    writer.failure_reported = writer.failure.is_some();
    writer.failure.clone()
}

pub fn finish(code: i32) {
    if let Some(log) = LOG.get() {
        if let Ok(mut writer) = log.lock() {
            writer.write("session_end", json!({"exit_code":code}));
            writer.file = None;
            if let Some(failure) = &writer.failure {
                eprintln!("slack-tui: {failure}");
            }
        }
    }
}

pub fn input(event: &Event, phase: &str) {
    record("input", input_data(event, phase));
}
fn input_data(event: &Event, phase: &str) -> Value {
    match event {
        Event::Key(key) => json!({"phase":phase,"type":"key","code":format!("{:?}",key.code),
            "modifiers":format!("{:?}",key.modifiers),"kind":format!("{:?}",key.kind),"state":format!("{:?}",key.state)}),
        Event::Paste(text) => {
            json!({"phase":phase,"type":"paste","bytes":text.len(),"characters":text.chars().count()})
        }
        other => json!({"phase":phase,"type":"terminal","value":format!("{other:?}")}),
    }
}

// Deliberately allowlisted: arbitrary worker errors may contain response bodies,
// request URLs, or subprocess output. Never put those in a session log.
pub fn error_code(error: &str) -> &'static str {
    let candidate = match error.split_once(": ") {
        Some((method, code))
            if method.contains('.')
                && method
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_') =>
        {
            code
        }
        _ => error,
    };
    for code in [
        "invalid_auth",
        "not_authed",
        "token_revoked",
        "account_inactive",
        "missing_scope",
        "ratelimited",
        "message_not_found",
        "channel_not_found",
        "not_in_channel",
        "cant_delete_message",
        "permission_denied",
        "restricted_action",
        "timeout",
        "timed out",
        "rate limited",
        "worker thread died",
    ] {
        if candidate == code {
            return code;
        }
    }
    "failed"
}

pub struct JobLog {
    id: u64,
    started: Instant,
    finished: bool,
}
impl JobLog {
    pub fn start(kind: &str) -> Self {
        let id = JOB_ID.fetch_add(1, Ordering::Relaxed);
        record("job_start", json!({"id":id,"kind":kind}));
        Self {
            id,
            started: Instant::now(),
            finished: false,
        }
    }
    pub fn complete(mut self, error: Option<&str>) -> u64 {
        record(
            "job_end",
            json!({"id":self.id,"duration_ms":self.started.elapsed().as_millis(),
            "ok":error.is_none(),"error":error.map(error_code)}),
        );
        self.finished = true;
        self.id
    }
}
impl Drop for JobLog {
    fn drop(&mut self) {
        if !self.finished {
            record(
                "job_aborted",
                json!({"id":self.id,"duration_ms":self.started.elapsed().as_millis()}),
            );
        }
    }
}

pub fn state(app: &App) -> Value {
    let list = app.active_list();
    let views: Vec<_> = app
        .stack
        .iter()
        .map(|v| match v {
            View::Thread { root, .. } => json!({"view":"thread","root":root}),
            View::Search { .. } => json!({"view":"search"}),
            View::Threads { .. } => json!({"view":"threads"}),
            View::Feed { section, .. } => json!({"view":"feed","section":section.label()}),
            View::Saved { .. } => json!({"view":"saved"}),
            View::Raw { browser, .. } => json!({"view":"raw","scroll":browser.scroll,"cursor":browser.cursor}),
            View::Reactions { scroll, .. } => json!({"view":"reactions","scroll":scroll}),
            View::ColorPalette { cursor, .. } => json!({"view":"palette","cursor":cursor}),
            View::Keys { cursor, .. } => json!({"view":"keys","cursor":cursor}),
            View::Image { index, zoom, .. } => json!({"view":"image","index":index,"zoom":zoom}),
        })
        .collect();
    json!({"focus":match app.focus {Focus::Convs=>"conversations",Focus::Msgs=>"messages"},
        "mode":match &app.mode {Mode::Normal=>"normal".into(),Mode::Prompt {kind,..}=>format!("{kind:?}")},
        "views":views,"conversation_cursor":app.conv_cursor,"conversation_offset":app.conv_offset,
        "visible_conversations":app.filtered.len(),"filter_characters":app.filter.chars().count(),
        "top_section":app.top_section.map(crate::app::TopSection::label),
        "highlighted_channel":if app.top_section.is_some() { None } else { app.filtered.get(app.conv_cursor).map(|i| &app.corpus.convs[*i].id) },
        "open_channel":app.open.as_ref().map(|o| &app.corpus.convs[o.conv].id),
        "message":list.and_then(|l| l.selected()).map(|m|m.id),"message_channel":list.and_then(|l| l.selected()).map(|m|&m.channel_id),
        "message_cursor":list.map(|l|l.cursor),"message_scroll":list.map(|l|l.scroll),"line_scroll":list.map(|l|l.line_scroll),
        "loaded_messages":list.map(|l|l.msgs.len()),"help":app.help,"pending_delete":app.pending_delete.is_some(),
        "conversation_menu":app.pane_menu.as_ref().map(|menu|menu.cursor),
        "browser":app.channel_browser.as_ref().filter(|b| b.visible).map(|b|b.log_state()),"quit":app.quit})
}

pub fn observe(app: &App, previous: &mut Value, reason: &str) {
    let current = state(app);
    if current != *previous {
        record("state", json!({"reason":reason,"state":current}));
        *previous = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    #[test]
    fn unique_private_logs_write_immediately_and_escape_input() {
        let directory = std::env::temp_dir().join(format!(
            "slack-session-log-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let (mut first, first_path) = Writer::create(&directory, "2026-09-08_12-00-00").unwrap();
        let (mut second, second_path) = Writer::create(&directory, "2026-09-08_12-00-00").unwrap();
        assert_ne!(first_path, second_path);
        for (index, kind) in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ]
        .into_iter()
        .enumerate()
        {
            let mut key = KeyEvent::new(
                KeyCode::Char('\n'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            );
            key.kind = kind;
            first.write("input", input_data(&Event::Key(key), "event_loop"));
            let contents = fs::read_to_string(&first_path).unwrap();
            assert_eq!(contents.lines().count(), index + 1);
            let row: Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
            assert_eq!(row["sequence"], index + 1);
            assert_eq!(row["data"]["kind"], format!("{kind:?}"));
            assert!(row["data"]["modifiers"].as_str().unwrap().contains("SHIFT"));
            assert!(chrono::DateTime::parse_from_rfc3339(row["time"].as_str().unwrap()).is_ok());
        }
        second.write(
            "input",
            input_data(
                &Event::Paste("paste payload must stay out\n界".into()),
                "event_loop",
            ),
        );
        let text = fs::read_to_string(&second_path).unwrap();
        assert!(!text.contains("payload") && !text.contains('界'));
        let entry: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(entry["data"]["type"], "paste");
        assert!(
            entry["data"]["bytes"].as_u64().unwrap()
                > entry["data"]["characters"].as_u64().unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&first_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(first);
        drop(second);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_records_keep_complete_lines_and_sequence_order() {
        let directory = std::env::temp_dir().join(format!(
            "slack-session-concurrency-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let (writer, path) = Writer::create(&directory, "concurrent").unwrap();
        let writer = std::sync::Arc::new(Mutex::new(writer));
        let handles: Vec<_> = (0..4)
            .map(|worker| {
                let writer = writer.clone();
                std::thread::spawn(move || {
                    for item in 0..50 {
                        writer
                            .lock()
                            .unwrap()
                            .write("test", json!({"worker":worker,"item":item}));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let rows: Vec<Value> = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 200);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row["sequence"], index + 1);
        }
        for worker in 0..4 {
            assert_eq!(
                rows.iter()
                    .filter(|row| row["data"]["worker"] == worker)
                    .count(),
                50
            );
        }
        drop(writer);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn permissive_existing_directory_is_rejected_without_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "slack-session-permissions-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        let result = Writer::create(&directory, "private");
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn state_omits_message_editor_status_and_raw_contents() {
        let mut app = crate::app::tests::mute_test_app();
        app.open_conv(0);
        app.status = "private status payload".into();
        app.mode = Mode::Prompt {
            kind: crate::app::PromptKind::Compose,
            buf: crate::edit::Editor::with("private editor payload".into()),
            previous: String::new(),
        };
        app.stack.push(View::Raw {
            title: "private title payload".into(),
            browser: {
                let mut browser = crate::raw::Browser::new(&json!({"text":"private raw payload"}));
                browser.scroll = 3;
                browser
            },
        });
        let snapshot = state(&app);
        let serialized = snapshot.to_string();
        assert!(!serialized.contains("private"));
        assert_eq!(snapshot["views"][0]["scroll"], 3);
        assert_eq!(snapshot["mode"], "Compose");
        assert_eq!(
            error_code("chat.delete: message_not_found"),
            "message_not_found"
        );
        assert_eq!(error_code("arbitrary private response"), "failed");
        assert_eq!(
            error_code("files.list: response body mentions invalid_auth"),
            "failed"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_failure_disables_logging_and_reports_error() {
        let mut writer = Writer {
            file: Some(OpenOptions::new().write(true).open("/dev/full").unwrap()),
            started: Instant::now(),
            sequence: 0,
            failure: None,
            failure_reported: false,
        };
        writer.write("test", json!({}));
        assert!(writer.file.is_none());
        assert!(writer
            .failure
            .as_ref()
            .unwrap()
            .contains("session logging stopped"));
        writer.write("test", json!({}));
        assert_eq!(writer.sequence, 1);
    }
}
