//! Slack text into terminal lines: mrkdwn (`<@U..>`, `<url|label>`, entities,
//! `*bold*`, fences), Block Kit `rich_text` and `section` blocks, legacy
//! attachments (what alert bots post), then greedy word-wrap into styled
//! ratatui lines.

use chrono::{Datelike, Local, NaiveDate, TimeZone, Utc};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use unicode_segmentation::UnicodeSegmentation;

use crate::archive::{Archive, Corpus, FileInfo, Msg};
use crate::palette::{Palette, Role};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tz {
    Utc,
    Local,
}

impl Tz {
    pub fn fmt(self, secs: i64, f: &str) -> String {
        let s = match self {
            Tz::Utc => Utc
                .timestamp_opt(secs, 0)
                .single()
                .map(|d| d.format(f).to_string()),
            Tz::Local => Local
                .timestamp_opt(secs, 0)
                .single()
                .map(|d| d.format(f).to_string()),
        };
        s.unwrap_or_else(|| "?".to_string())
    }

    /// Day number, for "did the day change between two messages".
    pub fn day(self, secs: i64) -> i64 {
        match self {
            Tz::Utc => Utc
                .timestamp_opt(secs, 0)
                .single()
                .map(|d| d.num_days_from_ce() as i64),
            Tz::Local => Local
                .timestamp_opt(secs, 0)
                .single()
                .map(|d| d.num_days_from_ce() as i64),
        }
        .unwrap_or(0)
    }

    pub fn date_label(self, secs: i64, today: i64) -> String {
        let day = self.day(secs);
        if day != 0 && day == today { "Today".into() }
        else { self.fmt(secs, "%a %Y-%m-%d") }
    }

    /// Midnight of a calendar day, as unix seconds.
    pub fn midnight(self, date: NaiveDate) -> Option<i64> {
        let ndt = date.and_hms_opt(0, 0, 0)?;
        match self {
            Tz::Utc => Some(Utc.from_utc_datetime(&ndt).timestamp()),
            Tz::Local => Local
                .from_local_datetime(&ndt)
                .single()
                .map(|d| d.timestamp()),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tz::Utc => "UTC",
            Tz::Local => "local time",
        }
    }
}

pub struct Ctx<'a> {
    /// The archive of the conversation on screen: its users and channels first.
    pub archive: Option<&'a Archive>,
    /// Every other archive: a user or channel named elsewhere still resolves.
    pub corpus: &'a Corpus,
    pub tz: Tz,
    /// Cell size in pixels when inline images are on: rows get reserved for them.
    pub image_font: Option<(u16, u16)>,
    /// The owner's read marker in this timeline: what follows it is new.
    pub last_read: Option<i64>,
    pub palette: &'a Palette,
}

/// Rows reserved under a message for one image, at their position.
#[derive(Clone, Debug)]
pub enum ImageSource {
    File(FileInfo),
}

#[derive(Clone, Debug)]
pub struct ImageSlot {
    pub source: ImageSource,
    /// Index of the first reserved line within the message's lines.
    pub line: usize,
    pub cols: u16,
    pub rows: u16,
}

pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub images: Vec<ImageSlot>,
    /// The part of the message each line belongs to, named after the function
    /// that built it, and empty for a line no tag belongs on: the rows a
    /// picture is reserved. One entry per line. Only a draw with `/labels` on
    /// reads them; every other caller drops them, so nothing is stored.
    pub tags: Vec<&'static str>,
}

/// Highlight one line of pretty-printed JSON without changing its contents.
/// Strings cannot span physical lines; escaped quotes stay inside the token.
pub fn json_line<'a>(text: &'a str, palette: &Palette) -> Line<'a> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        let role = match bytes[at] {
            b'"' => {
                at += 1;
                while at < bytes.len() {
                    match bytes[at] {
                        b'\\' => at = (at + 2).min(bytes.len()),
                        b'"' => {
                            at += 1;
                            break;
                        }
                        _ => at += 1,
                    }
                }
                if text[at..].trim_start().starts_with(':') {
                    Role::Link
                } else {
                    Role::Code
                }
            }
            b'-' | b'0'..=b'9' => {
                at += 1;
                while at < bytes.len()
                    && (bytes[at].is_ascii_digit() || b".eE+-".contains(&bytes[at]))
                {
                    at += 1;
                }
                Role::Mention
            }
            b't' | b'f' | b'n' => {
                while at < bytes.len() && bytes[at].is_ascii_alphabetic() {
                    at += 1;
                }
                if &text[start..at] == "null" {
                    Role::ThreadInfo
                } else {
                    Role::Unread
                }
            }
            _ => {
                at += text[at..].chars().next().unwrap().len_utf8();
                Role::InactiveAccent
            }
        };
        spans.push(Span::styled(
            &text[start..at],
            Style::new().fg(palette.get(role)),
        ));
    }
    Line::from(spans)
}

/// Cells an image takes: its natural size at this font, capped to the pane
/// and to 14 rows; the renderer keeps the aspect ratio inside that box.
pub fn image_cells(f: &FileInfo, font: (u16, u16), width: usize) -> Option<(u16, u16)> {
    if !f.is_image() || f.width == 0 || f.height == 0 {
        return None;
    }
    let (fw, fh) = (font.0.max(1) as f64, font.1.max(1) as f64);
    let max_cols = width.saturating_sub(3).clamp(1, 80) as f64;
    let cols = (f.width as f64 / fw).ceil().min(max_cols);
    let rows = (f.height as f64 * (cols * fw / f.width as f64) / fh)
        .ceil()
        .clamp(1.0, 14.0);
    let cols = (f.width as f64 * (rows * fh / f.height as f64) / fw)
        .ceil()
        .min(cols)
        .max(1.0);
    Some((cols as u16, rows as u16))
}

impl Ctx<'_> {
    fn usergroup<'a>(&'a self, id: &'a str) -> &'a str {
        self.corpus
            .usergroups
            .get(id)
            .map(String::as_str)
            .unwrap_or(if id.is_empty() { "group" } else { id })
    }
    fn channel(&self, cid: &str) -> String {
        self.archive
            .and_then(|a| a.channel_name(cid))
            .or_else(|| self.corpus.channel_names.get(cid).cloned())
            .unwrap_or_else(|| cid.to_string())
    }

    pub fn user(&self, uid: &str) -> String {
        if uid == "USLACKBOT" {
            return "Slackbot".to_string();
        }
        self.archive
            .and_then(|a| a.user(uid))
            .or_else(|| self.corpus.user_name(uid))
            .unwrap_or_else(|| uid.to_string())
    }

    pub fn user_is_bot(&self, uid: &str) -> bool {
        self.archive.is_some_and(|a| a.user_is_bot(uid)) || self.corpus.user_is_bot(uid)
    }

    /// Who wrote it, as a reader would name them.
    pub fn author(&self, m: &Msg) -> String {
        let d = &m.data;
        let username = d
            .get("username")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if let Some(uid) = &m.user {
            if let Some(name) = self
                .archive
                .and_then(|a| a.user(uid))
                .or_else(|| self.corpus.user_name(uid))
            {
                return name;
            }
            if uid == "USLACKBOT" {
                return "Slackbot".to_string();
            }
            // A search hit names its poster even when no archive knows the id.
            return username.map(str::to_string).unwrap_or_else(|| uid.clone());
        }
        if let Some(u) = username {
            return u.to_string();
        }
        if let Some(u) = d
            .pointer("/bot_profile/name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return u.to_string();
        }
        if let Some(u) = d
            .get("attachments")
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter()
                    .find_map(|x| x.get("author_name").and_then(Value::as_str))
            })
            .filter(|s| !s.is_empty())
        {
            return u.to_string();
        }
        "bot".to_string()
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Sty {
    pub highlight: Option<ratatui::style::Color>,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    /// Preformatted: preserve whitespace and source line breaks; wrap to fit.
    pub pre: bool,
    pub link: bool,
    pub mention: bool,
    pub quote: bool,
    pub dim: bool,
    /// The archive owner: their name and mentions of them stand out.
    pub me: bool,
}

#[derive(Clone, Debug)]
pub struct Seg {
    pub text: String,
    pub sty: Sty,
    table: Option<Table>,
}

#[derive(Clone, Debug)]
struct Table {
    rows: Vec<Vec<Vec<Seg>>>,
    align: Vec<String>,
}

impl Seg {
    fn new(text: impl Into<String>, sty: Sty) -> Seg {
        Seg {
            text: text.into(),
            sty,
            table: None,
        }
    }
    fn br() -> Seg {
        Seg::new("\n", Sty::default())
    }
    /// A break inside styled text: an empty quoted line keeps its bar.
    fn br_in(sty: Sty) -> Seg {
        Seg::new("\n", sty)
    }
}

pub fn plain(segs: &[Seg]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect()
}

fn has_content(segs: &[Seg]) -> bool {
    segs.iter().any(|seg| seg.table.as_ref().is_some_and(|table| table.rows.iter().any(|row| !row.is_empty()))) || !plain(segs).trim().is_empty()
}

pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn style_of(s: Sty, palette: &Palette) -> Style {
    let mut st = Style::new();
    if s.mention {
        st = st.fg(palette.get(Role::Mention));
    }
    if s.me {
        st = st.fg(palette.get(Role::OwnUsername));
    }
    if s.link {
        st = st
            .fg(palette.get(Role::Link))
            .add_modifier(Modifier::UNDERLINED);
    }
    if s.code || s.pre {
        st = st.fg(palette.get(Role::Code));
    }
    if s.bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    if s.italic {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if s.strike {
        st = st.add_modifier(Modifier::CROSSED_OUT);
    }
    if s.dim {
        st = st.add_modifier(Modifier::DIM);
    }
    if let Some(color) = s.highlight { st = st.fg(color); }
    st
}

// ------------------------------------------------------------------ mrkdwn

/// Slack mrkdwn to segments. `base` carries the surrounding style (an
/// attachment body is quoted, a header is bold).
pub fn mrkdwn(raw: &str, ctx: &Ctx, base: Sty) -> Vec<Seg> {
    let mut out = Vec::new();
    for (i, chunk) in raw.split("```").enumerate() {
        if i % 2 == 1 {
            let body = unescape(chunk);
            let body = body.trim_matches('\n');
            out.push(Seg::br_in(base));
            out.push(Seg::new(body, Sty { pre: true, ..base }));
            out.push(Seg::br_in(base));
        } else {
            for (li, line) in chunk.split('\n').enumerate() {
                if li > 0 {
                    out.push(Seg::br_in(base));
                }
                let (line, quote) = match line.strip_prefix("&gt;") {
                    Some(rest) => (rest.strip_prefix(' ').unwrap_or(rest), true),
                    None => (line, false),
                };
                let cs: Vec<char> = line.chars().collect();
                inline(
                    &cs,
                    Sty {
                        quote: quote || base.quote,
                        ..base
                    },
                    ctx,
                    &mut out,
                );
            }
        }
    }
    out
}

fn flush(buf: &mut String, sty: Sty, out: &mut Vec<Seg>) {
    if !buf.is_empty() {
        out.push(Seg::new(unescape(buf), sty));
        buf.clear();
    }
}

fn find_char(cs: &[char], from: usize, ch: char) -> Option<usize> {
    (from..cs.len()).find(|&j| cs[j] == ch)
}

fn can_open(cs: &[char], i: usize) -> bool {
    let c = cs[i];
    let prev_ok = i == 0 || !cs[i - 1].is_alphanumeric();
    let next_ok = i + 1 < cs.len() && !cs[i + 1].is_whitespace() && cs[i + 1] != c;
    prev_ok && next_ok
}

fn find_close(cs: &[char], i: usize) -> Option<usize> {
    let c = cs[i];
    (i + 2..cs.len()).find(|&j| {
        cs[j] == c
            && !cs[j - 1].is_whitespace()
            && (j + 1 == cs.len() || !cs[j + 1].is_alphanumeric())
    })
}

fn inline(cs: &[char], base: Sty, ctx: &Ctx, out: &mut Vec<Seg>) {
    let mut buf = String::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '<' => {
                if let Some(end) = find_char(cs, i + 1, '>') {
                    let inner: String = cs[i + 1..end].iter().collect();
                    if let Some(segs) = angle(&inner, base, ctx) {
                        flush(&mut buf, base, out);
                        out.extend(segs);
                        i = end + 1;
                        continue;
                    }
                }
            }
            '`' => {
                if let Some(end) = find_char(cs, i + 1, '`') {
                    if end > i + 1 {
                        flush(&mut buf, base, out);
                        let inner: String = cs[i + 1..end].iter().collect();
                        out.push(Seg::new(unescape(&inner), Sty { code: true, ..base }));
                        i = end + 1;
                        continue;
                    }
                }
            }
            ':' => {
                if let Some(end) = find_char(cs, i + 1, ':') {
                    let name = &cs[i + 1..end];
                    if !name.is_empty() && name.iter().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '+' | '-')) {
                        // Keep shortcode underscores out of emphasis parsing.
                        buf.extend(&cs[i..=end]);
                        i = end + 1;
                        continue;
                    }
                }
            }
            '*' | '_' | '~' => {
                if can_open(cs, i) {
                    if let Some(end) = find_close(cs, i) {
                        flush(&mut buf, base, out);
                        let sty = match c {
                            '*' => Sty { bold: true, ..base },
                            '_' => Sty {
                                italic: true,
                                ..base
                            },
                            _ => Sty {
                                strike: true,
                                ..base
                            },
                        };
                        inline(&cs[i + 1..end], sty, ctx, out);
                        i = end + 1;
                        continue;
                    }
                }
            }
            _ => {}
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, base, out);
}

/// `<...>` tokens: mentions, channels, specials, links. None = not a token.
fn angle(inner: &str, base: Sty, ctx: &Ctx) -> Option<Vec<Seg>> {
    let (head, label) = match inner.split_once('|') {
        Some((h, l)) => (h, Some(l)),
        None => (inner, None),
    };
    let m = Sty {
        mention: true,
        ..base
    };
    if let Some(uid) = head.strip_prefix('@') {
        let name = label.map(unescape).unwrap_or_else(|| ctx.user(uid));
        let me = ctx.corpus.me.as_deref() == Some(uid);
        return Some(vec![Seg::new(format!("@{name}"), Sty { me, ..m })]);
    }
    if let Some(cid) = head.strip_prefix('#') {
        let name = label.map(unescape).unwrap_or_else(|| ctx.channel(cid));
        return Some(vec![Seg::new(format!("#{name}"), m)]);
    }
    if let Some(bang) = head.strip_prefix('!') {
        let text = if let Some(l) = label {
            unescape(l)
        } else if let Some(rest) = bang.strip_prefix("subteam^") {
            format!("@{}", ctx.usergroup(rest))
        } else if bang.starts_with("date^") {
            bang.to_string()
        } else {
            format!("@{bang}")
        };
        return Some(vec![Seg::new(text, m)]);
    }
    if head.starts_with("http://") || head.starts_with("https://") || head.starts_with("mailto:") {
        let url = unescape(head);
        let shown = url.strip_prefix("mailto:").unwrap_or(&url).to_string();
        let l = Sty { link: true, ..base };
        return Some(match label.map(unescape) {
            Some(lab) if !lab.is_empty() && !label_is_url(&lab, &shown) => vec![
                Seg::new(lab, l),
                Seg::new(format!(" <{shown}>"), Sty { dim: true, ..base }),
            ],
            _ => vec![Seg::new(shown, l)],
        });
    }
    None
}

/// A label that is the URL itself, scheme dropped or shortened to
/// `host/…/tail` the way Slack does, says nothing the URL does not.
fn label_is_url(label: &str, url: &str) -> bool {
    let bare = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let label = label.trim_end_matches('/');
    let bare = bare.trim_end_matches('/');
    if label == url || label == bare {
        return true;
    }
    let Some((head, tail)) = label.split_once('…') else {
        return false;
    };
    bare.starts_with(head) && bare.ends_with(tail)
}

/// Every `<url|...>` / `<url>` in a mrkdwn string.
pub fn urls(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else { break };
        let inner = &after[..end];
        let head = inner.split('|').next().unwrap_or("");
        if head.starts_with("http://") || head.starts_with("https://") {
            out.push(unescape(head));
        }
        rest = &after[end + 1..];
    }
    out
}

// ------------------------------------------------------------------ blocks

fn arr<'a>(v: &'a Value, k: &str) -> impl Iterator<Item = &'a Value> {
    v.get(k)
        .and_then(Value::as_array)
        .map(|a| a.iter())
        .into_iter()
        .flatten()
}

fn text_obj(t: &Value, ctx: &Ctx, base: Sty) -> Vec<Seg> {
    if let Some(s) = t.as_str() {
        return mrkdwn(s, ctx, base);
    }
    let s = t.get("text").and_then(Value::as_str).unwrap_or("");
    if t.get("type").and_then(Value::as_str) == Some("plain_text") {
        vec![Seg::new(s, base)]
    } else {
        mrkdwn(s, ctx, base)
    }
}

/// Any string under a text-like key, for block types this renderer does not
/// know: a key named text/title/value/fallback at any depth is prose.
fn walk_texts(node: &Value, out: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            for (k, v) in map {
                match v {
                    Value::String(s)
                        if matches!(
                            k.as_str(),
                            "text" | "title" | "value" | "fallback" | "alt_text"
                        ) =>
                    {
                        if !s.is_empty() && !out.contains(s) {
                            out.push(s.clone());
                        }
                    }
                    Value::Object(_) | Value::Array(_) => walk_texts(v, out),
                    _ => {}
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|v| walk_texts(v, out)),
        _ => {}
    }
}

pub fn render_blocks(blocks: &[Value], ctx: &Ctx, base: Sty) -> Vec<Seg> {
    let dim = Sty { dim: true, ..base };
    let mut out = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str).unwrap_or("") {
            "table" => {
                let base = Sty { quote: false, ..base };
                let rows: Vec<Vec<Vec<Seg>>> = arr(b, "rows").filter_map(Value::as_array).map(|row| {
                    row.iter().map(|cell| match cell.get("type").and_then(Value::as_str) {
                        Some("rich_text") => render_blocks(std::slice::from_ref(cell), ctx, base),
                        Some("raw_number") => vec![Seg::new(cell.get("value").map(|value| value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string())).unwrap_or_default(), base)],
                        _ => vec![Seg::new(cell.get("text").and_then(Value::as_str).unwrap_or(""), base)],
                    }).collect()
                }).collect();
                let text = rows.iter().map(|row| row.iter().map(|cell| plain(cell)).collect::<Vec<_>>().join("\t")).collect::<Vec<_>>().join("\n");
                let align = arr(b, "column_settings").map(|column| column.get("align").and_then(Value::as_str).unwrap_or("left").to_string()).collect();
                let mut segment = Seg::new(text, base);
                segment.table = Some(Table { rows, align });
                out.push(segment);
                out.push(Seg::br());
            }
            "rich_text" => {
                for el in arr(b, "elements") {
                    rich_element(el, ctx, base, &mut out);
                }
            }
            "section" => {
                if let Some(t) = b.get("text") {
                    out.extend(text_obj(t, ctx, base));
                    out.push(Seg::br());
                }
                for f in arr(b, "fields") {
                    out.extend(text_obj(f, ctx, base));
                    out.push(Seg::br());
                }
            }
            "header" => {
                if let Some(t) = b.get("text") {
                    out.extend(text_obj(t, ctx, Sty { bold: true, ..base }));
                    out.push(Seg::br());
                }
            }
            "context" => {
                let mut first = true;
                for el in arr(b, "elements") {
                    if !first {
                        out.push(Seg::new("  ", base));
                    }
                    first = false;
                    if el.get("type").and_then(Value::as_str) == Some("image") {
                        let alt = el
                            .get("alt_text")
                            .and_then(Value::as_str)
                            .unwrap_or("image");
                        out.push(Seg::new(format!("[{alt}]"), dim));
                    } else {
                        out.extend(text_obj(el, ctx, dim));
                    }
                }
                out.push(Seg::br());
            }
            "divider" => {
                out.push(Seg::new("────────", dim));
                out.push(Seg::br());
            }
            "image" => {
                let alt = b
                    .get("alt_text")
                    .or_else(|| b.pointer("/title/text"))
                    .and_then(Value::as_str)
                    .unwrap_or("image");
                let url = b.get("image_url").and_then(Value::as_str).unwrap_or("");
                out.push(Seg::new(
                    format!("[image: {alt}] {url}").trim_end().to_string(),
                    dim,
                ));
                out.push(Seg::br());
            }
            "actions" => {
                for el in arr(b, "elements") {
                    if let Some(t) = el.pointer("/text/text").and_then(Value::as_str) {
                        out.push(Seg::new(format!("[{t}] "), dim));
                    }
                }
                out.push(Seg::br());
            }
            _ => {
                let mut texts = Vec::new();
                walk_texts(b, &mut texts);
                for t in texts {
                    out.extend(mrkdwn(&t, ctx, base));
                    out.push(Seg::br());
                }
            }
        }
    }
    out
}

