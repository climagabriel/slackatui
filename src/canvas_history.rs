//! Read-only canvas revision metadata from the endpoint used by Slack's client.
use crate::{
    api::Client,
    palette::{Palette, Role},
};
use chrono::{DateTime, Utc};
use ratatui::{
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{List, ListItem, ListState, Paragraph},
    Frame,
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[derive(Clone, Debug)]
pub struct Revision {
    pub id: String,
    pub sequence: i64,
    pub created_ms: i64,
    pub author: String,
    pub comment: bool,
}
impl Revision {
    fn time(&self) -> String {
        DateTime::<Utc>::from_timestamp_millis(self.created_ms)
            .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "Unknown date".into())
    }
}
#[derive(Clone, Debug)]
pub struct Page {
    pub revisions: Vec<Revision>,
    pub older: Option<i64>,
    pub limited: bool,
    pub names: HashMap<String, String>,
}
fn number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}
fn parse(response: &Value, requested: Option<i64>) -> Result<Page, String> {
    let rows = response["versions"]
        .as_array()
        .ok_or("Canvas history: Slack returned no revision list")?;
    let mut revisions = Vec::new();
    for row in rows {
        let id = row["version_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Canvas history: revision has no ID")?;
        revisions.push(Revision {
            id: id.into(),
            sequence: number(&row["sequence"]).ok_or("Canvas history: revision has no sequence")?,
            created_ms: number(&row["created_ms"]).ok_or("Canvas history: revision has no date")?,
            author: row["author"].as_str().unwrap_or("unknown").into(),
            comment: row["is_comment"].as_bool().unwrap_or(false),
        });
    }
    let cursor = number(&response["oldest_created_usec"])
        .ok_or("Canvas history: Slack returned no valid pagination cursor")?;
    let older =
        Some(cursor).filter(|cursor| *cursor >= 0 && requested.is_none_or(|last| *cursor < last));
    Ok(Page {
        revisions,
        older,
        limited: response["history_limited"].as_bool().unwrap_or(false),
        names: HashMap::new(),
    })
}
pub fn load(
    client: &Client,
    file: &str,
    older: Option<i64>,
    mut names: HashMap<String, String>,
    cancelled: &AtomicBool,
) -> Result<Page, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("Canvas history cancelled".into());
    }
    let cursor = older.map(|cursor| cursor.to_string());
    let mut params = vec![("file_id", file), ("limit", "100")];
    if let Some(cursor) = &cursor {
        params.push(("oldest_created_usec", cursor));
    }
    let response = client
        .call("quip.history.getVersions", &params)
        .map_err(|error| {
            if error.contains("owner_disabled") {
                "Canvas version history is disabled in Slack.".into()
            } else {
                format!("Canvas history: {error}")
            }
        })?;
    let mut page = parse(&response, older)?;
    // Resolve each distinct author once. A failed lookup keeps the usable ID.
    for revision in &page.revisions {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Canvas history cancelled".into());
        }
        if !names.contains_key(&revision.author) {
            let name = client
                .call("users.info", &[("user", &revision.author)])
                .ok()
                .and_then(|response| {
                    [
                        "/user/profile/display_name",
                        "/user/real_name",
                        "/user/name",
                    ]
                    .iter()
                    .find_map(|path| {
                        response
                            .pointer(path)
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    })
                })
                .unwrap_or_else(|| revision.author.clone());
            names.insert(revision.author.clone(), name);
        }
    }
    page.names = names;
    Ok(page)
}

