//! Leaf-value navigation over JSON, with Slack message and browser links.
use crate::{
    archive::Msg,
    palette::{Palette, Role},
    render,
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::Value;
use std::{
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub host: String,
    pub channel: String,
    pub focus: i64,
    pub root: Option<i64>,
}

fn timestamp(text: &str) -> Option<i64> {
    let (seconds, fraction) = text.split_once('.')?;
    if seconds.is_empty()
        || fraction.is_empty()
        || fraction.len() > 6
        || !seconds
            .bytes()
            .chain(fraction.bytes())
            .all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let seconds: i64 = seconds.parse().ok()?;
    let fraction: i64 = format!("{fraction:0<6}").parse().ok()?;
    seconds
        .checked_mul(1_000_000)?
        .checked_add(fraction)
        .filter(|id| *id > 0)
}

impl Link {
    pub fn parse(text: &str) -> Option<Self> {
        let url = text.replace("&amp;", "&");
        let (host, path) = url.strip_prefix("https://")?.split_once('/')?;
        let host = host.to_ascii_lowercase();
        let workspace = host.strip_suffix(".slack.com")?;
        if workspace.is_empty()
            || !workspace
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return None;
        }
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let mut parts = path.split('/');
        if parts.next()? != "archives" {
            return None;
        }
        let channel = parts.next()?;
        if !matches!(channel.as_bytes().first(), Some(b'C' | b'D' | b'G'))
            || !channel.bytes().all(|c| c.is_ascii_alphanumeric())
        {
            return None;
        }
        let packed = parts.next()?.split('#').next()?.strip_prefix('p')?;
        if parts.next().is_some() || packed.len() < 7 || !packed.bytes().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let (seconds, fraction) = packed.split_at(packed.len() - 6);
        let focus = timestamp(&format!("{seconds}.{fraction}"))?;
        let mut root = None;
        for parameter in query.split('#').next()?.split('&') {
            if let Some(value) = parameter.strip_prefix("thread_ts=") {
                let value = value.replace("%2E", ".").replace("%2e", ".");
                root = Some(timestamp(&value)?);
            }
        }
        Some(Self {
            url: url.clone(),
            host,
            channel: channel.into(),
            focus,
            root,
        })
    }

    pub fn in_workspace(&self, workspace_url: &str) -> bool {
        workspace_url
            .trim_end_matches('/')
            .strip_prefix("https://")
            .is_some_and(|host| host.eq_ignore_ascii_case(&self.host))
    }
}

pub struct Leaf {
    pub line: usize,
    pub value: Range<usize>,
    pub path: String,
    pub link: Option<Link>,
    pub web_url: Option<String>,
}

pub struct Browser {
    pub id: u64,
    pub lines: Vec<String>,
    pub leaves: Vec<Leaf>,
    pub cursor: usize,
    pub scroll: usize,
    reveal: bool,
    viewport: (usize, usize),
}