fn rich_element(el: &Value, ctx: &Ctx, base: Sty, out: &mut Vec<Seg>) {
    match el.get("type").and_then(Value::as_str).unwrap_or("") {
        "rich_text_list" => {
            let ordered = el.get("style").and_then(Value::as_str) == Some("ordered");
            let indent = el.get("indent").and_then(Value::as_u64).unwrap_or(0) as usize;
            let offset = el.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            for (i, item) in arr(el, "elements").enumerate() {
                let bullet = if ordered {
                    format!("{}. ", i + 1 + offset)
                } else {
                    "• ".to_string()
                };
                out.push(Seg::new(format!("{}{}", "  ".repeat(indent), bullet), base));
                section(item, ctx, base, out);
                out.push(Seg::br());
            }
        }
        "rich_text_quote" => {
            section(
                el,
                ctx,
                Sty {
                    quote: true,
                    ..base
                },
                out,
            );
            out.push(Seg::br());
        }
        "rich_text_preformatted" => {
            let mut segs = Vec::new();
            section(el, ctx, base, &mut segs);
            let text = plain(&segs);
            out.push(Seg::br());
            out.push(Seg::new(text.trim_matches('\n'), Sty { pre: true, ..base }));
            out.push(Seg::br());
        }
        _ => {
            section(el, ctx, base, out);
            out.push(Seg::br());
        }
    }
}

fn section(el: &Value, ctx: &Ctx, base: Sty, out: &mut Vec<Seg>) {
    let mention = Sty {
        mention: true,
        ..base
    };
    for e in arr(el, "elements") {
        let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("");
        match e.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let mut sty = base;
                if let Some(st) = e.get("style") {
                    let f = |k: &str| st.get(k).and_then(Value::as_bool).unwrap_or(false);
                    sty.bold |= f("bold");
                    sty.italic |= f("italic");
                    sty.strike |= f("strike");
                    sty.code |= f("code");
                }
                out.push(Seg::new(s("text"), sty));
            }
            "link" => {
                let url = s("url");
                let text = s("text");
                let l = Sty { link: true, ..base };
                if !text.is_empty() && !label_is_url(text, url) {
                    out.push(Seg::new(text, l));
                    out.push(Seg::new(format!(" <{url}>"), Sty { dim: true, ..base }));
                } else {
                    out.push(Seg::new(url, l));
                }
            }
            "user" => {
                let me = ctx.corpus.me.as_deref() == Some(s("user_id"));
                out.push(Seg::new(
                    format!("@{}", ctx.user(s("user_id"))),
                    Sty { me, ..mention },
                ));
            }
            "usergroup" => {
                let id = s("usergroup_id");
                out.push(Seg::new(format!("@{}", ctx.usergroup(id)), mention));
            }
            "channel" => out.push(Seg::new(
                format!("#{}", ctx.channel(s("channel_id"))),
                mention,
            )),
            "emoji" => out.push(Seg::new(
                format!(":{}:", s("name")),
                base,
            )),
            "broadcast" => out.push(Seg::new(format!("@{}", s("range")), mention)),
            "date" => {
                let fb = s("fallback");
                let text = if fb.is_empty() {
                    match e.get("timestamp").and_then(Value::as_i64) {
                        Some(t) => ctx.tz.fmt(t, "%Y-%m-%d %H:%M"),
                        None => "[date]".to_string(),
                    }
                } else {
                    fb.to_string()
                };
                out.push(Seg::new(text, base));
            }
            "canvas" => out.push(Seg::new("[canvas]", Sty { dim: true, ..base })),
            "color" => out.push(Seg::new(s("value"), base)),
            _ => {
                if !s("text").is_empty() {
                    out.push(Seg::new(s("text"), base));
                }
            }
        }
    }
}

// ------------------------------------------------------------- attachments

fn attachment(att: &Value, ctx: &Ctx, base: Sty) -> Vec<Seg> {
    let q = Sty {
        quote: true,
        ..base
    };
    let dim = Sty { dim: true, ..q };
    let s = |k: &str| att.get(k).and_then(Value::as_str).unwrap_or("");
    let mut out = Vec::new();
    let mut any = false;
    let br = || Seg::br_in(q);
    if !s("pretext").is_empty() {
        out.extend(mrkdwn(s("pretext"), ctx, q));
        out.push(br());
        any = true;
    }
    if !s("author_name").is_empty() {
        out.push(Seg::new(unescape(s("author_name")), dim));
        out.push(br());
        any = true;
    }
    if !s("title").is_empty() {
        out.extend(mrkdwn(s("title"), ctx, Sty { bold: true, ..q }));
        if !s("title_link").is_empty() {
            out.push(Seg::new(format!(" <{}>", unescape(s("title_link"))), dim));
        }
        out.push(br());
        any = true;
    }
    let mut block_segs = Vec::new();
    if let Some(bl) = att.get("blocks").and_then(Value::as_array) {
        block_segs = render_blocks(bl, ctx, q);
    }
    if has_content(&block_segs) {
        out.extend(block_segs);
        any = true;
    } else if !s("text").is_empty() {
        out.extend(mrkdwn(s("text"), ctx, q));
        out.push(br());
        any = true;
    }
    for f in arr(att, "fields") {
        let t = f.get("title").and_then(Value::as_str).unwrap_or("");
        let v = f.get("value").and_then(Value::as_str).unwrap_or("");
        if !t.is_empty() {
            out.push(Seg::new(
                format!("{}: ", unescape(t)),
                Sty { bold: true, ..q },
            ));
        }
        out.extend(mrkdwn(v, ctx, q));
        out.push(br());
        any = true;
    }
    if !s("footer").is_empty() {
        out.extend(mrkdwn(s("footer"), ctx, dim));
        out.push(br());
        any = true;
    }
    if !any && !s("fallback").is_empty() {
        out.extend(mrkdwn(s("fallback"), ctx, q));
        out.push(br());
    }
    if !s("image_url").is_empty() {
        out.push(Seg::new(format!("[image] <{}>", s("image_url")), dim));
        out.push(br());
    }
    out
}

/// The body a reader sees: blocks when they render to something, else the
/// text column, then every attachment.
pub fn body(m: &Msg, ctx: &Ctx) -> Vec<Seg> {
    let base = Sty::default();
    let mut segs = Vec::new();
    if let Some(bl) = m.data.get("blocks").and_then(Value::as_array) {
        segs = render_blocks(bl, ctx, base);
    }
    if !has_content(&segs) {
        segs = mrkdwn(&m.text, ctx, base);
    }
    if let Some(atts) = m.data.get("attachments").and_then(Value::as_array) {
        for att in atts {
            if has_content(&segs) {
                segs.push(Seg::br());
            }
            segs.extend(attachment(att, ctx, base));
        }
    }
    segs
}

/// Every URL a message carries: in its text, its attachments' links.
pub fn message_urls(m: &Msg) -> Vec<String> {
    let mut out = urls(&m.text);
    if let Some(atts) = m.data.get("attachments").and_then(Value::as_array) {
        for att in atts {
            for k in ["title_link", "from_url", "original_url", "image_url"] {
                if let Some(u) = att.get(k).and_then(Value::as_str) {
                    out.push(u.to_string());
                }
            }
            for k in ["text", "pretext", "fallback"] {
                if let Some(t) = att.get(k).and_then(Value::as_str) {
                    out.extend(urls(t));
                }
            }
        }
    }
    out
}

// -------------------------------------------------------------------- wrap

enum Item {
    Word(Vec<(String, Sty)>),
    Break(bool),
}

fn items(segs: &[Seg]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut cur: Vec<(String, Sty)> = Vec::new();
    for seg in segs {
        if seg.sty.pre {
            if !cur.is_empty() {
                out.push(Item::Word(std::mem::take(&mut cur)));
            }
            for (i, line) in seg.text.split('\n').enumerate() {
                if i > 0 {
                    out.push(Item::Break(seg.sty.quote));
                }
                if !line.is_empty() {
                    out.push(Item::Word(vec![(line.to_string(), seg.sty)]));
                }
            }
            continue;
        }
        let mut buf = String::new();
        for ch in seg.text.chars() {
            if ch == '\n' {
                if !buf.is_empty() {
                    cur.push((std::mem::take(&mut buf), seg.sty));
                }
                if !cur.is_empty() {
                    out.push(Item::Word(std::mem::take(&mut cur)));
                }
                out.push(Item::Break(seg.sty.quote));
            } else if ch.is_whitespace() {
                if !buf.is_empty() {
                    cur.push((std::mem::take(&mut buf), seg.sty));
                }
                if !cur.is_empty() {
                    out.push(Item::Word(std::mem::take(&mut cur)));
                }
            } else {
                buf.push(ch);
            }
        }
        if !buf.is_empty() {
            cur.push((buf, seg.sty));
        }
    }
    if !cur.is_empty() {
        out.push(Item::Word(cur));
    }
    out
}

