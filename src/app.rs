//! Application state and key handling. Views stack on top of the timeline:
//! thread, search hits, raw JSON. Esc pops.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;

use serde_json::Value;

use crate::api::Client;
use crate::archive::{ts_to_id, Archive, Conv, Corpus, Kind, Msg, PAGE, SEARCH_CAP};
use crate::complete;
use crate::edit::Editor;
use crate::keys::{Action, Chord, Keymap, DEFAULTS};
use crate::live::{self, Done, Job, JobKind};
use crate::palette::PRESETS;
use crate::palette::{Palette, Role, ROLES};
use crate::render::{self, Ctx, ImageSlot, Tz};
use image::DynamicImage;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::{Protocol, StatefulProtocol};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Convs,
    Msgs,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    /// Where the archive's owner wrote the most, first.
    Mine,
    Name,
    Recent,
    Size,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::Mine => "my activity",
            Sort::Name => "name",
            Sort::Recent => "recent",
            Sort::Size => "size",
        }
    }
    fn next(self) -> Sort {
        match self {
            Sort::Mine => Sort::Name,
            Sort::Name => Sort::Recent,
            Sort::Recent => Sort::Size,
            Sort::Size => Sort::Mine,
        }
    }
}

pub struct FlatLine {
    pub msg: Option<usize>,
    pub line: Line<'static>,
    /// An image starts on this line and takes the rows below it.
    pub image: Option<ImageSlot>,
}

/// A scrollable list of messages rendered into lines. The cursor is a
/// message; the viewport is lines.
#[derive(Default)]
pub struct MsgList {
    pub msgs: Vec<Msg>,
    pub cursor: usize,
    pub scroll: usize,
    /// Read the selected message by screen line, without moving its cursor.
    pub line_scroll: bool,
    pub flat: Vec<FlatLine>,
    pub first: Vec<usize>,
    pub last: Vec<usize>,
    collapsed: Vec<bool>,
    flat_w: usize,
    flat_date: Option<(Tz, i64)>,
    pane_height: Option<usize>,
    unread_count: Option<i64>,
    dirty: bool,
    pub in_thread: bool,
    pub top_note: Option<String>,
    pub bottom_note: Option<String>,
    /// After the next rebuild, put the cursor's message near the top of the
    /// view so what follows it is on screen (a jump target, a focused reply).
    pub align_top: bool,
}

impl MsgList {
    pub fn new(msgs: Vec<Msg>, in_thread: bool) -> MsgList {
        MsgList {
            msgs,
            dirty: true,
            in_thread,
            ..Default::default()
        }
    }

    pub fn len(&self) -> usize {
        self.msgs.len()
    }

    pub fn selected(&self) -> Option<&Msg> {
        self.msgs.get(self.cursor)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Refresh the divider when a live unread count arrives.
    pub fn set_unread_count(&mut self, count: Option<i64>) {
        if self.unread_count != count {
            self.unread_count = count;
            self.dirty = true;
        }
    }

    /// Render at the current pane size, keeping the selected message visible.
    pub fn rebuild_for_pane(&mut self, ctx: &Ctx, width: usize, height: usize) {
        let height = if self.line_scroll { None } else { Some(height) };
        if self.pane_height != height {
            self.pane_height = height;
            self.dirty = true;
        }
        self.rebuild(ctx, width);
    }

    pub fn rebuild(&mut self, ctx: &Ctx, width: usize) {
        self.rebuild_on_day(ctx, width, ctx.tz.day(chrono::Utc::now().timestamp()));
    }

    fn rebuild_on_day(&mut self, ctx: &Ctx, width: usize, today: i64) {
        if !self.dirty && self.flat_w == width && self.flat_date == Some((ctx.tz, today)) {
            return;
        }
        let inside = self
            .scroll
            .saturating_sub(self.first.get(self.cursor).copied().unwrap_or(0));
        let offset = self
            .first
            .get(self.cursor)
            .map(|f| f.saturating_sub(self.scroll))
            .unwrap_or(0);
        self.flat.clear();
        self.first.clear();
        self.last.clear();
        self.collapsed.clear();
        if let Some(note) = &self.top_note {
            self.flat.push(FlatLine {
                msg: None,
                line: render::divider(note, width),
                image: None,
            });
        }
        let mut prev_day = None;
        let mut new_marked = false;
        let count = match self.unread_count {
            Some(count) if count > 9 => "9+".to_string(),
            Some(count) if count > 0 => count.to_string(),
            _ => "—".to_string(),
        };
        let unread_label = format!("new ({count})");
        for (i, m) in self.msgs.iter().enumerate() {
            let day = ctx.tz.day(m.secs());
            // The first message past the read marker opens the unread part:
            // its day divider lights up, or a "new" line stands in for one.
            let new_here = !new_marked && ctx.last_read.is_some_and(|lr| m.id > lr);
            if prev_day != Some(day) {
                let text = ctx.tz.date_label(m.secs(), today);
                let line = if new_here {
                    render::divider_new(&format!("{text} · {unread_label}"), width, ctx.palette)
                } else {
                    render::divider(&text, width)
                };
                self.flat.push(FlatLine {
                    msg: None,
                    line,
                    image: None,
                });
                prev_day = Some(day);
            } else if new_here {
                self.flat.push(FlatLine {
                    msg: None,
                    line: render::divider_new(&unread_label, width, ctx.palette),
                    image: None,
                });
            }
            if new_here {
                new_marked = true;
            }
            let mut rendered = render::message_lines(m, ctx, width, self.in_thread, today);
            let collapsed = self.pane_height.is_some_and(|height| rendered.lines.len() + 2 > height / 2)
                && rendered.lines.len() > 3;
            self.collapsed.push(collapsed);
            if collapsed {
                let remaining = rendered.lines.len() - 3;
                rendered.lines.truncate(3);
                rendered.lines.push(Line::from(format!("  ({remaining} more lines)")));
                // Never paint a partial image over the preview or its count.
                rendered.images.retain(|slot| slot.line + slot.rows as usize <= 3);
            }
            self.first.push(self.flat.len());
            self.flat.push(FlatLine { msg: Some(i), line: Line::default(), image: None });
            let base = self.flat.len();
            for line in rendered.lines {
                self.flat.push(FlatLine {
                    msg: Some(i),
                    line,
                    image: None,
                });
            }
            for slot in rendered.images {
                if let Some(fl) = self.flat.get_mut(base + slot.line) {
                    fl.image = Some(slot);
                }
            }
            self.flat.push(FlatLine { msg: Some(i), line: Line::default(), image: None });
            self.last.push(self.flat.len().saturating_sub(1));
        }
        if let Some(note) = &self.bottom_note {
            self.flat.push(FlatLine {
                msg: None,
                line: render::divider(note, width),
                image: None,
            });
        }
        self.flat_w = width;
        self.flat_date = Some((ctx.tz, today));
        self.dirty = false;
        let first = self.first.get(self.cursor).copied().unwrap_or(0);
        // A fresh list, or a jump, keeps the line above the cursor on screen:
        // that is the day divider.
        let back = if self.align_top || offset == 0 {
            1
        } else {
            offset
        };
        self.scroll = if self.line_scroll {
            first.saturating_add(inside)
        } else {
            first.saturating_sub(back)
        };
        self.align_top = false;
    }

    /// Select a contiguous viewport containing complete message blocks only.
    pub fn whole_message_viewport(&mut self, height: usize) -> usize {
        if self.first.is_empty() || height == 0 { return 0; }
        self.cursor = self.cursor.min(self.first.len() - 1);
        self.line_scroll = false;
        self.scroll = self.scroll.min(self.first[self.cursor]);
        if let Some(index) = self.flat.get(self.scroll).and_then(|line| line.msg) {
            self.scroll = self.first[index];
        }
        while self.last[self.cursor] >= self.scroll + height {
            let Some(index) = (self.scroll..self.flat.len()).find_map(|row| self.flat[row].msg) else { break };
            if index == self.cursor {
                self.scroll = self.first[index];
                break;
            }
            self.scroll = self.last[index] + 1;
        }
        let mut end = self.scroll;
        while end < self.flat.len() {
            let next = self.flat[end].msg.map_or(end + 1, |index| self.last[index] + 1);
            if next > self.scroll + height { break; }
            end = next;
        }
        // Near the end of history, use spare rows for preceding whole messages.
        while self.scroll > 0 {
            let previous = self.scroll - 1;
            let start = self.flat[previous].msg.map_or(previous, |index| self.first[index]);
            if end - start > height { break; }
            self.scroll = start;
        }
        end
    }

    pub fn ensure_visible(&mut self, height: usize) {
        if height == 0 || self.first.is_empty() {
            return;
        }
        self.cursor = self.cursor.min(self.first.len() - 1);
        let first = self.first[self.cursor];
        let last = self.last[self.cursor];
        if self.line_scroll {
            self.scroll = self
                .scroll
                .clamp(first, (last + 1).saturating_sub(height).max(first));
            return;
        }
        if first < self.scroll {
            self.scroll = first;
        } else if last >= self.scroll + height {
            self.scroll = (last + 1 - height).min(first);
        }
        let max_scroll = self.flat.len().saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);
    }

    pub fn move_cursor(&mut self, delta: isize) {
        self.line_scroll = false;
        if self.msgs.is_empty() {
            return;
        }
        let n = self.msgs.len() as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, n - 1) as usize;
    }

    /// Move the cursor to the message `delta` screen lines away.
    pub fn move_lines(&mut self, delta: isize) {
        if self.flat.is_empty() || self.first.is_empty() {
            return;
        }
        let from = self.first.get(self.cursor).copied().unwrap_or(0) as isize;
        let target = (from + delta).clamp(0, self.flat.len() as isize - 1) as usize;
        let found = if delta >= 0 {
            (target..self.flat.len()).find_map(|i| self.flat[i].msg)
        } else {
            (0..=target).rev().find_map(|i| self.flat[i].msg)
        };
        if let Some(i) = found {
            self.cursor = i;
        } else {
            self.cursor = if delta >= 0 { self.msgs.len() - 1 } else { 0 };
        }
        self.scroll =
            (self.scroll as isize + delta).clamp(0, self.flat.len() as isize - 1) as usize;
    }
}

/// An unsent message and where it was meant to go.
pub struct Draft {
    pub cid: String,
    pub thread: Option<i64>,
    pub text: String,
}

/// A delete asked for and not confirmed yet: the same key again carries it
/// out, anything else drops it.
pub struct PendingDelete {
    pub cid: String,
    pub id: i64,
}

/// Where `c` sends: a conversation, and a thread in it when replying.
pub struct Compose {
    pub conv: usize,
    pub cid: String,
    pub thread: Option<i64>,
    /// What the prompt says it is writing to.
    pub label: String,
}

pub struct Open {
    pub conv: usize,
    pub list: MsgList,
    pub total: i64,
    pub has_older: bool,
    pub has_newer: bool,
    /// Paged from Slack: no archive behind it.
    pub api_only: bool,
}

pub enum View {
    Thread {
        root: i64,
        list: MsgList,
        /// The archive the thread was fetched into, when slackdump fetched it.
        live: Option<Box<Archive>>,
        /// Where the thread lives when that is not the open conversation.
        place: Option<String>,
    },
    Search {
        query: String,
        list: MsgList,
        capped: bool,
        /// Hits Slack added beyond the cache, once it answered.
        live_hits: Option<usize>,
        live_pending: bool,
    },
    Raw { title: String, browser: crate::raw::Browser },
    Reactions { title: String, lines: Vec<String>, scroll: usize },
    /// Roots of the threads the owner wrote in, across every archive.
    Threads { list: MsgList },
    /// `/colorpalette`: edit semantic UI colors with a live preview.
    ColorPalette {
        highlights: Option<crate::word_highlights::Menu>,
        cursor: usize,
        original: Palette,
        return_focus: Focus,
    },
    /// `/keys`: rebind what the lists' keys do.
    Keys {
        cursor: usize,
        original: Keymap,
        return_focus: Focus,
        /// Waiting for the key to bind; true keeps the action's other keys.
        capture: Option<bool>,
    },
    /// One message's images, full pane, one at a time.
    Image {
        files: Vec<crate::archive::FileInfo>,
        index: usize,
        zoom: u16,
        /// The fitted encoding of the current image, built on first draw.
        shown: Option<(String, StatefulProtocol)>,
    },
}

/// What is known about one file's pixels.
pub enum ImageState {
    /// Waiting for the one file download slot.
    Queued {
        url: String,
        dest: PathBuf,
    },
    Loading,
    Ready(DynamicImage),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptKind {
    /// `/`: `find`/`search TEXT` or `leave [#name]`.
    Command,
    Date,
    Archive,
    Compose,
    /// A color for the role under the palette's cursor.
    PaletteColor,
}

pub enum Mode {
    Normal,
    Prompt {
        kind: PromptKind,
        buf: Editor,
        previous: String,
    },
}

pub struct App {
    pub channel_browser: Option<crate::canvas::Browser>,
    pub corpus: Corpus,
    pub tz: Tz,
    pub focus: Focus,
    pub sort: Sort,
    pub filter: String,
    pub filtered: Vec<usize>,
    pub conv_cursor: usize,
    pub open: Option<Open>,
    pub stack: Vec<View>,
    pub mode: Mode,
    pub help: bool,
    pub status: String,
    pub quit: bool,
    /// Inner height of the messages pane at the last draw.
    pub msgs_height: usize,
    /// First conversation row drawn, kept across frames so the cursor moves
    /// inside the pane instead of the pane moving under it.
    pub conv_offset: usize,
    /// Days after which one of the owner's messages counts half, in the
    /// activity order.
    pub half_life_days: f64,
    /// The one slackdump run in flight, if any.
    pub job: Option<Job>,
    pub spinner: usize,
    /// Slack may be consulted when the cache cannot answer.
    pub live: bool,
    /// Fetched threads and search results land here.
    pub cache_dir: PathBuf,
    /// The lock the hourly refresh takes; a refresh from here takes it too.
    pub lock: PathBuf,
    /// The Web API, once the background sign-in succeeded.
    pub api: Option<Arc<Client>>,
    /// Quiet background work: sign-in, conversation list, counts, tails.
    pub bg: Option<Job>,
    profile_job: Option<Job>,
    unread_count_job: Option<Job>,
    counts_pending: bool,
    group_job: Option<Job>,
    dm_users: HashMap<String, String>,
    /// A slackdump binary answers; the fallback engine.
    pub slackdump: bool,
    pub poll_every: Duration,
    pub last_poll: Instant,
    pub last_counts: Instant,
    /// The terminal's image protocol, when images are on at all.
    pub picker: Option<Picker>,
    pub inline_images: bool,
    /// Decoded files by key: the file id for a thumbnail, `id:full` for the original.
    pub images: HashMap<String, ImageState>,
    /// Encoded inline thumbnails, by file id, with the cell size they were made for.
    pub inline: HashMap<String, (u16, u16, Protocol)>,
    pub file_job: Option<Job>,
    /// `/cache highlight on|off`: cached conversations in the palette's
    /// cached color.
    pub highlight_cached: bool,
    /// `/version`: the version in the status line's right corner.
    pub show_version: bool,
    /// `U`: unread conversations at the top of the list.
    pub unreads_first: bool,
    /// The target of the open compose prompt.
    pub compose: Option<Compose>,
    /// The file the next send carries, from /upload or the clipboard.
    pub attachment: Option<PathBuf>,
    /// Why the last attach attempt gave nothing, shown in the prompt: the
    /// status line is where the prompt itself is drawn.
    pub attach_note: Option<String>,
    /// What an upload left to fetch: the conversation, and the thread when
    /// the file went into one.
    tail_pending: Option<(usize, Option<i64>)>,
    /// A delete waiting for its second key press.
    pub pending_delete: Option<PendingDelete>,
    /// The last confirmed server snapshot; old local overrides are no longer read.
    pub muted: HashSet<String>,
    pub starred: HashSet<String>,
    starred_generation: u64,
    starred_pending: bool,
    muted_generation: u64,
    /// Slack's muted set still to fetch.
    pub muted_pending: bool,
    /// A message typed and not sent: Esc keeps it for the next `c` on the
    /// same target, so it cannot go to another conversation by reflex.
    pub draft: Option<Draft>,
    /// Bumped by every mark; a counts result from before it is stale.
    pub counts_gen: u64,
    /// Semantic UI colors, loaded from and saved to `palette_path`.
    pub palette: Palette,
    pub palette_path: Option<PathBuf>,
    /// What the keys do, loaded from and saved to `keys_path`.
    pub keymap: Keymap,
    pub keys_path: Option<PathBuf>,
    pub pane_menu: Option<crate::conversations_pane::Menu>,
    pub pane_settings: crate::conversations_pane::Settings,
    pane_path: Option<PathBuf>,
    pane_workspace: String,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        corpus: Corpus,
        tz: Tz,
        half_life_days: f64,
        live: bool,
        slackdump: bool,
        cache_dir: PathBuf,
        lock: PathBuf,
        poll_secs: u64,
        palette_path: Option<PathBuf>,
        keys_path: Option<PathBuf>,
    ) -> App {
        let (palette, palette_status) = match Palette::load(palette_path.as_deref()) {
            Ok(palette) => (palette, String::new()),
            Err(error) => (
                Palette::default(),
                format!("palette: {error}; using defaults"),
            ),
        };
        let (keymap, keys_status) = match Keymap::load(keys_path.as_deref()) {
            Ok(keymap) => (keymap, palette_status),
            Err(error) => (
                Keymap::default(),
                format!("keys: {error}; using the default keys"),
            ),
        };
        let pane_path = keys_path
            .as_ref()
            .or(palette_path.as_ref())
            .and_then(|p| p.parent())
            .map(|p| p.join("conversations-pane.json"));
        let pane_workspace = corpus.workspace_url.clone();
        let (pane_settings, keys_status) = match crate::conversations_pane::Settings::load(
            pane_path.as_deref(),
            &pane_workspace,
        ) {
            Ok(settings) => (settings, keys_status),
            Err(e) => (
                Default::default(),
                format!("conversations-pane: {e}; showing everything"),
            ),
        };
        let mut app = App {
            channel_browser: None,
            pane_menu: None,
            pane_settings,
            pane_path,
            pane_workspace,
            corpus,
            tz,
            focus: Focus::Convs,
            sort: Sort::Recent,
            filter: String::new(),
            filtered: Vec::new(),
            conv_cursor: 0,
            open: None,
            stack: Vec::new(),
            mode: Mode::Normal,
            help: false,
            status: keys_status,
            quit: false,
            msgs_height: 0,
            half_life_days,
            job: None,
            spinner: 0,
            live,
            cache_dir,
            lock,
            api: None,
            bg: None,
            profile_job: None,
            unread_count_job: None,
            counts_pending: false,
            group_job: None,
            dm_users: HashMap::new(),
            slackdump,
            poll_every: Duration::from_secs(poll_secs),
            last_poll: Instant::now(),
            last_counts: Instant::now(),
            picker: None,
            inline_images: false,
            images: HashMap::new(),
            inline: HashMap::new(),
            file_job: None,
            conv_offset: 0,
            highlight_cached: false,
            show_version: false,
            unreads_first: true,
            compose: None,
            attachment: None,
            attach_note: None,
            tail_pending: None,
            pending_delete: None,
            muted: HashSet::new(),
            starred: HashSet::new(),
            starred_generation: 0,
            starred_pending: false,
            muted_generation: 0,
            muted_pending: false,
            draft: None,
            counts_gen: 0,
            palette,
            palette_path,
            keymap,
            keys_path,
        };
        app.apply_filter();
        if live {
            app.bg = Some(live::sign_in(
                app.corpus.workspace_url.clone(),
                app.cache_dir.join("tmp"),
            ));
        }
        app
    }

    /// The sort as the title names it.
    /// `U`: unread conversations on top, or the plain sort order.
    fn toggle_unreads_first(&mut self) {
        self.unreads_first = !self.unreads_first;
        self.apply_filter();
        self.status = if self.unreads_first {
            "unread conversations first".to_string()
        } else {
            format!("conversations by {}", self.sort_label())
        };
    }

    /// Where `c` writes: the open thread replies in that thread, a search or
    /// threads hit replies in the hit's thread, a timeline posts to its
    /// conversation, the list posts to the highlighted conversation.
    fn compose_target(&self) -> Result<Compose, String> {
        let known = |cid: &str| -> Result<usize, String> {
            self.corpus
                .conv_by_channel(cid)
                .ok_or_else(|| format!("{cid} is not a conversation this tool knows"))
        };
        match self.stack.last() {
            Some(View::Thread { root, list, .. }) => {
                let cid = match list.msgs.first() {
                    Some(m) => m.channel_id.clone(),
                    None => self
                        .open
                        .as_ref()
                        .map(|o| self.corpus.convs[o.conv].id.clone())
                        .ok_or("no conversation")?,
                };
                let conv = known(&cid)?;
                let name = self.corpus.convs[conv].name.clone();
                Ok(Compose {
                    conv,
                    cid,
                    thread: Some(*root),
                    label: format!("reply in this thread in {name}"),
                })
            }
            Some(View::Search { .. }) | Some(View::Threads { .. }) => {
                let m = self.selected().ok_or("no message selected")?;
                let conv = known(&m.channel_id)?;
                let name = self.corpus.convs[conv].name.clone();
                let who = m
                    .user
                    .as_deref()
                    .and_then(|u| self.corpus.user_name(u))
                    .unwrap_or_else(|| "?".to_string());
                Ok(Compose {
                    conv,
                    cid: m.channel_id.clone(),
                    thread: Some(m.parent_id.unwrap_or(m.id)),
                    label: format!("reply in {who}'s thread in {name}"),
                })
            }
            Some(View::Raw { .. })
            | Some(View::Image { .. })
            | Some(View::Reactions { .. })
            | Some(View::ColorPalette { .. })
            | Some(View::Keys { .. }) => Err("close this view first".to_string()),
            None => {
                let conv = match (self.focus, self.open.as_ref()) {
                    (Focus::Msgs, Some(o)) => o.conv,
                    _ => *self
                        .filtered
                        .get(self.conv_cursor)
                        .ok_or("nothing highlighted")?,
                };
                let c = &self.corpus.convs[conv];
                Ok(Compose {
                    conv,
                    cid: c.id.clone(),
                    thread: None,
                    label: format!("message to {}", c.name),
                })
            }
        }
    }

    /// `/`: an empty command prompt; `find` alone clears the list filter.
    fn open_command(&mut self) {
        self.mode = Mode::Prompt {
            kind: PromptKind::Command,
            buf: Editor::default(),
            previous: self.filter.clone(),
        };
    }

    /// The list follows a `find`/`search` command as it is typed.
    fn filter_live(&mut self, line: &str) {
        if self.focus != Focus::Convs {
            return;
        }
        if let Some(Command::Find(text)) = parse_command(line) {
            self.filter = text;
            self.apply_filter();
        }
    }

    /// Enter in the command prompt.
    fn run_command(&mut self, line: &str, filter_before: &str) {
        let name = line.split_whitespace().next().unwrap_or("").trim_start_matches('/');
        crate::session_log::record("command", serde_json::json!({"name":complete::COMMANDS.iter().find(|c|c.name == name).map(|c|c.name).unwrap_or("unknown")}));
        match parse_command(line) {
            None => {
                if self.focus == Focus::Convs {
                    self.filter = filter_before.to_string();
                    self.apply_filter();
                }
                if !line.trim().is_empty() {
                    let names: Vec<&str> = complete::COMMANDS.iter().map(|c| c.name).collect();
                    self.status = format!(
                        "unknown command: {}; Tab completes, and the commands are {}",
                        line.split_whitespace().next().unwrap_or(""),
                        names.join(", ")
                    );
                }
            }
            Some(Command::Find(text)) => {
                if self.focus == Focus::Convs {
                    self.filter = text;
                    self.apply_filter();
                } else if text.is_empty() {
                    self.status = "search what?".to_string();
                } else {
                    self.run_search(&text);
                }
            }
            Some(Command::Leave(name)) => {
                self.restore_filter(filter_before);
                self.leave_conv(&name);
            }
            Some(Command::Cache(op, name)) => {
                self.restore_filter(filter_before);
                self.cache_cmd(&op, &name);
            }
            Some(Command::Star(on, name)) => {
                self.restore_filter(filter_before);
                self.star_cmd(on, &name);
            }
            Some(Command::Mute(on, name)) => {
                self.restore_filter(filter_before);
                self.mute_cmd(on, &name);
            }
            Some(Command::ColorPalette(preset)) => {
                self.restore_filter(filter_before);
                self.open_color_palette(&preset);
            }
            Some(Command::ConversationsPane) => {
                self.restore_filter(filter_before);
                self.open_conversations_pane();
            }
            Some(Command::Keys) => {
                self.restore_filter(filter_before);
                self.open_keys();
            }
            Some(Command::Version) => {
                self.restore_filter(filter_before);
                self.toggle_version();
            }
            Some(Command::Upload(path)) => {
                self.restore_filter(filter_before);
                self.attach(&path);
            }
        }
    }