impl Browser {
    pub fn new(value: &Value) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let mut browser = Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            lines: vec![],
            leaves: vec![],
            cursor: 0,
            scroll: 0,
            reveal: true,
            viewport: (0, 0),
        };
        browser.emit(value, 0, String::new(), "$".into(), false);
        browser.cursor = browser
            .leaves
            .iter()
            .position(|leaf| leaf.web_url.is_some())
            .unwrap_or(0);
        browser
    }

    fn emit(&mut self, value: &Value, depth: usize, key: String, path: String, comma: bool) {
        let prefix = format!("{}{key}", "  ".repeat(depth));
        let suffix = if comma { "," } else { "" };
        match value {
            Value::Object(map) if !map.is_empty() => {
                self.lines.push(format!("{prefix}{{"));
                for (index, (key, child)) in map.iter().enumerate() {
                    let label = serde_json::to_string(key).unwrap();
                    self.emit(
                        child,
                        depth + 1,
                        format!("{label}: "),
                        format!("{path}[{label}]"),
                        index + 1 < map.len(),
                    );
                }
                self.lines.push(format!("{}}}{suffix}", "  ".repeat(depth)));
            }
            Value::Array(array) if !array.is_empty() => {
                self.lines.push(format!("{prefix}["));
                for (index, child) in array.iter().enumerate() {
                    self.emit(
                        child,
                        depth + 1,
                        String::new(),
                        format!("{path}[{index}]"),
                        index + 1 < array.len(),
                    );
                }
                self.lines.push(format!("{}]{suffix}", "  ".repeat(depth)));
            }
            _ => {
                let serialized = serde_json::to_string(value).unwrap();
                let line = self.lines.len();
                let mut links = Vec::new();
                if let Some(text) = value.as_str() {
                    for (start, _) in text.to_ascii_lowercase().match_indices("http") {
                        let end = text[start..]
                            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '|' | '"'))
                            .map(|length| start + length).unwrap_or(text.len());
                        let mut candidate = text[start..end].trim_end_matches([',', '.', ';']);
                        if text[..start].ends_with('\'') { candidate = candidate.trim_end_matches('\''); }
                        loop {
                            let previous = candidate.len();
                            for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
                                while candidate.ends_with(close) && candidate.matches(close).count() > candidate.matches(open).count() {
                                    candidate = candidate.strip_suffix(close).unwrap();
                                }
                            }
                            if candidate.len() == previous { break; }
                        }
                        if let Some(url) = web_url(&candidate.replace("&amp;", "&")) {
                            let escaped_prefix = serde_json::to_string(&text[..start]).unwrap();
                            let escaped_link = serde_json::to_string(candidate).unwrap();
                            let begin = prefix.len() + escaped_prefix.len() - 1;
                            links.push(Leaf {
                                line, value: begin..begin + escaped_link.len() - 2,
                                path: path.clone(), link: Link::parse(candidate), web_url: Some(url),
                            });
                        }
                    }
                }
                if links.is_empty() {
                    self.leaves.push(Leaf {
                        line,
                        value: prefix.len()..prefix.len() + serialized.len(),
                        path,
                        link: None,
                        web_url: None,
                    });
                } else {
                    self.leaves.extend(links);
                }
                self.lines.push(format!("{prefix}{serialized}{suffix}"));
            }
        }
    }

    pub fn selected(&self) -> Option<&Leaf> {
        self.leaves.get(self.cursor)
    }
    pub fn move_cursor(&mut self, delta: isize) {
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.leaves.len().saturating_sub(1));
        self.reveal = true;
    }
    pub fn scroll_lines(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
        self.reveal = false;
    }
    pub fn label(&self) -> String {
        self.selected()
            .map(|leaf| {
                format!(
                    "{}/{} · {}{}",
                    self.cursor + 1,
                    self.leaves.len(),
                    leaf.path,
                    if leaf.link.is_some() {
                        " · Enter: follow Slack message"
                    } else if leaf.web_url.is_some() {
                        " · Enter: open in browser"
                    } else {
                        ""
                    }
                )
            })
            .unwrap_or_default()
    }
    pub fn rows(&mut self, width: usize, height: usize, palette: &Palette) -> Vec<Line<'static>> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        if self.viewport != (width, height) {
            self.viewport = (width, height);
            self.reveal = true;
        }
        let selected = self.selected();
        let mut rows = Vec::new();
        let mut selected_row = None;
        for (index, text) in self.lines.iter().enumerate() {
            let highlighted = selected
                .filter(|leaf| leaf.line == index)
                .map(|leaf| &leaf.value);
            let syntax = render::json_line(text, palette);
            let mut row = Vec::new();
            let mut columns = 0;
            let mut byte = 0;
            for span in syntax.spans {
                for grapheme in span.styled_graphemes(Style::default()) {
                    let symbol = grapheme.symbol;
                    let size = symbol.width();
                    let shown = if size > width { "�" } else { symbol };
                    let shown_width = shown.width();
                    if columns + shown_width > width && !row.is_empty() {
                        rows.push(Line::from(std::mem::take(&mut row)));
                        columns = 0;
                    }
                    let style = if highlighted.is_some_and(|range| range.contains(&byte)) {
                        selected_row.get_or_insert(rows.len());
                        Style::new()
                            .fg(palette.get(Role::SelectionText))
                            .bg(palette.get(Role::SelectionBackground))
                    } else {
                        span.style
                    };
                    if let Some(previous) = row
                        .last_mut()
                        .filter(|span: &&mut Span<'static>| span.style == style)
                    {
                        previous.content.to_mut().push_str(shown);
                    } else {
                        row.push(Span::styled(shown.to_string(), style));
                    }
                    columns += shown_width;
                    byte += symbol.len();
                }
            }
            rows.push(Line::from(row));
        }
        if self.reveal {
            if let Some(row) = selected_row {
                if row < self.scroll {
                    self.scroll = row;
                } else if row >= self.scroll.saturating_add(height) {
                    self.scroll = row.saturating_sub(height.saturating_sub(1));
                }
            }
            self.reveal = false;
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(height));
        rows.into_iter().skip(self.scroll).take(height).collect()
    }
}