fn finish(spans: Vec<Span<'static>>, indent: &str, quote: bool) -> Line<'static> {
    let mut all = Vec::with_capacity(spans.len() + 2);
    if !indent.is_empty() {
        all.push(Span::raw(indent.to_string()));
    }
    if quote {
        all.push(Span::styled("│ ", Style::new().add_modifier(Modifier::DIM)));
    }
    all.extend(spans);
    Line::from(all)
}

fn is_blank(line: &Line) -> bool {
    line.spans
        .iter()
        .all(|s| s.content.trim().is_empty() || s.content.as_ref() == "│ ")
}

/// Render cells only after the reading pane width is known.
fn table_lines(table: &Table, width: usize, indent: &str, palette: &Palette) -> Vec<Line<'static>> {
    let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 { return Vec::new(); }
    let available = width.saturating_sub(indent.width());
    let minimums: Vec<usize> = (0..columns).map(|column| {
        if table.rows.iter().filter_map(|row| row.get(column)).flatten().any(|seg| seg.sty.quote) { 4 } else { 2 }
    }).collect();
    // A grid needs at least one cell column and its separators. At very
    // narrow widths retain row/cell identity instead of dropping columns.
    if available < minimums.iter().sum::<usize>() + 3 * columns + 1 {
        let mut lines = Vec::new();
        for (row_index, row) in table.rows.iter().enumerate() {
            lines.extend(wrap(&[Seg::new(format!("Row {}", row_index + 1), Sty { bold: true, ..Sty::default() })], width, indent, palette));
            for (column, cell) in row.iter().enumerate() {
                let mut segments = vec![Seg::new(format!("{}: ", column + 1), Sty::default())];
                segments.extend(cell.clone());
                lines.extend(wrap(&segments, width, indent, palette));
            }
        }
        return lines;
    }
    let mut widths = minimums.clone();
    for row in &table.rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(plain(cell).lines().map(UnicodeWidthStr::width).max().unwrap_or(1));
        }
    }
    let budget = available - (3 * columns + 1);
    for width in &mut widths { *width = (*width).min(budget); }
    while widths.iter().sum::<usize>() > budget {
        let column = (0..columns).filter(|&column| widths[column] > minimums[column]).max_by_key(|&column| widths[column]).unwrap();
        widths[column] -= 1;
    }
    let border = |left: &str, joint: &str, right: &str| {
        Line::from(format!("{indent}{left}{}{right}", widths.iter().map(|width| "─".repeat(width + 2)).collect::<Vec<_>>().join(joint)))
    };
    let mut lines = vec![border("┌", "┬", "┐")];
    for (row_index, row) in table.rows.iter().enumerate() {
        let cells: Vec<Vec<Line<'static>>> = (0..columns).map(|column| row.get(column).map(|cell| wrap(cell, widths[column], "", palette)).unwrap_or_default()).collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for line_index in 0..height {
            let mut spans = vec![Span::raw(format!("{indent}│"))];
            for column in 0..columns {
                let cell = cells[column].get(line_index).cloned().unwrap_or_default();
                let padding = widths[column].saturating_sub(cell.width());
                let left = match table.align.get(column).map(String::as_str) {
                    Some("right") => padding,
                    Some("center") => padding / 2,
                    _ => 0,
                };
                spans.push(Span::raw(" ".repeat(left + 1)));
                spans.extend(cell.spans);
                spans.push(Span::raw(format!("{}│", " ".repeat(padding - left + 1))));
            }
            lines.push(Line::from(spans));
        }
        if row_index + 1 < table.rows.len() { lines.push(border("├", "┼", "┤")); }
    }
    lines.push(border("└", "┴", "┘"));
    lines
}

/// Greedy word-wrap, splitting oversized words and preformatted lines at
/// grapheme boundaries so message text remains visible.
pub fn wrap(segs: &[Seg], width: usize, indent: &str, palette: &Palette) -> Vec<Line<'static>> {
    if segs.iter().any(|seg| seg.table.is_some()) {
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, seg) in segs.iter().enumerate() {
            if let Some(table) = &seg.table {
                lines.extend(wrap(&segs[start..index], width, indent, palette));
                lines.extend(table_lines(table, width, indent, palette));
                start = index + 1;
            }
        }
        lines.extend(wrap(&segs[start..], width, indent, palette));
        return lines;
    }
    // Match source text before word wrapping, retaining each segment's semantics.
    let highlighted = palette.highlight_line(Line::from(segs.iter().map(|seg| Span::raw(seg.text.clone())).collect::<Vec<_>>()));
    let mut source = segs.iter().filter(|seg| !seg.text.is_empty());
    let mut current = source.next();
    let mut consumed = 0;
    let mut painted = Vec::new();
    for span in highlighted.spans {
        if span.content.is_empty() { continue; }
        let Some(seg) = current else { break; };
        // Preformatted segments must remain whole: items() treats each as a
        // verbatim unit. Highlight each verbatim word after tokenization.
        if seg.sty.pre {
            if consumed == 0 { painted.push(seg.clone()); }
            consumed += span.content.len();
        } else {
            let mut style = seg.sty;
            style.highlight = span.style.fg;
            consumed += span.content.len();
            painted.push(Seg::new(span.content.into_owned(), style));
        }
        if consumed == seg.text.len() { current = source.next(); consumed = 0; }
    }
    let segs = painted.as_slice();
    let avail = width.saturating_sub(indent.width()).max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let mut preserved_rows = std::collections::HashSet::new();
    let mut quote: Option<bool> = None;
    for item in items(segs) {
        match item {
            Item::Break(q) => {
                lines.push(finish(std::mem::take(&mut cur), indent, quote.unwrap_or(q)));
                cur_w = 0;
                quote = None;
            }
            Item::Word(parts) => {
                let w: usize = parts.iter().map(|(t, _)| t.width()).sum();
                let q = parts.first().map(|p| p.1.quote).unwrap_or(false);
                let room = avail.saturating_sub(if q { 2 } else { 0 }).max(1);
                if cur_w > 0 && cur_w + 1 + w > room {
                    lines.push(finish(
                        std::mem::take(&mut cur),
                        indent,
                        quote.unwrap_or(false),
                    ));
                    cur_w = 0;
                    quote = None;
                }
                if quote.is_none() {
                    quote = Some(q);
                }
                if cur_w > 0 {
                    cur.push(Span::raw(" "));
                    cur_w += 1;
                }
                let preformatted = parts.first().is_some_and(|(_, style)| style.pre);
                let word = Line::from(parts.into_iter().map(|(text, style)| Span::styled(text, style_of(style, palette))).collect::<Vec<_>>());
                let word = if preformatted { palette.highlight_line(word) } else { word };
                // Segment before applying span boundaries: highlights may split a
                // combining sequence, which must still occupy one display cell unit.
                let text = word.to_string();
                let mut spans = word.spans.iter();
                let mut span = spans.next();
                let mut offset = 0;
                for grapheme in text.graphemes(true) {
                    let cells = grapheme.width();
                    if cur_w > 0 && cur_w + cells > room {
                        if preformatted { preserved_rows.insert(lines.len()); }
                        lines.push(finish(std::mem::take(&mut cur), indent, q));
                        cur_w = 0;
                    }
                    let mut remaining = grapheme.len();
                    while remaining > 0 {
                        let current = span.expect("word spans cover its text");
                        let count = remaining.min(current.content.len() - offset);
                        let piece = &current.content[offset..offset + count];
                        if let Some(last) = cur.last_mut().filter(|last| last.style == current.style) {
                            last.content.to_mut().push_str(piece);
                        } else {
                            cur.push(Span::styled(piece.to_string(), current.style));
                        }
                        remaining -= count;
                        offset += count;
                        if offset == current.content.len() { span = spans.next(); offset = 0; }
                    }
                    cur_w += cells;
                }
                if preformatted { preserved_rows.insert(lines.len()); }
            }
        }
    }
    if cur_w > 0 {
        lines.push(finish(cur, indent, quote.unwrap_or(false)));
    }
    // Trim blank edges, collapse blank runs: fences and block joins leave
    // doubled breaks behind.
    let blank = |index: usize| !preserved_rows.contains(&index) && is_blank(&lines[index]);
    let first = (0..lines.len()).find(|&index| !blank(index)).unwrap_or(lines.len());
    let last = (first..lines.len()).rfind(|&index| !blank(index)).map_or(first, |index| index + 1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut previous_blank = false;
    for (index, line) in lines.into_iter().enumerate().take(last).skip(first) {
        let blank = !preserved_rows.contains(&index) && is_blank(&line);
        if !blank || !previous_blank { out.push(line); }
        previous_blank = blank;
    }
    out
}

// ----------------------------------------------------------------- message

const SYSTEM: &[&str] = &[
    "channel_join",
    "channel_leave",
    "group_join",
    "group_leave",
    "channel_topic",
    "channel_purpose",
    "channel_name",
    "channel_archive",
    "channel_unarchive",
    "pinned_item",
    "unpinned_item",
    "bot_add",
    "bot_remove",
    "bot_enable",
    "bot_disable",
    "reminder_add",
    "huddle_thread",
    "tabbed_canvas_updated",
    "channel_canvas_updated",
    "tombstone",
    "sh_room_created",
    "app_conversation_join",
];

pub fn is_system(m: &Msg) -> bool {
    m.subtype.as_deref().is_some_and(|s| SYSTEM.contains(&s))
}

fn human_bytes(n: i64) -> String {
    let f = n as f64;
    if f >= 1e9 {
        format!("{:.1}G", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1}M", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.0}K", f / 1e3)
    } else {
        format!("{n}B")
    }
}

/// The `[kind] name (filetype, size)` line a file gets in a message, directly
/// above the rows reserved for its image. Shared with the collapsed preview in
/// `app.rs`, which shows it in place of a reserved row it kept but whose image
/// it dropped, so both paths name a file identically.
pub fn file_label(f: &FileInfo) -> Line<'static> {
    let kind = match f.mode.as_str() {
        "snippet" => "snippet",
        "quip" => "canvas",
        _ => "file",
    };
    let mut s = format!("  [{kind}] {}", f.name);
    if !f.filetype.is_empty() {
        s.push_str(&format!(" ({}", f.filetype));
        if let Some(n) = f.size {
            s.push_str(&format!(", {}", human_bytes(n)));
        }
        s.push(')');
    }
    Line::from(Span::styled(s, Style::new().add_modifier(Modifier::DIM)))
}