    /// `/colorpalette [name]`: the editor, over a named palette when one is
    /// given. Esc puts back the colors it opened with.
    fn open_color_palette(&mut self, preset: &str) {
        let applied = if preset.trim().is_empty() {
            None
        } else {
            match Palette::preset(preset) {
                Some(palette) => Some(palette),
                None => {
                    let names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
                    self.status = format!(
                        "no palette named {}; the palettes are {}",
                        preset.trim(),
                        names.join(", ")
                    );
                    return;
                }
            }
        };
        let return_focus = self.focus;
        self.stack.push(View::ColorPalette {
            highlights: None,
            cursor: 0,
            original: self.palette.clone(),
            return_focus,
        });
        self.focus = Focus::Msgs;
        let status = match applied {
            Some(mut palette) => {
                palette.highlights = self.palette.highlights.clone();
                self.palette = palette;
                self.mark_all_dirty();
                format!(
                    "{} applied; Enter saves it, Esc puts the old colors back",
                    preset.trim().to_lowercase()
                )
            }
            None => "j/k a role; h/l a color; e types one (name or #rrggbb); d resets it, D resets all; Enter saves; Esc cancels"
                .to_string(),
        };
        self.status = status;
    }

    /// `/version`: the version in the corner of the status line, or not.
    fn toggle_version(&mut self) {
        self.show_version = !self.show_version;
        self.status = if self.show_version {
            format!("version {}", crate::version())
        } else {
            String::new()
        };
    }

    /// `/keys`: the key editor over the messages pane.
    fn open_keys(&mut self) {
        if matches!(self.stack.last(), Some(View::Keys { .. })) {
            return;
        }
        let return_focus = self.focus;
        self.stack.push(View::Keys {
            cursor: 0,
            original: self.keymap.clone(),
            return_focus,
            capture: None,
        });
        self.focus = Focus::Msgs;
        self.status = "j/k an action; e binds the next key you press, A adds one, d resets the action, D resets all; Enter saves; Esc cancels".to_string();
    }

    fn keys_action(&self) -> Option<Action> {
        match self.stack.last() {
            Some(View::Keys { cursor, .. }) => DEFAULTS.get(*cursor).map(|(action, _)| *action),
            _ => None,
        }
    }

    fn on_keys_key(&mut self, key: KeyEvent, control: bool) {
        let capture = match self.stack.last() {
            Some(View::Keys { capture, .. }) => *capture,
            _ => return,
        };
        if let Some(keep) = capture {
            if key.code == KeyCode::Esc {
                self.set_keys_capture(None);
                self.status = "nothing bound".to_string();
                return;
            }
            let Some(chord) = Chord::of(key) else {
                self.status = "that key cannot carry a binding".to_string();
                return;
            };
            let Some(action) = self.keys_action() else {
                return;
            };
            let stolen = self.keymap.bind(action, chord, keep);
            self.set_keys_capture(None);
            self.status = match stolen {
                Some(other) => format!(
                    "{} is now {:?}, no longer {:?}",
                    chord.text(),
                    action.label(),
                    other.label()
                ),
                None => format!("{} is now {:?}", chord.text(), action.label()),
            };
            return;
        }
        match (key.code, control) {
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => {
                if let Some(View::Keys { cursor, .. }) = self.stack.last_mut() {
                    *cursor = (*cursor + 1).min(DEFAULTS.len() - 1);
                }
            }
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => {
                if let Some(View::Keys { cursor, .. }) = self.stack.last_mut() {
                    *cursor = cursor.saturating_sub(1);
                }
            }
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => {
                if let Some(View::Keys { cursor, .. }) = self.stack.last_mut() {
                    *cursor = 0;
                }
            }
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => {
                if let Some(View::Keys { cursor, .. }) = self.stack.last_mut() {
                    *cursor = DEFAULTS.len() - 1;
                }
            }
            (KeyCode::Char('e'), false) => {
                self.set_keys_capture(Some(false));
                self.status = "press the key to bind, Esc to leave it alone".to_string();
            }
            (KeyCode::Char('A'), false) => {
                self.set_keys_capture(Some(true));
                self.status = "press the key to add, Esc to leave it alone".to_string();
            }
            (KeyCode::Char('d'), false) => {
                if let Some(action) = self.keys_action() {
                    self.keymap.reset(action);
                    self.status = format!("{}: {}", action.label(), self.keymap.text(action));
                }
            }
            (KeyCode::Char('D'), false) => {
                self.keymap = Keymap::default();
                self.status = "every key back to its default".to_string();
            }
            (KeyCode::Enter, _) => self.save_keys(),
            (KeyCode::Esc, _) => {
                let (original, return_focus) = match self.stack.pop() {
                    Some(View::Keys {
                        original,
                        return_focus,
                        ..
                    }) => (original, return_focus),
                    _ => return,
                };
                self.keymap = original;
                self.focus = return_focus;
                self.status = "keys unchanged".to_string();
            }
            _ => {}
        }
    }

    fn set_keys_capture(&mut self, next: Option<bool>) {
        if let Some(View::Keys { capture, .. }) = self.stack.last_mut() {
            *capture = next;
        }
    }

    fn save_keys(&mut self) {
        let return_focus = match self.stack.pop() {
            Some(View::Keys { return_focus, .. }) => return_focus,
            _ => return,
        };
        self.focus = return_focus;
        self.status = match self.keymap.save(self.keys_path.as_deref()) {
            Ok(path) => format!("keys saved to {}", path.display()),
            Err(error) => format!("keys: {error}"),
        };
    }

    /// `e` in the palette: a color typed as a name or as `#rrggbb`.
    fn set_palette_color(&mut self, text: &str) {
        let Some(role) = self.palette_role() else {
            return;
        };
        match crate::palette::parse_color(text) {
            Some(color) => {
                self.palette.set(role, color);
                self.mark_all_dirty();
                self.status = format!("{}: {}", role.label(), self.palette.color_name(role));
            }
            None => {
                self.status = format!("{text:?} is not a color name or #rrggbb");
            }
        }
    }

    fn palette_role(&self) -> Option<Role> {
        match self.stack.last() {
            Some(View::ColorPalette { cursor, .. }) => ROLES.get(*cursor).copied(),
            _ => None,
        }
    }

    fn on_palette_key(&mut self, key: KeyEvent, control: bool) {
        let editing_word = matches!(self.stack.last(), Some(View::ColorPalette { highlights: Some(menu), .. }) if menu.editing());
        if (control && key.code == KeyCode::Char('c')) || (!control && !editing_word && key.code == KeyCode::Char('q')) { self.quit = true; return; }
        if let Some(View::ColorPalette { highlights: Some(menu), .. }) = self.stack.last_mut() {
            let back = menu.key(key, &mut self.palette);
            if back {
                if let Some(View::ColorPalette { highlights, cursor, .. }) = self.stack.last_mut() { *highlights = None; *cursor = 0; }
            }
            self.mark_all_dirty();
            return;
        }
        let words_selected = matches!(self.stack.last(), Some(View::ColorPalette { cursor, .. }) if *cursor == ROLES.len());
        if !control && (key.code == KeyCode::Char('W') || (words_selected && matches!(key.code, KeyCode::Enter | KeyCode::Right | KeyCode::Char('l')))) {
            if let Some(View::ColorPalette { highlights, .. }) = self.stack.last_mut() {
                *highlights = Some(crate::word_highlights::Menu::default());
            }
            return;
        }
        match (key.code, control) {
            (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => self.quit = true,
            (KeyCode::Char('?'), false) | (KeyCode::Char('H'), false) => self.help = true,
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => {
                if let Some(View::ColorPalette { cursor, .. }) = self.stack.last_mut() {
                    *cursor = (*cursor + 1).min(ROLES.len());
                }
            }
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => {
                if let Some(View::ColorPalette { cursor, .. }) = self.stack.last_mut() {
                    *cursor = cursor.saturating_sub(1);
                }
            }
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => {
                if let Some(View::ColorPalette { cursor, .. }) = self.stack.last_mut() {
                    *cursor = 0;
                }
            }
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => {
                if let Some(View::ColorPalette { cursor, .. }) = self.stack.last_mut() {
                    *cursor = ROLES.len();
                }
            }
            (KeyCode::Char('h'), false) | (KeyCode::Left, _) => {
                if let Some(role) = self.palette_role() {
                    self.palette.cycle(role, -1);
                    self.mark_all_dirty();
                }
            }
            (KeyCode::Char('l'), false) | (KeyCode::Right, _) => {
                if let Some(role) = self.palette_role() {
                    self.palette.cycle(role, 1);
                    self.mark_all_dirty();
                }
            }
            (KeyCode::Char('d'), false) => {
                if let Some(role) = self.palette_role() {
                    self.palette.reset(role);
                    self.mark_all_dirty();
                }
            }
            (KeyCode::Char('e'), false) => {
                if let Some(role) = self.palette_role() {
                    self.mode = Mode::Prompt {
                        kind: PromptKind::PaletteColor,
                        buf: Editor::with(self.palette.color_name(role)),
                        previous: String::new(),
                    };
                }
            }
            (KeyCode::Char('D'), false) => {
                self.palette = Palette::default();
                self.mark_all_dirty();
            }
            (KeyCode::Enter, _) => {
                let path = match self.palette.save(self.palette_path.as_deref()) {
                    Ok(path) => path,
                    Err(error) => { self.status = format!("palette: {error}"); return; }
                };
                let return_focus = match self.stack.pop() {
                    Some(View::ColorPalette { return_focus, .. }) => return_focus,
                    _ => return,
                };
                self.focus = return_focus;
                self.status = format!("color palette saved to {}", path.display());
            }
            (KeyCode::Esc, _) => {
                let (original, return_focus) = match self.stack.pop() {
                    Some(View::ColorPalette {
                        original,
                        return_focus,
                        ..
                    }) => (original, return_focus),
                    _ => return,
                };
                self.palette = original;
                self.focus = return_focus;
                self.mark_all_dirty();
                self.status = "color palette unchanged".to_string();
            }
            _ => {}
        }
    }

    fn restore_filter(&mut self, before: &str) {
        if self.focus == Focus::Convs && self.filter != before {
            self.filter = before.to_string();
            self.apply_filter();
        }
    }

    /// The conversation a command acts on: the named one, else the open
    /// one, else the highlighted one.
    /// Conversation names for completion, each once, in list order.
    pub fn conv_names(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.filtered
            .iter()
            .copied()
            .chain(0..self.corpus.convs.len())
            .filter_map(|i| {
                let name = self.corpus.convs.get(i)?.name.clone();
                seen.insert(name.to_lowercase()).then_some(name)
            })
            .collect()
    }

    fn target_conv(&self, name: &str) -> Result<usize, String> {
        if !name.is_empty() {
            return self
                .corpus
                .conv_by_name(name)
                .ok_or_else(|| format!("no conversation named {name}"));
        }
        match (self.focus, self.open.as_ref()) {
            (Focus::Msgs, Some(o)) => Ok(o.conv),
            _ => self
                .filtered
                .get(self.conv_cursor)
                .copied()
                .ok_or_else(|| "nothing highlighted".to_string()),
        }
    }

    /// `/leave [#name]`. A direct message has no membership to give up.
    fn leave_conv(&mut self, name: &str) {
        let Some(c) = self.api.clone() else {
            self.status = "leaving needs the Slack sign-in".to_string();
            return;
        };
        let idx = match self.target_conv(name) {
            Ok(i) => i,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let conv = &self.corpus.convs[idx];
        if conv.kind == Kind::Im {
            self.status = format!("{} is a direct message; nothing to leave", conv.name);
            return;
        }
        if conv.left {
            self.status = format!("{} was already left", conv.name);
            return;
        }
        if self.job.is_some() {
            self.status = "a fetch is already running; try again in a moment".to_string();
            return;
        }
        self.job = Some(live::api_leave(c, idx, conv.id.clone()));
    }

    /// The archive directory behind a conversation, and whether other
    /// conversations share it (the multi-channel archive).
    fn archive_dir(&self, idx: usize) -> Option<(PathBuf, bool)> {
        let c = &self.corpus.convs[idx];
        if c.live_only {
            return None;
        }
        let archive = &self.corpus.archives[c.archive];
        let shared = archive.conn.query_row("SELECT COUNT(DISTINCT CHANNEL_ID) FROM main.MESSAGE", [], |row| row.get::<_,i64>(0))
            .map(|count| count > 1).unwrap_or(true)
            || self.corpus.archives.iter().any(|other| other.source_dirs.iter().skip(1).any(|dir| dir == &archive.dir));
        Some((self.corpus.archives[c.archive].dir.clone(), shared))
    }

    /// `/cache start|stop|wipe [#name]`: archive a conversation, pause its
    /// hourly refresh with a `.paused` marker the refresh script honours,
    /// or delete its archive.
    fn cache_cmd(&mut self, op: &str, name: &str) {
        if op == "highlight" {
            self.highlight_cached = match name.trim().to_lowercase().as_str() {
                "on" => true,
                "off" => false,
                "" => !self.highlight_cached,
                other => {
                    self.status = format!("/cache highlight takes on or off, not {other:?}");
                    return;
                }
            };
            self.status = if self.highlight_cached {
                "cached conversations in the palette's cached color".to_string()
            } else {
                "cached conversations no longer colored".to_string()
            };
            return;
        }
        let idx = match self.target_conv(name) {
            Ok(i) => i,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let cname = self.corpus.convs[idx].name.clone();
        if matches!(op, "stop" | "wipe") && self.corpus.conv_archive(&self.corpus.convs[idx])
            .is_some_and(|archive| archive.source_dirs.len() > 1) {
            self.status = format!("{cname} uses multiple archives; stop/wipe needs an explicit source archive");
            return;
        }
        match (op, self.archive_dir(idx)) {
            ("start", None) => {
                let id = self.corpus.convs[idx].id.clone();
                self.archive_new(&id);
            }
            ("start", Some((dir, _))) => {
                let marker = dir.join(".paused");
                if marker.exists() {
                    self.status = match std::fs::remove_file(&marker) {
                        Ok(()) => format!("{cname}: hourly refresh resumed"),
                        Err(e) => format!("{cname}: {e}"),
                    };
                } else {
                    self.status = format!("{cname} is cached and refreshed hourly already");
                }
            }
            ("stop", None) | ("wipe", None) => self.status = format!("{cname} is not cached"),
            (_, Some((_, true))) => {
                self.status = format!(
                    "{cname} lives in the shared multi-channel archive; stop and wipe apply to a conversation with its own archive"
                );
            }
            ("stop", Some((dir, false))) => {
                self.status =
                    match std::fs::write(dir.join(".paused"), b"paused by slack-tui /cache stop\n")
                    {
                        Ok(()) => {
                            format!("{cname}: hourly refresh paused; the archive stays readable")
                        }
                        Err(e) => format!("{cname}: {e}"),
                    };
            }
            ("wipe", Some((dir, false))) => {
                if let Err(e) = std::fs::remove_dir_all(&dir) {
                    self.status = format!("{cname}: {e}");
                    return;
                }
                if self.open.as_ref().map(|o| o.conv) == Some(idx) {
                    self.open = None;
                    self.stack.clear();
                }
                let c = &mut self.corpus.convs[idx];
                c.live_only = true;
                c.msgs = 0;
                self.apply_filter();
                self.mark_all_dirty();
                self.status = format!("{cname}: archive deleted; it is live-only now");
            }
            _ => {}
        }
    }

    /// Replace the confirmed server snapshot, including conversations unmuted elsewhere.
    fn take_muted(&mut self, ids: Vec<String>) {
        self.muted = ids.into_iter().collect();
        self.apply_filter();
        self.mark_all_dirty();
    }

    fn sync_muted(&mut self) {
        for c in self.corpus.convs.iter_mut() {
            c.muted = self.muted.contains(&c.id);
        }
    }

    fn star_cmd(&mut self, on: bool, name: &str) {
        let index = match self.target_conv(name) {
            Ok(index) => index,
            Err(error) => { self.status = error; return; }
        };
        let Some(client) = self.api.clone().filter(|_| self.live) else {
            self.status = "Star/unstar needs a Slack sign-in".into();
            return;
        };
        if self.job.is_some() {
            self.status = "Wait for the current Slack operation".into();
            return;
        }
        self.starred_generation = self.starred_generation.wrapping_add(1);
        self.job = Some(live::api_set_starred(client, self.conv(index).id.clone(), on));
    }
    fn take_starred_snapshot(&mut self, generation: u64, ids: Vec<String>) {
        if generation == self.starred_generation {
            self.starred = ids.into_iter().collect();
            self.apply_filter();
        }
    }
    fn finish_star(&mut self, cid: &str, starred: bool, ids: Vec<String>) {
        self.starred_generation = self.starred_generation.wrapping_add(1);
        self.take_starred_snapshot(self.starred_generation, ids);
        let name = self.corpus.convs.iter().find(|c| c.id == cid).map(|c| c.name.as_str()).unwrap_or(cid);
        self.status = format!("{} {} in Slack (verified)", name, if starred { "starred" } else { "unstarred" });
    }

    /// `/mute` and `/unmute` update Slack; local state changes only after verification.
    fn mute_cmd(&mut self, on: bool, name: &str) {
        let idx = match self.target_conv(name) {
            Ok(i) => i,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let Some(client) = self.api.clone().filter(|_| self.live) else {
            self.status = "Mute/unmute needs a Slack sign-in; no local override was changed".into();
            return;
        };
        if self.job.is_some() {
            self.status = "Wait for the current Slack operation before changing mute state".into();
            return;
        }
        let c = &self.corpus.convs[idx];
        self.status = format!(
            "{} {} in Slack…",
            if on { "Muting" } else { "Unmuting" },
            c.name
        );
        self.muted_generation = self.muted_generation.wrapping_add(1);
        self.job = Some(live::api_set_muted(client, c.id.clone(), on));
    }

    fn finish_mute(&mut self, cid: &str, muted: bool, ids: Vec<String>) {
        self.muted_generation = self.muted_generation.wrapping_add(1);
        self.take_muted(ids);
        let name = self
            .corpus
            .convs
            .iter()
            .find(|c| c.id == cid)
            .map(|c| c.name.as_str())
            .unwrap_or(cid);
        self.status = format!(
            "{name} {} in Slack (verified)",
            if muted { "muted" } else { "unmuted" }
        );
    }

    fn take_muted_snapshot(&mut self, gen: u64, ids: Vec<String>) {
        if gen == self.muted_generation {
            self.take_muted(ids);
        }
    }

    /// `D`: arm a delete of the selected message, or carry out the one
    /// already armed. Slack only lets the author withdraw a message, so a
    /// message written by someone else is refused before the round trip.
    fn delete_selected(&mut self) {
        if let Some(pending) = self.pending_delete.take() {
            let Some(c) = self.api.clone() else {
                self.status = "deleting needs the Slack sign-in".to_string();
                return;
            };
            if self.job.is_some() {
                self.status = "a fetch is already running; press D again in a moment".to_string();
                return;
            }
            self.job = Some(live::api_delete(c, pending.cid, pending.id));
            return;
        }
        if self.api.is_none() {
            self.status = "deleting needs the Slack sign-in".to_string();
            return;
        }
        let Some(m) = self.selected() else {
            self.status = "no message selected".to_string();
            return;
        };
        let Some(me) = self.corpus.me.as_deref() else {
            self.status = "own user id unknown (no DM archive): set SLACK_SELF_USER_ID".to_string();
            return;
        };
        if m.user.as_deref() != Some(me) {
            let who = m
                .user
                .as_deref()
                .and_then(|u| self.corpus.user_name(u))
                .unwrap_or_else(|| "someone else".to_string());
            self.status = format!("that message is {who}'s; Slack only deletes your own");
            return;
        }
        let first = m.text.lines().next().unwrap_or("").trim().to_string();
        let shown: String = first.chars().take(40).collect();
        self.pending_delete = Some(PendingDelete {
            cid: m.channel_id.clone(),
            id: m.id,
        });
        self.status = if shown.is_empty() {
            "delete this message? D again confirms, any other key cancels".to_string()
        } else {
            format!("delete \u{201c}{shown}\u{201d}? D again confirms, any other key cancels")
        };
    }

    /// Drop a deleted message from every list holding it.
    fn drop_message(&mut self, id: i64) {
        let mut lists: Vec<&mut MsgList> = Vec::new();
        if let Some(o) = self.open.as_mut() {
            lists.push(&mut o.list);
        }
        for view in self.stack.iter_mut() {
            match view {
                View::Thread { list, .. } | View::Search { list, .. } | View::Threads { list } => {
                    lists.push(list)
                }
                _ => {}
            }
        }
        for list in lists {
            let Some(at) = list.msgs.iter().position(|m| m.id == id) else {
                continue;
            };
            list.msgs.remove(at);
            list.cursor = list.cursor.min(list.len().saturating_sub(1));
            list.mark_dirty();
        }
    }

    fn view_reactions(&mut self) {
        let Some(message) = self.selected() else {
            self.status = "no message selected".into();
            return;
        };
        let conversation = self.corpus.conv_by_channel(&message.channel_id)
            .map(|index| &self.corpus.convs[index]);
        let archive = match self.stack.last() {
            Some(View::Thread { live: Some(archive), .. }) => Some(archive.as_ref()),
            _ => conversation.and_then(|conv| self.corpus.conv_archive(conv)),
        };
        let context = Ctx { archive, corpus: &self.corpus, tz: self.tz,
            image_font: None, last_read: None, palette: &self.palette };
        let channel = conversation.map(|conv| conv.name.clone())
            .or_else(|| message.channel_name.as_ref().map(|name| format!("#{}", name.trim_start_matches('#'))))
            .unwrap_or_else(|| message.channel_id.clone());
        let title = format!("{channel} · Reactions · {} {} · {}",
            self.tz.fmt(message.id / 1_000_000, "%Y-%m-%d %H:%M:%S"), self.tz.label(), context.author(message));
        let lines = reaction_details(message, &context);
        self.pending_delete = None;
        self.stack.push(View::Reactions { title, lines, scroll: 0 });
    }

    /// `/upload [path]`: hold a file for the next send and open the compose
    /// prompt, so a comment can go with it. No path means the clipboard.
    fn attach(&mut self, argument: &str) {
        let file = if argument.trim().is_empty() {
            crate::clip::image(&self.cache_dir.join("uploads"))
        } else {
            let text = argument.trim().trim_matches(['"', '\'']);
            let path = match text.strip_prefix("~/") {
                Some(rest) => match std::env::var_os("HOME") {
                    Some(home) => PathBuf::from(home).join(rest),
                    None => PathBuf::from(text),
                },
                None => PathBuf::from(text),
            };
            match path.is_file() {
                true => Ok(path),
                false => Err(format!("{}: not a file", path.display())),
            }
        };
        match file {
            Ok(path) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let name = file_name(&path);
                self.attachment = Some(path);
                self.compose();
                if self.compose.is_some() {
                    self.status = format!("{name} attached, {}", human_size(size));
                } else {
                    // No target took it: nothing to send it with.
                    self.attachment = None;
                }
            }
            Err(error) => self.status = error,
        }
    }

    /// Lets go of the held file, deleting it when this tool made it: a
    /// clipboard capture nobody sent has no other owner.
    fn drop_attachment(&mut self) -> Option<String> {
        let path = self.attachment.take()?;
        if path.starts_with(self.cache_dir.join("uploads")) {
            let _ = std::fs::remove_file(&path);
        }
        Some(file_name(&path))
    }

    /// Ctrl-v in the compose prompt: the clipboard's image, held for the send.
    fn attach_clipboard(&mut self) {
        match crate::clip::image(&self.cache_dir.join("uploads")) {
            Ok(path) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.drop_attachment();
                self.status = format!("{} attached, {}", file_name(&path), human_size(size));
                self.attach_note = None;
                self.attachment = Some(path);
            }
            Err(error) => {
                self.status = error.clone();
                self.attach_note = Some(clip_note(&error));
            }
        }
    }

    /// `c`: the compose prompt, with the unsent draft if one was kept.
    fn compose(&mut self) {
        if self.api.is_none() {
            self.status = "sending needs the Slack sign-in".to_string();
            return;
        }
        match self.compose_target() {
            Ok(t) => {
                let buf = match &self.draft {
                    Some(d) if d.cid == t.cid && d.thread == t.thread => d.text.clone(),
                    _ => String::new(),
                };
                self.compose = Some(t);
                self.attach_note = None;
                self.mode = Mode::Prompt {
                    kind: PromptKind::Compose,
                    buf: Editor::with(buf),
                    previous: String::new(),
                };
            }
            Err(e) => self.status = e,
        }
    }

    /// The typed text, remembered with the target the prompt was opened for.
    /// An emptied prompt drops its own draft and leaves another target's alone.
    fn keep_draft(&mut self, text: String) {
        let Some(t) = self.compose.as_ref() else {
            return;
        };
        let same = |d: &Draft| d.cid == t.cid && d.thread == t.thread;
        if text.trim().is_empty() {
            if self.draft.as_ref().is_some_and(same) {
                self.draft = None;
            }
            return;
        }
        self.draft = Some(Draft {
            cid: t.cid.clone(),
            thread: t.thread,
            text,
        });
    }

    /// Enter in the compose prompt. The draft stays until Slack confirms, so
    /// a failed send is not lost.
    fn send_message(&mut self, text: String) {
        let Some(t) = self.compose.take() else {
            return;
        };
        if text.trim().is_empty() && self.attachment.is_none() {
            self.status = "nothing to send".to_string();
            return;
        }
        let Some(c) = self.api.clone() else {
            self.compose = Some(t);
            self.keep_draft(text);
            self.status = "sending needs the Slack sign-in".to_string();
            return;
        };
        if self.job.is_some() {
            self.compose = Some(t);
            self.keep_draft(text);
            self.status = "a fetch is already running; press c again in a moment".to_string();
            return;
        }
        let wire = self.link_mentions(text.trim());
        self.draft = Some(Draft {
            cid: t.cid.clone(),
            thread: t.thread,
            text,
        });
        let uploads = self.cache_dir.join("uploads");
        self.job = match self.attachment.take() {
            Some(path) => Some(live::api_upload(
                c, t.conv, t.cid, t.thread, path, wire, &uploads,
            )),
            None => Some(live::api_send(c, t.conv, t.cid, t.thread, wire)),
        };
    }

    fn link_mentions(&self, text: &str) -> String {
        link_mentions(text, |h| self.corpus.user_id(h))
    }

    pub fn sort_label(&self) -> String {
        match self.sort {
            Sort::Mine => format!("my activity ({:.0}d half-life)", self.half_life_days),
            other => other.label().to_string(),
        }
    }

    pub fn conv(&self, i: usize) -> &Conv {
        &self.corpus.convs[i]
    }

    pub fn open_conv_ref(&self) -> Option<&Conv> {
        self.open.as_ref().map(|o| &self.corpus.convs[o.conv])
    }

    pub fn apply_filter(&mut self) {
        self.sync_muted();
        let needle = self.filter.trim_start_matches(['#', '@']).to_lowercase();
        // Substring first, then the characters in order anywhere in the name.
        let rank = |name: &str| -> Option<u8> {
            let n = name.to_lowercase();
            if needle.is_empty() || n.contains(&needle) {
                return Some(0);
            }
            let mut rest = n.chars();
            for ch in needle.chars() {
                if !rest.any(|c| c == ch) {
                    return None;
                }
            }
            Some(1)
        };
        // A live-only entry whose channel an archive also holds (one made by
        // `a` or `/cache start` this run) stays out of the list.
        let convs_all = &self.corpus.convs;
        let mut idx: Vec<usize> = (0..convs_all.len())
            .filter(|&i| {
                let c = &convs_all[i];
                let twin = c.live_only && convs_all.iter().any(|x| !x.live_only && x.id == c.id);
                !twin
                    && self.pane_settings.visible(c)
                    && (rank(&c.name).is_some() || c.id.to_lowercase() == needle)
            })
            .collect();
        let convs = &self.corpus.convs;
        match self.sort {
            // Channels first, public and private together; then group DMs, then DMs.
            Sort::Name => idx.sort_by(|&a, &b| {
                let class = |c: &Conv| match c.kind {
                    Kind::Channel | Kind::Private => 0,
                    Kind::Mpim => 1,
                    Kind::Im => 2,
                };
                (
                    class(&convs[a]),
                    convs[a].name.to_lowercase(),
                    convs[a].archive,
                )
                    .cmp(&(
                        class(&convs[b]),
                        convs[b].name.to_lowercase(),
                        convs[b].archive,
                    ))
            }),
            // Recency-weighted score first; ties by raw count, then last activity.
            Sort::Mine => idx.sort_by(|&a, &b| {
                convs[b]
                    .score
                    .partial_cmp(&convs[a].score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then((convs[b].mine, convs[b].last_id).cmp(&(convs[a].mine, convs[a].last_id)))
            }),
            Sort::Recent => idx.sort_by(|&a, &b| convs[b].last_id.cmp(&convs[a].last_id)),
            Sort::Size => idx.sort_by(|&a, &b| convs[b].msgs.cmp(&convs[a].msgs)),
        }
        // Unread conversations first, in the same order among themselves;
        // a typed filter still puts the closer name matches above.
        if self.unreads_first {
            idx.sort_by_key(|&i| !convs[i].unread);
        }
        if !needle.is_empty() {
            idx.sort_by_key(|&i| rank(&convs[i].name).unwrap_or(2));
        }
        // Muted conversations keep their unread color but sink to the end.
        idx.sort_by_key(|&i| convs[i].muted);
        // Stars take precedence over unread, mute, search rank and sort mode.
        idx.sort_by_key(|&i| !self.starred.contains(&convs[i].id));
        // Keep the highlighted conversation highlighted across a re-sort.
        let current = self.filtered.get(self.conv_cursor).copied();
        self.filtered = idx;
        self.conv_cursor = current
            .and_then(|c| self.filtered.iter().position(|&i| i == c))
            .unwrap_or(0)
            .min(self.filtered.len().saturating_sub(1));
        // A shorter list must not leave the pane scrolled past its cursor.
        self.conv_offset = self.conv_offset.min(self.conv_cursor);
    }

    // ------------------------------------------------------------- loading

    fn ctx_for(&self, conv: usize) -> Ctx<'_> {
        Ctx {
            archive: self.corpus.conv_archive(&self.corpus.convs[conv]),
            corpus: &self.corpus,
            tz: self.tz,
            image_font: self.image_font(),
            last_read: None,
            palette: &self.palette,
        }
    }

    pub fn open_conv(&mut self, idx: usize) -> bool {
        let conv = &self.corpus.convs[idx];
        let cid = conv.id.clone();
        if conv.live_only {
            let list = MsgList::new(Vec::new(), false);
            self.open = Some(Open {
                conv: idx,
                list,
                total: 0,
                has_older: false,
                has_newer: false,
                api_only: true,
            });
            self.stack.clear();
            self.focus = Focus::Msgs;
            self.counts_pending = true;
            match self.api.clone() {
                Some(c) if self.job.is_none() => {
                    self.job = Some(live::api_older(c, idx, cid, 0));
                    self.status = "loading from Slack".to_string();
                }
                Some(_) => self.status = "a fetch is already running".to_string(),
                None => {
                    self.status = "not signed in, and this conversation is not cached".to_string()
                }
            }
            self.update_notes();
            return true;
        }
        let a = &self.corpus.archives[conv.archive];
        let total = a.timeline_count(&cid).unwrap_or(0);
        let msgs = match a.timeline_page(&cid, None, None, PAGE) {
            Ok(m) => m,
            Err(e) => {
                self.status = format!("{}: {e}", a.rel);
                return false;
            }
        };
        let has_older = (msgs.len() as i64) < total;
        let since = msgs.last().map(|m| m.id).unwrap_or(0);
        let mut list = MsgList::new(msgs, false);
        list.cursor = list.len().saturating_sub(1);
        // Land on the first unread message when the page holds one, with
        // the highlighted divider above it on screen.
        let last_read = self.corpus.convs[idx].last_read;
        if last_read > 0 {
            if let Some(first_new) = list.msgs.iter().position(|m| m.id > last_read) {
                list.cursor = first_new;
                list.align_top = true;
            }
        }
        self.open = Some(Open {
            conv: idx,
            list,
            total,
            has_older,
            has_newer: false,
            api_only: false,
        });
        self.stack.clear();
        self.focus = Focus::Msgs;
        self.update_notes();
        self.status.clear();
        self.counts_pending = true;
        // Whatever Slack has past the archive's end, quietly.
        if let Some(c) = self.api.clone() {
            if self.bg.is_none() && since > 0 {
                self.bg = Some(live::api_tail(c, idx, cid, since, true));
                self.last_poll = Instant::now();
            }
        }
        true
    }

    fn update_notes(&mut self) {
        let Some(o) = self.open.as_mut() else {
            return;
        };
        let loaded = o.list.len() as i64;
        o.list.top_note = if o.api_only {
            o.has_older
                .then(|| "older messages on Slack · k loads more".to_string())
        } else if o.has_older {
            Some(format!(
                "{} older messages not loaded · k loads more, g loads the oldest",
                o.total - loaded
            ))
        } else {
            None
        };
        o.list.bottom_note = if o.has_newer {
            Some("newer messages not loaded · j loads more, G loads the newest".to_string())
        } else {
            None
        };
        o.list.mark_dirty();
    }

    fn load_older(&mut self) {
        let Some(o) = self.open.as_mut() else {
            return;
        };
        if !o.has_older {
            return;
        }
        if o.api_only {
            let Some(c) = self.api.clone() else {
                return;
            };
            if self.job.is_some() {
                return;
            }
            let idx = o.conv;
            let before = o.list.msgs.first().map(|m| m.id).unwrap_or(0);
            let cid = self.corpus.convs[idx].id.clone();
            self.job = Some(live::api_older(c, idx, cid, before));
            return;
        }
        let conv = &self.corpus.convs[o.conv];
        let a = &self.corpus.archives[conv.archive];
        let oldest = o.list.msgs.first().map(|m| m.id);
        let page = match a.timeline_page(&conv.id, oldest, None, PAGE) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("{e}");
                return;
            }
        };
        let n = page.len();
        if n == 0 {
            o.has_older = false;
        } else {
            o.list.msgs.splice(0..0, page);
            o.list.cursor += n;
            o.has_older = n >= PAGE;
            o.list.mark_dirty();
        }
        self.status = format!("loaded {n} older");
        self.update_notes();
    }