fn web_url(text: &str) -> Option<String> {
    let url = text;
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") { return None; }
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || url.chars().any(|c| c.is_control() || c.is_whitespace()) { return None; }
    Some(url.to_string())
}

pub fn browser_command(url: &str) -> Result<std::process::Command, String> {
    let url = web_url(url).ok_or("Only HTTP and HTTPS links can open in the browser")?;
    let mut command = std::process::Command::new("xdg-open");
    command.arg(url).stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    Ok(command)
}

pub fn open_browser(url: &str) -> Result<(), String> {
    let status = browser_command(url)?.status().map_err(|error| format!("Could not start browser: {error}"))?;
    if status.success() { Ok(()) } else { Err(format!("Browser opener failed: {status}")) }
}

/// A reply permalink may omit its thread timestamp; cached filenames use the root.
pub fn cached_reply(cache: &std::path::Path, channel: &str, focus: i64) -> Option<Vec<Msg>> {
    let prefix = format!("{channel}-");
    for entry in std::fs::read_dir(cache.join("threads")).ok()?.flatten() {
        let name = entry.file_name();
        let Some(root) = name
            .to_str()
            .and_then(|name| name.strip_prefix(&prefix))
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|root| root.parse::<i64>().ok())
        else {
            continue;
        };
        let Some(messages) = crate::live::cached_thread(cache, channel, root) else {
            continue;
        };
        if messages.iter().any(|message| message.id == focus) {
            return Some(messages);
        }
    }
    None
}