/// When the message was sent, as its header and its one-line system form
/// both open with.
fn message_time(m: &Msg, ctx: &Ctx, today: i64) -> String {
    format!(
        "{} {}",
        ctx.tz.date_label(m.secs(), today),
        ctx.tz.fmt(
            m.secs(),
            match ctx.tz {
                Tz::Utc => "%H:%M UTC",
                Tz::Local => "%H:%M %:z",
            },
        )
    )
}

/// The line above a message's body: when it was sent, who sent it, and what
/// the message is besides — a bot, a channel it was found in, an edit, a
/// thread reply that also went to the channel.
pub fn message_header(m: &Msg, ctx: &Ctx, in_thread: bool, today: i64) -> Line<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut spans = vec![
        Span::styled(message_time(m, ctx, today), dim),
        Span::raw("  "),
        Span::styled(String::new(), Style::new().add_modifier(Modifier::BOLD)),
    ];
    let author = ctx.author(m);
    let tag_bot =
        author != "bot" && (m.is_bot() || m.user.as_deref().is_some_and(|u| ctx.user_is_bot(u)));
    if tag_bot {
        spans.push(Span::styled(" bot", dim));
    }
    if let Some(c) = &m.channel_name {
        spans.push(Span::styled(format!(" in {c}"), dim));
    }
    if m.edited {
        spans.push(Span::styled(" (edited)", dim));
    }
    if m.broadcast {
        spans.push(Span::styled(
            if in_thread {
                " · also sent to the channel"
            } else {
                " · reply from a thread"
            },
            dim,
        ));
    }
    let is_me = m.user.is_some() && m.user.as_deref() == ctx.corpus.me.as_deref();
    let author_style = Style::new()
        .fg(ctx.palette.get(if is_me {
            Role::OwnUsername
        } else {
            Role::OtherUsername
        }))
        .add_modifier(Modifier::BOLD);
    spans[2] = Span::styled(author, author_style);
    Line::from(spans)
}

/// The message's own text, wrapped to `width` under the header's indent.
pub fn message_body(m: &Msg, ctx: &Ctx, width: usize) -> Vec<Line<'static>> {
    wrap(&body(m, ctx), width, "  ", ctx.palette)
}

/// The reactions on a message as `:name: count`, wrapped over as many rows as
/// they need. Empty when there are none.
pub fn reaction_lines(m: &Msg, width: usize) -> Vec<Line<'static>> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let reactions = m.reactions();
    if reactions.is_empty() {
        return Vec::new();
    }
    let text = reactions
        .iter()
        .map(|(name, count)| format!(":{name}:({count})"))
        .collect::<Vec<_>>()
        .join("   ");
    let available = width.saturating_sub(2).max(1);
    let mut lines = Vec::new();
    let mut row = String::new();
    let mut columns = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if columns + character_width > available && !row.is_empty() {
            lines.push(Line::from(Span::styled(format!("  {row}"), dim)));
            row.clear();
            columns = 0;
        }
        row.push(character);
        columns += character_width;
    }
    if !row.is_empty() {
        lines.push(Line::from(Span::styled(format!("  {row}"), dim)));
    }
    lines
}

/// How many replies hang off a message and how many of them the archive has.
/// None inside a thread, where the replies are on screen already.
pub fn thread_footer(m: &Msg, ctx: &Ctx, in_thread: bool) -> Option<Line<'static>> {
    if in_thread || !m.has_thread() {
        return None;
    }
    let n = m.archived_replies;
    let total = m.reply_count.max(n);
    let plural = |k: i64| if k == 1 { "reply" } else { "replies" };
    let s = if n == 0 {
        format!("  ↳ {total} {} · not archived", plural(total))
    } else if n < total {
        format!("  ↳ {n} of {total} {} archived", plural(total))
    } else {
        format!("  ↳ {n} {}", plural(n))
    };
    Some(Line::from(Span::styled(
        s,
        Style::new().fg(ctx.palette.get(Role::ThreadInfo)),
    )))
}

/// A join, a purpose change, a deleted message, a huddle: what Slack calls a
/// subtype rather than a message. One dim italic run behind the time, wrapped
/// under an indent of its own. It draws neither a header nor a body, so its
/// rows are neither.
pub fn system_message(m: &Msg, ctx: &Ctx, width: usize, today: i64) -> Vec<Line<'static>> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let time = message_time(m, ctx, today);
    let sub = m.subtype.as_deref().unwrap_or("");
    let text = match sub {
        "tombstone" => "(message deleted)".to_string(),
        "huddle_thread" => "[huddle]".to_string(),
        _ => {
            let p = plain(&body(m, ctx));
            let p = p.trim();
            if p.is_empty() {
                format!("[{sub}]")
            } else {
                p.split_whitespace().collect::<Vec<_>>().join(" ")
            }
        }
    };
    let segs = vec![Seg::new(
        text,
        Sty {
            dim: true,
            italic: true,
            ..Sty::default()
        },
    )];
    let indent = format!("{}  · ", " ".repeat(time.len()));
    let mut lines = wrap(&segs, width, &indent, ctx.palette);
    if let Some(first) = lines.first_mut() {
        first.spans[0] = Span::styled(format!("{time}  · "), dim);
    }
    lines
}

/// One message as lines: header, wrapped body, files, reactions, thread
/// footer. `in_thread` drops the footer (the replies are on screen).
pub fn message_lines(m: &Msg, ctx: &Ctx, width: usize, in_thread: bool, today: i64) -> Rendered {
    if is_system(m) {
        let lines = system_message(m, ctx, width, today);
        let tags = vec!["system_message"; lines.len()];
        return Rendered {
            lines,
            images: Vec::new(),
            tags,
        };
    }
    let mut lines = Vec::new();
    let mut images = Vec::new();
    let mut tags: Vec<&'static str> = Vec::new();
    lines.push(message_header(m, ctx, in_thread, today));
    tags.push("message_header");
    let body_start = lines.len();
    let body = message_body(m, ctx, width);
    tags.extend(std::iter::repeat_n("message_body", body.len()));
    lines.extend(body);
    let body_end = lines.len();
    for f in m.files() {
        lines.push(file_label(&f));
        tags.push("file_label");
        if let Some(font) = ctx.image_font {
            if let Some((cols, rows)) = image_cells(&f, font, width) {
                images.push(ImageSlot {
                    source: ImageSource::File(f.clone()),
                    line: lines.len(),
                    cols,
                    rows,
                });
                for _ in 0..rows {
                    lines.push(Line::from(""));
                    // A picture's rows are the picture's; a tag on one would
                    // be a tag over content.
                    tags.push("");
                }
            }
        }
    }
    let reactions = reaction_lines(m, width);
    tags.extend(std::iter::repeat_n("reaction_lines", reactions.len()));
    lines.extend(reactions);
    if let Some(footer) = thread_footer(m, ctx, in_thread) {
        lines.push(footer);
        tags.push("thread_footer");
    }
    let lines = lines.into_iter().enumerate().map(|(index, line)| {
        if (body_start..body_end).contains(&index) { line } else { ctx.palette.highlight_line(line) }
    }).collect();
    Rendered { lines, images, tags }
}

/// One row of the THREADS view. Slack's Threads screen draws a card per
/// thread; this carries what surrounds the root message — the conversation
/// and participants line above it, how many replies the card leaves out, and
/// the thread's newest reply. The root itself stays in the list's `msgs`, so
/// the cursor and every list helper keep working on one item per card.
#[derive(Clone, Debug, Default)]
pub struct ThreadCard {
    /// `#channel`, or the DM/group name as the sidebar shows it.
    pub conversation: String,
    /// `a, b, and 3 others`: who took part, from the root's `reply_users`.
    pub participants: String,
    /// Replies the card does not draw: every one but the last.
    pub hidden: i64,
    /// The newest reply the archive held when the card was built: the drawn
    /// reply's id, or the root's when there was none. A delete at or below it
    /// is a reply this card counted. Above it the card cannot tell a reply
    /// written since from one Slack's `reply_count` included and the archive
    /// never held, so it leaves the count alone for both: a count that reads
    /// high until the next open, rather than one that drops for a reply it
    /// never stood for.
    pub counted_through: i64,
    /// The thread's newest archived reply; None when the archive holds none.
    pub last: Option<Msg>,
}

/// How much of a card the pane can hold. `whole_message_viewport` draws
/// nothing at all for an item taller than the pane, so a short pane sheds
/// parts of the card rather than overflowing it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CardFit {
    /// Header, root, elision, last reply.
    Whole,
    /// The last reply folded into the elision, which now counts it.
    Folded,
    /// The elision folded into the header. Six rows with a collapsed root,
    /// which is the shortest pane `ui::draw` will draw a message into.
    Root,
}

/// `N more replies`, the count both elisions share.
fn replies_label(n: i64) -> String {
    format!("{n} more {}", if n == 1 { "reply" } else { "replies" })
}

/// Clip to `room`, or give up: a budget that would go entirely on the
/// ellipsis, or overflow on it, says nothing worth the columns.
fn clip_or_drop(text: &str, room: usize) -> String {
    if text.width() <= room {
        return text.to_string();
    }
    if room < 4 {
        return String::new();
    }
    crate::ui::clip(text, room)
}
/// The card's first line: which conversation the thread is in, who took part,
/// and — only when the pane was too short to give the count a line of its own
/// — how many replies are not drawn. Truncated to `width` here rather than by
/// the draw: a count clipped off the end would be nowhere, the elision line
/// having been dropped to make room in the first place. The count keeps its
/// columns; the participants give theirs up first, the conversation next.
pub fn card_header(
    card: &ThreadCard,
    palette: &Palette,
    replies: i64,
    width: usize,
) -> Line<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let tail = if replies > 0 {
        format!("  · {}", replies_label(replies))
    } else {
        String::new()
    };
    let room = width.saturating_sub(tail.width());
    let conversation = clip_or_drop(&card.conversation, room);
    let mut spans = Vec::new();
    // Nothing but the count fits: it is the line.
    if !conversation.is_empty() {
        spans.push(Span::styled(
            conversation.clone(),
            Style::new()
                .fg(palette.get(Role::Accent))
                .add_modifier(Modifier::BOLD),
        ));
        if !card.participants.is_empty() {
            let people = clip_or_drop(
                &format!("  {}", card.participants),
                room - conversation.width(),
            );
            if !people.is_empty() {
                spans.push(Span::styled(people, dim));
            }
        }
    }
    if !tail.is_empty() {
        spans.push(Span::styled(tail, dim));
    }
    Line::from(spans)
}