    fn load_newer(&mut self) {
        let Some(o) = self.open.as_mut() else {
            return;
        };
        if !o.has_newer { return; }
        if o.api_only {
            if self.job.is_none() {
                if let Some(client) = self.api.clone() {
                    let since = o.list.msgs.last().map(|message| message.id).unwrap_or(0);
                    self.job = Some(live::api_newer(client, o.conv, self.corpus.convs[o.conv].id.clone(), since));
                }
            }
            return;
        }
        let conv = &self.corpus.convs[o.conv];
        let a = &self.corpus.archives[conv.archive];
        let newest = o.list.msgs.last().map(|m| m.id + 1);
        let page = match a.timeline_page(&conv.id, None, newest, PAGE) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("{e}");
                return;
            }
        };
        let n = page.len();
        if n == 0 {
            o.has_newer = false;
        } else {
            o.list.msgs.extend(page);
            o.has_newer = n >= PAGE;
            o.list.mark_dirty();
        }
        self.status = format!("loaded {n} newer");
        self.update_notes();
    }

    fn open_file_message(&mut self, location: crate::file_message::Location) {
        let Some(conv) = self.corpus.conv_by_channel(&location.channel) else {
            if let Some(browser) = &mut self.channel_browser { browser.message_unavailable(); }
            return;
        };
        // Foreground fetches can replace the current view; wait instead of discarding a write.
        if self.job.is_some() {
            if let Some(browser) = &mut self.channel_browser { browser.message = Some(location); }
            return;
        }
        if self.bg.as_ref().is_some_and(|job| matches!(job.kind, JobKind::Tail { .. } | JobKind::Thread { .. })) {
            self.bg = None;
        }
        self.last_poll = Instant::now();
        let mut list = MsgList::new(location.timeline, false);
        list.cursor = list.msgs.iter().position(|m| m.id == location.root).unwrap_or(0);
        list.align_top = true;
        self.open = Some(Open { conv, total: self.corpus.convs[conv].msgs, list,
            has_older: location.has_older, has_newer: location.has_newer, api_only: true });
        self.stack.clear();
        if location.focus != location.root {
            let mut list = MsgList::new(location.replies, true);
            list.cursor = list.msgs.iter().position(|m| m.id == location.focus).unwrap_or(0);
            list.align_top = true;
            self.stack.push(View::Thread { root: location.root, list, live: None, place: None });
        }
        self.focus = Focus::Msgs;
        if let Some(index) = self.filtered.iter().position(|index| *index == conv) { self.conv_cursor = index; }
        if let Some(browser) = &mut self.channel_browser { browser.visible = false; }
        self.status = "Opened file sharing message".into();
        self.update_notes();
    }

    /// Re-centre the timeline on a message id: half a page each side.
    pub fn jump_to(&mut self, id: i64) {
        let Some(o) = self.open.as_mut() else { return };
        if o.api_only {
            self.status =
                "only a cached conversation can be positioned; a archives this one".to_string();
            return;
        }
        let conv = &self.corpus.convs[o.conv];
        let a = &self.corpus.archives[conv.archive];
        let half = PAGE / 2;
        let older = a
            .timeline_page(&conv.id, Some(id), None, half)
            .unwrap_or_default();
        let newer = a
            .timeline_page(&conv.id, None, Some(id), half)
            .unwrap_or_default();
        let cursor = if newer.is_empty() {
            older.len().saturating_sub(1)
        } else {
            older.len()
        };
        o.has_older = older.len() >= half;
        o.has_newer = newer.len() >= half;
        let mut msgs = older;
        msgs.extend(newer);
        let mut list = MsgList::new(msgs, false);
        list.cursor = cursor;
        list.align_top = true;
        o.list = list;
        self.update_notes();
    }

    /// A thread by channel id: the archive first, then the thread cache,
    /// then Slack in the background.
    pub fn open_thread_in(&mut self, cid: String, root: i64, focus: i64) {
        let here = self
            .open
            .as_ref()
            .map(|o| self.corpus.convs[o.conv].id == cid)
            .unwrap_or(false);
        let place = if here {
            None
        } else {
            Some(
                self.corpus
                    .channel_names
                    .get(&cid)
                    .map(|n| format!("#{n}"))
                    .unwrap_or_else(|| cid.clone()),
            )
        };
        let msgs = self
            .corpus
            .conv_by_channel(&cid)
            .filter(|&ci| !self.corpus.convs[ci].live_only)
            .map(|ci| {
                self.corpus.archives[self.corpus.convs[ci].archive]
                    .thread(&cid, root)
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let wanted = msgs
            .first()
            .filter(|m| m.id == root)
            .map(|m| m.reply_count)
            .unwrap_or(0);
        let have = msgs.len().saturating_sub(1) as i64;
        let complete = !msgs.is_empty() && have >= wanted;
        let mut list = MsgList::new(msgs, true);
        list.cursor = list.msgs.iter().position(|m| m.id == focus).unwrap_or(0);
        list.align_top = list.cursor > 0;
        self.stack.push(View::Thread {
            root,
            list,
            live: None,
            place,
        });
        // Slack has the thread's newest replies; the archive's copy is only
        // what the last archive run saw, and its reply count is that old too.
        // So a sign-in always re-asks, and the caches answer only without one.
        if let Some(c) = self.api.clone() {
            if self.job.is_some() {
                self.status = "a fetch is already running".to_string();
                return;
            }
            self.job = Some(live::api_thread(c, &self.cache_dir, cid, root, focus));
            return;
        }
        if complete {
            return;
        }
        if let Some(msgs) = live::cached_thread(&self.cache_dir, &cid, root) {
            self.apply_thread_msgs(msgs, root, focus, "the cache");
            return;
        }
        let dir = live::thread_dir(&self.cache_dir, &cid, root);
        if dir.join("slackdump.sqlite").is_file() {
            self.apply_thread_dir(&dir, &cid, root, focus);
            return;
        }
        if self.job.is_some() {
            self.status = "a fetch is already running".to_string();
            return;
        }
        if self.slackdump && self.live {
            self.job = Some(live::fetch_thread(
                &self.cache_dir,
                &self.corpus.workspace_url,
                cid,
                root,
                focus,
            ));
        } else {
            self.status = if wanted == 0 && have == 0 {
                "thread root is not in the archive; not signed in".to_string()
            } else {
                format!("{have} of {wanted} replies archived; not signed in")
            };
        }
    }

    /// Replies Slack has beyond the open thread, appended in place. The
    /// cursor and the scroll stay where the reader left them.
    fn extend_thread(&mut self, msgs: Vec<Msg>, root: i64) {
        let Some(View::Thread { root: r, list, .. }) = self.stack.last_mut() else {
            return;
        };
        if *r != root {
            return;
        }
        let known: HashSet<i64> = list.msgs.iter().map(|m| m.id).collect();
        let fresh: Vec<Msg> = msgs
            .into_iter()
            .filter(|m| !known.contains(&m.id))
            .collect();
        if fresh.is_empty() {
            return;
        }
        let n = fresh.len();
        let at_end = list.cursor + 1 >= list.len();
        list.msgs.extend(fresh);
        list.msgs.sort_by_key(|m| m.id);
        if at_end {
            list.cursor = list.len() - 1;
        }
        list.mark_dirty();
        self.status = format!("{n} new in this thread");
    }

    /// Show a thread fetched from Slack (or its JSON cache), replacing the
    /// view of the same thread when it is on top.
    fn apply_thread_msgs(&mut self, msgs: Vec<Msg>, root: i64, focus: i64, from: &str) {
        if msgs.is_empty() {
            self.status = "Slack returned no messages for that thread".to_string();
            return;
        }
        let n = msgs.len() - 1;
        let mut list = MsgList::new(msgs, true);
        list.cursor = list.msgs.iter().position(|m| m.id == focus).unwrap_or(0);
        list.align_top = list.cursor > 0;
        match self.stack.last_mut() {
            Some(View::Thread {
                root: r, list: l, ..
            }) if *r == root => *l = list,
            _ => self.stack.push(View::Thread {
                root,
                list,
                live: None,
                place: None,
            }),
        }
        self.status = format!(
            "{n} {} from {from}",
            if n == 1 { "reply" } else { "replies" }
        );
    }

    /// Show a thread from a fetched archive directory, replacing the view
    /// of the same thread when it is on top.
    fn apply_thread_dir(&mut self, dir: &Path, cid: &str, root: i64, focus: i64) {
        let rel = format!(
            "live/{}",
            dir.file_name().unwrap_or_default().to_string_lossy()
        );
        let mut a = match Archive::open(rel, dir) {
            Ok(a) => a,
            Err(e) => {
                self.status = format!("{e}");
                return;
            }
        };
        let _ = a.scan_convs(
            usize::MAX,
            self.corpus.me.as_deref(),
            self.corpus.half_life_days,
            None,
        );
        let msgs = a.thread(cid, root).unwrap_or_default();
        if msgs.is_empty() {
            self.status = "Slack returned no messages for that thread".to_string();
            return;
        }
        let n = msgs.len() - 1;
        let mut list = MsgList::new(msgs, true);
        list.cursor = list.msgs.iter().position(|m| m.id == focus).unwrap_or(0);
        list.align_top = list.cursor > 0;
        match self.stack.last_mut() {
            Some(View::Thread {
                root: r,
                list: l,
                live,
                ..
            }) if *r == root => {
                *l = list;
                *live = Some(Box::new(a));
            }
            _ => self.stack.push(View::Thread {
                root,
                list,
                live: Some(Box::new(a)),
                place: None,
            }),
        }
        self.status = format!(
            "{n} {} from Slack",
            if n == 1 { "reply" } else { "replies" }
        );
    }

    /// Open a message's thread wherever it lives: this conversation, another
    /// archived one (switched to underneath the search view), or Slack.
    fn open_hit(&mut self, cid: String, root: i64, focus: i64) {
        let here = self
            .open
            .as_ref()
            .map(|o| self.corpus.convs[o.conv].id == cid)
            .unwrap_or(false);
        if !here {
            // A conversation only on Slack is not switched to: its first page
            // would compete with the thread for the one fetch slot.
            if let Some(idx) = self
                .corpus
                .conv_by_channel(&cid)
                .filter(|&i| !self.corpus.convs[i].live_only)
            {
                let search = match self.stack.last() {
                    Some(View::Search { .. }) | Some(View::Threads { .. }) => self.stack.pop(),
                    _ => None,
                };
                self.open_conv(idx);
                self.jump_to(root);
                if let Some(v) = search {
                    self.stack.push(v);
                }
            }
        }
        self.open_thread_in(cid, root, focus);
    }

    pub fn run_search(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        let Some(o) = self.open.as_ref() else {
            return;
        };
        let conv_idx = o.conv;
        let ctx = self.ctx_for(conv_idx);
        let conv = &self.corpus.convs[conv_idx];
        let candidates = match ctx
            .archive
            .map_or_else(|| Ok(Vec::new()), |a| a.search(&conv.id, query, SEARCH_CAP))
        {
            Ok(c) => c,
            Err(e) => {
                self.status = format!("{e}");
                return;
            }
        };
        let capped = candidates.len() >= SEARCH_CAP;
        let needle = query.to_lowercase();
        // Decide on what a reader sees: the rendered text, or a link's URL.
        let hits: Vec<Msg> = candidates
            .into_iter()
            .filter(|m| {
                render::plain(&render::body(m, &ctx))
                    .to_lowercase()
                    .contains(&needle)
                    || render::message_urls(m)
                        .iter()
                        .any(|u| u.to_lowercase().contains(&needle))
                    || ctx.author(m).to_lowercase().contains(&needle)
            })
            .collect();
        let cached = hits.len();
        let go_live = self.live && self.job.is_none() && (self.api.is_some() || self.slackdump);
        if cached == 0 && !go_live {
            self.status = format!(
                "no message matching '{query}' in {}{}",
                conv.name,
                if self.live {
                    "; a fetch is already running"
                } else {
                    ""
                }
            );
            return;
        }
        let plural = if cached == 1 { "" } else { "s" };
        self.status = if go_live {
            format!("{cached} cached hit{plural}; searching Slack")
        } else {
            format!(
                "{cached} hit{plural} for '{query}'{}",
                if capped {
                    " (first 500 candidates only)"
                } else {
                    ""
                }
            )
        };
        // Newest first; the cursor starts on the newest hit.
        let list = MsgList::new(hits, false);
        self.stack.push(View::Search {
            query: query.to_string(),
            list,
            capped,
            live_hits: None,
            live_pending: go_live,
        });
        if go_live {
            self.job = Some(match self.api.clone() {
                Some(c) => live::api_search(c, query.to_string()),
                None => live::search(&self.cache_dir, query.to_string()),
            });
        }
    }

    /// Fold what Slack found into the search view still showing that query.
    fn merge_live_search(&mut self, query: &str, dir: &Path) {
        let hits = Archive::open("live/search".to_string(), dir).and_then(|a| a.search_hits());
        let _ = std::fs::remove_dir_all(dir);
        match hits {
            Ok(h) => self.merge_hits(query, h),
            Err(e) => self.status = format!("live search: {e}"),
        }
    }

    /// Fold what Slack found into the search view still showing that query.
    fn merge_hits(&mut self, query: &str, hits: Vec<Msg>) {
        let total = hits.len();
        match self
            .stack
            .iter_mut()
            .rev()
            .find(|v| matches!(v, View::Search { .. }))
        {
            Some(View::Search {
                query: q,
                list,
                live_hits,
                live_pending,
                ..
            }) if q == query => {
                let cursor_id = list.selected().map(|m| m.id);
                let mut seen: HashSet<(String, i64)> = list
                    .msgs
                    .iter()
                    .map(|m| (m.channel_id.clone(), m.id))
                    .collect();
                let mut added = 0;
                for h in hits {
                    if seen.insert((h.channel_id.clone(), h.id)) {
                        list.msgs.push(h);
                        added += 1;
                    }
                }
                list.msgs.sort_by(|x, y| y.id.cmp(&x.id));
                if let Some(id) = cursor_id {
                    list.cursor = list.msgs.iter().position(|m| m.id == id).unwrap_or(0);
                }
                *live_hits = Some(added);
                *live_pending = false;
                list.mark_dirty();
                self.status = format!(
                    "Slack: {total} hit{}, {added} not in the cache",
                    if total == 1 { "" } else { "s" }
                );
            }
            _ => self.status = format!("live search finished after its view closed: {total} hits"),
        }
    }

    fn refresh(&mut self) {
        if let Some(View::Thread { root, list, .. }) = self.stack.last() {
            let root = *root;
            let focus = list.selected().map(|m| m.id).unwrap_or(root);
            let cid = match list.msgs.first() {
                Some(m) => m.channel_id.clone(),
                None => match self.open.as_ref() {
                    Some(o) => self.corpus.convs[o.conv].id.clone(),
                    None => return,
                },
            };
            if self.job.is_some() {
                self.status = "a fetch is already running".to_string();
                return;
            }
            if let Some(c) = self.api.clone() {
                self.job = Some(live::api_thread(c, &self.cache_dir, cid, root, focus));
            } else {
                self.status = "refreshing a thread needs the Slack sign-in".to_string();
            }
            return;
        }
        let Some(o) = self.open.as_ref() else {
            return;
        };
        if self.job.is_some() {
            self.status = "a fetch is already running".to_string();
            return;
        }
        let idx = o.conv;
        let conv = &self.corpus.convs[idx];
        if let Some(c) = self.api.clone() {
            let since = o.list.msgs.last().map(|m| m.id).unwrap_or(conv.last_id);
            if since == 0 {
                self.job = Some(live::api_older(c, idx, conv.id.clone(), 0));
            } else {
                self.job = Some(live::api_tail(c, idx, conv.id.clone(), since, false));
            }
            return;
        }
        if !self.live || !self.slackdump || o.api_only {
            self.status = "not signed in, and no slackdump to fall back on".to_string();
            return;
        }
        let dir = self.corpus.archives[conv.archive].dir.clone();
        let lookback =
            live::lookback_hours(conv.last_id / 1_000_000, chrono::Utc::now().timestamp());
        self.job = Some(live::refresh(
            idx,
            o.total,
            dir,
            self.lock.clone(),
            lookback,
        ));
    }

    /// Messages Slack has after what is loaded, appended in place.
    fn append_tail(&mut self, conv: usize, mut msgs: Vec<Msg>, quiet: bool) {
        let Some(o) = self.open.as_mut() else {
            return;
        };
        if o.conv != conv || o.has_newer {
            return;
        }
        let known: HashSet<i64> = o.list.msgs.iter().map(|m| m.id).collect();
        msgs.retain(|m| !known.contains(&m.id));
        let n = msgs.len();
        if n == 0 {
            if !quiet {
                self.status = "nothing newer on Slack".to_string();
            }
            return;
        }
        let at_end = o.list.cursor + 1 >= o.list.len();
        o.list.msgs.extend(msgs);
        if at_end {
            o.list.cursor = o.list.len() - 1;
        }
        o.list.mark_dirty();
        if let Some(last) = o.list.msgs.last() {
            let c = &mut self.corpus.convs[conv];
            if last.id > c.last_id {
                c.last_id = last.id;
                c.unread_count = None;
                c.unread_snapshot = None;
            }
        }
        self.status = format!("{n} new from Slack");
    }

    /// An older page from Slack for a conversation with no archive.
    fn prepend_older(&mut self, conv: usize, mut msgs: Vec<Msg>, more: bool) {
        let Some(o) = self.open.as_mut() else {
            return;
        };
        if o.conv != conv {
            return;
        }
        let known: HashSet<i64> = o.list.msgs.iter().map(|m| m.id).collect();
        msgs.retain(|m| !known.contains(&m.id));
        let n = msgs.len();
        let was_empty = o.list.msgs.is_empty();
        o.has_older = more;
        if n > 0 {
            o.list.msgs.splice(0..0, msgs);
            o.list.cursor = if was_empty {
                o.list.len() - 1
            } else {
                o.list.cursor + n
            };
            if was_empty {
                let last_read = self.corpus.convs[conv].last_read;
                if last_read > 0 {
                    if let Some(first_new) = o.list.msgs.iter().position(|m| m.id > last_read) {
                        o.list.cursor = first_new;
                        o.list.align_top = true;
                    }
                }
            }
            o.list.mark_dirty();
        }
        o.total = o.list.len() as i64;
        if let (Some(first), Some(last)) = (o.list.msgs.first(), o.list.msgs.last()) {
            let c = &mut self.corpus.convs[conv];
            c.first_id = first.id;
            if last.id > c.last_id {
                c.last_id = last.id;
                c.unread_count = None;
                c.unread_snapshot = None;
            }
        }
        self.status = format!("{n} from Slack");
        self.update_notes();
    }

    /// Conversations the user is a member of that no archive holds.
    fn merge_conversations(&mut self, list: Vec<Value>) {
        let me = self.corpus.me.clone();
        let my_handle = me.as_deref().and_then(|m| self.corpus.user_name(m));
        let mut added = 0;
        for ch in list {
            let Some(id) = ch.get("id").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            if self.corpus.conv_by_channel(&id).is_some() {
                continue;
            }
            let s = |k: &str| ch.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let b = |k: &str| ch.get(k).and_then(Value::as_bool).unwrap_or(false);
            let raw = s("name");
            let (kind, name) = if b("is_im") {
                let u = s("user");
                self.dm_users.insert(id.clone(), u.clone());
                let n = self.corpus.user_name(&u).unwrap_or_else(|| u.clone());
                (
                    Kind::Im,
                    if Some(u.as_str()) == me.as_deref() {
                        "@me (self)".to_string()
                    } else {
                        format!("@{n}")
                    },
                )
            } else if b("is_mpim") {
                let stripped = raw.trim_start_matches("mpdm-");
                let stripped = stripped
                    .rsplit_once('-')
                    .map(|(a, _)| a)
                    .unwrap_or(stripped);
                let mut names: Vec<&str> = stripped.split("--").collect();
                names.retain(|n| Some(*n) != my_handle.as_deref());
                (Kind::Mpim, format!("@{}", names.join(",")))
            } else if b("is_private") {
                (Kind::Private, format!("#{raw}"))
            } else {
                (Kind::Channel, format!("#{raw}"))
            };
            let created = ch.get("created").and_then(Value::as_i64).unwrap_or(0) * 1_000_000;
            self.corpus.convs.push(Conv {
                archive: 0,
                id: id.clone(),
                name,
                kind,
                archived: false,
                msgs: 0,
                mine: 0,
                score: 0.0,
                first_id: created,
                last_id: 0,
                live_only: true,
                left: false,
                muted: false,
                unread: false,
                unread_count: None,
                unread_snapshot: None,
                mentions: 0,
                last_read: 0,
            });
            if !raw.is_empty() {
                self.corpus.channel_names.entry(id).or_insert(raw);
            }
            added += 1;
        }
        if added > 0 {
            self.apply_filter();
            self.status = format!(
                "{}; {added} conversations only on Slack, shown dim",
                self.status
            );
        }
    }

    fn apply_unread_counts(&mut self, snapshot: &Value) {
        for kind in ["channels", "ims", "mpims"] {
            for entry in snapshot[kind].as_array().into_iter().flatten() {
                let Some(index) = entry["id"].as_str().and_then(|id| self.corpus.conv_by_channel(id)) else { continue };
                let conversation = &mut self.corpus.convs[index];
                if conversation.unread && entry["has_unreads"].as_bool() == Some(true)
                    && conversation.unread_snapshot.is_some()
                    && crate::api::unread_fingerprint(entry) == conversation.unread_snapshot
                {
                    if let Some(count) = entry["unread_count"].as_i64().filter(|count| *count > 0) {
                        conversation.unread_count = Some(count);
                    }
                }
            }
        }
    }

    fn pump_requested_counts(&mut self) {
        if self.counts_pending && self.bg.is_none() && self.unread_count_job.is_none() {
            if let Some(client) = self.api.clone() {
                self.counts_pending = false;
                self.bg = Some(live::api_counts(client, self.counts_gen));
            }
        }
    }

    fn unread_count_targets(&self) -> Vec<String> {
        let mut targets: Vec<String> = if self.pane_settings.number == crate::conversations_pane::NumberColumn::Unread {
            self.filtered.iter().map(|&index| self.corpus.convs[index].id.clone()).collect()
        } else { Vec::new() };
        if let Some(open) = &self.open {
            let id = &self.corpus.convs[open.conv].id;
            if !targets.contains(id) { targets.push(id.clone()); }
        }
        targets
    }

    /// Unread markers from `client.counts`, and last activity for
    /// conversations no archive holds. Lists re-render only on a change.
    fn apply_counts(&mut self, v: &Value) {
        let mut changed = false;
        for key in ["channels", "ims", "mpims"] {
            for c in v.get(key).and_then(Value::as_array).into_iter().flatten() {
                let Some(id) = c.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(idx) = self.corpus.conv_by_channel(id) else {
                    continue;
                };
                let conv = &mut self.corpus.convs[idx];
                let before = (conv.unread, conv.unread_count, conv.mentions, conv.last_read, conv.last_id);
                conv.unread = c
                    .get("has_unreads")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                conv.mentions = c.get("mention_count").and_then(Value::as_i64).unwrap_or(0);
                if let Some(lr) = c
                    .get("last_read")
                    .and_then(Value::as_str)
                    .and_then(ts_to_id)
                {
                    conv.last_read = lr;
                }
                if let Some(latest) = c.get("latest").and_then(Value::as_str).and_then(ts_to_id) {
                    if latest > conv.last_id {
                        conv.last_id = latest;
                    }
                }
                let fingerprint = crate::api::unread_fingerprint(c);
                if conv.unread_snapshot != fingerprint || before.0 != conv.unread {
                    conv.unread_count = None;
                }
                conv.unread_snapshot = fingerprint;
                changed |= before != (conv.unread, conv.unread_count, conv.mentions, conv.last_read, conv.last_id);
            }
        }
        if changed {
            self.apply_filter();
            self.mark_all_dirty();
        }
    }

    fn refresh_conv_stats(&mut self, idx: usize) {
        let conv = &self.corpus.convs[idx];
        let stats = self.corpus.archives[conv.archive].channel_stats(&conv.id);
        if let Ok((msgs, first, last)) = stats {
            let c = &mut self.corpus.convs[idx];
            c.msgs = msgs;
            if first > 0 {
                c.first_id = first;
            }
            if last > 0 && last != c.last_id {
                c.last_id = last;
                c.unread_count = None;
                c.unread_snapshot = None;
            }
        }
    }

    /// Every thread the owner wrote in, newest reply first, from the cache.
    fn open_my_threads(&mut self) {
        let Some(me) = self.corpus.me.clone() else {
            self.status = "own user id unknown (no DM archive): set SLACK_SELF_USER_ID".to_string();
            return;
        };
        if self.open.is_none() {
            let Some(&idx) = self.filtered.get(self.conv_cursor) else {
                return;
            };
            if !self.open_conv(idx) {
                return;
            }
        }
        let mut roots: Vec<Msg> = Vec::new();
        let mut seen: HashSet<(String, i64)> = HashSet::new();
        for (ai, a) in self.corpus.archives.iter().enumerate() {
            let Ok(msgs) = a.my_threads(&me) else {
                continue;
            };
            for mut m in msgs {
                if !seen.insert((m.channel_id.clone(), m.id)) {
                    continue;
                }
                m.channel_name = self
                    .corpus
                    .convs
                    .iter()
                    .find(|c| c.archive == ai && c.id == m.channel_id && !c.live_only)
                    .map(|c| c.name.clone());
                roots.push(m);
            }
        }
        roots.sort_by_key(|m| std::cmp::Reverse(m.latest_reply_id.unwrap_or(m.id)));
        let n = roots.len();
        self.stack.push(View::Threads {
            list: MsgList::new(roots, false),
        });
        self.focus = Focus::Msgs;
        self.status = format!("{n} threads you took part in, newest reply first");
    }

    pub fn image_font(&self) -> Option<(u16, u16)> {
        if !self.inline_images {
            return None;
        }
        self.picker
            .as_ref()
            .map(|p| (p.font_size().width, p.font_size().height))
    }

    fn mark_all_dirty(&mut self) {
        if let Some(o) = self.open.as_mut() {
            o.list.mark_dirty();
        }
        for v in &mut self.stack {
            match v {
                View::Thread { list, .. } | View::Search { list, .. } | View::Threads { list } => {
                    list.mark_dirty()
                }
                _ => {}
            }
        }
    }

    /// A local copy of a file: the upload directory of the archive holding
    /// the file's conversation first, then the cache.
    fn local_file(&self, f: &crate::archive::FileInfo, full: bool) -> Option<PathBuf> {
        if let Some(idx) = self.corpus.conv_by_channel(&f.channel) {
            let conv = &self.corpus.convs[idx];
            if !conv.live_only {
                for source in &self.corpus.archives[conv.archive].source_dirs {
                    let dir = source.join("__uploads").join(&f.id);
                    if let Ok(rd) = std::fs::read_dir(&dir) {
                        if let Some(entry) = rd.flatten().find(|entry| entry.path().is_file()) {
                            return Some(entry.path());
                        }
                    }
                }
            }
        }
        let name = if full {
            format!("{}.{}", f.id, f.ext())
        } else {
            format!("{}.thumb.{}", f.id, f.ext())
        };
        let cached = self.cache_dir.join("files").join(name);
        cached.is_file().then_some(cached)
    }

    fn release_picture(&mut self, key: &str) {
        self.images.remove(key);
        if self.file_job.as_ref().is_some_and(|job| matches!(&job.kind, JobKind::File { id } if id == key)) {
            self.file_job = None;
        }
    }

    /// Have a file's pixels ready or on their way. `full` wants the original.
    /// A download waits in the queue until a sign-in provides the client, so
    /// an image first seen before sign-in still arrives.
    pub fn ensure_image(&mut self, f: &crate::archive::FileInfo, full: bool) {
        let key = if full {
            format!("{}:full", f.id)
        } else {
            f.id.clone()
        };
        if self.images.contains_key(&key) {
            return;
        }
        if let Some(path) = self.local_file(f, full) {
            let state = match image::open(&path) {
                Ok(img) => ImageState::Ready(img),
                Err(e) => ImageState::Failed(format!("{e}")),
            };
            self.images.insert(key, state);
            return;
        }
        let url = if full {
            f.url.clone()
        } else {
            f.thumb.clone().or_else(|| f.url.clone())
        };
        let state = match url {
            Some(url) => {
                let name = if full {
                    format!("{}.{}", f.id, f.ext())
                } else {
                    format!("{}.thumb.{}", f.id, f.ext())
                };
                ImageState::Queued {
                    url,
                    dest: self.cache_dir.join("files").join(name),
                }
            }
            None => ImageState::Failed("no download URL".to_string()),
        };
        self.images.insert(key, state);
    }

    /// Start the next queued download when the slot is free.
    fn pump_files(&mut self) {
        if self.file_job.is_some() {
            return;
        }
        let next = self.images.iter().find_map(|(k, s)| match s {
            ImageState::Queued { url, dest } if self.api.is_some() => Some((k.clone(), url.clone(), dest.clone())),
            _ => None,
        });
        if let Some((key, url, dest)) = next {
            self.images.insert(key.clone(), ImageState::Loading);
            self.file_job = Some(live::fetch_file(self.api.clone().expect("file client"), key, url, dest));
        }
    }

    /// The inline encoding of a thumbnail for a cell box, cached by size.
    pub fn inline_protocol(&mut self, id: &str, cols: u16, rows: u16) -> Option<&Protocol> {
        self.message_protocol(id, cols, rows, false)
    }

    pub fn message_protocol(&mut self, id: &str, cols: u16, rows: u16, grayscale: bool) -> Option<&Protocol> {
        let encoding_key = format!("{id}:{cols}x{rows}{}", if grayscale { ":gray" } else { "" });
        let fresh = matches!(self.inline.get(&encoding_key), Some((c, r, _)) if *c == cols && *r == rows);
        if !fresh {
            let picker = self.picker.as_ref()?;
            let img = match self.images.get(id) {
                Some(ImageState::Ready(img)) => img,
                _ => return None,
            };
            let size = ratatui::layout::Size::new(cols, rows);
            let proto = picker
                .new_protocol(if grayscale { img.grayscale() } else { img.clone() }, size, ratatui_image::Resize::Fit(None))
                .ok()?;
            self.inline.insert(encoding_key.clone(), (cols, rows, proto));
        }
        self.inline.get(&encoding_key).map(|(_, _, p)| p)
    }

    /// What a mark applies to: the highlighted conversation from the list,
    /// else the conversation of the selected message, which in a search or
    /// threads view is not necessarily the open one.
    fn mark_target(&self) -> Result<(usize, Option<Msg>), String> {
        match self.focus {
            Focus::Convs => {
                let idx = *self
                    .filtered
                    .get(self.conv_cursor)
                    .ok_or("nothing highlighted")?;
                Ok((idx, None))
            }
            Focus::Msgs => {
                let m = self.selected().cloned().ok_or("no message selected")?;
                let idx = self.corpus.conv_by_channel(&m.channel_id).ok_or_else(|| {
                    format!("{} is not a conversation this tool knows", m.channel_id)
                })?;
                Ok((idx, Some(m)))
            }
        }
    }

    fn mark_with(&mut self, idx: usize, id: i64) {
        let Some(c) = self.api.clone() else {
            self.status = "marking needs the Slack sign-in".to_string();
            return;
        };
        if self.job.is_some() {
            self.status = "a fetch is already running".to_string();
            return;
        }
        if id <= 0 {
            self.status =
                "nothing to mark: no message of that conversation is known yet".to_string();
            return;
        }
        let cid = self.corpus.convs[idx].id.clone();
        self.job = Some(live::api_mark(c, idx, cid, id));
    }

    /// `m`: the read marker moves to the newest message known of the target
    /// conversation: the newest loaded when it is the open one, else the
    /// newest the archive or Slack's counts reported.
    fn mark_read(&mut self) {
        let (idx, _) = match self.mark_target() {
            Ok(t) => t,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let newest_loaded = self
            .open
            .as_ref()
            .filter(|o| o.conv == idx)
            .and_then(|o| o.list.msgs.last().map(|m| m.id));
        let id = newest_loaded.unwrap_or(self.corpus.convs[idx].last_id);
        self.mark_with(idx, id);
    }

    /// `M`: the marker moves to the message before the one under the cursor,
    /// so that one and everything after it read as unread. When the previous
    /// message is not loaded, the timestamp one microsecond earlier stands in;
    /// from the list, the conversation's newest known message becomes unread.
    fn mark_unread(&mut self) {
        let (idx, sel) = match self.mark_target() {
            Ok(t) => t,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let id = match sel {
            Some(m) => {
                let previous = self.active_list().and_then(|l| {
                    let at = l.msgs.iter().position(|x| x.id == m.id)?;
                    l.msgs[..at]
                        .iter()
                        .rev()
                        .find(|x| x.channel_id == m.channel_id)
                        .map(|x| x.id)
                });
                previous.unwrap_or(m.id - 1)
            }
            None => self.corpus.convs[idx].last_id - 1,
        };
        self.mark_with(idx, id);
    }

    /// Esc in the list: the home view, with no filter and nothing open.
    fn escape_home(&mut self) {
        let at_home = self.focus == Focus::Convs && self.open.is_none() && self.stack.is_empty()
            && self.pane_menu.is_none() && !self.help && self.filter.is_empty()
            && matches!(self.mode, Mode::Normal)
            && !self.channel_browser.as_ref().is_some_and(|browser| browser.visible);
        if matches!(self.mode, Mode::Prompt { .. }) {
            // Preserve compose drafts and perform the normal prompt cancellation.
            self.on_prompt_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        if let Some(browser) = &mut self.channel_browser { browser.hide(); }
        if let Some(job) = &mut self.job { job.navigate_on_completion = false; }
        for view in self.stack.iter().rev() {
            match view {
                View::Keys { original, .. } => self.keymap = original.clone(),
                View::ColorPalette { original, .. } => self.palette = original.clone(),
                _ => {}
            }
        }
        self.mark_all_dirty();
        self.pane_menu = None;
        self.help = false;
        self.pending_delete = None;
        let image_keys: Vec<String> = self.stack.iter().filter_map(|view| match view {
            View::Image { files, .. } => Some(files.iter().map(|file| format!("{}:full", file.id))),
            _ => None,
        }).flatten().collect();
        for key in image_keys { self.release_picture(&key); }
        self.go_home();
        if at_home { self.conv_cursor = 0; self.conv_offset = 0; }
    }

    fn go_home(&mut self) {
        if !self.filter.is_empty() {
            self.filter.clear();
            self.apply_filter();
        }
        self.open = None;
        self.stack.clear();
        self.status.clear();
        self.focus = Focus::Convs;
    }

    /// `i`: the selected message's images, full pane.
    fn open_images(&mut self) {
        let Some(m) = self.selected() else {
            return;
        };
        let files: Vec<crate::archive::FileInfo> =
            m.files().into_iter().filter(|f| f.is_image()).collect();
        if files.is_empty() {
            self.status = "no image on this message".to_string();
            return;
        }
        if self.picker.is_none() {
            self.status = "images are off (--no-images)".to_string();
            return;
        }
        self.stack.push(View::Image {
            files,
            index: 0,
            zoom: 100,
            shown: None,
        });
    }

    fn prompt_archive(&mut self) {
        // A hit from a conversation not cached yet: offer its id.
        let prefill = self
            .selected()
            .filter(|m| self.corpus.conv_by_channel(&m.channel_id).is_none())
            .map(|m| m.channel_id.clone())
            .unwrap_or_default();
        self.mode = Mode::Prompt {
            kind: PromptKind::Archive,
            buf: Editor::with(prefill.clone()),
            previous: prefill,
        };
    }

    fn archive_new(&mut self, spec: &str) {
        let spec = spec.trim().to_string();
        if spec.is_empty() {
            return;
        }
        if !self.live {
            self.status = "live fetch is off (--no-live, or no slackdump)".to_string();
            return;
        }
        if self.job.is_some() {
            self.status = "a fetch is already running".to_string();
            return;
        }
        if !self.slackdump {
            self.status = "archiving a conversation needs slackdump on PATH".to_string();
            return;
        }
        self.job = Some(live::archive_new(&self.corpus.root, spec, 90));
    }

    /// Name a freshly written archive after its conversation, as the
    /// refresh scripts expect, and open it.
    fn finish_archive(&mut self, dir: &Path, spec: &str, navigate: bool) {
        let name = match Archive::open("new".to_string(), dir) {
            Ok(mut a) => a
                .scan_convs(
                    usize::MAX,
                    self.corpus.me.as_deref(),
                    self.corpus.half_life_days,
                    None,
                )
                .unwrap_or_default()
                .first()
                .map(|c| c.name.trim_start_matches(['#', '@']).to_string())
                .filter(|s| !s.is_empty()),
            Err(e) => {
                self.status = format!("{e}");
                return;
            }
        };
        let Some(name) = name else {
            let _ = std::fs::remove_dir_all(dir);
            self.status = format!("slackdump wrote no conversation for {spec}");
            return;
        };
        let slug: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let stamp = chrono::Utc::now().format("%Y%m%d");
        let final_dir = self
            .corpus
            .root
            .join("full")
            .join(format!("{slug}_{stamp}"));
        if final_dir.exists() {
            let _ = std::fs::remove_dir_all(dir);
            self.status = format!("already archived: {}", final_dir.display());
            return;
        }
        if let Err(e) = std::fs::rename(dir, &final_dir) {
            self.status = format!("{e}");
            return;
        }
        match self.corpus.add_archive(&final_dir) {
            Ok(new) => {
                self.apply_filter();
                if let Some(&idx) = new.first().filter(|_| navigate) {
                    self.conv_cursor = self.filtered.iter().position(|&i| i == idx).unwrap_or(0);
                    self.open_conv(idx);
                }
                self.status = format!("archived {name} into {}", final_dir.display());
            }
            Err(e) => self.status = e,
        }
    }

    fn take_profiles(&mut self, users: Vec<Value>) {
        self.corpus.merge_profiles(users);
        let names: Vec<_> = self
            .corpus
            .convs
            .iter()
            .enumerate()
            .filter_map(|(idx, conv)| {
                let uid = self.dm_users.get(&conv.id)?;
                let name = if self.corpus.me.as_deref() == Some(uid) {
                    "@me (self)".to_string()
                } else {
                    format!(
                        "@{}",
                        self.corpus.user_name(uid).unwrap_or_else(|| uid.clone())
                    )
                };
                Some((idx, name))
            })
            .collect();
        for (idx, name) in names {
            self.corpus.convs[idx].name = name;
        }
        self.mark_all_dirty();
        self.apply_filter();
        self.update_notes();
    }

    fn take_usergroups(&mut self, groups: Vec<Value>) {
        self.corpus.usergroups = groups
            .iter()
            .filter_map(|v| {
                Some((
                    v["id"].as_str()?.to_string(),
                    v["name"].as_str()?.to_string(),
                ))
            })
            .collect();
        self.mark_all_dirty();
    }

    /// Advance the spinner and collect a finished job.
    pub fn tick(&mut self) {
        if let Some(browser) = &mut self.channel_browser { browser.tick(); }
        if let Some(location) = self.channel_browser.as_mut().filter(|browser| browser.visible).and_then(|browser| browser.message.take()) {
            self.open_file_message(location);
        }
        self.spinner = self.spinner.wrapping_add(1);
        if let Some(outcome) = self.group_job.as_ref().and_then(|j| j.poll()) {
            self.group_job = None;
            match outcome {
                Ok(Done::Usergroups(groups, warning)) => {
                    self.take_usergroups(groups);
                    if let Some(warning) = warning {
                        self.status = warning;
                    }
                }
                Err(e) => self.status = format!("user groups: {e}"),
                _ => {}
            }
        }
        if let Some(outcome) = self.profile_job.as_ref().and_then(|j| j.poll()) {
            self.profile_job = None;
            match outcome {
                Ok(Done::Profiles(users, warning)) => {
                    self.take_profiles(users);
                    if let Some(warning) = warning {
                        self.status = warning;
                    }
                }
                Err(e) => self.status = format!("user profiles: {e}"),
                _ => {}
            }
        }
        // The file slot: one download at a time, decoded on arrival.
        if let Some(outcome) = self.file_job.as_ref().and_then(|j| j.poll()) {
            let job = self.file_job.take().expect("polled");
            if let JobKind::File { id } = job.kind {
                let state = match outcome {
                    Ok(Done::File(path)) => match image::open(&path) {
                        Ok(img) => ImageState::Ready(img),
                        Err(e) => ImageState::Failed(format!("{e}")),
                    },
                    Ok(_) => ImageState::Failed("unexpected result".to_string()),
                    Err(e) => ImageState::Failed(e),
                };
                self.images.insert(id, state);
            }
        }
        self.pump_files();
        // Count lookups must not hold up message tails or ordinary unread markers.
        if let Some(outcome) = self.unread_count_job.as_ref().and_then(|job| job.poll()) {
            let job = self.unread_count_job.take().expect("polled");
            if matches!(job.kind, JobKind::Counts { gen } if gen == self.counts_gen) {
                if let Ok(Done::Counts(snapshot)) = outcome {
                    self.apply_unread_counts(&snapshot);
                }
            }
        }
        // The quiet slot: sign-in, the conversation list, counts, tails.
        if let Some(outcome) = self.bg.as_ref().and_then(|j| j.poll()) {
            let job = self.bg.take().expect("polled");
            match outcome {
                Ok(Done::Auth(client, who)) => {
                    self.api = Some(client.clone());
                    self.profile_job =
                        Some(live::api_profiles(client.clone(), self.cache_dir.clone()));
                    self.group_job =
                        Some(live::api_usergroups(client.clone(), self.cache_dir.clone()));
                    self.status = format!("signed in as {who}");
                    self.bg = Some(live::api_conversations(client));
                }
                Ok(Done::Conversations(list)) => {
                    self.merge_conversations(list);
                    if let Some(c) = self.api.clone() {
                        self.bg = Some(live::api_counts(c, self.counts_gen));
                        self.muted_pending = true;
                        self.starred_pending = true;
                    }
                }
                Ok(Done::Counts(v)) => {
                    // A counts snapshot taken before a mark would undo it.
                    if matches!(job.kind, JobKind::Counts { gen } if gen == self.counts_gen) {
                        self.apply_counts(&v);
                        let targets = self.unread_count_targets();
                        if self.unread_count_job.is_none() && !targets.is_empty() {
                            if let Some(client) = self.api.clone() {
                                self.unread_count_job = Some(live::api_unread_counts(client, self.counts_gen, v, targets));
                            }
                        }
                    }
                    self.last_counts = Instant::now();
                    self.muted_pending = true;
                    self.starred_pending = true;
                }
                Ok(Done::StarredChannels(ids)) => {
                    if let JobKind::StarredChannels { gen } = job.kind { self.take_starred_snapshot(gen, ids); }
                }
                Ok(Done::StarChanged { cid, starred, ids }) => self.finish_star(&cid, starred, ids),
                Ok(Done::MutedChannels(ids)) => {
                    if let JobKind::MutedChannels { gen } = job.kind {
                        self.take_muted_snapshot(gen, ids);
                    }
                }
                Ok(Done::MuteChanged { cid, muted, ids }) => self.finish_mute(&cid, muted, ids),
                Ok(Done::ThreadMsgs(msgs)) => {
                    if let JobKind::Thread { root, .. } = job.kind {
                        self.extend_thread(msgs, root);
                    }
                }
                Ok(Done::Messages(msgs)) => {
                    if let JobKind::Tail { conv } = job.kind {
                        self.append_tail(conv, msgs, true);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    self.status = match job.kind {
                        JobKind::Auth => format!("not signed in: {e}"),
                        _ => e,
                    };
                }
            }
        }
        self.pump_requested_counts();
        if let Some(c) = self.api.clone() {
            if self.starred_pending && self.bg.is_none()
                && !matches!(self.job.as_ref().map(|job| &job.kind), Some(JobKind::SetStarred)) {
                self.starred_pending = false;
                self.bg = Some(live::api_starred_channels(c.clone(), self.starred_generation));
            }
            if self.muted_pending
                && self.bg.is_none()
                && !matches!(self.job.as_ref().map(|j| &j.kind), Some(JobKind::SetMuted))
            {
                self.muted_pending = false;
                self.bg = Some(live::api_muted_channels(c.clone(), self.muted_generation));
            }
            if let (Some((conv, thread)), true) = (self.tail_pending, self.bg.is_none()) {
                let open_here = self.open.as_ref().map(|o| o.conv) == Some(conv);
                let open_root = match self.stack.last() {
                    Some(View::Thread { root, .. }) => Some(*root),
                    _ => None,
                };
                let cid = self.corpus.convs[conv].id.clone();
                match (thread, open_root) {
                    // A reply lands in the thread pane, which history cannot
                    // fill; the timeline behind it waits for the next poll.
                    (Some(root), Some(open)) if open == root => {
                        self.tail_pending = None;
                        self.bg = Some(live::api_thread(
                            c.clone(),
                            &self.cache_dir,
                            cid,
                            root,
                            root,
                        ));
                    }
                    (None, _) if open_here => {
                        self.tail_pending = None;
                        let since = self
                            .open
                            .as_ref()
                            .and_then(|o| o.list.msgs.last().map(|m| m.id))
                            .unwrap_or(0);
                        self.bg = Some(live::api_tail(c.clone(), conv, cid, since, true));
                    }
                    // Nothing on screen wants it: drop the errand.
                    _ => self.tail_pending = None,
                }
            }
            if self.bg.is_none() && self.poll_every.as_secs() > 0 {
                if self.last_poll.elapsed() >= self.poll_every {
                    self.last_poll = Instant::now();
                    // An open thread is what the reader is looking at; the
                    // timeline behind it waits for the next tick.
                    let open_thread = match self.stack.last() {
                        Some(View::Thread { root, list, .. }) => list
                            .msgs
                            .first()
                            .map(|m| m.channel_id.clone())
                            .or_else(|| {
                                self.open
                                    .as_ref()
                                    .map(|o| self.corpus.convs[o.conv].id.clone())
                            })
                            .map(|cid| {
                                (cid, *root, list.selected().map(|m| m.id).unwrap_or(*root))
                            }),
                        _ => None,
                    };
                    if let Some((cid, root, focus)) = open_thread {
                        self.bg = Some(live::api_thread(
                            c.clone(),
                            &self.cache_dir,
                            cid,
                            root,
                            focus,
                        ));
                    } else if let Some(o) = self.open.as_ref() {
                        let since = o.list.msgs.last().map(|m| m.id).unwrap_or(0);
                        if !o.has_newer && since > 0 {
                            let idx = o.conv;
                            let cid = self.corpus.convs[idx].id.clone();
                            self.bg = Some(live::api_tail(c.clone(), idx, cid, since, true));
                        }
                    }
                } else if self.last_counts.elapsed() >= self.poll_every * 2 {
                    self.last_counts = Instant::now();
                    self.bg = Some(live::api_counts(c, self.counts_gen));
                }
            }
        }
        // The user's slot.
        let Some(outcome) = self.job.as_ref().and_then(|j| j.poll()) else {
            return;
        };
        let job = self.job.take().expect("a job was polled");
        if let JobKind::MessageLink { raw_id, .. } = &job.kind {
            if !matches!(self.stack.last(), Some(View::Raw { browser, .. }) if browser.id == *raw_id) { return; }
        }
        let done = match outcome {
            Ok(d) => d,
            Err(e) => {
                self.status = format!("Slack: {e}");
                if matches!(job.kind, JobKind::SetStarred) {
                    self.starred_generation = self.starred_generation.wrapping_add(1);
                    self.starred_pending = true;
                }
                if matches!(job.kind, JobKind::SetMuted) {
                    self.muted_generation = self.muted_generation.wrapping_add(1);
                    self.muted_pending = true;
                }
                if let JobKind::Upload { .. } = &job.kind {
                    // The file left the prompt when the send started; say so,
                    // rather than let the next conversation inherit it.
                    self.status = format!("Slack: {e}; the file is not attached any more");
                }
                if let JobKind::Search { query } = &job.kind {
                    if let Some(View::Search {
                        query: q,
                        live_pending,
                        ..
                    }) = self.stack.last_mut()
                    {
                        if q == query {
                            *live_pending = false;
                        }
                    }
                }
                return;
            }
        };
        match (job.kind, done) {
            (_, Done::StarChanged { cid, starred, ids }) => self.finish_star(&cid, starred, ids),
            (JobKind::StarredChannels { gen }, Done::StarredChannels(ids)) => self.take_starred_snapshot(gen, ids),
            (_, Done::MuteChanged { cid, muted, ids }) => self.finish_mute(&cid, muted, ids),
            (JobKind::MutedChannels { gen }, Done::MutedChannels(ids)) => {
                self.take_muted_snapshot(gen, ids)
            }
            (JobKind::Refresh { conv, before }, Done::Refreshed) => {
                self.refresh_conv_stats(conv);
                if job.navigate_on_completion { self.open_conv(conv); }
                let conversation = &self.corpus.convs[conv];
                let new = self.corpus.archives[conversation.archive]
                    .timeline_count(&conversation.id).unwrap_or(before) - before;
                self.status = format!(
                    "refreshed: {new} new top-level message{}",
                    if new == 1 { "" } else { "s" }
                );
            }
            (JobKind::MessageLink { raw_id, link }, Done::ThreadMsgs(messages)) => {
                self.show_linked_message(raw_id, &link, messages);
            }
            (JobKind::Thread { cid, root, focus }, Done::Thread(dir)) => {
                if matches!(self.stack.last(), Some(View::Thread { root: r, .. }) if *r == root) {
                    self.apply_thread_dir(&dir, &cid, root, focus);
                } else {
                    self.status = "thread fetched into the cache".to_string();
                }
            }
            (JobKind::Thread { root, focus, .. }, Done::ThreadMsgs(msgs)) => {
                if matches!(self.stack.last(), Some(View::Thread { root: r, .. }) if *r == root) {
                    self.apply_thread_msgs(msgs, root, focus, "Slack");
                } else {
                    self.status = "thread fetched into the cache".to_string();
                }
            }
            (JobKind::Search { query }, Done::Search(dir)) => self.merge_live_search(&query, &dir),
            (JobKind::Search { query }, Done::SearchHits(hits)) => self.merge_hits(&query, hits),
            (JobKind::ArchiveNew { spec }, Done::Archived(dir)) => self.finish_archive(&dir, &spec, job.navigate_on_completion),
            (JobKind::Tail { conv }, Done::Messages(msgs)) => self.append_tail(conv, msgs, false),
            (JobKind::Older { conv }, Done::OlderMessages(msgs, more)) => self.prepend_older(conv, msgs, more),
            (JobKind::Newer { conv }, Done::NewerMessages(msgs, more)) => {
                if let Some(open) = self.open.as_mut().filter(|open| open.conv == conv && open.api_only) {
                    open.has_newer = more;
                    let known: HashSet<i64> = open.list.msgs.iter().map(|message| message.id).collect();
                    open.list.msgs.extend(msgs.into_iter().filter(|message| !known.contains(&message.id)));
                    open.list.mark_dirty();
                }
                self.update_notes();
            }
            (JobKind::Mark { conv, id }, Done::Marked) => {
                let c = &mut self.corpus.convs[conv];
                c.last_read = id;
                c.unread = c.last_id > id;
                c.unread_count = None;
                c.unread_snapshot = None;
                self.counts_gen += 1;
                if !c.unread {
                    c.mentions = 0;
                }
                let name = c.name.clone();
                let what = if c.unread {
                    "marked unread"
                } else {
                    "marked read"
                };
                let at = self.conv_cursor;
                self.apply_filter();
                if self.focus == Focus::Convs && self.unreads_first {
                    // Triage from the list: the cursor stays put, on the next unread.
                    self.conv_cursor = at.min(self.filtered.len().saturating_sub(1));
                }
                self.mark_all_dirty();
                self.status = format!("{name} {what}");
            }
            (JobKind::Upload { conv, thread }, Done::Uploaded(name)) => {
                self.draft = None;
                self.attachment = None;
                let cname = self.corpus.convs[conv].name.clone();
                self.status = format!("{name} sent to {cname}");
                // Slack builds the message around the file, so it has to be
                // fetched; the background slot may still be busy.
                self.tail_pending = Some((conv, thread));
            }
            (JobKind::Send { conv, thread }, Done::Sent(msg)) => {
                let msg = *msg;
                self.draft = None;
                let name = self.corpus.convs[conv].name.clone();
                let c = &mut self.corpus.convs[conv];
                if msg.id > c.last_id {
                    c.last_id = msg.id;
                    c.unread_count = None;
                    c.unread_snapshot = None;
                }
                match thread {
                    Some(root) => {
                        if let Some(View::Thread { root: r, list, .. }) = self.stack.last_mut() {
                            if *r == root && !list.msgs.iter().any(|m| m.id == msg.id) {
                                list.msgs.push(msg);
                                list.cursor = list.len() - 1;
                                list.mark_dirty();
                            }
                        }
                        self.status = format!("reply sent in {name}");
                    }
                    None => {
                        self.append_tail(conv, vec![msg], true);
                        self.status = format!("sent to {name}");
                    }
                }
            }
            (JobKind::Delete { id }, Done::Deleted) => {
                self.drop_message(id);
                self.status = "message deleted".to_string();
            }
            (JobKind::Leave { conv }, Done::Left) => {
                let c = &mut self.corpus.convs[conv];
                c.left = true;
                c.unread = false;
                c.unread_count = None;
                c.unread_snapshot = None;
                c.mentions = 0;
                let name = c.name.clone();
                self.apply_filter();
                self.mark_all_dirty();
                self.status = format!("left {name}");
            }
            _ => {}
        }
    }

    pub fn open_raw(&mut self) {
        let Some(m) = self.active_list().and_then(|l| l.selected()).cloned() else {
            return;
        };
        let place = self.corpus.channel_names.get(&m.channel_id).map(|name| format!("#{name}"))
            .unwrap_or_else(|| m.channel_id.clone());
        let title = format!("raw · {} · {place}", m.ts);
        self.stack.push(View::Raw { title, browser: crate::raw::Browser::new(&m.data) });
    }

    fn goto_date(&mut self, text: &str) {
        let Ok(date) = chrono::NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d") else {
            self.status = format!("not a date: '{}' (want YYYY-MM-DD)", text.trim());
            return;
        };
        let Some(secs) = self.tz.midnight(date) else {
            return;
        };
        let Some(o) = self.open.as_ref() else { return };
        let conv = &self.corpus.convs[o.conv];
        let id = (secs * 1_000_000).clamp(conv.first_id, conv.last_id + 1);
        self.stack.clear();
        self.jump_to(id);
        self.status = format!("{}", date);
    }

    fn reload(&mut self) {
        if let Some(o) = self.open.as_ref() {
            let idx = o.conv;
            self.open_conv(idx);
            self.status = "reloaded".to_string();
        }
    }

    // ---------------------------------------------------------------- views

    pub fn active_list(&self) -> Option<&MsgList> {
        match self.stack.iter().rev().find(|v| {
            !matches!(
                v,
                View::Raw { .. }
                    | View::Image { .. }
                    | View::Reactions { .. }
                    | View::ColorPalette { .. }
            )
        }) {
            Some(View::Thread { list, .. })
            | Some(View::Search { list, .. })
            | Some(View::Threads { list }) => Some(list),
            _ => self.open.as_ref().map(|o| &o.list),
        }
    }

    pub fn active_list_mut(&mut self) -> Option<&mut MsgList> {
        match self.stack.iter_mut().rev().find(|v| {
            !matches!(
                v,
                View::Raw { .. }
                    | View::Image { .. }
                    | View::Reactions { .. }
                    | View::ColorPalette { .. }
            )
        }) {
            Some(View::Thread { list, .. })
            | Some(View::Search { list, .. })
            | Some(View::Threads { list }) => Some(list),
            _ => self.open.as_mut().map(|o| &mut o.list),
        }
    }

    pub fn in_timeline(&self) -> bool {
        self.stack.is_empty()
    }

    pub fn selected(&self) -> Option<&Msg> {
        self.active_list().and_then(|l| l.selected())
    }

    /// Rows the bottom line needs: one, or the lines of an open prompt.
    pub fn prompt_rows(&self) -> u16 {
        match &self.mode {
            Mode::Prompt { buf, .. } => (buf.text.matches('\n').count() as u16 + 1).min(8),
            Mode::Normal => 1,
        }
    }

    /// Title of the messages pane.
    pub fn title(&self) -> String {
        if matches!(self.stack.last(), Some(View::Keys { .. })) {
            return "keys · what each action answers to".to_string();
        }
        if matches!(self.stack.last(), Some(View::ColorPalette { .. })) {
            return "color palette · live preview".to_string();
        }
        let Some(o) = self.open.as_ref() else {
            return "messages".to_string();
        };
        let conv = &self.corpus.convs[o.conv];
        match self.stack.last() {
            Some(View::ColorPalette { .. }) | Some(View::Keys { .. }) => {
                unreachable!("handled before opening a conversation")
            }
            Some(View::Raw { title, .. }) => title.clone(),
            Some(View::Reactions { title, .. }) => title.clone(),
            Some(View::Image {
                files, index, zoom, ..
            }) => {
                let f = &files[*index];
                format!(
                    "image {}/{} · {} · {}x{} · {} · {zoom}% · +/- zoom · 0 fit",
                    index + 1,
                    files.len(),
                    f.name,
                    f.width,
                    f.height,
                    self.picker
                        .as_ref()
                        .map(|p| format!("{:?}", p.protocol_type()).to_lowercase())
                        .unwrap_or_default()
                )
            }
            Some(View::Threads { list }) => format!(
                "threads you took part in · {} · newest reply first",
                list.len()
            ),
            Some(View::Thread {
                list, live, place, ..
            }) => {
                let n = list.len().saturating_sub(1);
                let place = match (live, list.msgs.first()) {
                    _ if place.is_some() => place.clone().unwrap_or_default(),
                    (Some(a), Some(m)) => a
                        .channel_name(&m.channel_id)
                        .map(|c| format!("#{c}"))
                        .unwrap_or_else(|| conv.name.clone()),
                    _ => conv.name.clone(),
                };
                let from = if live.is_some()
                    || list.msgs.first().is_some_and(|m| m.channel_name.is_some())
                {
                    " · from Slack"
                } else {
                    ""
                };
                if n == 0 {
                    format!("message in {place} · no replies{from}")
                } else {
                    format!(
                        "thread in {place} · {n} {}{from}",
                        if n == 1 { "reply" } else { "replies" }
                    )
                }
            }
            Some(View::Search {
                query,
                list,
                capped,
                live_hits,
                live_pending,
            }) => {
                let live = if *live_pending {
                    " · searching Slack".to_string()
                } else {
                    live_hits
                        .map(|n| format!(" · {n} more from Slack"))
                        .unwrap_or_default()
                };
                format!(
                    "search '{query}' · {} hit{}{}{live}",
                    list.len(),
                    if list.len() == 1 { "" } else { "s" },
                    if *capped { " (capped)" } else { "" }
                )
            }
            None if o.api_only => format!(
                "{} · {} · {} loaded · live from Slack, not cached",
                conv.name,
                conv.kind.label(),
                o.list.len()
            ),
            None => {
                let span = format!(
                    "{} → {}",
                    self.tz.fmt(conv.first_id / 1_000_000, "%Y-%m-%d"),
                    self.tz.fmt(conv.last_id / 1_000_000, "%Y-%m-%d")
                );
                format!(
                    "{} · {} · {} messages, {} loaded · {span} · {}",
                    conv.name,
                    conv.kind.label(),
                    o.total,
                    o.list.len(),
                    self.corpus
                        .conv_archive(conv)
                        .map(|a| a.rel.as_str())
                        .unwrap_or("not cached")
                )
            }
        }
    }

    fn open_conversations_pane(&mut self) {
        self.sync_muted();
        self.pane_menu = Some(crate::conversations_pane::Menu::new(
            self.pane_settings.clone(),
            &self.corpus.convs,
        ));
        self.status.clear();
    }

    fn on_conversations_pane_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Esc {
            self.pane_menu = None;
            self.status.clear();
            return;
        }
        if self.pane_menu.as_ref().unwrap().cursor == 0 {
            let menu = self.pane_menu.as_mut().unwrap();
            match k.code {
                KeyCode::Enter | KeyCode::Tab | KeyCode::Down => menu.cursor = 1,
                KeyCode::Backspace => { menu.query.pop(); },
                KeyCode::Char('u') if ctrl(k) => menu.query.clear(),
                KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    menu.query.push(c);
                }
                KeyCode::PageDown => menu.cursor = 10.min(menu.rows().len() - 1),
                KeyCode::End => menu.cursor = menu.rows().len() - 1,
                _ => {},
            }
            return;
        }
        if k.code == KeyCode::Enter {
            let settings = self.pane_menu.as_ref().unwrap().settings.clone();
            if let Err(e) = settings.save(self.pane_path.as_deref(), &self.pane_workspace) {
                self.status = format!("conversations-pane: cannot save: {e}");
                return;
            }
            self.pane_settings = settings;
            self.pane_menu = None;
            self.apply_filter();
            if self
                .open
                .as_ref()
                .is_some_and(|o| !self.filtered.contains(&o.conv))
            {
                self.open = None;
                self.stack.clear();
                self.focus = Focus::Convs;
            }
            self.status = format!("conversations-pane: {} visible", self.filtered.len());
            self.mark_all_dirty();
            return;
        }
        let menu = self.pane_menu.as_mut().unwrap();
        let last = menu.rows().len().saturating_sub(1);
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => menu.cursor = menu.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => menu.cursor = (menu.cursor + 1).min(last),
            KeyCode::PageUp => menu.cursor = menu.cursor.saturating_sub(10),
            KeyCode::PageDown => menu.cursor = (menu.cursor + 10).min(last),
            KeyCode::Home => menu.cursor = 0,
            KeyCode::End => menu.cursor = last,
            KeyCode::Char('h') | KeyCode::Left => menu.set(false),
            KeyCode::Char('l') | KeyCode::Right => menu.set(true),
            KeyCode::Char(' ') => menu.toggle(),
            _ => {}
        }
    }

    // ----------------------------------------------------------------- keys

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Esc {
            if let Some(browser) = self.channel_browser.as_mut().filter(|browser| browser.visible && browser.escape_edits()) {
                browser.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            } else { self.escape_home(); }
            return;
        }
        if let Some(browser) = self.channel_browser.as_mut().filter(|b| b.visible) {
            let picture_key = browser.picture.as_ref().map(|picture| format!("{}:full", picture.file.id));
            if browser.can_toggle() && self.keymap.action(k)==Some(Action::ChannelTabs) {browser.hide();}
            else {browser.key(k);}
            if browser.picture.is_none() {
                if let Some(key) = picture_key { self.release_picture(&key); }
            }
            return;
        }

        if self.pane_menu.is_some() {
            self.on_conversations_pane_key(k);
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if matches!(self.mode, Mode::Prompt { .. }) {
            self.on_prompt_key(k);
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(View::Keys { .. }) = self.stack.last() {
            self.on_keys_key(k, ctrl);
            return;
        }
        if let Some(View::ColorPalette { .. }) = self.stack.last() {
            self.on_palette_key(k, ctrl);
            return;
        }
        if matches!(self.stack.last(), Some(View::Reactions { .. }))
            && !matches!(self.keymap.action(k), Some(Action::Quit | Action::Help)) {
            self.on_raw_key(k, ctrl);
            return;
        }
        let action = self.keymap.action(k);
        if action == Some(Action::ChannelTabs) {
            let index = if self.focus == Focus::Convs { self.filtered.get(self.conv_cursor).copied() }
                else { self.open.as_ref().map(|o| o.conv) };
            let same_channel = index.is_some_and(|index| self.channel_browser.as_ref().is_some_and(|b| b.channel == self.corpus.convs[index].id));
            if same_channel || self.channel_browser.as_ref().is_some_and(|b| b.busy_or_dirty()) {
                if let Some(browser) = &mut self.channel_browser { browser.visible = true; }
            } else if let (Some(client), Some(index)) = (self.api.clone(), index) {
                if self.focus == Focus::Convs { self.open_conv(index); }
                if let Some(key) = self.channel_browser.as_ref().and_then(|browser| browser.picture.as_ref())
                    .map(|picture| format!("{}:full", picture.file.id)) {
                    self.release_picture(&key);
                }
                let conv = &self.corpus.convs[index];
                self.channel_browser = Some(crate::canvas::Browser::new(client, conv.id.clone(), conv.name.clone()));
                self.focus = Focus::Msgs;
            } else { self.status = "Select a channel and sign in to Slack to read its tabs".into(); }
            return;
        }
        // An armed delete lives for exactly one more key, and only in the
        // pane that armed it.
        if self.pending_delete.is_some()
            && (action != Some(Action::Delete) || self.focus != Focus::Msgs)
        {
            self.pending_delete = None;
            self.status = "delete cancelled".to_string();
        }
        match action {
            Some(Action::Quit) => {
                if self.channel_browser.as_ref().is_some_and(|b| b.busy_or_dirty()) {
                    self.status = "Channel tabs have pending work or an unsaved draft; press T to return".into();
                    return;
                }
                self.quit = true;
                return;
            }
            Some(Action::Help) => {
                self.help = true;
                return;
            }
            _ => {}
        }
        if let Some(View::Raw { .. }) = self.stack.last() {
            self.on_json_key(k, ctrl);
            return;
        }
        if let Some(View::Image {
            files,
            index,
            shown,
            zoom,
        }) = self.stack.last_mut()
        {
            match k.code {
                KeyCode::Char('+' | '=') => {
                    *zoom = (*zoom + 25).min(800);
                    *shown = None;
                }
                KeyCode::Char('-' | '_') => {
                    *zoom = zoom.saturating_sub(25).max(25);
                    *shown = None;
                }
                KeyCode::Char('0') => {
                    *zoom = 100;
                    *shown = None;
                }
                KeyCode::Char('j')
                | KeyCode::Down
                | KeyCode::Char('l')
                | KeyCode::Right
                | KeyCode::Char('n') => {
                    if *index + 1 < files.len() {
                        *index += 1;
                        *zoom = 100;
                        *shown = None;
                    }
                }
                KeyCode::Char('k')
                | KeyCode::Up
                | KeyCode::Char('p') => {
                    if *index > 0 {
                        *index -= 1;
                        *zoom = 100;
                        *shown = None;
                    }
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('i' | 'h') | KeyCode::Left => {
                    // The originals are large; keep only thumbnails once the viewer closes.
                    if let Some(View::Image { files, .. }) = self.stack.pop() {
                        for f in files {
                            self.images.remove(&format!("{}:full", f.id));
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        if self.focus == Focus::Msgs
            && action == Some(Action::Open)
            && matches!(k.code, KeyCode::Char('l') | KeyCode::Right)
        {
            if !matches!(self.stack.last(), Some(View::Thread { .. }))
                && self.selected().is_some_and(|message| message.has_thread() || message.parent_id.is_some())
            {
                self.on_msg_key(Some(Action::Open));
            } else if self.active_list().is_some_and(|list|
                list.line_scroll || !list.collapsed.get(list.cursor).copied().unwrap_or(false)
            ) {
                self.open_raw();
            } else if let Some(list) = self.active_list_mut() {
                if list.selected().is_some() {
                    list.line_scroll = true;
                    list.scroll = list.first.get(list.cursor).copied().unwrap_or(0);
                    self.status = "read message · j/k or arrows: one line · PgUp/PgDn: page · l: raw · Enter: thread · h: back · Esc: home".into();
                }
            }
            return;
        }
        match self.focus {
            Focus::Convs => self.on_conv_key(action),
            Focus::Msgs => self.on_msg_key(action),
        }
    }

    fn on_json_key(&mut self, key: KeyEvent, control: bool) {
        let height = self.msgs_height.saturating_sub(2).max(1) as isize;
        if key.code == KeyCode::Enter && !control {
            self.follow_raw_link();
            return;
        }
        if matches!(key.code, KeyCode::Char('h') | KeyCode::Left) && !control {
            self.stack.pop();
            return;
        }
        let Some(View::Raw { browser, .. }) = self.stack.last_mut() else { return };
        match (key.code, control) {
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => browser.move_cursor(1),
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => browser.move_cursor(-1),
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => browser.move_cursor(isize::MIN),
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => browser.move_cursor(isize::MAX),
            (KeyCode::Char('d'), true) => browser.scroll_lines(height / 2),
            (KeyCode::Char('u'), true) => browser.scroll_lines(-height / 2),
            (KeyCode::Char('f'), true) | (KeyCode::PageDown, _) => browser.scroll_lines(height),
            (KeyCode::Char('b'), true) | (KeyCode::PageUp, _) => browser.scroll_lines(-height),
            _ => {}
        }
    }

    fn follow_raw_link(&mut self) {
        let Some(View::Raw { browser, .. }) = self.stack.last() else { return };
        let raw_id = browser.id;
        let Some(link) = browser.selected().and_then(|leaf| leaf.link.clone()) else {
            self.status = "Select a Slack message link with j/k, then Enter".into();
            return;
        };
        if !link.in_workspace(&self.corpus.workspace_url) {
            self.status = "This link belongs to another Slack workspace".into();
            return;
        }
        if self.job.is_some() {
            self.status = "A fetch is running; retry Enter when it finishes".into();
            return;
        }
        // Leave the source conversation and view stack intact for h to return to.
        let mut root = link.root.unwrap_or(link.focus);
        let archive = self.corpus.conv_by_channel(&link.channel)
            .and_then(|index| self.corpus.conv_archive(&self.corpus.convs[index]));
        let mut messages = archive.and_then(|archive| archive.thread(&link.channel, root).ok()).unwrap_or_default();
        if link.root.is_none() {
            if let Some(message) = messages.iter().find(|message| message.id == link.focus) {
                root = message.thread_root();
                if root != link.focus {
                    messages = archive.and_then(|archive| archive.thread(&link.channel, root).ok()).unwrap_or_default();
                }
            }
        }
        if !messages.iter().any(|message| message.id == link.focus) {
            messages = live::cached_thread(&self.cache_dir, &link.channel, root).unwrap_or_default();
        }
        if !messages.iter().any(|message| message.id == link.focus) && link.root.is_none() {
            messages = crate::raw::cached_reply(&self.cache_dir, &link.channel, link.focus).unwrap_or_default();
        }
        if messages.iter().any(|message| message.id == link.focus) {
            self.show_linked_message(raw_id, &link, messages);
        } else if let Some(client) = self.api.clone() {
            let mut link = link;
            if root != link.focus { link.root = Some(root); }
            self.job = Some(live::api_message_link(client, raw_id, link));
        } else {
            self.status = "Linked message is not cached; sign in to Slack to fetch it".into();
        }
    }

    fn show_linked_message(&mut self, raw_id: u64, link: &crate::raw::Link, messages: Vec<Msg>) {
        if !matches!(self.stack.last(), Some(View::Raw { browser, .. }) if browser.id == raw_id) { return; }
        let Some(cursor) = messages.iter().position(|message| message.id == link.focus && message.channel_id == link.channel) else {
            self.status = "Linked message is unavailable".into();
            return;
        };
        let root = messages[cursor].thread_root();
        let mut list = MsgList::new(messages, true);
        list.cursor = cursor;
        list.align_top = true;
        let place = self.corpus.channel_names.get(&link.channel).map(|name| format!("#{name}"))
            .unwrap_or_else(|| link.channel.clone());
        self.stack.push(View::Thread { root, list, live: None, place: Some(place) });
        self.status = "Opened linked Slack message · h: back".into();
    }

    fn on_raw_key(&mut self, k: KeyEvent, ctrl: bool) {
        let height = self.msgs_height.max(1);
        let Some(View::Reactions { lines, scroll, .. }) = self.stack.last_mut() else {
            return;
        };
        let max = lines.len().saturating_sub(height);
        match (k.code, ctrl) {
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => *scroll = (*scroll + 1).min(max),
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => *scroll = scroll.saturating_sub(1),
            (KeyCode::Char('d'), true) => *scroll = (*scroll + height / 2).min(max),
            (KeyCode::Char('u'), true) => *scroll = scroll.saturating_sub(height / 2),
            (KeyCode::Char('f'), true) | (KeyCode::PageDown, _) => {
                *scroll = (*scroll + height).min(max)
            }
            (KeyCode::Char('b'), true) | (KeyCode::PageUp, _) => {
                *scroll = scroll.saturating_sub(height)
            }
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => *scroll = 0,
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => *scroll = max,
            (KeyCode::Esc, _)
            | (KeyCode::Char('h'), false)
            | (KeyCode::Left, _)
            | (KeyCode::Enter, _) => {
                self.stack.pop();
            }
            _ => {}
        }
    }

    fn on_conv_key(&mut self, action: Option<Action>) {
        let n = self.filtered.len();
        let last = n.saturating_sub(1);
        match action {
            Some(Action::Down) => self.conv_cursor = (self.conv_cursor + 1).min(last),
            Some(Action::Up) => self.conv_cursor = self.conv_cursor.saturating_sub(1),
            Some(Action::First) => self.conv_cursor = 0,
            Some(Action::Last) => self.conv_cursor = last,
            Some(Action::HalfPageDown) => self.conv_cursor = (self.conv_cursor + 10).min(last),
            Some(Action::HalfPageUp) => self.conv_cursor = self.conv_cursor.saturating_sub(10),
            Some(Action::PageDown) => self.conv_cursor = (self.conv_cursor + 20).min(last),
            Some(Action::PageUp) => self.conv_cursor = self.conv_cursor.saturating_sub(20),
            Some(Action::Open) => {
                if let Some(&idx) = self.filtered.get(self.conv_cursor) {
                    if self.open.as_ref().map(|o| o.conv) == Some(idx) {
                        self.focus = Focus::Msgs;
                    } else {
                        self.open_conv(idx);
                    }
                }
            }
            Some(Action::OtherPane) => {
                if self.open.is_some() {
                    self.focus = Focus::Msgs;
                }
            }
            Some(Action::Command) => self.open_command(),
            Some(Action::Close) => self.escape_home(),
            Some(Action::Archive) => self.prompt_archive(),
            Some(Action::MyThreads) => self.open_my_threads(),
            Some(Action::UnreadsFirst) => self.toggle_unreads_first(),
            Some(Action::Compose) => self.compose(),
            Some(Action::React) => self.view_reactions(),
            Some(Action::MarkRead) => self.mark_read(),
            Some(Action::MarkUnread) => self.mark_unread(),
            Some(Action::Sort) => {
                self.sort = self.sort.next();
                self.apply_filter();
                // A new order is a new list: read it from the top rather than
                // chasing where the highlighted conversation landed.
                self.conv_cursor = 0;
                self.conv_offset = 0;
                self.status = format!("sorted by {}", self.sort_label());
                if self.sort == Sort::Mine && self.corpus.me.is_none() {
                    self.status =
                        "own user id unknown (no DM archive): set SLACK_SELF_USER_ID".to_string();
                }
            }
            Some(Action::ConversationsPane) => self.open_conversations_pane(),
            Some(Action::Keys) => self.open_keys(),
            _ => {}
        }
    }

    fn on_msg_key(&mut self, action: Option<Action>) {
        if self.open.is_none() {
            self.focus = Focus::Convs;
            return;
        }
        let height = self.msgs_height.max(2) as isize;
        let line_height = self.msgs_height.max(1) as isize;
        if let Some(list) = self.active_list_mut().filter(|l| l.line_scroll) {
            let height = line_height;
            let first = list.first.get(list.cursor).copied().unwrap_or(0);
            let last = list.last.get(list.cursor).copied().unwrap_or(first);
            let max = (last + 1).saturating_sub(height as usize).max(first);
            let delta = match action {
                Some(Action::Down) => Some(1),
                Some(Action::Up) => Some(-1),
                Some(Action::HalfPageDown) => Some(height / 2),
                Some(Action::HalfPageUp) => Some(-height / 2),
                Some(Action::PageDown) => Some(height - 1),
                Some(Action::PageUp) => Some(1 - height),
                Some(Action::First) => Some(-(list.scroll as isize)),
                Some(Action::Last) => Some(max as isize),
                _ => None,
            };
            if let Some(delta) = delta {
                list.scroll = list.scroll.saturating_add_signed(delta).clamp(first, max);
                return;
            }
        }
        let timeline = self.in_timeline();
        match action {
            Some(Action::Down) => {
                let at_end = self
                    .active_list()
                    .map(|l| l.cursor + 1 >= l.len())
                    .unwrap_or(true);
                if at_end && timeline {
                    self.load_newer();
                } else if let Some(l) = self.active_list_mut() {
                    l.move_cursor(1);
                }
            }
            Some(Action::Up) => {
                let at_start = self.active_list().map(|l| l.cursor == 0).unwrap_or(true);
                if at_start && timeline {
                    self.load_older();
                } else if let Some(l) = self.active_list_mut() {
                    l.move_cursor(-1);
                }
            }
            Some(Action::First) => {
                let has_older = self.open.as_ref().is_some_and(|o| o.has_older);
                if timeline && has_older {
                    let first = self.open_conv_ref().map(|c| c.first_id).unwrap_or(0);
                    self.jump_to(first);
                    if let Some(l) = self.active_list_mut() {
                        l.cursor = 0;
                    }
                } else if let Some(l) = self.active_list_mut() {
                    l.cursor = 0;
                }
            }
            Some(Action::Last) => {
                let has_newer = self.open.as_ref().is_some_and(|o| o.has_newer);
                if timeline && has_newer {
                    self.reload();
                } else if let Some(l) = self.active_list_mut() {
                    l.cursor = l.len().saturating_sub(1);
                }
            }
            Some(Action::HalfPageDown) => {
                if let Some(l) = self.active_list_mut() {
                    l.move_lines(height / 2);
                }
            }
            Some(Action::HalfPageUp) => {
                if let Some(l) = self.active_list_mut() {
                    l.move_lines(-(height / 2));
                }
            }
            Some(Action::PageDown) => {
                if let Some(l) = self.active_list_mut() {
                    l.move_lines(height - 1);
                }
            }
            Some(Action::PageUp) => {
                if let Some(l) = self.active_list_mut() {
                    l.move_lines(-(height - 1));
                }
            }
            Some(Action::Open) => {
                if matches!(self.stack.last(), Some(View::Thread { .. })) {
                    self.open_raw();
                } else if let Some(m) = self.selected() {
                    let (cid, root, id) = (m.channel_id.clone(), m.thread_root(), m.id);
                    self.open_hit(cid, root, id);
                }
            }
            Some(Action::RawJson) => self.open_raw(),
            Some(Action::ShowInChannel) => {
                let sel = self.selected().map(|m| {
                    (
                        m.channel_id.clone(),
                        m.thread_root(),
                        m.channel_name.clone(),
                    )
                });
                if let (false, Some((cid, root, name))) = (timeline, sel) {
                    match self.corpus.conv_by_channel(&cid) {
                        Some(idx) => {
                            self.stack.clear();
                            if self.open.as_ref().map(|o| o.conv) != Some(idx) {
                                self.open_conv(idx);
                            }
                            self.jump_to(root);
                        }
                        None => {
                            self.status =
                                format!("#{} is not archived; a archives it", name.unwrap_or(cid));
                        }
                    }
                }
            }
            Some(Action::Command) => self.open_command(),
            Some(Action::GoToDate) => {
                self.mode = Mode::Prompt {
                    kind: PromptKind::Date,
                    buf: Editor::default(),
                    previous: String::new(),
                };
            }
            Some(Action::Reload) => self.reload(),
            Some(Action::Refresh) => self.refresh(),
            Some(Action::Archive) => self.prompt_archive(),
            Some(Action::MyThreads) => self.open_my_threads(),
            Some(Action::UnreadsFirst) => self.toggle_unreads_first(),
            Some(Action::Compose) => self.compose(),
            Some(Action::React) => self.view_reactions(),
            Some(Action::Delete) => self.delete_selected(),
            Some(Action::Images) => self.open_images(),
            Some(Action::MarkRead) => self.mark_read(),
            Some(Action::MarkUnread) => self.mark_unread(),
            Some(Action::InlineImages) => {
                self.inline_images = !self.inline_images && self.picker.is_some();
                self.mark_all_dirty();
                self.status = if self.inline_images {
                    "inline images on"
                } else {
                    "inline images off"
                }
                .to_string();
            }
            Some(Action::Close) => self.escape_home(),
            Some(Action::Back) => {
                if let Some(list) = self.active_list_mut().filter(|list| list.line_scroll) {
                    list.line_scroll = false;
                    self.status.clear();
                    return;
                }
                // Unwind one stacked view; from the bare timeline, straight home.
                if self.stack.pop().is_none() {
                    self.go_home();
                }
            }
            Some(Action::OtherPane) => self.focus = Focus::Convs,
            Some(Action::ConversationsPane) => self.open_conversations_pane(),
            Some(Action::Keys) => self.open_keys(),
            _ => {}
        }
    }

    fn on_prompt_key(&mut self, k: KeyEvent) {
        let Mode::Prompt {
            kind,
            buf,
            previous,
        } = &mut self.mode
        else {
            return;
        };
        let kind = *kind;
        match k.code {
            KeyCode::Esc => {
                let typed = buf.text.clone();
                if kind == PromptKind::Command && self.focus == Focus::Convs {
                    self.filter = previous.clone();
                    self.mode = Mode::Normal;
                    self.apply_filter();
                } else {
                    self.mode = Mode::Normal;
                }
                if kind == PromptKind::Compose {
                    self.keep_draft(typed);
                    if let Some(name) = self.drop_attachment() {
                        self.status = format!("{name} not sent");
                    }
                }
            }
            // Alt-Enter, and Shift-Enter where the terminal reports it, break
            // the line instead of sending; Ctrl-j does the same from the editor.
            KeyCode::Enter
                if kind == PromptKind::Compose
                    && k.modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                buf.newline();
            }
            KeyCode::Enter => {
                let text = buf.text.clone();
                let before = previous.clone();
                self.mode = Mode::Normal;
                match kind {
                    PromptKind::Command => self.run_command(&text, &before),
                    PromptKind::Date => self.goto_date(&text),
                    PromptKind::Archive => self.archive_new(&text),
                    PromptKind::Compose => self.send_message(text),
                    PromptKind::PaletteColor => self.set_palette_color(&text),
                }
            }
            KeyCode::Tab if kind == PromptKind::Command => {
                let line = buf.text.clone();
                if let Some(done) = complete::apply(&line, &self.conv_names()) {
                    if let Mode::Prompt { buf, .. } = &mut self.mode {
                        *buf = Editor::with(done.clone());
                    }
                    self.filter_live(&done);
                }
            }
            KeyCode::Char('v') if kind == PromptKind::Compose && ctrl(k) => {
                self.attach_clipboard();
            }
            _ => {
                if buf.key(k, kind == PromptKind::Compose) {
                    let live = buf.text.clone();
                    if kind == PromptKind::Command {
                        self.filter_live(&live);
                    }
                }
            }
        }
    }
}

/// What a `/` line asks for.
#[derive(Debug, PartialEq)]
enum Command {
    /// `find TEXT` and `search TEXT`: filter the list, or search the open conversation.
    Find(String),
    /// `leave [#name]`.
    Leave(String),
    /// `cache start|stop|wipe [#name]`, and `cache highlight [on|off]`.
    Cache(String, String),
    /// `mute [#name]` (true) and `unmute [#name]` (false).
    Mute(bool, String),
    Star(bool, String),
    /// `colorpalette [name]`: edit and persist the semantic UI colors,
    /// starting from a named palette when one is given.
    ColorPalette(String),
    ConversationsPane,
    /// `keys`: rebind what the lists' keys do.
    Keys,
    /// `version`: show the version in the corner, or hide it again.
    Version,
    /// `upload [path]`: attach a file, the clipboard's image without a path.
    Upload(String),
}

/// `find x`, `search x`, `leave`, `leave #name`; a leading slash is ignored.
fn parse_command(line: &str) -> Option<Command> {
    let line = line.trim().trim_start_matches('/').trim_start();
    let (word, rest) = match line.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (line, ""),
    };
    match word.to_lowercase().as_str() {
        "find" | "search" | "f" | "s" => Some(Command::Find(rest.to_string())),
        "leave" => Some(Command::Leave(rest.to_string())),
        "star" | "pin" => Some(Command::Star(true, rest.to_string())),
        "unstar" | "unpin" => Some(Command::Star(false, rest.to_string())),
        "mute" => Some(Command::Mute(true, rest.to_string())),
        "unmute" => Some(Command::Mute(false, rest.to_string())),
        "colorpalette" | "palette" | "colors" => Some(Command::ColorPalette(rest.to_string())),
        "conversations-pane" | "conversation-pane" if rest.is_empty() => {
            Some(Command::ConversationsPane)
        }
        "keys" | "keybindings" if rest.is_empty() => Some(Command::Keys),
        "version" if rest.is_empty() => Some(Command::Version),
        "upload" | "attach" => Some(Command::Upload(rest.to_string())),
        "cache" => {
            let (op, name) = match rest.split_once(char::is_whitespace) {
                Some((o, n)) => (o, n.trim()),
                None => (rest, ""),
            };
            matches!(op, "start" | "stop" | "wipe" | "highlight")
                .then(|| Command::Cache(op.to_string(), name.to_string()))
        }
        _ => None,
    }
}

fn ctrl(k: KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL)
}

/// The prompt has room for a reason, not for a helper's whole complaint.
fn clip_note(error: &str) -> String {
    let one_line = error.replace('\n', " ");
    match one_line.char_indices().nth(60) {
        Some((at, _)) => format!("{}…", &one_line[..at]),
        None => one_line,
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string()
}

fn human_size(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.0} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

fn reaction_details(message: &Msg, context: &Ctx) -> Vec<String> {
    let mut lines = vec!["Read-only · loaded message · j/k scroll · h back · Esc home".into()];
    let reactions = message.data.get("reactions").and_then(Value::as_array);
    for reaction in reactions.into_iter().flatten() {
        let name = reaction["name"].as_str().unwrap_or("?");
        let count = reaction["count"].as_u64();
        let mut seen = HashSet::new();
        let users: Vec<&str> = reaction["users"].as_array().into_iter().flatten()
            .filter_map(Value::as_str).filter(|user| seen.insert(*user)).collect();
        lines.push(String::new());
        lines.push(format!(":{name}: {}", count.map(|n| n.to_string()).unwrap_or_else(|| "count unknown".into())));
        for user in &users {
            let name = context.user(user);
            let label = if name == *user { name } else { format!("@{name}") };
            lines.push(format!("  {label}"));
        }
        match count {
            Some(count) if count > users.len() as u64 => {
                lines.push(format!("  Partial: {} missing users from this payload", count - users.len() as u64));
            }
            None => lines.push("  Partial: total user count unavailable".into()),
            _ => {}
        }
    }
    if lines.len() == 1 { lines.push("No reactions in this payload".into()); }
    lines
}

/// `@handle` becomes a real mention when `user_id` knows the handle;
/// anything else, `@channel` and `@here` included, stays literal text.
fn link_mentions(text: &str, user_id: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, word) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let is_tail = |ch: char| ch.is_ascii_punctuation() && !matches!(ch, '.' | '-' | '_' | '@');
        let (core, tail) = match word.find(is_tail) {
            Some(p) => word.split_at(p),
            None => (word, ""),
        };
        let handle = core.strip_prefix('@').map(|h| h.trim_end_matches('.'));
        match handle.and_then(&user_id) {
            Some(id) => {
                out.push_str(&format!("<@{id}>"));
                if core.ends_with('.') {
                    out.push('.');
                }
                out.push_str(tail);
            }
            None => out.push_str(word),
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::archive::{Archive, Corpus, Msg};
    use crate::render::{line_text, Ctx, Tz};
    use serde_json::json;

    fn msg(secs: i64, text: &str) -> Msg {
        Msg::from_api(
            "C1".to_string(),
            json!({ "ts": format!("{secs}.000000"), "user": "U1", "text": text }),
        )
        .unwrap()
    }

    #[test]
    fn cached_today_labels_refresh_at_midnight_in_headers_and_dividers() {
        let reference = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc,2026,9,8,12,0,0).unwrap().timestamp();
        let corpus = Corpus::stub(&[]);
        let palette = Palette::default();
        let mut context = Ctx {archive:None,corpus:&corpus,tz:Tz::Utc,
            image_font:None,last_read:Some(reference*1_000_000-1),palette:&palette};
        for thread in [false,true] {
            let mut list = MsgList::new(vec![msg(reference,"first"),msg(reference+86400,"second")],thread);
            list.set_unread_count(Some(2));
            let today = context.tz.day(reference);
            list.rebuild_on_day(&context,120,today);
            assert!(list.flat[list.first[0]+1].line.to_string().starts_with("Today 12:00 UTC"));
            assert!(list.flat.iter().any(|row| row.msg.is_none() && row.line.to_string().contains("Today · new (2)")));
            assert!(list.flat[list.first[1]+1].line.to_string().starts_with("Wed 2026-09-09"));
            assert!(!list.dirty);
            list.rebuild_on_day(&context,120,today+1);
            assert!(list.flat[list.first[0]+1].line.to_string().starts_with("Tue 2026-09-08"));
            assert!(list.flat[list.first[1]+1].line.to_string().starts_with("Today 12:00 UTC"));
            assert!(list.flat.iter().any(|row| row.msg.is_none() && row.line.to_string().contains("Tue 2026-09-08 · new (2)")));
            // A timezone change also invalidates an otherwise unchanged list.
            context.tz = Tz::Local;
            list.rebuild_on_day(&context,120,context.tz.day(reference+86400));
            assert!(list.flat[list.first[1]+1].line.to_string().starts_with(&format!("Today {}",context.tz.fmt(reference+86400,"%H:%M %:z"))));
            context.tz = Tz::Utc;
        }
    }

    #[test]
    fn file_message_opens_timeline_then_thread_and_back_returns_to_root() {
        let mut app = mute_test_app();
        let location = |thread| crate::file_message::Location {
            channel: "C1".into(), root: 1000000, focus: if thread {2000000} else {1000000},
            timeline: vec![msg(1,"root")], replies: vec![msg(1,"root"),msg(2,"file reply")],
            has_older: false, has_newer: true,
        };
        app.bg = Some(live::api_tail(Arc::new(Client::for_test(|_,_| Ok(json!({"messages":[]})))), 0, "C1".into(), 0, true));
        app.open_file_message(location(false));
        assert!(app.bg.is_none());
        assert_eq!(app.open.as_ref().unwrap().list.selected().unwrap().id,1000000);
        assert!(app.stack.is_empty());
        app.open_file_message(location(true));
        assert_eq!(app.active_list().unwrap().selected().unwrap().id,2000000);
        app.on_key(KeyEvent::new(KeyCode::Char('h'),KeyModifiers::NONE));
        assert!(app.stack.is_empty());
        assert_eq!(app.open.as_ref().unwrap().list.selected().unwrap().id,1000000);
    }

    #[test]
    fn escape_rolls_back_settings_and_disables_late_job_navigation() {
        let mut app = mute_test_app();
        let old_keys = app.keymap.text(Action::Down);
        app.open_keys();
        app.keymap.bind(Action::Down,crate::keys::Chord::parse("z").unwrap(),false);
        app.on_key(KeyEvent::new(KeyCode::Esc,KeyModifiers::NONE));
        assert_eq!(app.keymap.text(Action::Down),old_keys);
        let original = app.palette.get(crate::palette::Role::Accent);
        app.open_color_palette("");
        app.palette.set(crate::palette::Role::Accent,ratatui::style::Color::Red);
        app.on_key(KeyEvent::new(KeyCode::Esc,KeyModifiers::NONE));
        assert_eq!(app.palette.get(crate::palette::Role::Accent),original);
        app.corpus.archives.push(Archive::stub(&[],&[]));
        app.corpus.convs[0].archive = 0;
        let (job,sender) = live::pending_job(JobKind::Refresh {conv:0,before:0});
        app.job = Some(job);
        app.on_key(KeyEvent::new(KeyCode::Esc,KeyModifiers::NONE));
        assert!(!app.job.as_ref().unwrap().navigate_on_completion);
        sender.send(Ok(Done::Refreshed)).unwrap();
        app.tick();
        assert!(app.open.is_none() && app.focus == Focus::Convs);
        app.stack.push(View::Image {files:vec![],index:0,shown:None,zoom:100});
        app.on_key(KeyEvent::new(KeyCode::Char('h'),KeyModifiers::NONE));
        assert!(app.stack.is_empty());
    }

    #[test]
    fn escape_returns_home_then_resets_conversation_cursor_and_scroll() {
        let mut app = mute_test_app();
        app.conv_cursor = 1;
        let index = app.filtered[1];
        app.open_conv(index);
        app.stack.push(View::Thread {root:1000000,list:MsgList::new(vec![msg(1,"thread")],true),live:None,place:None});
        app.stack.push(View::Raw {title:"nested".into(),browser:crate::raw::Browser::new(&json!({}))});
        app.on_key(KeyEvent::new(KeyCode::Esc,KeyModifiers::NONE));
        assert!(app.stack.is_empty() && app.open.is_none() && app.focus == Focus::Convs);
        assert_eq!(app.conv_cursor,1);
        app.conv_offset = 1;
        app.on_key(KeyEvent::new(KeyCode::Esc,KeyModifiers::NONE));
        assert_eq!((app.conv_cursor,app.conv_offset),(0,0));
        app.help = true;
        app.open_conversations_pane();
        app.on_key(KeyEvent::new(KeyCode::Esc,KeyModifiers::NONE));
        assert!(app.pane_menu.is_none() && !app.help);
        app.mode = Mode::Prompt {kind:PromptKind::Command,buf:Editor::with("filter".into()),previous:String::new()};
        app.on_key(KeyEvent::new(KeyCode::Esc,KeyModifiers::NONE));
        assert!(matches!(app.mode,Mode::Normal));
        app.compose = Some(Compose {conv:0,cid:"C1".into(),thread:None,label:"test".into()});
        app.mode = Mode::Prompt {kind:PromptKind::Compose,buf:Editor::with("unsent text".into()),previous:String::new()};
        app.on_key(KeyEvent::new(KeyCode::Esc,KeyModifiers::NONE));
        assert_eq!(app.draft.as_ref().unwrap().text,"unsent text");
        assert!(app.open.is_none());
    }

    #[test]
    fn word_menu_quit_keys_reach_the_application() {
        let mut app = mute_test_app();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.open_color_palette("");
        app.on_key(key(KeyCode::Char('W')));
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.quit);
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
        app.quit = false;
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Char('k')));
        app.on_key(key(KeyCode::Char('l')));
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn word_highlights_render_in_selected_conversation_messages_and_title() {
        let mut app = mute_test_app();
        let index = app.corpus.convs.iter().position(|conv| conv.id == "C1").unwrap();
        app.corpus.convs[index].name = "#NGINX-chat".into();
        app.open_conv(index);
        app.open.as_mut().unwrap().list = MsgList::new(vec![msg(1, "nginx and *NGINX* with nginx-fork")], false);
        app.conv_cursor = app.filtered.iter().position(|i| *i == index).unwrap();
        app.focus = Focus::Convs;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut hits = 0;
        for y in 0..20 {
            for x in 0..116 {
                let text: String = (x..x + 5).map(|column| buffer[(column,y)].symbol()).collect();
                if text.eq_ignore_ascii_case("nginx") {
                    hits += 1;
                    for column in x..x + 5 { assert_eq!(buffer[(column,y)].fg, ratatui::style::Color::Green, "{x},{y}"); }
                }
            }
        }
        assert!(hits >= 5, "conversation name, title, and three body matches: {hits}; {}", buffer.content.iter().map(|cell| cell.symbol()).collect::<String>());
    }

    #[test]
    fn palette_word_menu_is_nested_and_cancel_restores_rules() {
        let mut app = mute_test_app();
        let original = app.palette.clone();
        app.open_color_palette("");
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.on_palette_key(key(KeyCode::Char('G')), false);
        app.on_palette_key(key(KeyCode::Enter), false);
        assert!(matches!(app.stack.last(), Some(View::ColorPalette { highlights: Some(_), .. })));
        app.on_palette_key(key(KeyCode::Char('d')), false);
        assert!(app.palette.highlights.is_empty());
        app.on_palette_key(key(KeyCode::Esc), false);
        assert!(matches!(app.stack.last(), Some(View::ColorPalette { highlights: None, .. })));
        app.on_palette_key(key(KeyCode::Esc), false);
        assert_eq!(app.palette, original);
        app.palette.highlights.clear();
        app.open_color_palette("vintage");
        assert!(app.palette.highlights.is_empty());
    }

    #[test]
    fn long_message_reads_by_line_before_raw_and_keeps_position() {
        let mut app = App::new(
            Corpus::stub(&[]),
            Tz::Utc,
            30.0,
            false,
            false,
            PathBuf::new(),
            PathBuf::new(),
            60,
            None,
            None,
        );
        app.merge_conversations(vec![json!({"id":"C1","name":"long","is_member":true})]);
        app.open_conv(0);
        app.focus = Focus::Msgs;
        let body = (0..100).map(|n| format!("line {n}\n")).collect::<String>();
        app.open.as_mut().unwrap().list = MsgList::new(vec![msg(1, &body), msg(2, "next")], false);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        let draw =
            |app: &mut App, terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>| {
                terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
            };
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        draw(&mut app, &mut terminal);
        app.on_key(key(KeyCode::Char('l')));
        draw(&mut app, &mut terminal);
        assert!(app.stack.is_empty());
        let first = app.active_list().unwrap().first[0];
        for n in 1..=12 {
            app.on_key(key(KeyCode::Down));
            draw(&mut app, &mut terminal);
            assert_eq!(app.active_list().unwrap().scroll, first + n);
            assert_eq!(app.active_list().unwrap().cursor, 0);
        }
        app.mark_all_dirty();
        draw(&mut app, &mut terminal);
        assert_eq!(app.active_list().unwrap().scroll, first + 12);
        app.on_key(key(KeyCode::Char('k')));
        draw(&mut app, &mut terminal);
        assert_eq!(app.active_list().unwrap().scroll, first + 11);
        app.on_key(key(KeyCode::Char('l')));
        assert!(matches!(app.stack.last(), Some(View::Raw { .. })));
        app.on_key(key(KeyCode::Char('h')));
        draw(&mut app, &mut terminal);
        assert_eq!(app.active_list().unwrap().scroll, first + 11);
        app.on_key(key(KeyCode::End));
        draw(&mut app, &mut terminal);
        let end = app.active_list().unwrap().last[0] + 1 - app.msgs_height;
        assert_eq!(app.active_list().unwrap().scroll, end);
        app.on_key(key(KeyCode::Down));
        draw(&mut app, &mut terminal);
        assert_eq!(app.active_list().unwrap().scroll, end);
        app.on_key(key(KeyCode::Home));
        draw(&mut app, &mut terminal);
        assert_eq!(app.active_list().unwrap().scroll, first);
        app.on_key(key(KeyCode::Char('h')));
        assert!(app.open.is_some());
        assert!(!app.active_list().unwrap().line_scroll);
        app.on_key(key(KeyCode::Char('h')));
        assert!(app.open.is_none());
    }

    #[test]
    fn opening_requests_counts_without_polling_and_waits_for_busy_lookup() {
        let mut app = mute_test_app();
        app.pane_settings.number = crate::conversations_pane::NumberColumn::Hidden;
        let client = Arc::new(Client::for_test(|method, _| {
            assert_eq!(method, "client.counts");
            Ok(json!({"channels":[]}))
        }));
        assert_eq!(app.poll_every, Duration::ZERO);
        // An earlier enrichment is still occupying its separate slot.
        app.unread_count_job = Some(live::api_unread_counts(client.clone(), 0, json!({}), vec![]));
        app.open_conv(0);
        app.api = Some(client);
        assert_eq!(app.unread_count_targets(), vec!["C1"]);
        assert!(app.counts_pending);
        app.pump_requested_counts();
        assert!(app.counts_pending);
        assert!(app.bg.is_none());
        app.unread_count_job = None;
        app.pump_requested_counts();
        assert!(!app.counts_pending);
        assert!(matches!(app.bg.as_ref().map(|job| &job.kind), Some(JobKind::Counts { .. })));
    }

    #[test]
    fn unread_counts_preserve_snapshot_identity_and_clear_after_read() {
        let mut app = mute_test_app();
        let snapshot = json!({"channels":[{"id":"C1","has_unreads":true,"last_read":"1.000000","latest":"5.000000","unread_count":4}]});
        app.corpus.convs[0].last_id = 100_000_000;
        app.apply_counts(&snapshot);
        assert_eq!(app.corpus.convs[0].unread_count, None);
        app.apply_unread_counts(&snapshot);
        assert_eq!(app.corpus.convs[0].unread_count, Some(4));
        app.apply_counts(&snapshot);
        assert_eq!(app.corpus.convs[0].unread_count, Some(4));
        let mut invalidated = snapshot.clone();
        invalidated["channels"][0]["history_invalid"] = json!("changed");
        app.apply_counts(&invalidated);
        app.apply_unread_counts(&snapshot);
        assert_eq!(app.corpus.convs[0].unread_count, None);
        let mut newer = snapshot.clone();
        newer["channels"][0]["latest"] = json!("6.000000");
        app.apply_counts(&newer);
        assert_eq!(app.corpus.convs[0].unread_count, None);
        app.apply_unread_counts(&snapshot);
        assert_eq!(app.corpus.convs[0].unread_count, None);
        newer["channels"][0]["has_unreads"] = json!(false);
        newer["channels"][0]["last_read"] = json!("6.000000");
        app.apply_counts(&newer);
        app.apply_unread_counts(&snapshot);
        assert!(!app.corpus.convs[0].unread);
        assert_eq!(crate::conversations_pane::NumberColumn::Unread.value(&app.corpus.convs[0], false), None);
    }

    #[test]
    fn conversation_number_modes_and_menu() {
        use crate::conversations_pane::{Menu, NumberColumn as N, Settings};
        let mut app = App::new(
            Corpus::stub(&[]),
            Tz::Utc,
            30.0,
            false,
            false,
            PathBuf::new(),
            PathBuf::new(),
            60,
            None,
            None,
        );
        app.merge_conversations(vec![json!({"id":"C1","name":"one","is_member":true})]);
        let c = &mut app.corpus.convs[0];
        c.msgs = 123;
        c.mine = 12;
        c.score = 3.6;
        c.mentions = 2;
        assert_eq!(N::Messages.value(c, true), None);
        assert_eq!(N::Mentions.value(c, false), Some(2));
        assert_eq!(N::Sort.value(c, true), Some(4));
        assert_eq!(N::Sort.value(c, false), Some(123));
        c.live_only = false;
        for sort in [true, false] {
            assert_eq!(N::Messages.value(c, sort), Some(123));
            assert_eq!(N::Mine.value(c, sort), Some(12));
            assert_eq!(N::Activity.value(c, sort), Some(4));
            assert_eq!(N::Hidden.value(c, sort), None);
        }
        let mut menu = Menu::new(Settings::default(), &app.corpus.convs);
        menu.cursor = 8;
        menu.toggle();
        assert_eq!(menu.settings.number, N::Messages);
        assert!(menu.rows()[8].contains("Cached messages"));
        menu.cursor = 9;
        menu.toggle();
        assert_eq!(menu.settings.overrides.get("C1"), Some(&true));
        menu.cursor = 1;
        menu.toggle();
        assert_eq!(menu.settings, Settings::default());
        app.run_command("conversation-pane", "");
        app.pane_menu.as_mut().unwrap().cursor = 8;
        app.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.pane_settings.number, N::Sort);
        app.run_command("conversation-pane", "");
        app.pane_menu.as_mut().unwrap().cursor = 8;
        app.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.pane_settings.number, N::Messages);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let row = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
            (1..29)
                .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
                .collect::<String>()
        };
        assert!(row(&terminal).ends_with("123"));
        app.pane_settings.number = N::Unread;
        app.corpus.convs[0].unread = true;
        for (count, expected) in [(Some(1), "1"), (Some(9), "9"), (Some(10), "9+"), (Some(500), "9+"), (None, "—")] {
            app.corpus.convs[0].unread_count = count;
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            assert!(row(&terminal).ends_with(expected));
        }
        app.corpus.convs[0].unread = false;
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert!(row(&terminal).trim_end().ends_with("#one"));
        app.pane_settings.number = N::Hidden;
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert!(!row(&terminal).contains("123"));
        app.pane_settings.number = N::Messages;
        app.corpus.convs[0].live_only = true;
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        assert!(row(&terminal).ends_with('—'));
    }

    #[test]
    fn image_zoom_keys_accept_shifted_and_base_characters() {
        let mut app = App::new(
            Corpus::stub(&[]),
            Tz::Utc,
            30.0,
            false,
            false,
            PathBuf::new(),
            PathBuf::new(),
            60,
            None,
            None,
        );
        app.stack.push(View::Image {
            files: vec![],
            index: 0,
            zoom: 100,
            shown: None,
        });
        let zoom = |app: &App| match app.stack.last().unwrap() {
            View::Image { zoom, .. } => *zoom,
            _ => panic!(),
        };
        for c in ['=', '+'] {
            app.on_key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ));
        }
        assert_eq!(zoom(&app), 150);
        for c in ['-', '_'] {
            app.on_key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ));
        }
        assert_eq!(zoom(&app), 100);
        for _ in 0..40 {
            app.on_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
        }
        assert_eq!(zoom(&app), 800);
        for _ in 0..40 {
            app.on_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
        }
        assert_eq!(zoom(&app), 25);
        app.on_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        assert_eq!(zoom(&app), 100);
    }

    #[test]
    fn conversations_pane_navigation_and_focused_search() {
        let mut app = App::new(
            Corpus::stub(&[]), Tz::Utc, 30.0, false, false,
            PathBuf::new(), PathBuf::new(), 60, None, None,
        );
        app.merge_conversations(vec![
            json!({"id":"C1", "name":"alpha", "is_member":true}),
            json!({"id":"C2", "name":"beta", "is_member":true}),
        ]);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.open_conversations_pane();
        assert_eq!(app.pane_menu.as_ref().unwrap().cursor, 1);
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.pane_menu.as_ref().unwrap().cursor, 2);
        for _ in 0..2 { app.on_key(key(KeyCode::Char('h'))); }
        assert!(app.pane_menu.as_ref().unwrap().settings.hidden.contains("public"));
        for _ in 0..2 { app.on_key(key(KeyCode::Char('l'))); }
        assert!(!app.pane_menu.as_ref().unwrap().settings.hidden.contains("public"));
        app.on_key(key(KeyCode::Char('x')));
        app.on_key(key(KeyCode::Backspace));
        assert!(app.pane_menu.as_ref().unwrap().query.is_empty());
        app.on_key(key(KeyCode::Char('k')));
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.pane_menu.as_ref().unwrap().cursor, 0);
        for c in "jkh l".chars() { app.on_key(key(KeyCode::Char(c))); }
        assert_eq!(app.pane_menu.as_ref().unwrap().query, "jkh l");
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.pane_menu.as_ref().unwrap().query, "jkh ");
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "beta".chars() { app.on_key(key(KeyCode::Char(c))); }
        assert_eq!(app.pane_menu.as_ref().unwrap().matching().len(), 1);
        let mut terminal = ratatui::Terminal::new(
            ratatui::backend::TestBackend::new(120, 30),
        ).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let search = (1..20).map(|x| terminal.backend().buffer()[(x, 5)].symbol()).collect::<String>();
        assert!(search.starts_with("Search: beta"));
        assert_ne!(terminal.backend().buffer()[(1, 5)].bg, terminal.backend().buffer()[(1, 6)].bg);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.pane_menu.as_ref().unwrap().cursor, 1);
        app.on_key(key(KeyCode::End));
        app.on_key(key(KeyCode::Char('h')));
        assert_eq!(app.pane_menu.as_ref().unwrap().settings.overrides.get("C2"), Some(&false));
        app.on_key(key(KeyCode::Char('l')));
        app.on_key(key(KeyCode::Char('l')));
        assert_eq!(app.pane_menu.as_ref().unwrap().settings.overrides.get("C2"), Some(&true));
        app.pane_menu.as_mut().unwrap().cursor = 7;
        app.on_key(key(KeyCode::Char('h')));
        assert!(app.pane_menu.as_ref().unwrap().settings.only_muted);
        app.on_key(key(KeyCode::Char('l')));
        assert!(!app.pane_menu.as_ref().unwrap().settings.only_muted);
        app.pane_menu.as_mut().unwrap().cursor = 8;
        app.on_key(key(KeyCode::Char('h')));
        assert_eq!(app.pane_menu.as_ref().unwrap().settings.number, crate::conversations_pane::NumberColumn::Hidden);
        app.on_key(key(KeyCode::Char('l')));
        assert_eq!(app.pane_menu.as_ref().unwrap().settings.number, crate::conversations_pane::NumberColumn::Sort);
        for leave in [KeyCode::Down, KeyCode::Tab] {
            app.on_key(key(KeyCode::Home));
            app.on_key(key(leave));
            assert_eq!(app.pane_menu.as_ref().unwrap().cursor, 1);
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.pane_menu.is_none());
        assert_eq!(app.pane_settings.overrides.get("C2"), Some(&true));
    }

    #[test]
    fn conversations_pane_save_cancel_reset_and_shortcut() {
        let mut app = App::new(
            Corpus::stub(&[]),
            Tz::Utc,
            30.0,
            false,
            false,
            PathBuf::new(),
            PathBuf::new(),
            60,
            None,
            None,
        );
        app.merge_conversations(vec![
            json!({"id":"C1", "name":"public", "is_member":true}),
            json!({"id":"C2", "name":"private", "is_private":true, "is_member":true}),
        ]);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.on_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert!(app.pane_menu.is_some());
        app.pane_menu.as_mut().unwrap().settings.toggle_category(0);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.filtered.len(), 2);
        app.run_command("conversations-pane", "");
        app.pane_menu.as_mut().unwrap().settings.toggle_category(0);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.filtered.len(), 1);
        app.open_conversations_pane();
        assert_eq!(app.pane_menu.as_ref().unwrap().entries.len(), 2);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.filtered.len(), 2);
        app.open_conv(app.filtered[0]);
        app.open_conversations_pane();
        let id = app.corpus.convs[app.open.as_ref().unwrap().conv].id.clone();
        app.pane_menu
            .as_mut()
            .unwrap()
            .settings
            .overrides
            .insert(id, false);
        app.on_key(key(KeyCode::Enter));
        assert!(app.open.is_none());
        assert!(app.stack.is_empty());
        app.open_conversations_pane();
        app.pane_menu.as_mut().unwrap().settings = Default::default();
        for category in 0..6 {
            app.pane_menu
                .as_mut()
                .unwrap()
                .settings
                .toggle_category(category);
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.filtered.is_empty());
        app.run_command("conversations-pane", "");
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Char('p')));
        assert_eq!(app.pane_menu.as_ref().unwrap().matching().len(), 2);
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(app.pane_menu.as_ref().unwrap().matching().len(), 1);
        app.on_key(key(KeyCode::Esc));
        assert!(app.filtered.is_empty());
        app.open_conversations_pane();
        app.on_key(key(KeyCode::Char(' ')));
        // A directory is not a settings file: preserve active choices and keep
        // the pending menu open when saving fails.
        app.pane_path = Some(std::env::temp_dir());
        app.on_key(key(KeyCode::Enter));
        assert!(app.pane_menu.is_some());
        assert!(app.filtered.is_empty());
        assert!(app.status.contains("cannot save"));
        app.pane_path = None;
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.filtered.len(), 2);
    }

    #[test]
    fn live_only_conversation_renders_without_an_archive() {
        let mut app = App::new(
            Corpus::stub(&[("C1", "live-channel")]),
            Tz::Utc,
            30.0,
            false,
            false,
            PathBuf::new(),
            PathBuf::new(),
            60,
            None,
            None,
        );
        app.merge_conversations(vec![
            json!({"id": "C1", "name": "live-channel", "is_member": true}),
        ]);
        assert_eq!(app.corpus.convs.len(), 1);
        assert!(app.corpus.convs[0].live_only);
        assert!(app.open_conv(0));
        assert!(app.title().contains("live from Slack"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        app.open.as_mut().unwrap().list = MsgList::new(vec![msg(1, "hello <#C1>")], false);
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        assert!(crate::ui::dump(&mut app, 80).contains("live-channel"));
        app.open.as_mut().unwrap().list =
            MsgList::new(vec![msg(1, "hello <#C1> <!subteam^S1>")], false);
        assert!(crate::ui::dump(&mut app, 80).contains("@S1"));
        app.take_usergroups(vec![json!({"id":"S1", "name":"oncall"})]);
        assert!(crate::ui::dump(&mut app, 80).contains("@oncall"));
        assert_eq!(app.ctx_for(0).user("U1"), "U1");
        app.run_search("hello");
        assert!(app.status.contains("no message matching"));

        app.merge_conversations(vec![json!({"id":"D1", "is_im":true, "user":"U1"})]);
        app.take_profiles(vec![json!({"id":"U1", "name":"Ada", "is_bot":false})]);
        assert_eq!(app.corpus.convs[1].name, "@Ada");
        assert_eq!(app.ctx_for(0).author(&msg(1, "hello")), "Ada");
        assert!(crate::ui::dump(&mut app, 80).contains("Ada"));
        app.merge_conversations(vec![json!({"id":"D2", "is_im":true, "user":"U1"})]);
        assert_eq!(app.corpus.convs[2].name, "@Ada");

        // Index zero must not become the live conversation's archive when a
        // different conversation has a local cache.
        app.corpus
            .archives
            .push(Archive::stub(&[], &[("C1", "wrong-channel")]));
        assert!(app.ctx_for(0).archive.is_none());
        assert!(crate::ui::dump(&mut app, 80).contains("live-channel"));
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
    }

    #[test]
    fn reaction_details_resolve_deduplicate_and_report_partial_payloads() {
        let mut corpus = Corpus::stub(&[]);
        corpus.merge_profiles(vec![json!({"id":"U1", "name":"ada"})]);
        let mut message = msg(1, "reactions");
        message.data["reactions"] = json!([
            {"name":"eyes", "count":5, "users":["U1", "U1", "UNKNOWN", "UNKNOWN"]},
            {"name":"custom", "count":2},
            {"name":"unknown-count", "users":["U1"]}
        ]);
        let before = message.data.clone();
        let palette = Palette::default();
        let context = Ctx { archive: None, corpus: &corpus, tz: Tz::Utc, image_font: None, last_read: None, palette: &palette };
        let lines = reaction_details(&message, &context);
        assert!(lines.contains(&":eyes: 5".into()));
        assert_eq!(lines.iter().filter(|line| *line == "  UNKNOWN").count(), 1);
        assert_eq!(lines.iter().filter(|line| *line == "  @ada").count(), 2);
        assert!(lines.contains(&"  Partial: 3 missing users from this payload".into()));
        assert!(lines.contains(&"  Partial: 2 missing users from this payload".into()));
        assert!(lines.contains(&"  Partial: total user count unavailable".into()));
        assert_eq!(message.data, before);
        assert!(reaction_details(&msg(2, "empty"), &context).contains(&"No reactions in this payload".into()));
    }

    #[test]
    fn reaction_details_target_selection_and_isolate_navigation_offline() {
        let mut app = mute_test_app();
        app.open_conv(0);
        app.corpus.merge_profiles(vec![json!({"id":"U1", "name":"ada"})]);
        let mut root = msg(1, "root");
        root.data["reactions"] = json!([{"name":"root", "count":1, "users":["U1"]}]);
        let mut reply = msg(2, "reply");
        reply.data["reactions"] = json!([{"name":"reply", "count":3, "users":["U1", "UNKNOWN"]}]);
        app.open.as_mut().unwrap().list = MsgList::new(vec![root.clone()], false);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for thread in [false, true] {
            if thread {
                let mut list = MsgList::new(vec![root.clone(), reply.clone()], true);
                list.cursor = 1;
                app.stack.push(View::Thread { root: root.id, list, live: None, place: None });
            }
            let depth = app.stack.len();
            app.on_key(key(KeyCode::Char('e')));
            let Some(View::Reactions { title, lines, .. }) = app.stack.last() else { panic!("details missing") };
            assert!(title.contains(if thread { "1970-01-01 00:00:02" } else { "1970-01-01 00:00:01" }));
            assert!(lines.contains(&if thread { ":reply: 3" } else { ":root: 1" }.into()));
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
            terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
            let text = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
            assert!(text.contains("ada"));
            if thread { assert!(text.contains("UNKNOWN")); assert!(text.contains("Partial: 1 missing")); }
            app.msgs_height = 1;
            app.on_key(key(KeyCode::Char('j')));
            assert!(matches!(app.stack.last(), Some(View::Reactions { scroll: 1, .. })));
            app.on_key(key(KeyCode::Char('k')));
            assert!(matches!(app.stack.last(), Some(View::Reactions { scroll: 0, .. })));
            for character in ['D', 'c', 'e', 'm', 'M', 'R', 'a', 'T', '/', 'I'] {
                app.on_key(key(KeyCode::Char(character)));
                assert_eq!(app.stack.len(), depth + 1);
                assert!(matches!(app.mode, Mode::Normal));
                assert!(app.pending_delete.is_none());
                assert!(app.job.is_none());
                assert!(app.api.is_none());
            }
            app.on_key(key(KeyCode::Char('h')));
            assert_eq!(app.stack.len(), depth);
            assert_eq!(app.selected().unwrap().id, if thread { reply.id } else { root.id });
        }
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(app.open.as_ref().unwrap().list.msgs[0].data, root.data);
        app.on_key(key(KeyCode::Esc));
        assert!(app.stack.is_empty());
        assert_eq!(app.focus, Focus::Convs);
        assert!(app.open.is_none());
    }

    #[test]
    fn reaction_details_use_the_selected_channel_and_archive_profiles() {
        let mut app = mute_test_app();
        app.open_conv(0);
        let open_channel = app.open_conv_ref().unwrap().id.clone();
        app.merge_conversations(vec![json!({"id":"COTHER", "name":"other-channel", "is_member":true})]);
        let index = app.corpus.conv_by_channel("COTHER").unwrap();
        app.corpus.archives.push(Archive::stub(&[("UARCHIVE","archive.user")], &[]));
        app.corpus.convs[index].archive = app.corpus.archives.len() - 1;
        app.corpus.convs[index].live_only = false;
        let mut hit = msg(1,"other channel hit");
        hit.channel_id = "COTHER".into();
        hit.data["reactions"] = json!([{"name":"eyes", "count":1, "users":["UARCHIVE"]}]);
        app.stack.push(View::Search {query:"hit".into(),list:MsgList::new(vec![hit.clone()],false),
            capped:false,live_hits:None,live_pending:false});
        app.view_reactions();
        assert!(app.title().starts_with("#other-channel · Reactions"));
        assert_eq!(app.open_conv_ref().unwrap().id,open_channel);
        let Some(View::Reactions {lines,..}) = app.stack.last() else {panic!()};
        assert!(lines.contains(&"  @archive.user".into()));
        app.stack.clear();
        hit.data["reactions"] = json!([{"name":"thread", "count":1, "users":["UTHREAD"]}]);
        app.stack.push(View::Thread {root:hit.id,list:MsgList::new(vec![hit],true),
            live:Some(Box::new(Archive::stub(&[("UTHREAD","thread.user")], &[]))),place:None});
        app.view_reactions();
        let Some(View::Reactions {lines,..}) = app.stack.last() else {panic!()};
        assert!(lines.contains(&"  @thread.user".into()));
        app.on_key(KeyEvent::new(KeyCode::Char('?'),KeyModifiers::NONE));
        assert!(app.help);
        app.on_key(KeyEvent::new(KeyCode::Char('?'),KeyModifiers::NONE));
        assert!(!app.help);
        app.on_key(KeyEvent::new(KeyCode::Char('q'),KeyModifiers::NONE));
        assert!(app.quit);
    }

    #[test]
    fn raw_enter_follows_cached_link_and_h_restores_highlight() {
        let mut app = mute_test_app();
        app.corpus.workspace_url = "https://myorg.slack.com".into();
        let cache = std::env::temp_dir().join(format!("slack-raw-link-test-{}-{}",std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        app.cache_dir = cache.clone();
        app.open_conv(0);
        app.open.as_mut().unwrap().list = MsgList::new(vec![msg(1,
            "https://myorg.slack.com/archives/COTHER/p1788811422381186?thread_ts=1788765950.129609")],false);
        std::fs::create_dir_all(cache.join("threads")).unwrap();
        std::fs::write(live::thread_file(&cache,"COTHER",1788765950129609),json!([
            {"ts":"1788765950.129609","text":"linked root","reply_count":1},
            {"ts":"1788811422.381186","thread_ts":"1788765950.129609","text":"linked reply"}
        ]).to_string()).unwrap();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100,28)).unwrap();
        let key = |code| KeyEvent::new(code,KeyModifiers::NONE);
        app.on_key(key(KeyCode::Char('l')));
        terminal.draw(|frame| crate::ui::draw(frame,&mut app)).unwrap();
        let original = terminal.backend().buffer().clone();
        assert!(original.content.iter().any(|cell| cell.bg == app.palette.get(Role::SelectionBackground)));
        let source = app.open.as_ref().unwrap().conv;
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.selected().unwrap().text,"linked reply");
        assert_eq!(app.open.as_ref().unwrap().conv,source);
        assert!(app.title().contains("COTHER"));
        app.on_key(key(KeyCode::Char('h')));
        terminal.draw(|frame| crate::ui::draw(frame,&mut app)).unwrap();
        let before = match app.stack.last().unwrap() {View::Raw {browser,..}=>(browser.cursor,browser.scroll),_=>panic!()};
        app.on_key(key(KeyCode::Down));
        let after = match app.stack.last().unwrap() {View::Raw {browser,..}=>browser.cursor,_=>panic!()};
        assert_eq!(after,before.0+1);
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.selected().unwrap().text,"linked reply");
        app.on_key(key(KeyCode::Char('h')));
        if let Some(View::Raw {browser,..}) = app.stack.last_mut() {
            *browser = crate::raw::Browser::new(&json!({"text":"https://myorg.slack.com/archives/COTHER/p1788811422381186"}));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.selected().unwrap().text,"linked reply"); // Resolve rootless links from root-keyed caches.
        app.on_key(key(KeyCode::Char('h')));
        app.corpus.workspace_url = "https://other.slack.com".into();
        app.on_key(key(KeyCode::Enter));
        assert!(app.status.contains("another Slack workspace"));
        assert!(matches!(app.stack.last(),Some(View::Raw {..})));
        assert!(app.job.is_none());
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn raw_links_keep_source_stack_and_ignore_late_navigation() {
        let mut app = mute_test_app();
        app.corpus.workspace_url = "https://myorg.slack.com".into();
        app.open_conv(0);
        let source = app.open.as_ref().unwrap().conv;
        let url = "https://myorg.slack.com/archives/COTHER/p1788811422381186?thread_ts=1788765950.129609";
        let mut message = msg(1, url);
        message.data["text"] = json!(url);
        app.open.as_mut().unwrap().list = MsgList::new(vec![message],false);
        app.open_raw();
        let key = |code| KeyEvent::new(code,KeyModifiers::NONE);
        let (raw_id, cursor) = match app.stack.last_mut().unwrap() {
            View::Raw {browser,..} => {browser.scroll=3; (browser.id,browser.cursor)}, _=>panic!()
        };
        let link = crate::raw::Link::parse(url).unwrap();
        let messages = vec![
            Msg::from_api("COTHER".into(),json!({"ts":"1788765950.129609","text":"root","reply_count":1})).unwrap(),
            Msg::from_api("COTHER".into(),json!({"ts":"1788811422.381186","text":"reply","thread_ts":"1788765950.129609"})).unwrap(),
        ];
        app.job = Some(Job::completed_for_test(JobKind::MessageLink {raw_id,link:link.clone()},Ok(Done::ThreadMsgs(messages.clone()))));
        app.tick();
        assert_eq!(app.open.as_ref().unwrap().conv,source);
        assert_eq!(app.stack.len(),2);
        assert_eq!(app.selected().unwrap().id,link.focus);
        app.on_key(key(KeyCode::Char('h')));
        let View::Raw {browser,..} = app.stack.last().unwrap() else {panic!()};
        assert_eq!((browser.id,browser.cursor,browser.scroll),(raw_id,cursor,3));
        app.on_key(key(KeyCode::Char('h')));
        assert!(app.stack.is_empty());
        app.open_raw();
        app.job = Some(Job::completed_for_test(JobKind::MessageLink {raw_id,link:link.clone()},Ok(Done::ThreadMsgs(messages))));
        app.tick();
        assert_eq!(app.stack.len(),1); // An old fetch cannot hijack a newly opened raw view.
        app.status = "current view status".into();
        app.job = Some(Job::completed_for_test(JobKind::MessageLink {raw_id,link},Err("old fetch failure".into())));
        app.tick();
        assert_eq!(app.status,"current view status");
        app.on_key(key(KeyCode::Home));
        app.on_key(key(KeyCode::Enter)); // Nonlink leaf: stay in raw view.
        assert_eq!(app.stack.len(),1);
        app.on_key(key(KeyCode::Esc));
        assert!(app.stack.is_empty() && app.open.is_none());
    }

    #[test]
    fn horizontal_open_reads_only_collapsed_messages() {
        for forward in [KeyCode::Char('l'), KeyCode::Right] {
            for thread in [false, true] {
                let mut app = mute_test_app();
                app.open_conv(0);
                let message = msg(1, &"body line\n".repeat(12));
                if thread {
                    app.stack.push(View::Thread { root: message.id,
                        list: MsgList::new(vec![message], true), live: None, place: None });
                } else {
                    app.open.as_mut().unwrap().list = MsgList::new(vec![message], false);
                }
                let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
                let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
                for height in [24, 100, 24] {
                    terminal.backend_mut().resize(120, height);
                    terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
                    let collapsed = height == 24;
                    assert_eq!(app.active_list().unwrap().collapsed, vec![collapsed]);
                    assert_eq!(app.active_list().unwrap().flat.iter().any(|line|
                        line.line.to_string().contains("more lines)")), collapsed);
                    app.on_key(key(forward));
                    if collapsed {
                        assert!(app.active_list().unwrap().line_scroll);
                        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
                        assert!(!app.active_list().unwrap().flat.iter().any(|line|
                            line.line.to_string().contains("more lines)")));
                        app.on_key(key(forward));
                    }
                    assert!(matches!(app.stack.last(), Some(View::Raw { .. })));
                    app.on_key(key(KeyCode::Char('h')));
                    assert_eq!(app.active_list().unwrap().line_scroll, collapsed);
                    if collapsed { app.on_key(key(KeyCode::Char('h'))); }
                    assert_eq!(app.stack.len(), usize::from(thread));
                }
                assert!(app.job.is_none());
            }
        }
    }

    #[test]
    fn horizontal_navigation_unwinds_one_level_at_a_time() {
        for (forward, back) in [(KeyCode::Char('l'), KeyCode::Char('h')), (KeyCode::Right, KeyCode::Left)] {
            let mut app = App::new(
                Corpus::stub(&[]), Tz::Utc, 30.0, false, false,
                PathBuf::new(), PathBuf::new(), 60, None, None,
            );
            app.merge_conversations(vec![json!({"id":"C1", "name":"test", "is_member":true})]);
            let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
            app.on_key(key(forward));
            assert!(app.focus == Focus::Msgs);
            let mut root = msg(3, "thread root");
            root.reply_count = 2;
            app.open.as_mut().unwrap().list = MsgList::new(vec![msg(1, "first"), msg(2, "second"), root.clone()], false);
            app.on_key(key(KeyCode::Char('j')));
            app.on_key(key(KeyCode::Char('j')));
            assert_eq!(app.active_list().unwrap().cursor, 2);
            app.on_key(key(forward));
            assert!(matches!(app.stack.last(), Some(View::Thread { .. })));
            assert!(!app.open.as_ref().unwrap().list.line_scroll);
            // Supply offline replies; this test must not access Slack.
            let View::Thread { list, .. } = app.stack.last_mut().unwrap() else { panic!() };
            *list = MsgList::new(vec![root, msg(4, "reply"), msg(5, "another reply")], true);
            app.on_key(key(KeyCode::Char('j')));
            app.on_key(key(KeyCode::Char('j')));
            app.on_key(key(back));
            assert!(app.stack.is_empty());
            assert_eq!(app.active_list().unwrap().cursor, 2);
            assert!(app.focus == Focus::Msgs);
            // Re-enter, then descend through message reading and raw JSON.
            app.on_key(key(forward));
            let View::Thread { list, .. } = app.stack.last_mut().unwrap() else { panic!() };
            *list = MsgList::new(vec![msg(4, &"reply\n".repeat(40))], true);
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
            terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
            app.on_key(key(forward));
            assert!(app.active_list().unwrap().line_scroll);
            app.on_key(key(forward));
            assert!(matches!(app.stack.last(), Some(View::Raw { .. })));
            app.on_key(key(back));
            assert!(matches!(app.stack.last(), Some(View::Thread { .. })));
            assert!(app.active_list().unwrap().line_scroll);
            app.on_key(key(back));
            assert!(!app.active_list().unwrap().line_scroll);
            assert!(matches!(app.stack.last(), Some(View::Thread { .. })));
            app.on_key(key(back));
            assert!(app.stack.is_empty());
            assert!(app.open.is_some());
            app.on_key(key(back));
            assert!(app.open.is_none());
            assert!(app.focus == Focus::Convs);
            app.on_key(key(back));
            assert!(app.focus == Focus::Convs);
        }
    }

    #[test]
    fn back_unwinds_one_view_before_returning_home() {
        let mut app = App::new(
            Corpus::stub(&[]),
            Tz::Utc,
            30.0,
            false,
            false,
            PathBuf::new(),
            PathBuf::new(),
            60,
            None,
            None,
        );
        app.merge_conversations(vec![json!({"id":"C1", "name":"test", "is_member":true})]);
        for action in [Action::Back] {
            app.open_conv(0);
            app.stack.push(View::Raw {
                title: "raw".into(),
                browser: crate::raw::Browser::new(&json!({"text":"test"})),
            });
            app.on_msg_key(Some(action));
            assert!(app.open.is_some());
            assert!(app.stack.is_empty());
            app.on_msg_key(Some(action));
            assert!(app.open.is_none());
            assert!(app.focus == Focus::Convs);
            assert!(app.status.is_empty());
            assert_eq!(app.title(), "messages");
        }
    }

    #[test]
    fn slash_lines_parse_into_commands() {
        assert_eq!(
            parse_command("find team nginx"),
            Some(Command::Find("team nginx".into()))
        );
        assert_eq!(parse_command("/Search x"), Some(Command::Find("x".into())));
        assert_eq!(parse_command("find"), Some(Command::Find(String::new())));
        assert_eq!(
            parse_command("leave #kudos-to-you"),
            Some(Command::Leave("#kudos-to-you".into()))
        );
        assert_eq!(parse_command("leave"), Some(Command::Leave(String::new())));
        assert_eq!(parse_command("leav #x"), None);
        assert_eq!(
            parse_command("cache stop #x"),
            Some(Command::Cache("stop".into(), "#x".into()))
        );
        assert_eq!(parse_command("cache purge"), None);
        assert_eq!(
            parse_command("unmute"),
            Some(Command::Mute(false, String::new()))
        );
        assert_eq!(
            parse_command("/colorpalette"),
            Some(Command::ColorPalette(String::new()))
        );
        assert_eq!(
            parse_command("colors"),
            Some(Command::ColorPalette(String::new()))
        );
        assert_eq!(
            parse_command("colorpalette vintage"),
            Some(Command::ColorPalette("vintage".into()))
        );
        assert_eq!(parse_command("/version"), Some(Command::Version));
        assert_eq!(parse_command("version now"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn mentions_link_known_handles_only() {
        let users = |h: &str| (h == "gabriel.clima").then(|| "U1".to_string());
        assert_eq!(
            link_mentions("hi @gabriel.clima, see @nobody and @gabriel.clima.", users),
            "hi <@U1>, see @nobody and <@U1>."
        );
        assert_eq!(
            link_mentions("@channel @here (@gabriel.clima)", users),
            "@channel @here (@gabriel.clima)"
        );
        assert_eq!(link_mentions("", users), "");
    }

    #[test]
    fn unread_divider_opens_at_the_first_message_past_the_marker() {
        // Two days, four messages; the marker sits after the second.
        let a = Archive::stub(&[], &[]);
        let corpus = Corpus::stub(&[]);
        let day = 86_400;
        let msgs = vec![
            msg(day + 10, "old one"),
            msg(day + 20, "old two"),
            msg(2 * day + 10, "new one"),
            msg(2 * day + 20, "new two"),
        ];
        let read_marker = 2 * day * 1_000_000; // between day 1 and day 2
        let mut list = MsgList::new(msgs, false);
        let ctx = Ctx {
            archive: Some(&a),
            corpus: &corpus,
            tz: Tz::Utc,
            image_font: None,
            last_read: Some(read_marker),
            palette: &Palette::default(),
        };
        list.rebuild(&ctx, 60);
        let texts: Vec<String> = list.flat.iter().map(|fl| line_text(&fl.line)).collect();
        let new_line = texts
            .iter()
            .position(|t| t.contains("new"))
            .expect("a new divider");
        // The divider is the day-2 header carrying "new", above "new one".
        assert!(texts[new_line].contains("new"), "{:?}", texts[new_line]);
        let body = texts.iter().position(|t| t.contains("new one")).unwrap();
        assert!(new_line < body);
        // Nothing before the marker is flagged.
        assert!(texts[..new_line].iter().all(|t| !t.contains("· new")));
    }

    #[test]
    fn no_marker_means_no_new_divider() {
        let a = Archive::stub(&[], &[]);
        let corpus = Corpus::stub(&[]);
        let mut list = MsgList::new(vec![msg(100, "a"), msg(200, "b")], false);
        let ctx = Ctx {
            archive: Some(&a),
            corpus: &corpus,
            tz: Tz::Utc,
            image_font: None,
            last_read: None,
            palette: &Palette::default(),
        };
        list.rebuild(&ctx, 60);
        assert!(list
            .flat
            .iter()
            .all(|fl| !line_text(&fl.line).contains("new")));
    }
    #[test]
    fn star_aliases_and_stale_snapshots_preserve_confirmed_changes() {
        for word in ["star", "pin"] {
            assert_eq!(parse_command(&format!("/{word} #one")), Some(Command::Star(true, "#one".into())));
        }
        for word in ["unstar", "unpin"] {
            assert_eq!(parse_command(word), Some(Command::Star(false, String::new())));
        }
        let mut app = mute_test_app();
        app.finish_star("C1", true, vec!["C1".into()]);
        app.take_starred_snapshot(0, vec![]);
        assert!(app.starred.contains("C1"));
        app.bg = Some(live::completed_job(JobKind::StarredChannels { gen: app.starred_generation }, Ok(Done::StarredChannels(vec!["D1".into()]))));
        app.tick();
        assert!(!app.starred.contains("C1"));
        assert!(app.starred.contains("D1"));
        app.job = Some(live::completed_job(JobKind::SetStarred, Ok(Done::StarChanged { cid: "D1".into(), starred: false, ids: vec![] })));
        app.tick();
        assert!(app.starred.is_empty());
        assert!(app.status.contains("unstarred in Slack"));
        app.starred.insert("C1".into());
        app.job = Some(live::completed_job(JobKind::SetStarred, Err("denied".into())));
        app.tick();
        assert!(app.starred.contains("C1"));
        assert!(app.starred_pending);
    }

    #[test]
    fn starred_conversations_precede_unreads_and_mutes_with_a_nonselectable_divider() {
        let mut app = mute_test_app();
        app.starred.insert("D1".into());
        app.muted.insert("D1".into());
        app.corpus.convs[0].unread = true;
        for sort in [Sort::Name, Sort::Mine, Sort::Recent, Sort::Size] {
            app.sort = sort;
            app.apply_filter();
            assert_eq!(app.conv(app.filtered[0]).id, "D1");
        }
        app.focus = Focus::Convs;
        app.conv_cursor = 0;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("────────"));
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.conv_cursor, 1);
        assert_eq!(app.conv(app.filtered[app.conv_cursor]).id, "C1");
        assert_eq!(app.filtered.len(), 2);
        app.starred.clear();
        app.apply_filter();
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!((2..10).all(|y| buffer[(2,y)].symbol() != "─"));
    }

    #[test]
    fn combined_archives_find_secondary_files_and_refuse_partial_wipes() {
        let root = std::env::temp_dir().join(format!("slack-union-files-{}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        let file_path = second.join("__uploads/FTEST/image.png");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, b"fixture").unwrap();
        let mut app = mute_test_app();
        let mut archive = crate::archive::Archive::stub(&[], &[]);
        archive.dir = first.clone();
        archive.source_dirs = vec![first.clone(), second];
        app.corpus.archives.push(archive);
        app.corpus.convs[0].archive = 0;
        app.corpus.convs[0].live_only = false;
        let file = crate::archive::FileInfo::from_slack(&json!({"id":"FTEST","title":"image.png","mimetype":"image/png"}),"C1");
        assert_eq!(app.local_file(&file,true),Some(file_path.clone()));
        app.cache_cmd("wipe","#one");
        assert!(app.status.contains("multiple archives"));
        assert!(first.is_dir());
        assert!(file_path.is_file());
        let mut secondary = crate::archive::Archive::stub(&[], &[]);
        secondary.dir = app.corpus.archives[0].source_dirs[1].clone();
        secondary.source_dirs = vec![secondary.dir.clone()];
        secondary.conn.execute_batch("CREATE TABLE MESSAGE(CHANNEL_ID TEXT); INSERT INTO MESSAGE VALUES ('D1');").unwrap();
        app.corpus.archives.push(secondary);
        app.corpus.convs[1].archive = 1;
        app.corpus.convs[1].live_only = false;
        assert!(app.archive_dir(1).unwrap().1);
        let name = app.corpus.convs[1].name.clone();
        app.cache_cmd("wipe", &name);
        assert!(app.status.contains("shared multi-channel archive"));
        assert!(file_path.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    pub(crate) fn mute_test_app() -> App {
        let mut app = App::new(
            Corpus::stub(&[]),
            Tz::Utc,
            30.0,
            false,
            false,
            PathBuf::new(),
            PathBuf::new(),
            0,
            None,
            None,
        );
        app.merge_conversations(vec![
            json!({"id":"C1","name":"one","is_member":true}),
            json!({"id":"D1","user":"U1","is_im":true}),
        ]);
        app
    }
    #[test]
    fn mute_snapshots_replace_removed_ids_and_ignore_stale_reads() {
        let mut app = mute_test_app();
        app.take_muted(vec!["C1".into()]);
        let generation = app.muted_generation;
        app.finish_mute("C1", false, vec!["D1".into()]);
        assert!(!app.muted.contains("C1"));
        assert!(app.muted.contains("D1"));
        app.take_muted_snapshot(generation, vec!["C1".into()]);
        assert!(!app.muted.contains("C1"));
        app.take_muted_snapshot(app.muted_generation, vec![]);
        assert!(app.muted.is_empty());
        assert!(app.corpus.convs.iter().all(|c| !c.muted));
    }
    #[test]
    fn mute_completion_applies_target_not_cursor_and_failures_preserve_state() {
        let mut app = mute_test_app();
        app.job = Some(live::completed_job(
            JobKind::SetMuted,
            Ok(Done::MuteChanged {
                cid: "C1".into(),
                muted: true,
                ids: vec!["C1".into()],
            }),
        ));
        app.conv_cursor = 1;
        app.tick();
        assert!(app.muted.contains("C1"));
        assert!(app.status.contains("#one muted in Slack (verified)"));
        let before = app.muted_generation;
        app.job = Some(live::completed_job(
            JobKind::SetMuted,
            Err("verification failed".into()),
        ));
        app.tick();
        assert!(app.muted.contains("C1"));
        assert!(app.status.contains("verification failed"));
        assert!(app.muted_generation > before);
        // Quiet and foreground result handling must agree.
        app.bg = Some(live::completed_job(
            JobKind::SetMuted,
            Ok(Done::MuteChanged {
                cid: "C1".into(),
                muted: false,
                ids: vec![],
            }),
        ));
        app.tick();
        assert!(app.muted.is_empty());
    }
    #[test]
    fn mute_offline_never_creates_a_local_override() {
        let mut app = mute_test_app();
        app.mute_cmd(true, "#one");
        assert!(app.job.is_none());
        assert!(app.muted.is_empty());
        assert!(app.status.contains("sign-in"));
    }
    #[test]
    fn mute_dispatch_invalidates_reads_and_blocks_preference_scheduling() {
        use std::sync::{mpsc, Mutex};
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let release_receiver = Mutex::new(release_receiver);
        let client = Client::for_test(move |method, _| {
            assert_eq!(method, "users.prefs.setNotifications");
            started_sender.send(()).unwrap();
            release_receiver.lock().unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
            Err("write denied".into())
        });
        let mut app = mute_test_app();
        app.api = Some(Arc::new(client));
        app.live = true;
        let generation = app.muted_generation;
        let (snapshot, snapshot_sender) = live::pending_job(JobKind::MutedChannels { gen: generation });
        app.bg = Some(snapshot);
        app.mute_cmd(true, "#one");
        started_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(app.muted_generation > generation);
        snapshot_sender.send(Ok(Done::MutedChannels(vec!["D1".into()]))).ok().unwrap();
        app.muted_pending = true;
        app.tick();
        assert!(app.muted.is_empty());
        assert!(app.bg.is_none());
        assert!(app.muted_pending);
        assert!(matches!(app.job.as_ref().map(|job| &job.kind), Some(JobKind::SetMuted)));
        // Release the worker without timing-dependent polling of its result.
        release_sender.send(()).unwrap();
        app.api = None;
        let generation = app.muted_generation;
        app.job = Some(live::completed_job(JobKind::SetMuted, Err("write denied".into())));
        app.muted_pending = false;
        app.tick();
        assert!(app.muted_pending);
        assert!(app.muted_generation > generation);
        app.bg = Some(live::completed_job(JobKind::MutedChannels { gen: generation }, Ok(Done::MutedChannels(vec!["D1".into()]))));
        app.tick();
        assert!(app.muted.is_empty());
    }

    #[test]
    fn release_picture_drops_pending_completion_and_keeps_other_images() {
        let mut app = mute_test_app();
        app.images.insert("F1:full".into(), ImageState::Loading);
        app.images.insert("F2:full".into(), ImageState::Loading);
        app.file_job = Some(live::completed_job(JobKind::File { id: "F1:full".into() },
            Ok(Done::File(PathBuf::from("unused-image.png")))));
        app.release_picture("F2:full");
        assert!(app.file_job.is_some());
        app.release_picture("F1:full");
        assert!(app.file_job.is_none());
        app.tick();
        assert!(!app.images.contains_key("F1:full"));
    }

}
