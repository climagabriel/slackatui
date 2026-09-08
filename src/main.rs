//! slack-tui: read the local slackdump archives in the terminal.

mod api;
mod app;
mod canvas;
mod archive;
mod auth;
mod clip;
mod complete;
mod custom_emoji;
mod conversations_pane;
mod edit;
mod keys;
mod live;
mod palette;
mod word_highlights;
mod profiles;
mod render;
mod storage;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

/// What `/version` shows: the plugin version the launcher exports, and the
/// crate's own when slack-tui was started some other way.
pub fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        std::env::var("SLACK_TUI_VERSION")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
    })
}

use crate::app::{App, Focus};
use crate::archive::Corpus;
use crate::render::Tz;
use ratatui_image::picker::Picker;

const HELP: &str = "\
slack-tui - read the local slackdump archives in the terminal

usage: slack-tui [--root DIR] [--channel NAME] [--local]
       slack-tui --list [--root DIR]
       slack-tui --dump NAME [--limit N] [--width W] [--root DIR] [--local]
       slack-tui --help

Opens every slackdump.sqlite under <root>/full/ and <root>/dms/ read-only
(a resume writing the same database at the same time is fine) and shows
the conversations on the left, the messages of the selected one on the
right. Enter on a message opens its thread; Esc goes back.
l or Right opens the selected message's thread from the conversation.
On a message without a thread, or inside a thread, it enables line-by-line
reading: j/k or arrows scroll one line, PageUp/PageDown scroll a page,
Home/End reach its start/end. Press l or Right again for raw JSON.
h/Left or Esc returns one level along the path you entered, ending at
the conversation list. JSON colors follow /colorpalette.

Conversations are ordered by your own activity, weighted by recency: each
message you wrote counts 2^(-age / half-life), so where you wrote last
week outranks where you wrote a lot a year ago (s cycles to name, recent,
size). Your user id comes from the DM archive, or from SLACK_SELF_USER_ID.

