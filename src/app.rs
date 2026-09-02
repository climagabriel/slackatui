//! Application state and key handling. Views stack on top of the timeline:
//! thread, search hits, raw JSON. Esc pops.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;

use crate::archive::{Conv, Corpus, Kind, Msg, PAGE, SEARCH_CAP};
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
}

pub enum View {
    Thread {
        list: MsgList,
    },
    Search {
        query: String,
        list: MsgList,
        capped: bool,
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
}

impl App {
    pub fn new(corpus: Corpus, tz: Tz, half_life_days: f64) -> App {
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
        };
        app.apply_filter();
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
            tz: self.tz,
            channels: &self.corpus.channel_names,
        }
    }

    pub fn open_conv(&mut self, idx: usize) -> bool {
        let conv = &self.corpus.convs[idx];
        let a = &self.corpus.archives[conv.archive];
        let total = a.timeline_count(&conv.id).unwrap_or(0);
        let msgs = match a.timeline_page(&conv.id, None, None, PAGE) {
            Ok(m) => m,
            Err(e) => {
                self.status = format!("{}: {e}", a.rel);
                return false;
            }
        };
        let has_older = (msgs.len() as i64) < total;
        let mut list = MsgList::new(msgs, false);
        list.cursor = list.len().saturating_sub(1);
        self.open = Some(Open {
            conv: idx,
            list,
            total,
            has_older,
            has_newer: false,
        });
        self.stack.clear();
        self.focus = Focus::Msgs;
        self.update_notes();
        self.status.clear();
        true
    }

    fn update_notes(&mut self) {
        let Some(o) = self.open.as_mut() else { return };
        let loaded = o.list.len() as i64;
        o.list.top_note = if o.has_older {
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
        let Some(o) = self.open.as_mut() else { return };
        if !o.has_older {
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
        let Some(o) = self.open.as_mut() else { return };
        if !o.has_newer {
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

    pub fn open_thread(&mut self, root: i64, focus: i64) {
        let Some(o) = self.open.as_ref() else { return };
        let conv = &self.corpus.convs[o.conv];
        let a = &self.corpus.archives[conv.archive];
        let msgs = match a.thread(&conv.id, root) {
            Ok(m) => m,
            Err(e) => {
                self.status = format!("{e}");
                return;
            }
        };
        if msgs.is_empty() {
            self.status = "thread root is not in the archive".to_string();
            return;
        }
        let mut list = MsgList::new(msgs, true);
        list.cursor = list.msgs.iter().position(|m| m.id == focus).unwrap_or(0);
        list.align_top = list.cursor > 0;
        self.stack.push(View::Thread { list });
    }

    pub fn run_search(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        let Some(o) = self.open.as_ref() else { return };
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
                    || ctx.archive.author(m).to_lowercase().contains(&needle)
            })
            .collect();
        if hits.is_empty() {
            self.status = format!("no message matching '{query}' in {}", conv.name);
            return;
        }
        self.status = format!(
            "{} hit{} for '{query}'{}",
            hits.len(),
            if hits.len() == 1 { "" } else { "s" },
            if capped {
                " (first 500 candidates only)"
            } else {
                ""
            }
        );
        // Newest first; the cursor starts on the newest hit.
        let list = MsgList::new(hits, false);
        self.stack.push(View::Search {
            query: query.to_string(),
            list,
            capped,
        });
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
            Some(View::Thread { list, .. }) => {
                let n = list.len().saturating_sub(1);
                if n == 0 {
                    format!("message in {} · no replies", conv.name)
                } else {
                    format!(
                        "thread in {} · {n} {}",
                        conv.name,
                        if n == 1 { "reply" } else { "replies" }
                    )
                }
            }
            Some(View::Search {
                query,
                list,
                capped,
            }) => format!(
                "search '{query}' in {} · {} hit{}{}",
                conv.name,
                list.len(),
                if list.len() == 1 { "" } else { "s" },
                if *capped { " (capped)" } else { "" }
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
            (Focus::Convs, _) => "j/k move  Enter open  / filter  s sort (my activity, name, recent, size)  Tab messages  ? help  q quit",
            (_, Some(View::Raw { .. })) => "j/k scroll  Esc back  q quit",
            (_, Some(View::Thread { .. })) => "j/k move  Enter raw  o show in channel  / search  Esc back  q quit",
            (_, Some(View::Search { .. })) => "j/k move  Enter thread  o show in channel  Esc back  q quit",
            _ => "j/k move  Enter thread  / search  d date  v raw  g/G ends  r reload  h conversations  ? help  q quit",
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
            (KeyCode::Char('d'), true) | (KeyCode::PageDown, _) => {
                *scroll = (*scroll + height / 2).min(max)
            }
            (KeyCode::Char('u'), true) | (KeyCode::PageUp, _) => {
                *scroll = scroll.saturating_sub(height / 2)
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
            (KeyCode::Char('d'), true) | (KeyCode::PageDown, _) => {
                self.conv_cursor = (self.conv_cursor + 10).min(last)
            }
            (KeyCode::Char('u'), true) | (KeyCode::PageUp, _) => {
                self.conv_cursor = self.conv_cursor.saturating_sub(10)
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
            (KeyCode::Char('d'), true) | (KeyCode::PageDown, _) => {
                if let Some(l) = self.active_list_mut() {
                    l.move_lines(height / 2);
                }
            }
            (KeyCode::Char('u'), true) | (KeyCode::PageUp, _) => {
                if let Some(l) = self.active_list_mut() {
                    l.move_lines(-(height / 2));
                }
            }
            (KeyCode::Enter, _) | (KeyCode::Char('l'), false) | (KeyCode::Right, _) => {
                if matches!(self.stack.last(), Some(View::Thread { .. })) {
                    self.open_raw();
                } else if let Some(m) = self.selected() {
                    let (root, id) = (m.thread_root(), m.id);
                    self.open_thread(root, id);
                }
            }
            (KeyCode::Char('v'), false) => self.open_raw(),
            (KeyCode::Char('o'), false) => {
                if let (false, Some(m)) = (timeline, self.selected()) {
                    let root = m.thread_root();
                    self.stack.clear();
                    self.jump_to(root);
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