/// The replies the card does not draw, elided. Reads like the
/// collapsed-message elision, so the two are one idiom.
pub fn card_elision(hidden: i64) -> Line<'static> {
    Line::from(Span::styled(
        format!("  … {}", replies_label(hidden)),
        Style::new().add_modifier(Modifier::DIM),
    ))
}

impl ThreadCard {
    /// Replies this card does not draw at `fit`: everything but the last one,
    /// plus that one once it is folded away.
    pub fn elided(&self, fit: CardFit) -> i64 {
        self.hidden + i64::from(fit != CardFit::Whole && self.last.is_some())
    }
}

/// Who took part in a thread, as Slack names them: at most two, then a count.
/// `reply_users` on the root is the list Slack sends and `reply_users_count`
/// how many there were, which can exceed what the list carries. An archived
/// root without `reply_users` names its author alone; nothing here queries
/// the thread, which would be one round trip per card.
pub fn participants(m: &Msg, ctx: &Ctx) -> String {
    let users: Vec<String> = m
        .data
        .get("reply_users")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|u| ctx.user(u))
                .collect()
        })
        .unwrap_or_default();
    if users.is_empty() {
        return ctx.author(m);
    }
    let total = m
        .data
        .get("reply_users_count")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(users.len() as i64) as usize;
    let named = users.len().min(2);
    let shown = users[..named].join(", ");
    match total - named {
        0 => shown,
        1 => format!("{shown}, and 1 other"),
        others => format!("{shown}, and {others} others"),
    }
}

/// Each bar of a divider: the line fits `width`, bars capped at 40 cells.
fn bar_len(text: &str, width: usize) -> usize {
    (width.saturating_sub(text.width() + 2) / 2).clamp(1, 40)
}

/// The divider that opens the unread part of a conversation.
pub fn divider_new(text: &str, width: usize, palette: &Palette) -> Line<'static> {
    let style = Style::new()
        .fg(palette.get(Role::Unread))
        .add_modifier(Modifier::BOLD);
    let bar = "─".repeat(bar_len(text, width));
    Line::from(Span::styled(format!("{bar} {text} {bar}"), style))
}

pub fn divider(text: &str, width: usize) -> Line<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let bar = "─".repeat(bar_len(text, width));
    Line::from(Span::styled(format!("{bar} {text} {bar}"), dim))
}