/// Resolve a permalink using only Slack reads. Never navigate to a nearby message.
pub fn fetch(client: &crate::api::Client, link: &Link) -> Result<Vec<Msg>, String> {
    let mut root = link.root.unwrap_or(link.focus);
    if link.root.is_none() {
        let timestamp = crate::live::id_to_ts(link.focus);
        let response = client.call(
            "conversations.history",
            &[
                ("channel", &link.channel),
                ("oldest", &timestamp),
                ("latest", &timestamp),
                ("inclusive", "true"),
                ("limit", "1"),
            ],
        )?;
        if let Some(message) = response["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| Msg::from_api(link.channel.clone(), value.clone()))
            .find(|message| message.id == link.focus)
        {
            root = message.thread_root();
            if !message.has_thread() && message.parent_id.is_none() {
                return Ok(vec![message]);
            }
        }
    }
    let messages: Vec<_> = client
        .replies(&link.channel, &crate::live::id_to_ts(root))?
        .into_iter()
        .filter_map(|value| Msg::from_api(link.channel.clone(), value))
        .collect();
    if !messages.iter().any(|message| message.id == link.focus) {
        return Err("Linked message is unavailable or not accessible in Slack".into());
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const FIRST: &str = "https://myorg.slack.com/archives/C123/p1788811422381186?thread_ts=1788765950.129609&cid=C123";
    const SECOND: &str = "https://myorg.slack.com/archives/D456/p1788811422381187";

    #[test]
    fn leaf_navigation_preserves_json_and_skips_containers() {
        let data =
            json!({"a":[{"b":true,"c":null},3],"empty":{},"list":[],"text":"é \"quoted\"\nline"});
        let mut browser = Browser::new(&data);
        assert_eq!(
            browser.lines.join("\n"),
            serde_json::to_string_pretty(&data).unwrap()
        );
        let values: Vec<_> = browser
            .leaves
            .iter()
            .map(|leaf| browser.lines[leaf.line][leaf.value.clone()].to_string())
            .collect();
        assert_eq!(
            values,
            [
                "true",
                "null",
                "3",
                "{}",
                "[]",
                "\"é \\\"quoted\\\"\\nline\""
            ]
        );
        assert_eq!(browser.leaves[0].path, "$[\"a\"][0][\"b\"]");
        browser.move_cursor(isize::MAX);
        assert_eq!(browser.cursor, 5);
        browser.move_cursor(isize::MIN);
        assert_eq!(browser.cursor, 0);
    }

    #[test]
    fn links_in_one_leaf_are_individually_selectable_after_escaped_unicode() {
        let data = json!({"before":false,"text":format!("é \"quoted\"\n<{FIRST}|first> and <{SECOND}|second>")});
        let mut browser = Browser::new(&data);
        assert_eq!(browser.leaves.len(), 3);
        assert_eq!(browser.cursor, 1);
        for expected in [FIRST, SECOND] {
            let leaf = browser.selected().unwrap();
            assert_eq!(leaf.link.as_ref().unwrap().url, expected);
            assert_eq!(&browser.lines[leaf.line][leaf.value.clone()], expected);
            browser.move_cursor(1);
        }
        assert_eq!(
            browser.lines.join("\n"),
            serde_json::to_string_pretty(&data).unwrap()
        );
    }

    #[test]
    fn web_links_are_selectable_and_browser_arguments_are_literal() {
        let external = "https://example.org/page_(details)?a=1&amp;b=2";
        let data=json!({"text":format!("é \"quoted\" <{external}|label> then (http://example.org/second). {FIRST}")});
        let mut browser=Browser::new(&data);
        assert_eq!(browser.leaves.len(),3);
        assert!(browser.selected().unwrap().link.is_none());
        assert_eq!(browser.selected().unwrap().web_url.as_deref(),Some("https://example.org/page_(details)?a=1&b=2"));
        assert!(browser.label().contains("open in browser"));
        assert_eq!(&browser.lines[browser.selected().unwrap().line][browser.selected().unwrap().value.clone()],external);
        browser.move_cursor(1);
        assert_eq!(browser.selected().unwrap().web_url.as_deref(),Some("http://example.org/second"));
        browser.move_cursor(1);
        assert_eq!(browser.selected().unwrap().link.as_ref().unwrap().url,FIRST);
        assert!(browser.label().contains("follow Slack message"));
        assert_eq!(browser.lines.join("\n"),serde_json::to_string_pretty(&data).unwrap());
        for text in [format!("[({SECOND})]"),format!("({SECOND})")] {
            let browser=Browser::new(&json!({"text":text}));
            assert_eq!(browser.selected().unwrap().link.as_ref().unwrap().url,SECOND);
        }
        for text in ["https://example.org/O'Reilly", "<https://example.org/O'Reilly|label>"] {
            let browser=Browser::new(&json!({"text":text}));
            assert_eq!(browser.selected().unwrap().web_url.as_deref(),Some("https://example.org/O'Reilly"));
        }
        let browser=Browser::new(&json!({"url":"https://example.org/?x=&amp;amp;y=1"}));
        let command=browser_command(browser.selected().unwrap().web_url.as_ref().unwrap()).unwrap();
        assert_eq!(command.get_args().next().unwrap(),"https://example.org/?x=&amp;y=1");
        let url="https://example.org/?value=$(touch%20/tmp/should-not-exist)&x=1";
        let command=browser_command(url).unwrap();
        assert_eq!(command.get_program(),"xdg-open");
        assert_eq!(command.get_args().collect::<Vec<_>>(),vec![std::ffi::OsStr::new(url)]);
        for bad in ["file:///etc/passwd","javascript:alert(1)","--help","https:///missing-host","http://example.org/\n"] {
            assert!(browser_command(bad).is_err());
        }
    }

    #[test]
    fn permalink_parsing_is_bounded_and_preserves_reply_focus() {
        let link = Link::parse(FIRST).unwrap();
        assert_eq!(
            (link.focus, link.root),
            (1788811422381186, Some(1788765950129609))
        );
        assert!(link.in_workspace("https://myorg.slack.com/"));
        assert!(!link.in_workspace("https://other.slack.com"));
        assert_eq!(Link::parse(&FIRST.replace("&cid", "&amp;cid")), Some(link));
        for bad in [
            "https://myorg.slack.com.evil/archives/C1/p1788811422381186",
            "https://myorg.slack.com@evil/archives/C1/p1788811422381186",
            "https://myorg.slack.com/archives/../p1788811422381186",
            "https://myorg.slack.com/archives/C1/p9999999999999999999999999999",
            "https://myorg.slack.com/archives/C1/p1788811422381186?thread_ts=-1.5",
            "https://myorg.slack.com/archives/C1/p1788811422381186?thread_ts=99999999999999.123456",
        ] {
            assert!(Link::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn wrapped_link_selection_stays_visible_after_navigation_and_resize() {
        let mut browser = Browser::new(
            &json!({"text":format!("{} {FIRST} {} {SECOND}","long prefix ".repeat(20),"between ".repeat(20))}),
        );
        let palette = Palette::default();
        for width in [12, 80, 20] {
            for cursor in [0, 1, 0] {
                browser.move_cursor(cursor as isize - browser.cursor as isize);
                let rows = browser.rows(width, 5, &palette);
                assert!(rows.iter().all(|row| row.width() <= width));
                assert!(rows
                    .iter()
                    .flat_map(|row| &row.spans)
                    .any(|span| span.style.bg == Some(palette.get(Role::SelectionBackground))));
            }
        }
        browser.scroll_lines(isize::MAX);
        assert!(!browser.rows(12, 5, &palette).is_empty());
        assert!(browser.rows(1, 0, &palette).is_empty());
    }

    #[test]
    fn wrapping_keeps_graphemes_whole_and_handles_single_column_panes() {
        let palette = Palette::default();
        let mut browser = Browser::new(&json!({"text":"👩‍💻é終"}));
        let original = browser.lines.concat();
        let rows = browser.rows(2, 100, &palette);
        assert_eq!(
            rows.iter().map(Line::to_string).collect::<String>(),
            original
        );
        assert!(rows.iter().any(|line| line.to_string() == "👩‍💻"));
        assert!(rows.iter().any(|line| line.to_string().contains("é")));
        let rows = browser.rows(1, 100, &palette);
        assert!(rows.iter().all(|line| line.width() <= 1));
        assert!(rows.iter().any(|line| line.to_string() == "�"));
        let mut ascii = Browser::new(&json!({"number":123}));
        let original = ascii.lines.concat();
        assert_eq!(
            ascii
                .rows(1, 100, &palette)
                .iter()
                .map(Line::to_string)
                .collect::<String>(),
            original
        );
        assert!(ascii.rows(0, 100, &palette).is_empty());
    }

    #[test]
    fn fetching_links_is_read_only_and_requires_exact_target() {
        let link = Link::parse(FIRST).unwrap();
        let client = crate::api::Client::for_test(|method, params| {
            assert_eq!(method, "conversations.replies");
            assert!(params.contains(&("channel", "C123")));
            assert!(params.contains(&("ts", "1788765950.129609")));
            Ok(
                json!({"messages":[{"ts":"1788765950.129609","reply_count":1},
                {"ts":"1788811422.381186","thread_ts":"1788765950.129609","text":"target"}]}),
            )
        });
        let messages = fetch(&client, &link).unwrap();
        assert_eq!(messages[1].id, link.focus);
        let mut missing = link.clone();
        missing.focus += 1;
        assert!(fetch(&client, &missing).is_err());
        let plain = Link::parse(SECOND).unwrap();
        let client = crate::api::Client::for_test(|method, params| {
            assert_eq!(method, "conversations.history");
            assert!(params.contains(&("oldest", "1788811422.381187")));
            assert!(params.contains(&("latest", "1788811422.381187")));
            Ok(json!({"messages":[{"ts":"1788811422.381187","text":"single message"}]}))
        });
        assert_eq!(fetch(&client, &plain).unwrap().len(), 1);
        let client = crate::api::Client::for_test(|_, _| Err("channel_not_found".into()));
        assert_eq!(fetch(&client, &plain).unwrap_err(), "channel_not_found");
    }
}
