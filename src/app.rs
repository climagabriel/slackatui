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
use crate::edit::Editor;
use crate::live::{self, Done, Job, JobKind};
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
    pub flat: Vec<FlatLine>,
    pub first: Vec<usize>,
    pub last: Vec<usize>,
    flat_w: usize,
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

    /// Render every message at this width, keeping the cursor where it was
    /// on screen.
    pub fn rebuild(&mut self, ctx: &Ctx, width: usize) {
        if !self.dirty && self.flat_w == width {
            return;
        }
        let offset = self
            .first
            .get(self.cursor)
            .map(|f| f.saturating_sub(self.scroll))
            .unwrap_or(0);
        self.flat.clear();
        self.first.clear();
        self.last.clear();
        if let Some(note) = &self.top_note {
            self.flat.push(FlatLine {
                msg: None,
                line: render::divider(note, width),
                image: None,
            });
        }
        let mut prev_day = None;
        let mut new_marked = false;
        for (i, m) in self.msgs.iter().enumerate() {
            let day = ctx.tz.day(m.secs());
            // The first message past the read marker opens the unread part:
            // its day divider lights up, or a "new" line stands in for one.
            let new_here = !new_marked && ctx.last_read.is_some_and(|lr| m.id > lr);
            if prev_day != Some(day) {
                let text = ctx.tz.fmt(m.secs(), "%a %Y-%m-%d");
                let line = if new_here {
                    render::divider_new(&format!("{text} · new"), width)
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
                    line: render::divider_new("new", width),
                    image: None,
                });
            }
            if new_here {
                new_marked = true;
            }
            let rendered = render::message_lines(m, ctx, width, self.in_thread);
            let base = self.flat.len();
            self.first.push(base);
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
            self.last.push(self.flat.len().saturating_sub(1));
            if self.in_thread && i == 0 && self.msgs.len() > 1 {
                let n = self.msgs.len() - 1;
                let text = format!("{n} {}", if n == 1 { "reply" } else { "replies" });
                self.flat.push(FlatLine {
                    msg: None,
                    line: render::divider(&text, width),
                    image: None,
                });
            }
        }
        if let Some(note) = &self.bottom_note {
            self.flat.push(FlatLine {
                msg: None,
                line: render::divider(note, width),
                image: None,
            });
        }
        self.flat_w = width;
        self.dirty = false;
        let first = self.first.get(self.cursor).copied().unwrap_or(0);
        // A fresh list, or a jump, keeps the line above the cursor on screen:
        // that is the day divider.
        let back = if self.align_top || offset == 0 {
            1
        } else {
            offset
        };
        self.scroll = first.saturating_sub(back);
        self.align_top = false;
    }

    pub fn ensure_visible(&mut self, height: usize) {
        if height == 0 || self.first.is_empty() {
            return;
        }
        self.cursor = self.cursor.min(self.first.len() - 1);
        let first = self.first[self.cursor];
        let last = self.last[self.cursor];
        if first < self.scroll {
            self.scroll = first;
        } else if last >= self.scroll + height {
            self.scroll = (last + 1 - height).min(first);
        }
        let max_scroll = self.flat.len().saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);
    }

    pub fn move_cursor(&mut self, delta: isize) {
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

/// The message `e` reacts to.
pub struct ReactTarget {
    pub cid: String,
    pub id: i64,
    pub label: String,
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
    Raw {
        title: String,
        lines: Vec<String>,
        scroll: usize,
    },
    /// Roots of the threads the owner wrote in, across every archive.
    Threads { list: MsgList },
    /// The reaction picker: a searchable emoji list over the messages pane.
    Emoji {
        target: ReactTarget,
        query: Editor,
        cursor: usize,
        /// Indices into the emoji table that match the query.
        matches: Vec<usize>,
    },
    /// One message's images, full pane, one at a time.
    Image {
        files: Vec<crate::archive::FileInfo>,
        index: usize,
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
    /// `C`: cached conversations in light green.
    pub highlight_cached: bool,
    /// `U`: unread conversations at the top of the list.
    pub unreads_first: bool,
    /// The target of the open compose prompt.
    pub compose: Option<Compose>,
    /// The target of a reaction being sent.
    pub react: Option<ReactTarget>,
    /// (name, glyph) for the picker: Unicode emoji, then the workspace's custom ones as `:name:`.
    pub emoji_table: Vec<(String, String)>,
    /// Muted conversation ids, from `<cache>/muted.json`.
    pub muted: HashSet<String>,
    pub muted_loaded: bool,
    /// A message typed and not sent: Esc keeps it for the next `c` on the
    /// same target, so it cannot go to another conversation by reflex.
    pub draft: Option<Draft>,
    /// Bumped by every mark; a counts result from before it is stale.
    pub counts_gen: u64,
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
    ) -> App {
        let mut app = App {
            corpus,
            tz,
            focus: Focus::Convs,
            sort: Sort::Mine,
            filter: String::new(),
            filtered: Vec::new(),
            conv_cursor: 0,
            open: None,
            stack: Vec::new(),
            mode: Mode::Normal,
            help: false,
            status: String::new(),
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
            slackdump,
            poll_every: Duration::from_secs(poll_secs),
            last_poll: Instant::now(),
            last_counts: Instant::now(),
            picker: None,
            inline_images: false,
            images: HashMap::new(),
            inline: HashMap::new(),
            file_job: None,
            highlight_cached: false,
            unreads_first: true,
            compose: None,
            react: None,
            emoji_table: Vec::new(),
            muted: HashSet::new(),
            muted_loaded: false,
            draft: None,
            counts_gen: 0,
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
            Some(View::Raw { .. }) | Some(View::Image { .. }) | Some(View::Emoji { .. }) => {
                Err("close this view first".to_string())
            }
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
        match parse_command(line) {
            None => {
                if self.focus == Focus::Convs {
                    self.filter = filter_before.to_string();
                    self.apply_filter();
                }
                if !line.trim().is_empty() {
                    self.status = format!(
                        "unknown command: {}; commands: find|search TEXT, leave, mute, unmute, cache start|stop|wipe, each with an optional #name",
                        line.split_whitespace().next().unwrap_or("")
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
            Some(Command::Mute(on, name)) => {
                self.restore_filter(filter_before);
                self.mute_cmd(on, &name);
            }
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
        let shared = self
            .corpus
            .convs
            .iter()
            .filter(|x| !x.live_only && x.archive == c.archive)
            .count()
            > 1;
        Some((self.corpus.archives[c.archive].dir.clone(), shared))
    }

    /// `/cache start|stop|wipe [#name]`: archive a conversation, pause its
    /// hourly refresh with a `.paused` marker the refresh script honours,
    /// or delete its archive.
    fn cache_cmd(&mut self, op: &str, name: &str) {
        let idx = match self.target_conv(name) {
            Ok(i) => i,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let cname = self.corpus.convs[idx].name.clone();
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

    /// The muted set follows `<cache>/muted.json`; the flags on the
    /// conversations follow the set, so merged-in conversations get theirs.
    fn sync_muted(&mut self) {
        if !self.muted_loaded {
            self.muted_loaded = true;
            if let Some(ids) = std::fs::read_to_string(self.cache_dir.join("muted.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
            {
                self.muted = ids.into_iter().collect();
            }
        }
        for c in self.corpus.convs.iter_mut() {
            c.muted = self.muted.contains(&c.id);
        }
    }

    /// `/mute [#name]` and `/unmute [#name]`: never, or again, shown as unread.
    fn mute_cmd(&mut self, on: bool, name: &str) {
        let idx = match self.target_conv(name) {
            Ok(i) => i,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let (id, cname) = {
            let c = &self.corpus.convs[idx];
            (c.id.clone(), c.name.clone())
        };
        if on {
            self.muted.insert(id);
        } else {
            self.muted.remove(&id);
        }
        let mut ids: Vec<&String> = self.muted.iter().collect();
        ids.sort();
        let _ = std::fs::create_dir_all(&self.cache_dir);
        let saved = serde_json::to_string(&ids)
            .map_err(|e| e.to_string())
            .and_then(|t| {
                std::fs::write(self.cache_dir.join("muted.json"), t).map_err(|e| e.to_string())
            });
        self.status = match saved {
            Ok(()) if on => format!("{cname} muted: never shown as unread"),
            Ok(()) => format!("{cname} unmuted"),
            Err(e) => format!("{cname}: {e}"),
        };
        self.apply_filter();
        self.mark_all_dirty();
    }

    /// Every message list on screen: the open timeline and the stacked views.
    fn lists_mut(&mut self) -> Vec<&mut MsgList> {
        let mut out: Vec<&mut MsgList> = Vec::new();
        if let Some(o) = self.open.as_mut() {
            out.push(&mut o.list);
        }
        for v in self.stack.iter_mut() {
            match v {
                View::Thread { list, .. } | View::Search { list, .. } | View::Threads { list } => {
                    out.push(list)
                }
                _ => {}
            }
        }
        out
    }

    /// `e`: the reaction picker for the selected message.
    fn react(&mut self) {
        if self.api.is_none() {
            self.status = "reacting needs the Slack sign-in".to_string();
            return;
        }
        let Some(m) = self.selected() else {
            self.status = "no message selected".to_string();
            return;
        };
        let who = m
            .user
            .as_deref()
            .and_then(|u| self.corpus.user_name(u))
            .unwrap_or_else(|| "?".to_string());
        let target = ReactTarget {
            cid: m.channel_id.clone(),
            id: m.id,
            label: format!("react to {who}'s message"),
        };
        self.build_emoji_table();
        let matches = (0..self.emoji_table.len()).collect();
        self.stack.push(View::Emoji {
            target,
            query: Editor::default(),
            cursor: 0,
            matches,
        });
    }

    /// Unicode emoji from the `emojis` crate, then the workspace's custom
    /// names from the cache file; the file is fetched once when missing.
    fn build_emoji_table(&mut self) {
        if !self.emoji_table.is_empty() {
            return;
        }
        for e in emojis::iter() {
            if let Some(sc) = e.shortcode() {
                self.emoji_table
                    .push((sc.to_string(), e.as_str().to_string()));
            }
        }
        let path = self.cache_dir.join("emoji.json");
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
        {
            Some(names) => self.add_custom_emoji(names),
            None => {
                if let (Some(c), true) = (self.api.clone(), self.bg.is_none()) {
                    self.bg = Some(live::api_emoji_list(c));
                }
            }
        }
    }

    fn add_custom_emoji(&mut self, names: Vec<String>) {
        let known: HashSet<String> = self.emoji_table.iter().map(|(n, _)| n.clone()).collect();
        let mut names: Vec<String> = names.into_iter().filter(|n| !known.contains(n)).collect();
        names.sort();
        for n in names {
            let glyph = format!(":{n}:");
            self.emoji_table.push((n, glyph));
        }
        self.emoji_filter();
    }

    /// The picker's matches for its query: names starting with it first,
    /// then names containing it, table order within each group.
    fn emoji_filter(&mut self) {
        let table = &self.emoji_table;
        let Some(View::Emoji {
            query,
            cursor,
            matches,
            ..
        }) = self.stack.last_mut()
        else {
            return;
        };
        let q = query.text.trim().trim_matches(':').to_lowercase();
        if q.is_empty() {
            *matches = (0..table.len()).collect();
        } else {
            let mut starts = Vec::new();
            let mut contains = Vec::new();
            for (i, (name, _)) in table.iter().enumerate() {
                if name.starts_with(&q) {
                    starts.push(i);
                } else if name.contains(&q) {
                    contains.push(i);
                }
            }
            starts.extend(contains);
            *matches = starts;
        }
        *cursor = 0;
    }

    fn on_emoji_key(&mut self, k: KeyEvent, ctrl: bool) {
        let table_len = self.emoji_table.len();
        let Some(View::Emoji {
            query,
            cursor,
            matches,
            ..
        }) = self.stack.last_mut()
        else {
            return;
        };
        match (k.code, ctrl) {
            (KeyCode::Char('c'), true) => self.quit = true,
            (KeyCode::Esc, _) => {
                self.stack.pop();
            }
            (KeyCode::Enter, _) => {
                let picked_index = matches.get(*cursor).copied().filter(|&i| i < table_len);
                let typed = query.text.trim().trim_matches(':').to_string();
                let Some(View::Emoji { target, .. }) = self.stack.pop() else {
                    return;
                };
                let picked = match picked_index {
                    Some(i) => self.emoji_table[i].0.clone(),
                    None => typed,
                };
                if picked.is_empty() {
                    self.status = "no reaction picked".to_string();
                    return;
                }
                self.react = Some(target);
                self.send_reaction(&picked);
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), true) => {
                *cursor = cursor.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), true) => {
                if *cursor + 1 < matches.len() {
                    *cursor += 1;
                }
            }
            (KeyCode::PageUp, _) => *cursor = cursor.saturating_sub(10),
            (KeyCode::PageDown, _) => *cursor = (*cursor + 10).min(matches.len().saturating_sub(1)),
            _ => {
                if query.key(k, false) {
                    self.emoji_filter();
                }
            }
        }
    }

    /// Enter in the reaction prompt: add the reaction, or remove it when it
    /// is already yours.
    fn send_reaction(&mut self, text: &str) {
        let Some(t) = self.react.take() else {
            return;
        };
        let Some(name) = reaction_name(text) else {
            self.status = "no reaction name".to_string();
            return;
        };
        let Some(c) = self.api.clone() else {
            self.status = "reacting needs the Slack sign-in".to_string();
            return;
        };
        if self.job.is_some() {
            self.status = "a fetch is already running; press e again in a moment".to_string();
            return;
        }
        let me = self.corpus.me.clone().unwrap_or_default();
        let mine = self
            .lists_mut()
            .iter()
            .flat_map(|l| l.msgs.iter())
            .find(|m| m.id == t.id)
            .map(|m| has_reaction(&m.data, &name, &me))
            .unwrap_or(false);
        self.job = Some(live::api_react(c, t.cid, t.id, name, !mine));
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
        if text.trim().is_empty() {
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
        self.job = Some(live::api_send(c, t.conv, t.cid, t.thread, wire));
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
                !twin && (rank(&c.name).is_some() || c.id.to_lowercase() == needle)
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
        // a typed filter still puts the closer name matches above. A muted
        // conversation never counts as unread here.
        if self.unreads_first {
            idx.sort_by_key(|&i| !convs[i].unread || convs[i].muted);
        }
        if !needle.is_empty() {
            idx.sort_by_key(|&i| rank(&convs[i].name).unwrap_or(2));
        }
        // Keep the highlighted conversation highlighted across a re-sort.
        let current = self.filtered.get(self.conv_cursor).copied();
        self.filtered = idx;
        self.conv_cursor = current
            .and_then(|c| self.filtered.iter().position(|&i| i == c))
            .unwrap_or(0)
            .min(self.filtered.len().saturating_sub(1));
    }

    // ------------------------------------------------------------- loading

    fn ctx_for(&self, conv: usize) -> Ctx<'_> {
        Ctx {
            archive: &self.corpus.archives[self.corpus.convs[conv].archive],
            corpus: &self.corpus,
            tz: self.tz,
            image_font: self.image_font(),
            last_read: None,
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
        if !o.has_newer || o.api_only {
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
        if complete {
            return;
        }
        // The cache answers first, in either format; Slack only when it cannot.
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
        if let Some(c) = self.api.clone() {
            self.job = Some(live::api_thread(c, &self.cache_dir, cid, root, focus));
        } else if self.slackdump && self.live {
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
        let candidates = match ctx.archive.search(&conv.id, query, SEARCH_CAP) {
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
            }
        }
        self.status = format!("{n} new from Slack");
    }

    /// An older page from Slack for a conversation with no archive.
    fn prepend_older(&mut self, conv: usize, mut msgs: Vec<Msg>) {
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
        o.has_older = n >= PAGE;
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
                let before = (conv.unread, conv.mentions, conv.last_read, conv.last_id);
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
                changed |= before != (conv.unread, conv.mentions, conv.last_read, conv.last_id);
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
            if last > 0 {
                c.last_id = last;
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
                let dir = self.corpus.archives[conv.archive]
                    .dir
                    .join("__uploads")
                    .join(&f.id);
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    if let Some(e) = rd.flatten().find(|e| e.path().is_file()) {
                        return Some(e.path());
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
        let Some(client) = self.api.clone() else {
            return;
        };
        let next = self.images.iter().find_map(|(k, s)| match s {
            ImageState::Queued { url, dest } => Some((k.clone(), url.clone(), dest.clone())),
            _ => None,
        });
        if let Some((key, url, dest)) = next {
            self.images.insert(key.clone(), ImageState::Loading);
            self.file_job = Some(live::fetch_file(client, key, url, dest));
        }
    }

    /// The inline encoding of a thumbnail for a cell box, cached by size.
    pub fn inline_protocol(&mut self, id: &str, cols: u16, rows: u16) -> Option<&Protocol> {
        let fresh = matches!(self.inline.get(id), Some((c, r, _)) if *c == cols && *r == rows);
        if !fresh {
            let picker = self.picker.as_ref()?;
            let img = match self.images.get(id) {
                Some(ImageState::Ready(img)) => img,
                _ => return None,
            };
            let size = ratatui::layout::Size::new(cols, rows);
            let proto = picker
                .new_protocol(img.clone(), size, ratatui_image::Resize::Fit(None))
                .ok()?;
            self.inline.insert(id.to_string(), (cols, rows, proto));
        }
        self.inline.get(id).map(|(_, _, p)| p)
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
    fn finish_archive(&mut self, dir: &Path, spec: &str) {
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
                if let Some(&idx) = new.first() {
                    self.conv_cursor = self.filtered.iter().position(|&i| i == idx).unwrap_or(0);
                    self.open_conv(idx);
                }
                self.status = format!("archived {name} into {}", final_dir.display());
            }
            Err(e) => self.status = e,
        }
    }

    /// Advance the spinner and collect a finished job.
    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
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
        // The quiet slot: sign-in, the conversation list, counts, tails.
        if let Some(outcome) = self.bg.as_ref().and_then(|j| j.poll()) {
            let job = self.bg.take().expect("polled");
            match outcome {
                Ok(Done::Auth(client, who)) => {
                    self.api = Some(client.clone());
                    self.status = format!("signed in as {who}");
                    self.bg = Some(live::api_conversations(client));
                }
                Ok(Done::Conversations(list)) => {
                    self.merge_conversations(list);
                    if let Some(c) = self.api.clone() {
                        self.bg = Some(live::api_counts(c, self.counts_gen));
                    }
                }
                Ok(Done::Counts(v)) => {
                    // A counts snapshot taken before a mark would undo it.
                    if matches!(job.kind, JobKind::Counts { gen } if gen == self.counts_gen) {
                        self.apply_counts(&v);
                    }
                    self.last_counts = Instant::now();
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
        if let Some(c) = self.api.clone() {
            if self.bg.is_none() && self.poll_every.as_secs() > 0 {
                if self.last_poll.elapsed() >= self.poll_every {
                    self.last_poll = Instant::now();
                    if let Some(o) = self.open.as_ref() {
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
        let done = match outcome {
            Ok(d) => d,
            Err(e) => {
                self.status = format!("Slack: {e}");
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
            (JobKind::Refresh { conv, before }, Done::Refreshed) => {
                self.refresh_conv_stats(conv);
                self.open_conv(conv);
                let new = self.open.as_ref().map(|o| o.total).unwrap_or(before) - before;
                self.status = format!(
                    "refreshed: {new} new top-level message{}",
                    if new == 1 { "" } else { "s" }
                );
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
            (JobKind::ArchiveNew { spec }, Done::Archived(dir)) => self.finish_archive(&dir, &spec),
            (JobKind::Tail { conv }, Done::Messages(msgs)) => self.append_tail(conv, msgs, false),
            (JobKind::Older { conv }, Done::Messages(msgs)) => self.prepend_older(conv, msgs),
            (JobKind::Mark { conv, id }, Done::Marked) => {
                let c = &mut self.corpus.convs[conv];
                c.last_read = id;
                c.unread = c.last_id > id;
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
            (JobKind::Send { conv, thread }, Done::Sent(msg)) => {
                let msg = *msg;
                self.draft = None;
                let name = self.corpus.convs[conv].name.clone();
                let c = &mut self.corpus.convs[conv];
                if msg.id > c.last_id {
                    c.last_id = msg.id;
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
            (JobKind::EmojiList, Done::EmojiList(names)) => {
                let path = self.cache_dir.join("emoji.json");
                if let Ok(t) = serde_json::to_string(&names) {
                    let _ = std::fs::create_dir_all(&self.cache_dir);
                    let _ = std::fs::write(path, t);
                }
                if !self.emoji_table.is_empty() {
                    self.add_custom_emoji(names);
                }
            }
            (JobKind::Leave { conv }, Done::Left) => {
                let c = &mut self.corpus.convs[conv];
                c.left = true;
                c.unread = false;
                c.mentions = 0;
                let name = c.name.clone();
                self.apply_filter();
                self.mark_all_dirty();
                self.status = format!("left {name}");
            }
            (JobKind::React { id, name, add }, Done::Reacted) => {
                let me = self.corpus.me.clone().unwrap_or_default();
                let mut touched = 0;
                for list in self.lists_mut() {
                    for m in list.msgs.iter_mut().filter(|m| m.id == id) {
                        patch_reaction(&mut m.data, &name, &me, add);
                        touched += 1;
                    }
                    if touched > 0 {
                        list.mark_dirty();
                    }
                }
                self.status = if add {
                    format!("reacted :{name}:")
                } else {
                    format!("reaction :{name}: removed")
                };
            }
            _ => {}
        }
    }

    pub fn open_raw(&mut self) {
        let Some(m) = self.active_list().and_then(|l| l.selected()).cloned() else {
            return;
        };
        let title = format!("raw · {}", m.ts);
        let text = serde_json::to_string_pretty(&m.data).unwrap_or_default();
        let lines = text.lines().map(str::to_string).collect();
        self.stack.push(View::Raw {
            title,
            lines,
            scroll: 0,
        });
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
                View::Raw { .. } | View::Image { .. } | View::Emoji { .. }
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
                View::Raw { .. } | View::Image { .. } | View::Emoji { .. }
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
        let Some(o) = self.open.as_ref() else {
            return "messages".to_string();
        };
        let conv = &self.corpus.convs[o.conv];
        let a = &self.corpus.archives[conv.archive];
        match self.stack.last() {
            Some(View::Raw { title, .. }) => format!("{title} · {}", conv.name),
            Some(View::Image { files, index, .. }) => {
                let f = &files[*index];
                format!(
                    "image {}/{} · {} · {}x{} · {}",
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
            Some(View::Emoji { matches, .. }) => {
                format!(
                    "pick a reaction · {} matching · {}",
                    matches.len(),
                    conv.name
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
                    a.rel
                )
            }
        }
    }

    // ----------------------------------------------------------------- keys

    pub fn on_key(&mut self, k: KeyEvent) {
        if self.help {
            self.help = false;
            return;
        }
        if matches!(self.mode, Mode::Prompt { .. }) {
            self.on_prompt_key(k);
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(View::Emoji { .. }) = self.stack.last() {
            self.on_emoji_key(k, ctrl);
            return;
        }
        match (k.code, ctrl) {
            (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => {
                self.quit = true;
                return;
            }
            (KeyCode::Char('?'), false) | (KeyCode::Char('H'), false) => {
                self.help = true;
                return;
            }
            _ => {}
        }
        if let Some(View::Raw { .. }) = self.stack.last() {
            self.on_raw_key(k, ctrl);
            return;
        }
        if let Some(View::Image {
            files,
            index,
            shown,
        }) = self.stack.last_mut()
        {
            match k.code {
                KeyCode::Char('j')
                | KeyCode::Down
                | KeyCode::Char('l')
                | KeyCode::Right
                | KeyCode::Char('n') => {
                    if *index + 1 < files.len() {
                        *index += 1;
                        *shown = None;
                    }
                }
                KeyCode::Char('k')
                | KeyCode::Up
                | KeyCode::Char('h')
                | KeyCode::Left
                | KeyCode::Char('p') => {
                    if *index > 0 {
                        *index -= 1;
                        *shown = None;
                    }
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('i') => {
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
        match self.focus {
            Focus::Convs => self.on_conv_key(k, ctrl),
            Focus::Msgs => self.on_msg_key(k, ctrl),
        }
    }

    fn on_raw_key(&mut self, k: KeyEvent, ctrl: bool) {
        let height = self.msgs_height.max(1);
        let Some(View::Raw { lines, scroll, .. }) = self.stack.last_mut() else {
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

    fn on_conv_key(&mut self, k: KeyEvent, ctrl: bool) {
        let n = self.filtered.len();
        let last = n.saturating_sub(1);
        match (k.code, ctrl) {
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => {
                self.conv_cursor = (self.conv_cursor + 1).min(last)
            }
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => {
                self.conv_cursor = self.conv_cursor.saturating_sub(1)
            }
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => self.conv_cursor = 0,
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => self.conv_cursor = last,
            (KeyCode::Char('d'), true) => self.conv_cursor = (self.conv_cursor + 10).min(last),
            (KeyCode::Char('u'), true) => self.conv_cursor = self.conv_cursor.saturating_sub(10),
            (KeyCode::Char('f'), true) | (KeyCode::PageDown, _) => {
                self.conv_cursor = (self.conv_cursor + 20).min(last)
            }
            (KeyCode::Char('b'), true) | (KeyCode::PageUp, _) => {
                self.conv_cursor = self.conv_cursor.saturating_sub(20)
            }
            (KeyCode::Enter, _) | (KeyCode::Char('l'), false) | (KeyCode::Right, _) => {
                if let Some(&idx) = self.filtered.get(self.conv_cursor) {
                    if self.open.as_ref().map(|o| o.conv) == Some(idx) {
                        self.focus = Focus::Msgs;
                    } else {
                        self.open_conv(idx);
                    }
                }
            }
            (KeyCode::Tab, _) => {
                if self.open.is_some() {
                    self.focus = Focus::Msgs;
                }
            }
            (KeyCode::Char('/'), false) => self.open_command(),
            (KeyCode::Esc, _) => self.go_home(),
            (KeyCode::Char('a'), false) => self.prompt_archive(),
            (KeyCode::Char('T'), false) => self.open_my_threads(),
            (KeyCode::Char('C'), false) => {
                self.highlight_cached = !self.highlight_cached;
            }
            (KeyCode::Char('U'), false) => self.toggle_unreads_first(),
            (KeyCode::Char('c'), false) => self.compose(),
            (KeyCode::Char('e'), false) => self.react(),
            (KeyCode::Char('m'), false) => self.mark_read(),
            (KeyCode::Char('M'), false) => self.mark_unread(),
            (KeyCode::Char('s'), false) => {
                self.sort = self.sort.next();
                self.apply_filter();
                self.status = format!("sorted by {}", self.sort_label());
                if self.sort == Sort::Mine && self.corpus.me.is_none() {
                    self.status =
                        "own user id unknown (no DM archive): set SLACK_SELF_USER_ID".to_string();
                }
            }
            _ => {}
        }
    }

    fn on_msg_key(&mut self, k: KeyEvent, ctrl: bool) {
        if self.open.is_none() {
            self.focus = Focus::Convs;
            return;
        }
        let height = self.msgs_height.max(2) as isize;
        let timeline = self.in_timeline();
        match (k.code, ctrl) {
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => {
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
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => {
                let at_start = self.active_list().map(|l| l.cursor == 0).unwrap_or(true);
                if at_start && timeline {
                    self.load_older();
                } else if let Some(l) = self.active_list_mut() {
                    l.move_cursor(-1);
                }
            }
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => {
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
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => {
                let has_newer = self.open.as_ref().is_some_and(|o| o.has_newer);
                if timeline && has_newer {
                    self.reload();
                } else if let Some(l) = self.active_list_mut() {
                    l.cursor = l.len().saturating_sub(1);
                }
            }
            (KeyCode::Char('d'), true) => {
                if let Some(l) = self.active_list_mut() {
                    l.move_lines(height / 2);
                }
            }
            (KeyCode::Char('u'), true) => {
                if let Some(l) = self.active_list_mut() {
                    l.move_lines(-(height / 2));
                }
            }
            (KeyCode::Char('f'), true) | (KeyCode::PageDown, _) => {
                if let Some(l) = self.active_list_mut() {
                    l.move_lines(height - 1);
                }
            }
            (KeyCode::Char('b'), true) | (KeyCode::PageUp, _) => {
                if let Some(l) = self.active_list_mut() {
                    l.move_lines(-(height - 1));
                }
            }
            (KeyCode::Enter, _) | (KeyCode::Char('l'), false) | (KeyCode::Right, _) => {
                if matches!(self.stack.last(), Some(View::Thread { .. })) {
                    self.open_raw();
                } else if let Some(m) = self.selected() {
                    let (cid, root, id) = (m.channel_id.clone(), m.thread_root(), m.id);
                    self.open_hit(cid, root, id);
                }
            }
            (KeyCode::Char('v'), false) => self.open_raw(),
            (KeyCode::Char('o'), false) => {
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
            (KeyCode::Char('/'), false) => self.open_command(),
            (KeyCode::Char('d'), false) => {
                self.mode = Mode::Prompt {
                    kind: PromptKind::Date,
                    buf: Editor::default(),
                    previous: String::new(),
                };
            }
            (KeyCode::Char('r'), false) => self.reload(),
            (KeyCode::Char('R'), false) => self.refresh(),
            (KeyCode::Char('a'), false) => self.prompt_archive(),
            (KeyCode::Char('T'), false) => self.open_my_threads(),
            (KeyCode::Char('C'), false) => {
                self.highlight_cached = !self.highlight_cached;
            }
            (KeyCode::Char('U'), false) => self.toggle_unreads_first(),
            (KeyCode::Char('c'), false) => self.compose(),
            (KeyCode::Char('e'), false) => self.react(),
            (KeyCode::Char('i'), false) => self.open_images(),
            (KeyCode::Char('m'), false) => self.mark_read(),
            (KeyCode::Char('M'), false) => self.mark_unread(),
            (KeyCode::Char('I'), false) => {
                self.inline_images = !self.inline_images && self.picker.is_some();
                self.mark_all_dirty();
                self.status = if self.inline_images {
                    "inline images on"
                } else {
                    "inline images off"
                }
                .to_string();
            }
            (KeyCode::Esc, _) => {
                // Unwind one stacked view; from the bare timeline, straight home.
                if self.stack.pop().is_none() {
                    self.go_home();
                }
            }
            (KeyCode::Char('h'), false) | (KeyCode::Left, _) => {
                // Back out one view, keeping the conversation open to browse the list.
                if self.stack.pop().is_none() {
                    self.focus = Focus::Convs;
                }
            }
            (KeyCode::Tab, _) => self.focus = Focus::Convs,
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
                }
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
                }
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
    /// `cache start|stop|wipe [#name]`.
    Cache(String, String),
    /// `mute [#name]` (true) and `unmute [#name]` (false).
    Mute(bool, String),
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
        "mute" => Some(Command::Mute(true, rest.to_string())),
        "unmute" => Some(Command::Mute(false, rest.to_string())),
        "cache" => {
            let (op, name) = match rest.split_once(char::is_whitespace) {
                Some((o, n)) => (o, n.trim()),
                None => (rest, ""),
            };
            matches!(op, "start" | "stop" | "wipe")
                .then(|| Command::Cache(op.to_string(), name.to_string()))
        }
        _ => None,
    }
}

/// A typed reaction as Slack names it: `eyes`, `:eyes:` or the emoji itself.
fn reaction_name(text: &str) -> Option<String> {
    let s = text.trim().trim_matches(':').trim();
    if s.is_empty() {
        return None;
    }
    if s.is_ascii() {
        return Some(s.to_lowercase());
    }
    emojis::get(s)
        .and_then(|e| e.shortcode())
        .map(str::to_string)
}

/// Whether `me` already reacted with `name` on a message.
fn has_reaction(data: &Value, name: &str, me: &str) -> bool {
    data.get("reactions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("name").and_then(Value::as_str) == Some(name))
        .any(|r| {
            r.get("users")
                .and_then(Value::as_array)
                .map(|u| u.iter().any(|x| x.as_str() == Some(me)))
                .unwrap_or(false)
        })
}

/// The message JSON after your reaction `name` was added or removed, as
/// Slack would report it.
fn patch_reaction(data: &mut Value, name: &str, me: &str, add: bool) {
    let obj = match data.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let rs = obj
        .entry("reactions")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(rs) = rs.as_array_mut() else {
        return;
    };
    let at = rs
        .iter()
        .position(|r| r.get("name").and_then(Value::as_str) == Some(name));
    match (at, add) {
        (None, true) => rs.push(serde_json::json!({"name": name, "users": [me], "count": 1})),
        (None, false) => {}
        (Some(i), add) => {
            let r = &mut rs[i];
            let mut users: Vec<String> = r
                .get("users")
                .and_then(Value::as_array)
                .map(|u| {
                    u.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let had = users.iter().any(|u| u == me);
            if add && !had {
                users.push(me.to_string());
            } else if !add && had {
                users.retain(|u| u != me);
            } else {
                return;
            }
            let count =
                r.get("count").and_then(Value::as_i64).unwrap_or(0) + if add { 1 } else { -1 };
            if count <= 0 {
                rs.remove(i);
            } else {
                r["users"] = Value::Array(users.into_iter().map(Value::String).collect());
                r["count"] = Value::from(count);
            }
        }
    }
    if rs.is_empty() {
        obj.remove("reactions");
    }
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
mod tests {
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
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn reactions_toggle_in_the_message_json() {
        let mut d = serde_json::json!({"text": "x"});
        patch_reaction(&mut d, "eyes", "U1", true);
        assert_eq!(
            d["reactions"],
            serde_json::json!([{"name": "eyes", "users": ["U1"], "count": 1}])
        );
        assert!(has_reaction(&d, "eyes", "U1"));
        patch_reaction(&mut d, "eyes", "U2", true);
        assert_eq!(d["reactions"][0]["count"], 2);
        patch_reaction(&mut d, "eyes", "U1", false);
        assert_eq!(
            d["reactions"],
            serde_json::json!([{"name": "eyes", "users": ["U2"], "count": 1}])
        );
        assert!(!has_reaction(&d, "eyes", "U1"));
        patch_reaction(&mut d, "eyes", "U2", false);
        assert!(d.get("reactions").is_none());
        patch_reaction(&mut d, "eyes", "U1", false);
        assert!(d.get("reactions").is_none());
        assert_eq!(reaction_name(" :Eyes: "), Some("eyes".to_string()));
        assert_eq!(reaction_name("👀"), Some("eyes".to_string()));
        assert_eq!(reaction_name("+1"), Some("+1".to_string()));
        assert_eq!(reaction_name("::"), None);
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
            archive: &a,
            corpus: &corpus,
            tz: Tz::Utc,
            image_font: None,
            last_read: Some(read_marker),
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
            archive: &a,
            corpus: &corpus,
            tz: Tz::Utc,
            image_font: None,
            last_read: None,
        };
        list.rebuild(&ctx, 60);
        assert!(list
            .flat
            .iter()
            .all(|fl| !line_text(&fl.line).contains("new")));
    }
}