flags
  --root DIR      archive root (default $SLACKDUMPS, then /srv/slackdumps)
                  missing or inaccessible storage exits with repair commands;
                  no directory is created and no fallback is selected.
                  Existing empty archive directories are supported.
  --channel NAME  open this conversation at once (#team-alpha, @someone, or the id)
  --local         show times in local time instead of UTC
  --list          print the conversations (with your message count) and exit
  --dump NAME     print the newest messages of one conversation as text and exit
  --limit N       with --dump: how many top-level messages (default 100)
  --width W       with --dump: wrap width (default 100)
  --half-life D   days after which one of your messages counts half in the
                  activity order (default 30)
  --no-live       never go to Slack: cache only
  --poll SECS     re-check the open conversation and unread counts this often
                  when signed in (default 60, 0 = never)
  --auth-check    sign in through the desktop app and print who you are
  --no-images     no inline thumbnails and no image viewer
  --image-protocol MODE
                  query (default): ask the terminal for a native kitty, Sixel
                  or iTerm2 protocol, half-blocks when it has none. halfblocks:
                  no query. Under tmux or screen the query's responses can eat
                  the first key, so there it runs only when asked for.
  --call 'METHOD k=v ...'
                  one allowlisted read-only Web API call, JSON out (debug)
  --dump-canvas ID read a Slack canvas as Markdown and exit
  --delete-message URL
                  delete your own message at that permalink (debug)
  --fetch-file URL OUT
                  download one Slack file with the signed-in session (debug)
  --help          this text

environment
  SLACK_APP_DIR        the Slack desktop app's profile (default: the snap and
                       classic locations under $HOME, then under /home/*)
  SLACK_TOKEN,
  SLACK_COOKIE         a session token and its d cookie, instead of the app
                       (the minted pair is cached in ~/.config/slack-tui/auth.json)
  SLACKDUMP            the slackdump binary (default: slackdump on PATH)
  SLACKDUMP_LOCK       lock file shared with the hourly refresh
                       (default /var/lock/slackdump-sync.lock)
  SLACK_TUI_CACHE      where fetched threads and user profiles live
                       (default $XDG_CACHE_HOME/slack-tui/live, i.e. ~/.cache/...)
  SLACK_TUI_PALETTE    where /colorpalette saves UI colors, the vintage
                       palette included (default
                       $XDG_CONFIG_HOME/slack-tui/palette.json, or ~/.config/...)
  SLACK_TUI_KEYS       where /keys saves the key bindings (default keys.json
                       beside the palette)
  SLACK_TUI_VERSION    the version /version shows (the launcher sets it from
                       the plugin manifest; the crate's own version otherwise)
  SLACKDUMPS           archive root when --root is not given
  SLACK_WORKSPACE      workspace name or HTTPS Slack URL when no archive
                       identifies it; defaults to slackdump's selected workspace
  SLACKDUMP_CACHE      slackdump workspace selection directory (default
                       $XDG_CACHE_HOME/slackdump, or $HOME/.cache/slackdump)
  SLACK_SELF_USER_ID   your own user id: names direct messages by the other
                       party and counts your messages per conversation
                       (derived from the DM archive otherwise)

keys (also ? inside)
  j/k move, Ctrl-d/Ctrl-u half page, g/G oldest/newest, h/l or Tab panes,
  Enter thread, Esc back, / a command (Tab completes: upload, keys,
  colorpalette, version, find, search, leave, mute, unmute, star, unstar, pin, unpin, cache),
  d go to date, v raw JSON,
  o show a hit or a thread root in the channel, r reload, s sort, q quit,
  R refresh from Slack, a archive a conversation not cached yet,
  i view a message's images, I inline thumbnails on/off,
  m mark read, M mark unread from the cursor, D delete your own message,
  T channel tabs and canvases; Ctrl-T threads you participated in,
  H the key guide,
  Esc in the list closes the conversation

Channel tabs: T toggles the menu in browsing and normal mode;
j/k select, l/Enter opens, h/Esc returns. Ctrl-T opens participated threads.
Canvases: j/k moves by line; i edits that section as Markdown. Esc leaves
insert mode; hjkl moves the cursor. In normal mode, :w saves,
:wq saves and closes, :q closes a clean draft, :q! discards it.
:w PATH exports the draft to a new local file for conflict recovery.
Drafts are held in memory; they do not survive application exits.
Saves replace the selected section after checking exported HTML.
Concurrent edits after that check can be overwritten; Slack provides no
atomic revision check. Unsupported sections are read-only text previews.
Files & links lists channel files plus bookmarks; Bookmarks lists bookmarks.

Images: thumbnails under messages and a full-pane viewer, through the
kitty, Sixel or iTerm2 protocol when the terminal has one and half-block
cells otherwise. Files come from the archive's own uploads, then from the
cache, then from Slack when signed in.

After sign-in, user profiles load automatically in a separate background job.
Their workspace-scoped cache is reused for 24 hours, then refreshed on the
next start. Failed refreshes retain stale names; failures without a cache
leave user IDs visible. Only IDs, resolved names and bot flags are stored
in owner-only files under the profile subdirectory of the cache configured
by SLACK_TUI_CACHE.

User groups load independently through usergroups.list (usergroups:read).
Their IDs and handles are cached in usergroups/<team-id>.json under the same
cache root, with the same 24-hour lifetime and stale-on-error behavior.
Rich-text group mentions and <!subteam^ID> resolve through this directory;
explicit mention labels are preserved and unknown groups keep their IDs.

When the cache cannot answer, Slack is asked in the background. Signed in
through the desktop app's session (nothing to copy; SLACK_TOKEN and
SLACK_COOKIE override), the Web API serves threads, search, the newest
messages, every conversation you are a member of, and unread markers; a
thread lands in the cache, the open conversation is re-checked every
--poll seconds. Without a sign-in, slackdump does the same more slowly,
and `a` still archives a new conversation into the root. Writes to Slack:
m and M move your own read marker; c composes a message, Ctrl-j and Alt-Enter
break the line and Enter sends it; D deletes one of your own messages, the
same key again confirming; e toggles your reaction on the selected message; /upload [path] sends a file
with the next message, and Ctrl-v in the compose prompt attaches the image on
the clipboard (through wl-paste or xclip); /leave leaves a channel.
/cache start archives a conversation; /cache stop pauses its hourly refresh;
/cache wipe deletes its archive; /cache highlight on|off colors the cached
conversations in the list; /colorpalette vintage opens the editor over the
vintage palette (terracotta, amber, sand, olive and slate, over whichever
background the terminal already draws), and
/colorpalette default over the terminal's own sixteen colors; /keys rebinds
what the keys do in the two lists, one action per row; /version shows the
version in the status line's right corner, and hides it again.
/mute and /unmute update your preference in Slack and verify it
by reading it back. They require sign-in; failures do not create a local mute.
Old muted.json overrides are ignored. Muted conversations sort last;
Slack preference refreshes follow unread-count polling.
/conversations-pane selects visible categories and individual conversations.
Ctrl-Shift-P opens the same menu and /keys can rebind it. Terminals must report
the Shift modifier separately; otherwise use the command or rebind the action.
Outside Search: j/k move; h/l unset/set (hide/show for individuals,
previous/next for Muted and Number). Space cycles.
Type only on the Search row; Down, Tab or Enter leaves Search.
Enter elsewhere saves; Esc cancels. Reset to default
enables every category and removes individual overrides. Muted cycles through include, hide and only.
Only shows muted conversations across all categories; individual hides apply.
Include and hide respect categories; individual show/hide overrides take precedence.
Preferences persist per workspace in conversations-pane.json beside keys.json.
The Number column row cycles through follow sorting, cached messages, your
cached messages, activity score, mentions and hidden. Explicit choices are
independent of sorting; unavailable archive counts show a dash. Reset restores
follow sorting. /conversation-pane is an alias for /conversations-pane.
In the image viewer, Ctrl-Shift-= / Ctrl-Shift-- zoom in/out; plain + / - also
work, and 0 restores fit. Zoom is centered, from 25% to 800% of the fitted size.
Terminal font shortcuts must be disabled or reassigned in terminal preferences
if they intercept these keys before slack-tui receives them.

exit codes
  0  ok        1  storage unavailable, or the conversation was not found
  2  bad usage

examples
  slack-tui
  slack-tui --channel '#team-alpha'
  slack-tui --dump alerts-alpha --limit 20
  SLACKDUMPS=/data/slackdumps slack-tui --list
";

struct Opts {
    root: PathBuf,
    channel: Option<String>,
    tz: Tz,
    list: bool,
    dump: Option<String>,
    limit: usize,
    width: usize,
    half_life: f64,
    no_live: bool,
    auth_check: bool,
    poll: u64,
    no_images: bool,
    /// None: query unless a multiplexer is in the way.
    image_protocol: Option<bool>,
    fetch_file: Option<(String, String)>,
    delete_message: Option<String>,
    call: Option<String>,
    dump_canvas: Option<String>,
}

/// Methods the debug `--call` escape hatch may invoke. Keep this exact: Slack
/// method-name prefixes mix reads and writes.
const READ_ONLY_CALL_METHODS: &[&str] = &[
    "auth.test",
    "bots.info",
    "bookmarks.list",
    "client.counts",
    "conversations.history",
    "conversations.info",
    "conversations.list",
    "conversations.members",
    "conversations.replies",
    "dnd.info",
    "emoji.list",
    "files.info",
    "files.list",
    "pins.list",
    "reactions.get",
    "reminders.info",
    "reminders.list",
    "search.all",
    "search.files",
    "search.messages",
    "stars.list",
    "team.info",
    "team.preferences.list",
    "usergroups.list",
    "usergroups.users.list",
    "users.conversations",
    "users.info",
    "users.list",
    "users.prefs.get",
    "users.profile.get",
];

fn read_only_call_method(spec: &str) -> Result<&str, String> {
    let method = spec
        .split_whitespace()
        .next()
        .ok_or_else(|| "--call needs a method name".to_string())?;
    if READ_ONLY_CALL_METHODS.contains(&method) {
        Ok(method)
    } else {
        Err(format!(
            "--call refuses '{method}': method is not in the read-only allowlist"
        ))
    }
}

fn parse_args() -> Result<Opts, String> {
    let mut opts = Opts {
        root: std::env::var_os("SLACKDUMPS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/srv/slackdumps")),
        channel: None,
        tz: Tz::Utc,
        list: false,
        dump: None,
        limit: 100,
        width: 100,
        half_life: 30.0,
        no_live: false,
        auth_check: false,
        poll: 60,
        no_images: false,
        image_protocol: None,
        fetch_file: None,
        delete_message: None,
        call: None,
        dump_canvas: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |flag: &str| args.next().ok_or_else(|| format!("{flag} needs a value"));
        match a.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--root" => opts.root = PathBuf::from(value("--root")?),
            "--channel" => opts.channel = Some(value("--channel")?),
            "--local" => opts.tz = Tz::Local,
            "--list" => opts.list = true,
            "--dump" => opts.dump = Some(value("--dump")?),
            "--limit" => {
                opts.limit = value("--limit")?
                    .parse()
                    .map_err(|_| "--limit wants a number".to_string())?
            }
            "--no-live" => opts.no_live = true,
            "--auth-check" => opts.auth_check = true,
            "--no-images" => opts.no_images = true,
            "--image-protocol" => {
                let v = value("--image-protocol")?;
                opts.image_protocol = match v.as_str() {
                    "query" => Some(true),
                    "halfblocks" => Some(false),
                    "auto" => None,
                    other => {
                        return Err(format!(
                            "--image-protocol: {other} is not query, halfblocks or auto"
                        ))
                    }
                };
            }
            "--fetch-file" => {
                let url = value("--fetch-file")?;
                let out = value("--fetch-file")?;
                opts.fetch_file = Some((url, out));
            }
            "--delete-message" => {
                opts.delete_message = Some(value("--delete-message")?);
            }
            "--dump-canvas" => opts.dump_canvas = Some(value("--dump-canvas")?),
            "--call" => {
                let spec = value("--call")?;
                read_only_call_method(&spec)?;
                opts.call = Some(spec);
            }
            "--poll" => {
                opts.poll = value("--poll")?
                    .parse()
                    .map_err(|_| "--poll wants seconds".to_string())?
            }
            "--half-life" => {
                opts.half_life = value("--half-life")?
                    .parse()
                    .ok()
                    .filter(|d: &f64| *d > 0.0)
                    .ok_or_else(|| "--half-life wants a positive number of days".to_string())?
            }
            "--width" => {
                opts.width = value("--width")?
                    .parse()
                    .map_err(|_| "--width wants a number".to_string())?
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(opts)
}

fn main() {
    std::process::exit(run());
}

/// Timing breadcrumbs appended to `$SLACK_TUI_TRACE` when it is set.
pub fn trace(what: &str) {
    use std::io::Write;
    if let Some(path) = std::env::var_os("SLACK_TUI_TRACE") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let _ = writeln!(f, "{now:.3} {what}");
        }
    }
}

/// Print, and treat a closed pipe (`| head`) as a normal end.
fn emit(text: &str) -> i32 {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|_| out.flush()) {
        Ok(()) => 0,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => 0,
        Err(e) => {
            eprintln!("slack-tui: {e}");
            1
        }
    }
}

fn run() -> i32 {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("slack-tui: {e}\n\n{HELP}");
            return 2;
        }
    };
    // Own variable first: XDG_CACHE_HOME also moves slackdump's credential
    // store, so it cannot serve as a test knob.
    let cache_dir = std::env::var_os("SLACK_TUI_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
                .unwrap_or_else(|| PathBuf::from("."))
                .join("slack-tui")
                .join("live")
        });
    let root = match storage::resolve_root(&opts.root) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("slack-tui: {error}");
            return 1;
        }
    };
    let corpus = match Corpus::open(&root, opts.half_life, Some(cache_dir.join("stats.json"))) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("slack-tui: {e}");
            return 1;
        }
    };
    if opts.auth_check
        || opts.fetch_file.is_some()
        || opts.delete_message.is_some()
        || opts.call.is_some()
        || opts.dump_canvas.is_some()
    {
        let scratch = std::env::temp_dir().join("slack-tui");
        let agent = api::agent();
        let auth = match auth::from_env() {
            Some(a) => a,
            None => match auth::from_desktop(&agent, &corpus.workspace_url, &scratch) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("slack-tui: {e}");
                    return 1;
                }
            },
        };
        eprintln!("credentials: {}", auth.source);
        if let Some(id) = &opts.dump_canvas {
            let client = api::Client::new(auth);
            return match canvas::load_document(&client, id) {
                Ok(document) => emit(&(document.sections.iter().map(|s|s.markdown.as_str()).collect::<Vec<_>>().join("\n\n") + "\n")),
                Err(error) => { eprintln!("slack-tui: {error}"); 1 },
            };
        }
        if let Some(spec) = &opts.call {
            let mut words = spec.split_whitespace();
            let method = words
                .next()
                .expect("parse_args validated the --call method");
            let params: Vec<(String, String)> = words
                .filter_map(|w| {
                    w.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                })
                .collect();
            let refs: Vec<(&str, &str)> = params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let client = api::Client::new(auth);
            return match client.call(method, &refs) {
                Ok(v) => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    0
                }
                Err(e) => {
                    eprintln!("slack-tui: {e}");
                    1
                }
            };
        }
        if let Some(link) = &opts.delete_message {
            let Some((cid, ts)) = api::parse_permalink(link) else {
                eprintln!("slack-tui: not a message permalink: {link}");
                return 2;
            };
            let client = api::Client::new(auth);
            return match client.delete_message(&cid, &ts) {
                Ok(()) => {
                    println!("deleted {cid} {ts}");
                    0
                }
                Err(e) => {
                    eprintln!("slack-tui: {e}");
                    1
                }
            };
        }
        if let Some((url, out)) = &opts.fetch_file {
            let client = api::Client::new(auth);
            return match client.download(url) {
                Ok(bytes) => {
                    let kind = image::guess_format(&bytes)
                        .map(|f| format!("{f:?}"))
                        .unwrap_or_else(|_| "not an image".to_string());
                    match std::fs::write(out, &bytes) {
                        Ok(()) => {
                            println!("fetched {} bytes ({kind}) into {out}", bytes.len());
                            0
                        }
                        Err(e) => {
                            eprintln!("slack-tui: {e}");
                            1
                        }
                    }
                }
                Err(e) => {
                    eprintln!("slack-tui: {e}");
                    1
                }
            };
        }
        let client = api::Client::new(auth);
        match client.auth_test() {
            Ok((uid, user, team)) => println!("auth.test: ok, user {user} ({uid}), team {team}"),
            Err(e) => {
                eprintln!("slack-tui: {e}");
                return 1;
            }
        }
        match client.counts() {
            Ok(v) => {
                let unread = ["channels", "ims", "mpims"]
                    .iter()
                    .flat_map(|k| v.get(*k).and_then(|a| a.as_array()).into_iter().flatten())
                    .filter(|c| {
                        c.get("has_unreads")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false)
                    })
                    .count();
                println!("client.counts: ok, {unread} conversations with unreads");
                if let Some(c) = v
                    .get("channels")
                    .and_then(|a| a.as_array())
                    .and_then(|a| a.first())
                {
                    let keys: Vec<&str> = c
                        .as_object()
                        .map(|o| o.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    println!(
                        "client.counts: channel fields {keys:?}; last_read={:?}",
                        c.get("last_read")
                    );
                }
            }
            Err(e) => println!("client.counts: {e} (unread markers will be off)"),
        }
        return 0;
    }
    let live_enabled = !opts.no_live;
    let slackdump_ok = live_enabled && live::slackdump_available();
    let lock = std::env::var_os("SLACKDUMP_LOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lock/slackdump-sync.lock"));
    let palette_path = std::env::var_os("SLACK_TUI_PALETTE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .filter(|value| !value.is_empty())
                        .map(|home| PathBuf::from(home).join(".config"))
                })
                .map(|dir| dir.join("slack-tui").join("palette.json"))
        });
    let keys_path = std::env::var_os("SLACK_TUI_KEYS")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            palette_path
                .as_ref()
                .and_then(|path| path.parent())
                .map(|dir| dir.join("keys.json"))
        });
    let mut app = App::new(
        corpus,
        opts.tz,
        opts.half_life,
        live_enabled,
        slackdump_ok,
        cache_dir,
        lock,
        opts.poll,
        palette_path,
        keys_path,
    );
    if opts.list {
        let mut text = String::new();
        text.push_str(&format!(
            "{:<48} {:>7} {:>6} {:>8}  {:<10} → {:<10}  {}\n",
            "CONVERSATION", "SCORE", "MINE", "MSGS", "FIRST", "LAST", "ARCHIVE"
        ));
        for &i in &app.filtered {
            let c = app.conv(i);
            let a = &app.corpus.archives[c.archive];
            text.push_str(&format!(
                "{:<48} {:>7.1} {:>6} {:>8}  {} → {}  {}\n",
                c.name,
                c.score,
                c.mine,
                c.msgs,
                app.tz.fmt(c.first_id / 1_000_000, "%Y-%m-%d"),
                app.tz.fmt(c.last_id / 1_000_000, "%Y-%m-%d"),
                a.rel
            ));
        }
        return emit(&text);
    }
    if let Some(name) = opts.dump.as_deref().or(opts.channel.as_deref()) {
        let Some(idx) = app.corpus.find_conv(name) else {
            eprintln!("slack-tui: no conversation named '{name}'; --list shows them");
            return 1;
        };
        if !app.open_conv(idx) {
            eprintln!("slack-tui: {}", app.status);
            return 1;
        }
        if opts.dump.is_some() {
            if let Some(o) = app.open.as_mut() {
                let keep = opts.limit.min(o.list.len());
                let drop = o.list.len() - keep;
                o.list.msgs.drain(0..drop);
                o.list.cursor = keep.saturating_sub(1);
                o.list.top_note = None;
                o.list.bottom_note = None;
                o.list.mark_dirty();
            }
            let text = ui::dump(&mut app, opts.width) + "\n";
            return emit(&text);
        }
    }
    match tui(&mut app, opts.no_images, opts.image_protocol) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("slack-tui: {e}");
            1
        }
    }
}

/// tmux and screen answer the capability query themselves; the responses
/// reached the key reader there and swallowed the first keypress.
fn under_multiplexer() -> bool {
    std::env::var_os("TMUX").is_some()
        || std::env::var_os("STY").is_some()
        || std::env::var("TERM")
            .map(|t| t.starts_with("screen") || t.starts_with("tmux"))
            .unwrap_or(false)
}

fn tui(app: &mut App, no_images: bool, image_protocol: Option<bool>) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::SetTitle("slack-tui")
    );
    struct KeyboardGuard;
    impl Drop for KeyboardGuard {
        fn drop(&mut self) {
            let _ =
                ratatui::crossterm::execute!(std::io::stdout(), event::PopKeyboardEnhancementFlags);
        }
    }
    // Unsupported terminals ignore this; supporting terminals distinguish Ctrl-Shift-P.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        event::PushKeyboardEnhancementFlags(
            event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    );
    let keyboard_guard = KeyboardGuard;
    if !no_images {
        // The capability query asks which of kitty/Sixel/iTerm2 the terminal
        // speaks and falls back to half-blocks; its responses come back as
        // input, so the loop drains them before reading a real key. Under a
        // multiplexer the drain was not enough, so there the query runs only
        // on request.
        let query = image_protocol.unwrap_or_else(|| !under_multiplexer());
        app.picker = Some(if query {
            let p = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
            let cap = std::time::Instant::now() + Duration::from_millis(1000);
            while event::poll(Duration::from_millis(100)).unwrap_or(false) {
                let _ = event::read();
                if std::time::Instant::now() >= cap {
                    break;
                }
            }
            p
        } else {
            Picker::halfblocks()
        });
        app.inline_images = true;
    }
    if app.open.is_none() {
        app.focus = Focus::Convs;
    }
    let result = loop {
        let t0 = std::time::Instant::now();
        if let Err(e) = terminal.draw(|frame| ui::draw(frame, app)) {
            break Err(e);
        }
        if t0.elapsed().as_millis() > 50 {
            trace(&format!("draw {} ms", t0.elapsed().as_millis()));
        }
        match event::poll(Duration::from_millis(250)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    let t = std::time::Instant::now();
                    app.on_key(k);
                    trace(&format!("key {:?} {} ms", k.code, t.elapsed().as_millis()));
                }
                Ok(_) => {}
                Err(e) => break Err(e),
            },
            Ok(false) => {}
            Err(e) => break Err(e),
        }
        let t = std::time::Instant::now();
        app.tick();
        if t.elapsed().as_millis() > 50 {
            trace(&format!("tick {} ms", t.elapsed().as_millis()));
        }
        if app.quit {
            break Ok(());
        }
    };
    drop(keyboard_guard);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::{read_only_call_method, READ_ONLY_CALL_METHODS};

    #[test]
    fn call_allowlist_accepts_established_inspection_methods() {
        for method in READ_ONLY_CALL_METHODS {
            assert_eq!(read_only_call_method(method), Ok(*method));
        }
        assert_eq!(
            read_only_call_method("conversations.info channel=C123"),
            Ok("conversations.info")
        );
    }

    #[test]
    fn call_allowlist_rejects_writes_and_non_methods() {
        for method in [
            "apps.connections.open",
            "chat.delete",
            "chat.postMessage",
            "conversations.join",
            "conversations.leave",
            "conversations.mark",
            "files.completeUploadExternal",
            "files.getUploadURLExternal",
            "reactions.add",
            "reactions.remove",
            "users.prefs.set",
            "users.prefs.setNotifications",
        ] {
            assert!(
                read_only_call_method(method).is_err(),
                "write method {method} passed the read-only allowlist"
            );
        }
        assert!(read_only_call_method("").is_err());
        assert!(read_only_call_method("conversations.info/../chat.postMessage").is_err());
    }
}