pub fn line_text(l: &Line) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn highlights_survive_wrapping_without_changing_preformatted_spacing() {
        let mut palette = Palette::default();
        palette.highlights.push(crate::palette::Highlight { word: "upstream timeout".into(), color: ratatui::style::Color::Red });
        palette.highlights.push(crate::palette::Highlight { word: "timeout".into(), color: ratatui::style::Color::Green });
        let segments = vec![Seg::new("upstream timeout", Sty::default())];
        for width in [8, 16, 80] {
            let lines = wrap(&segments, width, "", &palette);
            let red: String = lines.iter().flat_map(|line| &line.spans).filter(|span| span.style.fg == Some(ratatui::style::Color::Red)).map(|span| span.content.as_ref()).collect();
            assert_eq!(red, "upstreamtimeout");
        }
        let segments = vec![Seg::new("  nginx   -t\nNGINX", Sty { pre: true, ..Sty::default() })];
        let lines = wrap(&segments, 8, "", &palette);
        assert_eq!(lines.iter().map(Line::to_string).collect::<Vec<_>>(), vec!["  nginx ", "  -t", "NGINX"]);
    }

    #[test]
    fn table_cells_keep_rows_repeated_values_styles_and_alignment() {
        let archive = Archive::stub(&[], &[]);
        let corpus = Corpus::stub(&[]);
        let context = ctx(&archive, &corpus);
        let blocks = serde_json::json!([{"type":"table", "column_settings":[{}, {"align":"right"}], "rows":[
            [{"type":"raw_text","text":"Metric"},{"type":"raw_text","text":"Value"}],
            [{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"TLS","style":{"bold":true}}]}]},{"type":"raw_number","value":199}],
            [{"type":"raw_text","text":"Same"},{"type":"raw_number","value":199}]
        ]}]);
        let segments = render_blocks(blocks.as_array().unwrap(), &context, Sty::default());
        let lines = wrap(&segments, 80, "", &TEST_PALETTE);
        assert_eq!(lines.iter().map(Line::to_string).collect::<Vec<_>>(), vec![
            "┌────────┬───────┐", "│ Metric │ Value │", "├────────┼───────┤",
            "│ TLS    │   199 │", "├────────┼───────┤", "│ Same   │   199 │", "└────────┴───────┘"
        ]);
        assert!(lines[3].spans.iter().any(|span| span.content == "TLS" && span.style.add_modifier.contains(Modifier::BOLD)));
        for width in [12, 20] {
            let lines = wrap(&segments, width, "  ", &TEST_PALETTE);
            assert!(lines.iter().all(|line| line.width() <= width));
            assert_eq!(lines.iter().map(Line::to_string).collect::<String>().matches("199").count(), 2);
        }
    }

    #[test]
    fn empty_tables_survive_fallback_and_quoted_cells_fit_columns() {
        let archive = Archive::stub(&[], &[]);
        let corpus = Corpus::stub(&[]);
        let context = ctx(&archive, &corpus);
        let table = serde_json::json!({"type":"table","rows":[[{"type":"raw_text","text":""},{"type":"raw_text","text":""}]]});
        for attachment in [false, true] {
            let mut value = serde_json::json!({"ts":"1.0","text":"fallback"});
            if attachment { value["text"] = serde_json::json!(""); value["attachments"] = serde_json::json!([{"fallback":"fallback","blocks":[table.clone()]}]); }
            else { value["blocks"] = serde_json::json!([table.clone()]); }
            let message = Msg::from_api("C1".into(), value).unwrap();
            let lines = wrap(&body(&message, &context), 40, "", &TEST_PALETTE);
            assert!(lines[0].to_string().contains('┌'));
            assert!(!lines.iter().any(|line| line.to_string().contains("fallback")));
        }
        let blocks = serde_json::json!([{"type":"table","rows":[[
            {"type":"rich_text","elements":[{"type":"rich_text_quote","elements":[{"type":"text","text":"界"}]}]},
            {"type":"raw_text","text":"a long string"}
        ]]}]);
        for width in [12, 13, 20] {
            let lines = wrap(&render_blocks(blocks.as_array().unwrap(), &context, Sty::default()), width, "", &TEST_PALETTE);
            assert!(lines.iter().all(|line| line.width() <= width));
        }
    }

    #[test]
    fn table_wrapping_keeps_empty_cells_multiline_content_and_surrounding_text() {
        let archive = Archive::stub(&[], &[]);
        let corpus = Corpus::stub(&[]);
        let context = ctx(&archive, &corpus);
        let blocks = serde_json::json!([
            {"type":"section","text":{"type":"plain_text","text":"Before"}},
            {"type":"table","rows":[[{"type":"raw_text","text":"abcdefghijk"},{"type":"raw_text","text":""}],
                [{"type":"raw_text","text":"界界"},{"type":"raw_text","text":"x\ny"}]]},
            {"type":"section","text":{"type":"plain_text","text":"After"}}
        ]);
        let segments = render_blocks(blocks.as_array().unwrap(), &context, Sty::default());
        let lines = wrap(&segments, 15, "", &TEST_PALETTE);
        assert!(lines.iter().all(|line| line.width() <= 15));
        assert_eq!(lines.first().unwrap().to_string(), "Before");
        assert_eq!(lines.last().unwrap().to_string(), "After");
        let text = lines.iter().map(Line::to_string).collect::<String>();
        for value in ["abcdef", "ghijk", "界界", "x", "y"] { assert!(text.contains(value), "{value}: {text}"); }
    }

    #[test]
    fn oversized_words_and_code_wrap_without_losing_text() {
        let palette = Palette::default();
        for pre in [false, true] {
            for value in ["https://example.org/averylongpath", "界e\u{301}界e\u{301}界e\u{301}"] {
                for width in [4, 8, 20] {
                    let lines = wrap(&[Seg::new(value, Sty { pre, ..Sty::default() })], width, "", &palette);
                    assert!(lines.iter().all(|line| line.width() <= width));
                    assert_eq!(lines.iter().map(Line::to_string).collect::<String>(), value);
                }
            }
        }
        let indented = "                        x";
        let lines = wrap(&[Seg::new(indented, Sty { pre: true, ..Sty::default() })], 8, "", &palette);
        assert_eq!(lines.iter().map(Line::to_string).collect::<String>(), indented);
        let mut highlighted = palette.clone();
        highlighted.highlights.push(crate::palette::Highlight { word: "e".into(), color: ratatui::style::Color::Red });
        let lines = wrap(&[Seg::new("123e\u{301}x", Sty::default())], 4, "", &highlighted);
        assert_eq!(lines.iter().map(Line::to_string).collect::<Vec<_>>(), vec!["123e\u{301}", "x"]);
        let value = "    upstream timed out   while reading response";
        let lines = wrap(&[Seg::new(value, Sty { pre: true, quote: true, ..Sty::default() })], 16, "  ", &palette);
        assert!(lines.iter().all(|line| line.width() <= 16));
        assert_eq!(lines.iter().map(|line| line.to_string().strip_prefix("  │ ").unwrap().to_string()).collect::<String>(), value);
    }

    #[test]
    fn usergroup_mentions_resolve_both_formats_and_keep_fallbacks() {
        let a = Archive::stub(&[], &[]);
        let mut corpus = Corpus::stub(&[]);
        corpus.usergroups.insert("S1".into(), "oncall".into());
        let c = ctx(&a, &corpus);
        assert_eq!(
            text(&mrkdwn(
                "<!subteam^S1> <!subteam^S2> <!subteam^S1|@explicit>",
                &c,
                Sty::default()
            )),
            "@oncall @S2 @explicit"
        );
        let value = serde_json::json!([
            {"type":"usergroup", "usergroup_id":"S1"},
            {"type":"usergroup", "usergroup_id":"S2"},
            {"type":"usergroup"}
        ]);
        let original = value.clone();
        let mut out = Vec::new();
        section(
            &serde_json::json!({"elements":value}),
            &c,
            Sty::default(),
            &mut out,
        );
        assert_eq!(text(&out), "@oncall@S2@group");
        assert_eq!(value, original);
    }
    #[test]
    fn json_highlighting_preserves_escapes_unicode_and_token_kinds() {
        use crate::palette::{Palette, Role};
        let palette = Palette::default();
        let input = r#"  "key\"é": ["hello\\world", -1.25e+3, true, false, null],"#;
        let line = super::json_line(input, &palette);
        assert_eq!(super::line_text(&line), input);
        for (token, role) in [
            (r#""key\"é""#, Role::Link),
            (r#""hello\\world""#, Role::Code),
            ("-1.25e+3", Role::Mention),
            ("true", Role::Unread),
            ("false", Role::Unread),
            ("null", Role::ThreadInfo),
        ] {
            assert!(
                line.spans
                    .iter()
                    .any(|s| s.content == token && s.style.fg == Some(palette.get(role))),
                "{token}"
            );
        }
        let data = serde_json::json!({"text":"é 😀 quoted \" value\nnext", "bool":false, "n":null});
        for raw in serde_json::to_string_pretty(&data).unwrap().lines() {
            assert_eq!(super::line_text(&super::json_line(raw, &palette)), raw);
        }
    }

    use super::*;

    static TEST_PALETTE: std::sync::LazyLock<Palette> = std::sync::LazyLock::new(Palette::default);

    fn ctx<'a>(archive: &'a Archive, corpus: &'a Corpus) -> Ctx<'a> {
        Ctx {
            archive: Some(archive),
            corpus,
            tz: Tz::Utc,
            image_font: None,
            last_read: None,
            palette: &TEST_PALETTE,
        }
    }

    fn text(segs: &[Seg]) -> String {
        plain(segs)
    }

    /// A THREADS card names at most two participants and counts the rest,
    /// resolving the ids the root carries through the user map. Slack's own
    /// count wins when it exceeds the list it sent; a root without
    /// `reply_users` names its author alone.
    #[test]
    fn a_card_names_two_participants_and_counts_the_rest() {
        let archive = Archive::stub(
            &[("U1", "ann"), ("U2", "bea"), ("U3", "cyd"), ("U4", "dee")],
            &[],
        );
        let corpus = Corpus::stub(&[]);
        let context = ctx(&archive, &corpus);
        let root = |extra: Value| {
            let mut data = serde_json::json!({"ts": "1.000000", "user": "U1", "text": "root"});
            let (Value::Object(data_map), Value::Object(extra_map)) = (&mut data, extra) else {
                unreachable!("both are objects")
            };
            data_map.extend(extra_map);
            Msg::from_api("C1".to_string(), data).expect("a root")
        };
        let cases = [
            (serde_json::json!({}), "ann"),
            (serde_json::json!({"reply_users": []}), "ann"),
            (serde_json::json!({"reply_users": ["U2"]}), "bea"),
            (serde_json::json!({"reply_users": ["U2", "U3"]}), "bea, cyd"),
            (
                serde_json::json!({"reply_users": ["U2", "U3", "U4"]}),
                "bea, cyd, and 1 other",
            ),
            (
                // Slack sends five participants and names four of them.
                serde_json::json!({"reply_users": ["U2","U3","U4","U9"], "reply_users_count": 5}),
                "bea, cyd, and 3 others",
            ),
            (
                // A count behind the list it came with does not shrink it.
                serde_json::json!({"reply_users": ["U2","U3","U4"], "reply_users_count": 1}),
                "bea, cyd, and 1 other",
            ),
            (
                // An id no archive knows stays the id.
                serde_json::json!({"reply_users": ["U9", "U2"]}),
                "U9, bea",
            ),
        ];
        for (extra, want) in cases {
            let message = root(extra.clone());
            assert_eq!(participants(&message, &context), want, "{extra}");
        }
    }

    /// The card's own two lines: the conversation and participants above the
    /// root, and the dim elision that stands for the replies it leaves out.
    #[test]
    fn a_card_heads_with_its_conversation_and_elides_the_replies_between() {
        let card = ThreadCard {
            conversation: "#team-alpha".to_string(),
            participants: "bea, cyd, and 3 others".to_string(),
            hidden: 5,
            counted_through: 9_000_000,
            last: None,
        };
        let header = card_header(&card, &TEST_PALETTE, 0, 80);
        assert_eq!(
            line_text(&header),
            "#team-alpha  bea, cyd, and 3 others"
        );
        assert!(header.spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert!(header.spans[1].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(line_text(&card_elision(5)), "  … 5 more replies");
        assert_eq!(line_text(&card_elision(1)), "  … 1 more reply");
        assert!(card_elision(5).spans[0]
            .style
            .add_modifier
            .contains(Modifier::DIM));
        // A pane too short for an elision line of its own carries the count
        // in the header instead, in the same words.
        assert_eq!(
            line_text(&card_header(&card, &TEST_PALETTE, 6, 80)),
            "#team-alpha  bea, cyd, and 3 others  · 6 more replies"
        );
        // That count is the only place the number is left, so it keeps its
        // columns and the names above it give theirs up: the participants
        // first, then the conversation, then the conversation entirely.
        // 18 columns is the count itself; below that nothing can be saved.
        for width in 18..=53 {
            let header = line_text(&card_header(&card, &TEST_PALETTE, 6, width));
            assert!(header.width() <= width, "{width}: {header:?}");
            assert!(header.ends_with("· 6 more replies"), "{width}: {header:?}");
        }
        assert_eq!(
            line_text(&card_header(&card, &TEST_PALETTE, 6, 40)),
            "#team-alpha  bea, cyd…  · 6 more replies"
        );
        assert_eq!(
            line_text(&card_header(&card, &TEST_PALETTE, 6, 26)),
            "#team-a…  · 6 more replies"
        );
        assert_eq!(
            line_text(&card_header(&card, &TEST_PALETTE, 6, 18)),
            "  · 6 more replies"
        );
        // Folding the last reply away adds it to the count; dropping the
        // elision line does not change what the count is.
        let counted = ThreadCard { last: Some(Msg::from_api("C1".into(),
            serde_json::json!({"ts": "2.000000", "user": "U2"})).expect("a reply")), ..card.clone() };
        assert_eq!(counted.elided(CardFit::Whole), 5);
        assert_eq!(counted.elided(CardFit::Folded), 6);
        assert_eq!(counted.elided(CardFit::Root), 6);
        // A thread whose only replies are already drawn folds to nothing.
        let single = ThreadCard { hidden: 0, ..counted };
        assert_eq!(single.elided(CardFit::Whole), 0);
        assert_eq!(single.elided(CardFit::Folded), 1);
        // No last reply at all: folding cannot invent one.
        assert_eq!(card.elided(CardFit::Folded), 5);
        // No participants known: the header is the conversation alone.
        let bare = ThreadCard { participants: String::new(), ..card };
        assert_eq!(line_text(&card_header(&bare, &TEST_PALETTE, 0, 80)), "#team-alpha");
    }

    #[test]
    fn shortcodes_rich_text_and_reaction_footers_stay_text_only() {
        let archive = Archive::stub(&[], &[]);
        let corpus = Corpus::stub(&[]);
        let mut context = ctx(&archive, &corpus);
        let literal = ":eyes: :custom_emoji: :_custom_: :skin-tone-2: 👀";
        assert_eq!(plain(&mrkdwn(literal, &context, Sty::default())), literal);
        let mut segments = Vec::new();
        rich_element(&serde_json::json!({"type":"rich_text_section", "elements":[
            {"type":"emoji", "name":"eyes", "unicode":"1f440"},
            {"type":"emoji", "name":"custom_emoji"}
        ]}), &context, Sty::default(), &mut segments);
        assert_eq!(plain(&segments), ":eyes::custom_emoji:\n");
        let message = Msg::from_api("C1".into(), serde_json::json!({
            "ts":"1.000000", "text":literal,
            "reactions":[{"name":"eyes", "count":2}, {"name":"custom_emoji", "count":3}]
        })).unwrap();
        for image_font in [None, Some((8, 16))] {
            context.image_font = image_font;
            for thread in [false, true] {
                let rendered = message_lines(&message, &context, 120, thread, 0);
                assert!(rendered.images.is_empty());
                assert!(rendered.lines.iter().any(|line| line_text(line).contains(":eyes:(2)   :custom_emoji:(3)")));
            }
        }
    }

    #[test]
    fn narrow_reaction_footers_preserve_every_name_and_count() {
        let archive = Archive::stub(&[], &[]);
        let corpus = Corpus::stub(&[]);
        let context = ctx(&archive, &corpus);
        let reactions = serde_json::json!([
            {"name":"a_very_long_custom_reaction_name", "count":12},
            {"name":"eyes", "count":3}, {"name":"final_reaction", "count":4}]);
        let with = Msg::from_api("C1".into(),serde_json::json!({"ts":"1.000000", "text":"body", "reactions":reactions})).unwrap();
        let mut without = with.clone();
        without.data.as_object_mut().unwrap().remove("reactions");
        for width in [12,20,40] {
            let baseline = message_lines(&without,&context,width,true,0).lines.len();
            let rendered = message_lines(&with,&context,width,true,0);
            let footer = &rendered.lines[baseline..];
            assert!(footer.len()>1);
            assert!(footer.iter().all(|line| line.width()<=width));
            let rebuilt = footer.iter().map(|line| line_text(line).strip_prefix("  ").unwrap().to_string()).collect::<String>();
            assert_eq!(rebuilt,":a_very_long_custom_reaction_name:(12)   :eyes:(3)   :final_reaction:(4)");
        }
    }

    #[test]
    fn today_labels_use_the_selected_calendar_date_in_headers() {
        let archive = Archive::stub(&[("U1", "Ada")], &[]);
        let corpus = Corpus::stub(&[]);
        let mut context = ctx(&archive, &corpus);
        let reference = Utc.with_ymd_and_hms(2026, 9, 8, 0, 30, 0).unwrap().timestamp();
        for timezone in [Tz::Utc, Tz::Local] {
            context.tz = timezone;
            let date = |seconds| match timezone {
                Tz::Utc => Utc.timestamp_opt(seconds,0).unwrap().date_naive(),
                Tz::Local => Local.timestamp_opt(seconds,0).unwrap().date_naive(),
            };
            for seconds in [reference-86400, reference, reference+8*3600, reference+86400] {
                let expected = if date(seconds)==date(reference) {"Today".to_string()}
                    else {timezone.fmt(seconds,"%a %Y-%m-%d")};
                assert_eq!(timezone.date_label(seconds,timezone.day(reference)),expected);
                for subtype in ["", "channel_join"] {
                    let message = Msg::from_api("C1".into(),serde_json::json!({
                        "ts":format!("{seconds}.000000"),"user":"U1","text":"hello","subtype":subtype
                    })).unwrap();
                    let time = timezone.fmt(seconds,match timezone {Tz::Utc=>"%H:%M UTC",Tz::Local=>"%H:%M %:z"});
                    for thread in [false,true] {
                        let rendered = message_lines(&message,&context,120,thread,timezone.day(reference));
                        assert!(line_text(&rendered.lines[0]).starts_with(&format!("{expected} {time}")));
                    }
                }
            }
        }
    }

    #[test]
    fn message_headers_include_date_and_timezone() {
        let a = Archive::stub(&[("U1", "Ada")], &[]);
        let corpus = Corpus::stub(&[]);
        let mut c = ctx(&a, &corpus);
        let reference = Utc.with_ymd_and_hms(2026, 9, 9, 12, 0, 0).unwrap().timestamp();
        let secs = Utc
            .with_ymd_and_hms(2026, 9, 7, 10, 59, 0)
            .unwrap()
            .timestamp();
        for subtype in ["", "channel_join"] {
            let m = Msg::from_api(
                "C1".into(),
                serde_json::json!({
                    "ts": format!("{secs}.000000"), "user":"U1", "text":"joined the channel",
                    "subtype": subtype,
                }),
            )
            .unwrap();
            for in_thread in [false, true] {
                let rendered = message_lines(&m, &c, 120, in_thread, c.tz.day(reference));
                assert!(line_text(&rendered.lines[0]).starts_with("Mon 2026-09-07 10:59 UTC"));
            }
            c.tz = Tz::Local;
            let expected = Local
                .timestamp_opt(secs, 0)
                .unwrap()
                .format("%a %Y-%m-%d %H:%M %:z")
                .to_string();
            assert!(line_text(&message_lines(&m, &c, 120, false, c.tz.day(reference)).lines[0]).starts_with(&expected));
            c.tz = Tz::Utc;
        }
    }

    #[test]
    fn mentions_links_and_entities() {
        let a = Archive::stub(&[("U1", "gabriel.clima")], &[("C1", "team-alpha")]);
        let none = Corpus::stub(&[]);
        let c = ctx(&a, &none);
        let segs = mrkdwn(
            "<@U1> see <#C1|team-alpha> &amp; <#C1>: <https://x.y/z|the page> or <https://x.y/z>",
            &c,
            Sty::default(),
        );
        assert_eq!(text(&segs), "@gabriel.clima see #team-alpha & #team-alpha: the page <https://x.y/z> or https://x.y/z");
        assert!(segs
            .iter()
            .any(|s| s.sty.mention && s.text == "@gabriel.clima"));
        assert!(segs.iter().any(|s| s.sty.link && s.text == "the page"));
    }

    #[test]
    fn palette_colors_own_author_and_unread_differently() {
        let mut palette = Palette::default();
        palette.set(Role::OwnUsername, ratatui::style::Color::Green);
        palette.set(Role::Unread, ratatui::style::Color::LightRed);
        let own = style_of(
            Sty {
                me: true,
                ..Sty::default()
            },
            &palette,
        );
        assert_eq!(own.fg, Some(ratatui::style::Color::Green));
        let divider = divider_new("new", 20, &palette);
        assert_eq!(
            divider.spans[0].style.fg,
            Some(ratatui::style::Color::LightRed)
        );
    }

    #[test]
    fn channel_name_falls_back_to_the_corpus() {
        let a = Archive::stub(&[], &[]);
        let all = Corpus::stub(&[("C9", "alerts-alpha")]);
        let c = ctx(&a, &all);
        assert_eq!(
            text(&mrkdwn("on<#C9>", &c, Sty::default())),
            "on#alerts-alpha"
        );
        assert_eq!(text(&mrkdwn("<#C8>", &c, Sty::default())), "#C8");
    }

    #[test]
    fn label_that_is_the_url_is_dropped() {
        assert!(label_is_url(
            "jira.example.com/browse/TKT-1",
            "https://jira.example.com/browse/TKT-1"
        ));
        assert!(label_is_url(
            "harbor.example.com/harbor/…/sample_builder_image",
            "https://harbor.example.com/harbor/projects/22/repositories/sample_builder_image/"
        ));
        assert!(!label_is_url(
            "PRJ-278",
            "https://jira.example.com/browse/PRJ-278"
        ));
    }

    #[test]
    fn emphasis_code_and_fences() {
        let a = Archive::stub(&[], &[]);
        let none = Corpus::stub(&[]);
        let c = ctx(&a, &none);
        let segs = mrkdwn(
            "*bold* and `code` but snake_case and 2*3*4 stay",
            &c,
            Sty::default(),
        );
        assert_eq!(text(&segs), "bold and code but snake_case and 2*3*4 stay");
        assert!(segs.iter().any(|s| s.sty.bold && s.text == "bold"));
        assert!(segs.iter().any(|s| s.sty.code && s.text == "code"));
        let segs = mrkdwn(
            "before\n```\nint x = 1 &lt; 2;\n```\nafter",
            &c,
            Sty::default(),
        );
        let pre: Vec<&Seg> = segs.iter().filter(|s| s.sty.pre).collect();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].text, "int x = 1 < 2;");
    }

    #[test]
    fn quotes_keep_their_bar_on_blank_lines() {
        let a = Archive::stub(&[], &[]);
        let none = Corpus::stub(&[]);
        let c = ctx(&a, &none);
        let q = Sty {
            quote: true,
            ..Sty::default()
        };
        let segs = mrkdwn("first\n\nsecond", &c, q);
        let lines = wrap(&segs, 40, "  ", &TEST_PALETTE);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts, vec!["  │ first", "  │ ", "  │ second"]);
        let segs = mrkdwn("&gt; quoted line\nplain", &c, Sty::default());
        let texts: Vec<String> = wrap(&segs, 40, "", &TEST_PALETTE)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(texts, vec!["│ quoted line", "plain"]);
    }

    #[test]
    fn dividers_fit_the_width_and_images_get_a_column_on_narrow_panes() {
        for w in [4usize, 12, 20, 60, 200] {
            let d = line_text(&divider("Wed 2026-09-02", w));
            let n = line_text(&divider_new("new", w, &TEST_PALETTE));
            assert!(
                d.width() <= w.max(20),
                "divider {} wide for width {w}",
                d.width()
            );
            assert!(
                n.width() <= w.max(9),
                "new divider {} wide for width {w}",
                n.width()
            );
        }
        let f = FileInfo {
            id: "F1".into(),
            channel: "C1".into(),
            name: "a.png".into(),
            filetype: "png".into(),
            size: None,
            mode: "hosted".into(),
            mimetype: "image/png".into(),
            width: 4000,
            height: 1000,
            thumb: None,
            url: None,
        };
        let (cols, rows) = image_cells(&f, (10, 20), 5).unwrap();
        assert!(cols >= 1 && rows >= 1);
        let (cols, rows) = image_cells(&f, (10, 20), 120).unwrap();
        assert!(cols <= 80 && rows <= 14, "{cols}x{rows}");
    }

    #[test]
    fn wrap_breaks_between_words_and_splits_long_words() {
        let segs = vec![Seg::new("one two three four five", Sty::default())];
        let texts: Vec<String> = wrap(&segs, 11, "", &TEST_PALETTE)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(texts, vec!["one two", "three four", "five"]);
        let segs = vec![Seg::new(
            "x https://very.long.example/path/that/does/not/fit y",
            Sty::default(),
        )];
        let texts: Vec<String> = wrap(&segs, 12, "", &TEST_PALETTE)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            texts,
            vec!["x", "https://very", ".long.exampl", "e/path/that/", "does/not/fit", "y"]
        );
    }

    #[test]
    fn rich_text_blocks_render_lists_and_links() {
        let a = Archive::stub(&[("U1", "gwen.parker")], &[]);
        let none = Corpus::stub(&[]);
        let c = ctx(&a, &none);
        let blocks: Value = serde_json::from_str(
            r#"[{"type":"rich_text","elements":[
                {"type":"rich_text_section","elements":[{"type":"text","text":"hi "},{"type":"user","user_id":"U1"},{"type":"text","text":" see "},{"type":"link","url":"https://a.b/c","text":"here"}]},
                {"type":"rich_text_list","style":"bullet","elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"one"}]},
                    {"type":"rich_text_section","elements":[{"type":"text","text":"two","style":{"bold":true}}]}]}]}]"#,
        )
        .unwrap();
        let segs = render_blocks(blocks.as_array().unwrap(), &c, Sty::default());
        let texts: Vec<String> = wrap(&segs, 60, "", &TEST_PALETTE)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            texts,
            vec!["hi @gwen.parker see here <https://a.b/c>", "• one", "• two"]
        );
    }
}
