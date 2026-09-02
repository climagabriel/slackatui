//! Application state and key handling. Views stack on top of the timeline:
//! thread, search hits, raw JSON. Esc pops.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;

use serde_json::Value;

use crate::api::Client;
use crate::archive::{ts_to_id, Archive, Conv, Corpus, Kind, Msg, PAGE, SEARCH_CAP};
use crate::live::{self, Done, Job, JobKind};
use crate::render::{self, Ctx, Tz};

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
            });
        }
        let mut prev_day = None;
        for (i, m) in self.msgs.iter().enumerate() {
            let day = ctx.tz.day(m.secs());
            if prev_day != Some(day) {
                let text = ctx.tz.fmt(m.secs(), "%a %Y-%m-%d");
                self.flat.push(FlatLine {
                    msg: None,
                    line: render::divider(&text, width),
                });
                prev_day = Some(day);
            }
            let lines = render::message_lines(m, ctx, width, self.in_thread);
            self.first.push(self.flat.len());
            for line in lines {
                self.flat.push(FlatLine { msg: Some(i), line });
            }
            self.last.push(self.flat.len().saturating_sub(1));
            if self.in_thread && i == 0 && self.msgs.len() > 1 {
                let n = self.msgs.len() - 1;
                let text = format!("{n} {}", if n == 1 { "reply" } else { "replies" });
                self.flat.push(FlatLine {
                    msg: None,
                    line: render::divider(&text, width),
                });
            }
        }
        if let Some(note) = &self.bottom_note {
            self.flat.push(FlatLine {
                msg: None,
                line: render::divider(note, width),
            });
        }
        self.flat_w = width;
        self.dirty = false;
        let first = self.first.get(self.cursor).copied().unwrap_or(0);
        self.scroll = if self.align_top {
            first.saturating_sub(1)
        } else {
            first.saturating_sub(offset)
        };
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptKind {
    Filter,
    Search,
    Date,
    Archive,
}

pub enum Mode {
    Normal,
    Prompt {
        kind: PromptKind,
        buf: String,
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
        let needle = self.filter.trim_start_matches(['#', '@']).to_lowercase();
        let mut idx: Vec<usize> = (0..self.corpus.convs.len())
            .filter(|&i| {
                let c = &self.corpus.convs[i];
                needle.is_empty()
                    || c.name.to_lowercase().contains(&needle)
                    || c.id.to_lowercase() == needle
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
                    Some(View::Search { .. }) => self.stack.pop(),
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
                last_id: created,
                live_only: true,
                unread: false,
                mentions: 0,
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
    /// conversations no archive holds.
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
                conv.unread = c
                    .get("has_unreads")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                conv.mentions = c.get("mention_count").and_then(Value::as_i64).unwrap_or(0);
                if let Some(latest) = c.get("latest").and_then(Value::as_str).and_then(ts_to_id) {
                    if latest > conv.last_id {
                        conv.last_id = latest;
                    }
                }
                changed = true;
            }
        }
        if changed {
            self.apply_filter();
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

    fn prompt_archive(&mut self) {
        // A hit from a conversation not cached yet: offer its id.
        let prefill = self
            .selected()
            .filter(|m| self.corpus.conv_by_channel(&m.channel_id).is_none())
            .map(|m| m.channel_id.clone())
            .unwrap_or_default();
        self.mode = Mode::Prompt {
            kind: PromptKind::Archive,
            buf: prefill.clone(),
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
                        self.bg = Some(live::api_counts(c));
                    }
                }
                Ok(Done::Counts(v)) => {
                    self.apply_counts(&v);
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
                    self.bg = Some(live::api_counts(c));
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
        match self
            .stack
            .iter()
            .rev()
            .find(|v| !matches!(v, View::Raw { .. }))
        {
            Some(View::Thread { list, .. }) | Some(View::Search { list, .. }) => Some(list),
            _ => self.open.as_ref().map(|o| &o.list),
        }
    }

    pub fn active_list_mut(&mut self) -> Option<&mut MsgList> {
        match self
            .stack
            .iter_mut()
            .rev()
            .find(|v| !matches!(v, View::Raw { .. }))
        {
            Some(View::Thread { list, .. }) | Some(View::Search { list, .. }) => Some(list),
            _ => self.open.as_mut().map(|o| &mut o.list),
        }
    }

    pub fn in_timeline(&self) -> bool {
        self.stack.is_empty()
    }

    pub fn selected(&self) -> Option<&Msg> {
        self.active_list().and_then(|l| l.selected())
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

    pub fn hints(&self) -> &'static str {
        match (self.focus, self.stack.last()) {
            (Focus::Convs, _) => "j/k move  Enter open  / filter  s sort  a archive from Slack  Tab messages  ? help  q quit",
            (_, Some(View::Raw { .. })) => "j/k scroll  Esc back  q quit",
            (_, Some(View::Thread { .. })) => "j/k move  Enter raw  o show in channel  / search  R refresh  Esc back  q quit",
            (_, Some(View::Search { .. })) => "j/k move  Enter open hit  o show in channel  a archive its channel  Esc back  q quit",
            _ => "j/k move  Enter thread  / search  d date  v raw  g/G ends  r reload  R refresh from Slack  h conversations  ? help  q quit",
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
        match (k.code, ctrl) {
            (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => {
                self.quit = true;
                return;
            }
            (KeyCode::Char('?'), false) => {
                self.help = true;
                return;
            }
            _ => {}
        }
        if let Some(View::Raw { .. }) = self.stack.last() {
            self.on_raw_key(k, ctrl);
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
            (KeyCode::Char('/'), false) => {
                self.mode = Mode::Prompt {
                    kind: PromptKind::Filter,
                    buf: self.filter.clone(),
                    previous: self.filter.clone(),
                };
            }
            (KeyCode::Esc, _) => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.apply_filter();
                }
            }
            (KeyCode::Char('a'), false) => self.prompt_archive(),
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
            (KeyCode::Char('/'), false) => {
                self.mode = Mode::Prompt {
                    kind: PromptKind::Search,
                    buf: String::new(),
                    previous: String::new(),
                };
            }
            (KeyCode::Char('d'), false) => {
                self.mode = Mode::Prompt {
                    kind: PromptKind::Date,
                    buf: String::new(),
                    previous: String::new(),
                };
            }
            (KeyCode::Char('r'), false) => self.reload(),
            (KeyCode::Char('R'), false) => self.refresh(),
            (KeyCode::Char('a'), false) => self.prompt_archive(),
            (KeyCode::Esc, _) | (KeyCode::Char('h'), false) | (KeyCode::Left, _) => {
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
                if kind == PromptKind::Filter {
                    self.filter = previous.clone();
                    self.mode = Mode::Normal;
                    self.apply_filter();
                } else {
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Enter => {
                let text = buf.clone();
                self.mode = Mode::Normal;
                match kind {
                    PromptKind::Filter => {
                        self.filter = text;
                        self.apply_filter();
                    }
                    PromptKind::Search => self.run_search(&text),
                    PromptKind::Date => self.goto_date(&text),
                    PromptKind::Archive => self.archive_new(&text),
                }
            }
            KeyCode::Backspace => {
                buf.pop();
                if kind == PromptKind::Filter {
                    self.filter = buf.clone();
                    self.apply_filter();
                }
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                buf.push(c);
                if kind == PromptKind::Filter {
                    self.filter = buf.clone();
                    self.apply_filter();
                }
            }
            _ => {}
        }
    }
}
