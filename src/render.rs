//! Slack text into terminal lines: mrkdwn (`<@U..>`, `<url|label>`, entities,
//! `*bold*`, fences), Block Kit `rich_text` and `section` blocks, legacy
//! attachments (what alert bots post), then greedy word-wrap into styled
//! ratatui lines.

use chrono::{Datelike, Local, NaiveDate, TimeZone, Utc};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

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
    pub archive: &'a Archive,
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
pub struct ImageSlot {
    pub file: FileInfo,
    /// Index of the first reserved line within the message's lines.
    pub line: usize,
    pub cols: u16,
    pub rows: u16,
}

pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub images: Vec<ImageSlot>,
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
    fn channel(&self, cid: &str) -> String {
        self.archive
            .channel_name(cid)
            .or_else(|| self.corpus.channel_names.get(cid).cloned())
            .unwrap_or_else(|| cid.to_string())
    }

    pub fn user(&self, uid: &str) -> String {
        if uid == "USLACKBOT" {
            return "Slackbot".to_string();
        }
        self.archive
            .user(uid)
            .or_else(|| self.corpus.user_name(uid))
            .unwrap_or_else(|| uid.to_string())
    }

    pub fn user_is_bot(&self, uid: &str) -> bool {
        self.archive.user_is_bot(uid) || self.corpus.user_is_bot(uid)
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
                .user(uid)
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
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    /// Preformatted: kept verbatim, one line per source line, never wrapped.
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
}

impl Seg {
    fn new(text: impl Into<String>, sty: Sty) -> Seg {
        Seg {
            text: text.into(),
            sty,
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

/// A Slack emoji as a character: the element's own code points when it
/// carries them, else the shortcode table; a custom emoji stays `:name:`.
pub fn emoji(name: &str, unicode: Option<&str>) -> String {
    if let Some(u) = unicode.filter(|u| !u.is_empty()) {
        let chars: Option<String> = u
            .split('-')
            .map(|h| u32::from_str_radix(h, 16).ok().and_then(char::from_u32))
            .collect();
        if let Some(s) = chars {
            return s;
        }
    }
    let bare = name.split("::").next().unwrap_or(name);
    match emojis::get_by_shortcode(bare) {
        Some(e) => e.as_str().to_string(),
        None => format!(":{name}:"),
    }
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
                // `:name:` with a known shortcode becomes the character.
                if let Some(end) = find_char(cs, i + 1, ':') {
                    let name: String = cs[i + 1..end].iter().collect();
                    let plausible = !name.is_empty()
                        && name.len() <= 40
                        && name
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '+' | '-'));
                    if plausible {
                        if let Some(e) = emojis::get_by_shortcode(&name) {
                            flush(&mut buf, base, out);
                            out.push(Seg::new(e.as_str(), base));
                            i = end + 1;
                            continue;
                        }
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
            format!("@{rest}")
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
                out.push(Seg::new(
                    format!("@{}", if id.is_empty() { "group" } else { id }),
                    mention,
                ));
            }
            "channel" => out.push(Seg::new(
                format!("#{}", ctx.channel(s("channel_id"))),
                mention,
            )),
            "emoji" => out.push(Seg::new(
                emoji(s("name"), e.get("unicode").and_then(Value::as_str)),
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
    if !plain(&block_segs).trim().is_empty() {
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
    if plain(&segs).trim().is_empty() {
        segs = mrkdwn(&m.text, ctx, base);
    }
    if let Some(atts) = m.data.get("attachments").and_then(Value::as_array) {
        for att in atts {
            if !plain(&segs).trim().is_empty() {
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

/// Greedy word-wrap. A word wider than the line stays whole on its own line
/// (a long URL is clipped, never split, so it stays clickable).
pub fn wrap(segs: &[Seg], width: usize, indent: &str, palette: &Palette) -> Vec<Line<'static>> {
    let avail = width.saturating_sub(indent.width()).max(8);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
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
                let room = avail.saturating_sub(if q { 2 } else { 0 });
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
                for (t, s) in parts {
                    cur.push(Span::styled(t, style_of(s, palette)));
                }
                cur_w += w;
            }
        }
    }
    if cur_w > 0 {
        lines.push(finish(cur, indent, quote.unwrap_or(false)));
    }
    // Trim blank edges, collapse blank runs: fences and block joins leave
    // doubled breaks behind.
    while lines.first().is_some_and(is_blank) {
        lines.remove(0);
    }
    while lines.last().is_some_and(is_blank) {
        lines.pop();
    }
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    for l in lines {
        if is_blank(&l) && out.last().is_some_and(is_blank) {
            continue;
        }
        out.push(l);
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

/// One message as lines: header, wrapped body, files, reactions, thread
/// footer. `in_thread` drops the footer (the replies are on screen).
pub fn message_lines(m: &Msg, ctx: &Ctx, width: usize, in_thread: bool) -> Rendered {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let time = ctx.tz.fmt(m.secs(), "%H:%M");
    let sub = m.subtype.as_deref().unwrap_or("");
    if is_system(m) {
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
        let mut lines = wrap(&segs, width, "       · ", ctx.palette);
        if let Some(first) = lines.first_mut() {
            first.spans[0] = Span::styled(format!("{time}  · "), dim);
        }
        return Rendered {
            lines,
            images: Vec::new(),
        };
    }
    let mut lines = Vec::new();
    let mut images = Vec::new();
    let mut spans = vec![
        Span::styled(time, dim),
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
    lines.push(Line::from(spans));
    lines.extend(wrap(&body(m, ctx), width, "  ", ctx.palette));
    for f in m.files() {
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
        lines.push(Line::from(Span::styled(s, dim)));
        if let Some(font) = ctx.image_font {
            if let Some((cols, rows)) = image_cells(&f, font, width) {
                images.push(ImageSlot {
                    file: f.clone(),
                    line: lines.len(),
                    cols,
                    rows,
                });
                for _ in 0..rows {
                    lines.push(Line::from(""));
                }
            }
        }
    }
    let reactions = m.reactions();
    if !reactions.is_empty() {
        let s = reactions
            .iter()
            .map(|(n, c)| format!("{} {c}", emoji(n, None)))
            .collect::<Vec<_>>()
            .join("   ");
        lines.push(Line::from(Span::styled(format!("  {s}"), dim)));
    }
    if !in_thread && m.has_thread() {
        let last = m
            .latest_reply_id
            .map(|id| format!(" · last {}", ctx.tz.fmt(id / 1_000_000, "%Y-%m-%d %H:%M")))
            .unwrap_or_default();
        let n = m.archived_replies;
        let total = m.reply_count.max(n);
        let plural = |k: i64| if k == 1 { "reply" } else { "replies" };
        let s = if n == 0 {
            format!("  ↳ {total} {} · not archived", plural(total))
        } else if n < total {
            format!("  ↳ {n} of {total} {} archived{last}", plural(total))
        } else {
            format!("  ↳ {n} {}{last}", plural(n))
        };
        lines.push(Line::from(Span::styled(
            s,
            Style::new().fg(ctx.palette.get(Role::ThreadInfo)),
        )));
    }
    Rendered { lines, images }
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
    use super::*;

    static TEST_PALETTE: std::sync::LazyLock<Palette> = std::sync::LazyLock::new(Palette::default);

    fn ctx<'a>(archive: &'a Archive, corpus: &'a Corpus) -> Ctx<'a> {
        Ctx {
            archive,
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
    fn wrap_breaks_between_words_and_keeps_long_words_whole() {
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
            vec!["x", "https://very.long.example/path/that/does/not/fit", "y"]
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