#[derive(Default)]
pub struct History {
    pub cancelled: Arc<AtomicBool>,
    pub revisions: Vec<Revision>,
    pub names: HashMap<String, String>,
    pub older: Option<i64>,
    pub limited: bool,
    pub cursor: usize,
    pub details: bool,
    pub loaded: bool,
    pub failed: bool,
    pub scroll: usize,
}
impl Drop for History {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
impl History {
    pub fn append(&mut self, page: Page) {
        let mut seen: HashSet<_> = self
            .revisions
            .iter()
            .map(|r| (r.id.clone(), r.sequence))
            .collect();
        self.revisions.extend(
            page.revisions
                .into_iter()
                .filter(|r| seen.insert((r.id.clone(), r.sequence))),
        );
        self.revisions
            .sort_by_key(|r| std::cmp::Reverse((r.created_ms, r.sequence)));
        self.names.extend(page.names);
        self.older = page.older;
        self.limited |= page.limited;
        self.loaded = true;
        self.failed = false;
        self.cursor = self.cursor.min(self.count().saturating_sub(1));
    }
    pub fn count(&self) -> usize {
        self.revisions.len() + usize::from(self.older.is_some())
    }
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, palette: &Palette, permalink: &str) {
        if self.details {
            if let Some(revision) = self.revisions.get(self.cursor) {
                let author = self.names.get(&revision.author).unwrap_or(&revision.author);
                let text = format!("{}\n{} ({})\n{}\n\nRevision: {}\nSequence: {}\n\nRevision contents and visual diffs are available in Slack:\n{}",
                    revision.time(), author, revision.author, if revision.comment { "Comment added" } else { "Canvas edited" }, revision.id, revision.sequence,
                    if permalink.is_empty() { "Open this canvas in Slack and choose Version history." } else { permalink });
                let rows = crate::canvas::wrap_lines(&text, area.width as usize);
                self.scroll = self
                    .scroll
                    .min(rows.len().saturating_sub(area.height as usize));
                let rows: Vec<_> = rows
                    .into_iter()
                    .skip(self.scroll)
                    .take(area.height as usize)
                    .map(Line::from)
                    .collect();
                frame.render_widget(Paragraph::new(rows), area);
            }
            return;
        }
        let mut rows: Vec<ListItem> = self
            .revisions
            .iter()
            .map(|revision| {
                let author = self.names.get(&revision.author).unwrap_or(&revision.author);
                ListItem::new(Line::from(format!(
                    "{}  {}{}",
                    revision.time(),
                    author,
                    if revision.comment { " · comment" } else { "" }
                )))
            })
            .collect();
        if self.older.is_some() {
            rows.push(ListItem::new("Load older revisions…"));
        }
        if rows.is_empty() {
            let note = if !self.loaded && self.failed {
                "History unavailable. Press r to retry or h to return to the canvas."
            } else if !self.loaded {
                "Loading canvas history…"
            } else if self.limited {
                "No revisions available within Slack's history limit."
            } else {
                "Slack returned no revisions for this canvas."
            };
            frame.render_widget(Paragraph::new(note), area);
            return;
        }
        let mut state = ListState::default().with_selected(Some(self.cursor));
        frame.render_stateful_widget(
            List::new(rows).highlight_style(
                Style::new()
                    .fg(palette.get(Role::SelectionText))
                    .bg(palette.get(Role::SelectionBackground)),
            ),
            area,
            &mut state,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn history_pagination_keeps_microseconds_and_deduplicates_versions() {
        let first = parse(&json!({"versions":[{"version_id":"one","sequence":2,"created_ms":1788860206343_i64,"author":"U1"}],"oldest_created_usec":1788847670219036_i64}), None).unwrap();
        assert_eq!(first.older, Some(1788847670219036));
        let mut history = History::default();
        history.append(first.clone());
        history.append(first);
        assert_eq!(history.revisions.len(), 1);
        let end = parse(
            &json!({"versions":[],"oldest_created_usec":-1,"history_limited":true}),
            history.older,
        )
        .unwrap();
        history.append(end);
        assert!(history.older.is_none());
        assert!(history.limited);
        assert_eq!(history.revisions.len(), 1);
        assert!(
            parse(&json!({"versions":[],"oldest_created_usec":10}), Some(10))
                .unwrap()
                .older
                .is_none()
        );
        assert!(
            parse(&json!({"versions":[],"oldest_created_usec":9}), Some(10))
                .unwrap()
                .older
                .is_some()
        );
        assert!(parse(&json!({}), None).is_err());
        assert!(parse(&json!({"versions":[],"oldest_created_usec":null}), None).is_err());
        assert!(parse(&json!({"versions":[{}]}), None).is_err());
    }
    #[test]
    fn loader_only_reads_and_reuses_author_names() {
        let client = Client::for_test(|method, params| match method {
            "quip.history.getVersions" => {
                assert!(params.contains(&("file_id", "F1")));
                assert!(params.contains(&("oldest_created_usec", "1788847670219036")));
                Ok(
                    json!({"versions":[{"version_id":"one","sequence":1,"created_ms":1000,"author":"U1"},{"version_id":"two","sequence":2,"created_ms":2000,"author":"U2"}],"oldest_created_usec":-1}),
                )
            }
            "users.info" => {
                assert_eq!(params, &[("user", "U2")]);
                Ok(json!({"user":{"profile":{"display_name":"Second author"}}}))
            }
            _ => panic!("Unexpected call: {method}"),
        });
        let page = load(
            &client,
            "F1",
            Some(1788847670219036),
            HashMap::from([("U1".into(), "First author".into())]),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(page.names["U1"], "First author");
        assert_eq!(page.names["U2"], "Second author");
        let client =
            Client::for_test(|_, _| Err("quip.history.getVersions: owner_disabled".into()));
        assert!(
            load(&client, "F1", None, HashMap::new(), &AtomicBool::new(false))
                .unwrap_err()
                .contains("disabled in Slack")
        );
    }
    #[test]
    fn cancellation_stops_author_requests_after_history_response() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let client = Client::for_test(move |method, _| {
            assert_eq!(method, "quip.history.getVersions");
            signal.store(true, Ordering::Relaxed);
            Ok(
                json!({"versions":[{"version_id":"one","sequence":1,"created_ms":1000,"author":"U1"}],"oldest_created_usec":-1}),
            )
        });
        assert!(load(&client, "F1", None, HashMap::new(), &cancelled)
            .unwrap_err()
            .contains("cancelled"));
        let history = History::default();
        let signal = history.cancelled.clone();
        drop(history);
        assert!(signal.load(Ordering::Relaxed));
    }
    #[test]
    fn history_renders_authors_and_narrow_details_scroll_to_canvas_link() {
        let mut history = History::default();
        let mut page = parse(&json!({"versions":[{"version_id":"revision-one","sequence":1,"created_ms":1000,"author":"U1"}],"oldest_created_usec":-1}), None).unwrap();
        page.names.insert("U1".into(), "Canvas author".into());
        history.append(page);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|frame| {
                history.draw(
                    frame,
                    frame.area(),
                    &Palette::default(),
                    "https://example.invalid/canvas",
                )
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("1970-01-01 00:00:01 UTC  Canvas author"));
        history.details = true;
        history.scroll = usize::MAX;
        terminal
            .draw(|frame| {
                history.draw(
                    frame,
                    frame.area(),
                    &Palette::default(),
                    "https://example.invalid/canvas",
                )
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("https://example.invalid/canvas"));
        assert!(text.contains("Revision contents and visual diffs are available in Slack"));
    }
}
