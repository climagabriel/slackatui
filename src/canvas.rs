//! Channel tabs and section-scoped canvas editing. Network work stays off the UI thread.
use crate::{
    api::Client,
    edit::Editor,
    palette::{Palette, Role},
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, List, ListItem, ListState, Paragraph},
    Frame,
};
use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Value};
use std::sync::{
    mpsc::{self, Receiver},
    Arc,
};

#[derive(Clone, Debug)]
pub struct Section {
    pub id: String,
    pub markdown: String,
    pub display: String,
    pub original: String,
    pub editable: bool,
}
#[derive(Clone, Debug)]
pub struct Document {
    pub permalink: String,
    pub id: String,
    pub title: String,
    pub writable: bool,
    pub sections: Vec<Section>,
}
#[derive(Clone, Debug)]
enum Target {
    Messages,
    Canvas(String),
    Files,
    Image(crate::archive::FileInfo, String),
    Bookmarks,
    Link(String),
}
#[derive(Clone, Debug)]
struct Tab {
    file: Option<crate::file_message::Metadata>,
    label: String,
    target: Target,
}
fn string(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").into()
}

// Convert only supported sections for editing. Unknown structures remain readable.
fn markdown(element: ElementRef<'_>, safe: &mut bool) -> String {
    let name = element.value().name();
    let mut content = String::new();
    if element.value().attrs().any(|(key, value)| match key {
        "id" => false,
        "class" => !matches!(value, "line" | "default-content"),
        "href" => !matches!(name, "a" | "lnk"),
        "value" => name != "li" || value != "1",
        "data-remapped" => name != "control" || value != "true",
        _ => true,
    }) {
        *safe = false;
    }

    if element.value().attr("style").is_some()
        || element.value().attr("data-section-style").is_some()
    {
        *safe = false;
    }
    if name == "table" {
        *safe = false;
        let rows = Selector::parse("tr").unwrap();
        let cells = Selector::parse("th, td").unwrap();
        return element
            .select(&rows)
            .map(|row| {
                row.select(&cells)
                    .map(|cell| cell.text().collect::<String>())
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .collect::<Vec<_>>()
            .join("\n");
    }

    if name == "code" || name == "pre" {
        if name == "pre"
            || element.value().attrs().any(|(key, _)| key != "id")
            || element.children().any(|c| ElementRef::wrap(c).is_some())
        {
            *safe = false;
        }
        let raw = element.text().collect::<String>();
        let fence = "`".repeat(
            raw.split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0)
                .max(if name == "pre" { 2 } else { 0 })
                + 1,
        );
        return if name == "pre" {
            format!("{fence}\n{raw}\n{fence}")
        } else {
            format!("{fence} {raw} {fence}")
        };
    }
    for child in element.children() {
        if let Some(text) = child.value().as_text() {
            for c in text.chars() {
                if "\\`*_[]<>!#+-.=|~".contains(c) {
                    content.push('\\');
                }
                content.push(c);
            }
        } else if let Some(child) = ElementRef::wrap(child) {
            content.push_str(&markdown(child, safe));
        }
    }
    match name {
        "b" | "strong" => format!("**{content}**"),
        "i" | "em" => format!("*{content}*"),
        "s" | "del" | "strike" => format!("~~{content}~~"),
        "br" => "  \n".into(),
        "h1" => format!("# {content}"),
        "h2" => format!("## {content}"),
        "h3" => format!("### {content}"),
        "lnk" | "a" => {
            if let Some(url) = element.value().attr("href") {
                format!(
                    "[{content}](<{}>)",
                    url.replace('>', "%3E").replace('<', "%3C")
                )
            } else if (content.starts_with("@U") || content.starts_with("@W"))
                && content[1..]
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            {
                format!("![]({})", content)
            } else {
                *safe = false;
                content
            }
        }
        "p" if element
            .parent()
            .and_then(ElementRef::wrap)
            .is_some_and(|p| p.value().name() == "blockquote") =>
        {
            format!("{content}\n\n")
        }
        "p" | "span" | "control" => content,
        "li" => {
            let prefix = if element
                .parent()
                .and_then(ElementRef::wrap)
                .is_some_and(|e| e.value().name() == "ol")
            {
                "1. "
            } else {
                "- "
            };
            format!("{prefix}{}", content.trim_end().replace('\n', "\n  "))
        }
        "blockquote" => content
            .lines()
            .map(|s| format!("> {s}"))
            .collect::<Vec<_>>()
            .join("\n"),
        "hr" => "---".into(),
        "img" => {
            *safe = false;
            format!(
                "[image: {}]",
                element.value().attr("alt").unwrap_or("image")
            )
        }
        _ => {
            *safe = false;
            content
        }
    }
}

fn plain(element: ElementRef<'_>) -> String {
    let name = element.value().name();
    if name == "img" {
        return format!(
            "[image: {}]",
            element.value().attr("alt").unwrap_or("image")
        );
    }
    if name == "br" {
        return "\n".into();
    }
    if name == "hr" {
        return "────────────────".into();
    }
    let mut text = String::new();
    for child in element.children() {
        if let Some(value) = child.value().as_text() {
            text.push_str(value);
        } else if let Some(child) = ElementRef::wrap(child) {
            text.push_str(&plain(child));
        }
    }
    if let Some(url) = element.value().attr("href") {
        if text != url {
            text.push_str(&format!(" ({url})"));
        }
    }
    match name {
        "p" | "li" | "tr" | "div" | "blockquote" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            text.push('\n')
        }
        "td" | "th" => text.push_str(" | "),
        _ => {}
    }
    text
}
fn collect_sections(parent: ElementRef<'_>, inherited_safe: bool, sections: &mut Vec<Section>) {
    for child in parent.children() {
        let Some(element) = ElementRef::wrap(child) else {
            if let Some(text) = child.value().as_text().filter(|t| !t.trim().is_empty()) {
                sections.push(Section {
                    id: String::new(),
                    markdown: text.to_string(),
                    display: text.to_string(),
                    original: text.to_string(),
                    editable: false,
                });
            }
            continue;
        };
        let name = element.value().name();
        if matches!(name, "div" | "ul" | "ol") {
            let basic_list_wrapper = element.value().attr("class")
                == Some("list-numbering-restart-at")
                && element.value().attr("data-section-style") == Some("5")
                && element.value().attr("style") == Some("--indent0: 0");
            let known_attributes = element.value().attrs().all(|(key, _)| {
                key == "id"
                    || (basic_list_wrapper
                        && matches!(key, "class" | "data-section-style" | "style"))
            });
            let safe = inherited_safe && known_attributes && name != "ol";
            collect_sections(element, safe, sections);
            continue;
        }
        let mut safe = inherited_safe;
        let text = markdown(element, &mut safe);
        let id = element.value().attr("id").unwrap_or("").to_string();
        let title = element
            .value()
            .attr("class")
            .is_some_and(|s| s.split_whitespace().any(|c| c == "default-content"));
        sections.push(Section {
            editable: safe && !id.is_empty() && !title,
            id,
            markdown: text.trim_end().into(),
            display: plain(element).trim_end().into(),
            original: format!(
                "{}{}",
                element
                    .ancestors()
                    .skip(1)
                    .filter_map(ElementRef::wrap)
                    .take_while(|parent| !parent
                        .value()
                        .classes()
                        .any(|class| class == "quip-canvas-content"))
                    .map(|parent| format!(
                        "{}{:?}",
                        parent.value().name(),
                        parent.value().attrs().collect::<Vec<_>>()
                    ))
                    .collect::<String>(),
                element.html()
            ),
        });
    }
}

pub fn parse_document(file: &Value, html: &str) -> Result<Document, String> {
    let tree = Html::parse_fragment(html);
    let root_selector = Selector::parse(".quip-canvas-content").unwrap();
    let root = tree
        .select(&root_selector)
        .next()
        .ok_or("Slack returned no canvas content (session expired?)")?;
    let mut sections = Vec::new();
    collect_sections(root, true, &mut sections);
    Ok(Document {
        permalink: string(file, "permalink"),
        id: string(file, "id"),
        title: string(file, "title"),
        writable: file.get("editable").and_then(Value::as_bool) == Some(true)
            || string(file, "access") == "write",
        sections,
    })
}

pub fn load_document(client: &Client, id: &str) -> Result<Document, String> {
    let response = client.call("files.info", &[("file", id)])?;
    let file = &response["file"];
    if string(file, "filetype") != "quip" {
        return Err("this file is not a canvas".into());
    }
    let url = string(file, "url_private_download");
    let html = client.download_canvas(&url)?;
    parse_document(file, &html)
}

fn tabs_from(info: &Value) -> Vec<Tab> {
    let mut tabs = vec![Tab { file: None,
        label: "Messages".into(),
        target: Target::Messages,
    }];
    if let Some(rows) = info
        .pointer("/channel/properties/tabs")
        .and_then(Value::as_array)
    {
        for row in rows {
            if row["is_disabled"].as_bool() == Some(true) {
                continue;
            }
            let kind = string(row, "type");
            let target = match kind.as_str() {
                "canvas" => Target::Canvas(
                    row.pointer("/data/file_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                ),
                "channel_canvas" => Target::Canvas(
                    info.pointer("/channel/properties/canvas/file_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                ),
                "files" => Target::Files,
                "folder" | "bookmarks" => Target::Bookmarks,
                _ => continue,
            };
            let label = string(row, "label");
            let label = if label.is_empty() {
                match &target {
                    Target::Canvas(_) => "Canvas",
                    Target::Files => "Files & links",
                    _ => "Bookmarks",
                }
                .into()
            } else {
                label
            };
            tabs.push(Tab { file: None, label, target });
        }
    }
    if !tabs.iter().any(|t| matches!(t.target, Target::Files)) {
        tabs.push(Tab { file: None,
            label: "Files & links".into(),
            target: Target::Files,
        });
    }
    if !tabs.iter().any(|t| matches!(t.target, Target::Bookmarks)) {
        tabs.push(Tab { file: None,
            label: "Bookmarks".into(),
            target: Target::Bookmarks,
        });
    }
    tabs
}
fn load_tabs(client: &Client, channel: &str) -> Result<Vec<Tab>, String> {
    let response = client.call("conversations.info", &[("channel", channel)])?;
    let mut tabs = tabs_from(&response);
    for tab in &mut tabs {
        if let Target::Canvas(id) = &tab.target {
            if tab.label == "Canvas" && !id.is_empty() {
                if let Ok(v) = client.call("files.info", &[("file", id)]) {
                    let title = string(&v["file"], "title");
                    if !title.is_empty() {
                        tab.label = title;
                    }
                }
            }
        }
    }
    Ok(tabs)
}
fn load_entries(client: &Client, channel: &str, files: bool) -> Result<Vec<Tab>, String> {
    let mut entries = Vec::new();
    if files {
        let mut page = 1;
        loop {
            let page_text = page.to_string();
            let response = client.call(
                "files.list",
                &[("channel", channel), ("count", "100"), ("page", &page_text)],
            )?;
            if let Some(rows) = response["files"].as_array() {
                for file in rows {
                    entries.push(Tab { file: Some(crate::file_message::Metadata::from_file(file)),
                        label: string(file, "title"),
                        target: if string(file, "filetype") == "quip" {
                            Target::Canvas(string(file, "id"))
                        } else if crate::archive::FileInfo::from_slack(file, channel).is_image() {
                            Target::Image(crate::archive::FileInfo::from_slack(file, channel), string(file, "permalink"))
                        } else {
                            Target::Link(string(file, "permalink"))
                        },
                    });
                }
            }
            if page
                >= response
                    .pointer("/paging/pages")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
            {
                break;
            }
            page += 1;
        }
    }
    let response = client.call("bookmarks.list", &[("channel_id", channel)])?;
    if let Some(rows) = response["bookmarks"].as_array() {
        for bookmark in rows {
            entries.push(Tab { file: None,
                label: string(bookmark, "title"),
                target: Target::Link(string(bookmark, "link")),
            });
        }
    }
    Ok(entries)
}

#[derive(Clone)]
struct Draft {
    section: Section,
    editor: Editor,
    insert: bool,
    command: Option<Editor>,
    undo: Vec<Editor>,
}
impl Draft {
    fn new(section: Section) -> Self {
        let mut editor = Editor::with(section.markdown.clone());
        editor.cursor = 0;
        Self {
            section,
            editor,
            insert: true,
            command: None,
            undo: vec![],
        }
    }
    fn remember(&mut self, before: Editor) {
        self.undo.push(before);
        while self.undo.len() > 100
            || self.undo.iter().map(|e| e.text.len()).sum::<usize>() > 8 * 1024 * 1024
        {
            self.undo.remove(0);
        }
    }
    fn dirty(&self) -> bool {
        self.editor.text != self.section.markdown
    }
    fn vertical(&mut self, delta: isize) {
        let lines: Vec<_> = self.editor.text.split('\n').collect();
        let row = self.editor.text[..self.editor.cursor]
            .bytes()
            .filter(|b| *b == b'\n')
            .count();
        let start: usize = lines.iter().take(row).map(|s| s.len() + 1).sum();
        let column = self.editor.text[start..self.editor.cursor].chars().count();
        let next = row.saturating_add_signed(delta).min(lines.len() - 1);
        self.editor.cursor = lines.iter().take(next).map(|s| s.len() + 1).sum::<usize>()
            + lines[next]
                .char_indices()
                .nth(column)
                .map(|(i, _)| i)
                .unwrap_or(lines[next].len());
    }
    fn key(&mut self, key: KeyEvent) -> Option<String> {
        if let Some(command) = &mut self.command {
            match key.code {
                KeyCode::Esc => self.command = None,
                KeyCode::Enter => return self.command.take().map(|s| s.text),
                _ => {
                    command.key(key, false);
                }
            }
            return None;
        }
        if self.insert {
            if key.code == KeyCode::Esc {
                self.insert = false;
                return None;
            }
            let before = self.editor.clone();
            match key.code {
                KeyCode::Enter => self.editor.newline(),
                KeyCode::Up => self.vertical(-1),
                KeyCode::Down => self.vertical(1),
                _ => {
                    self.editor.key(key, true);
                }
            }
            if self.editor.text.len() > 1024 * 1024 {
                self.editor = before;
                return None;
            }
            if before.text != self.editor.text {
                self.remember(before);
            }
            return None;
        }
        match key.code {
            KeyCode::Char(':') => self.command = Some(Editor::default()),
            KeyCode::Char('i') => self.insert = true,
            KeyCode::Char('h') | KeyCode::Left => {
                self.editor
                    .key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), true);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.editor
                    .key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), true);
            }
            KeyCode::Char('j') | KeyCode::Down => self.vertical(1),
            KeyCode::Char('k') | KeyCode::Up => self.vertical(-1),
            KeyCode::Char('0') | KeyCode::Home => {
                self.editor
                    .key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), true);
            }
            KeyCode::Char('$') | KeyCode::End => {
                self.editor
                    .key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), true);
            }
            KeyCode::Char('G') => self.editor.cursor = self.editor.text.len(),
            KeyCode::Char('g') => self.editor.cursor = 0,
            KeyCode::Char('a') => {
                self.editor
                    .key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), true);
                self.insert = true;
            }
            KeyCode::Char('o') => {
                self.remember(self.editor.clone());
                self.editor
                    .key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), true);
                self.editor.newline();
                self.insert = true;
            }
            KeyCode::Char('x') => {
                self.remember(self.editor.clone());
                self.editor
                    .key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), true);
            }
            KeyCode::Char('u') => {
                if let Some(previous) = self.undo.pop() {
                    self.editor = previous;
                }
            }
            KeyCode::Esc => return Some("q".into()),
            _ => {}
        }
        None
    }
}

pub fn replacement(section: &Section, text: &str) -> Result<Value, String> {
    if !section.editable || section.id.is_empty() {
        return Err("This section contains unsupported formatting; edit it in Slack.".into());
    }
    if text.trim().is_empty() {
        return Err("A section cannot be saved empty.".into());
    }
    if text.len() > 1024 * 1024 {
        return Err("Section exceeds the 1 MiB edit limit.".into());
    }
    Ok(
        json!([{"operation":"replace","section_id":section.id,"document_content":{"type":"markdown","markdown":text}}]),
    )
}
fn save_document(client: &Client, document: &Document, draft: &Draft) -> Result<Document, String> {
    save_with(
        draft,
        || load_document(client, &document.id),
        |changes| {
            client
                .call(
                    "canvases.edit",
                    &[
                        ("canvas_id", &document.id),
                        ("changes", &changes.to_string()),
                    ],
                )
                .map(|_| ())
        },
    )
}
fn save_with(
    draft: &Draft,
    mut load: impl FnMut() -> Result<Document, String>,
    mut write: impl FnMut(Value) -> Result<(), String>,
) -> Result<Document, String> {
    let changes = replacement(&draft.section, &draft.editor.text)?;
    let current = load()?;
    if !current.writable {
        return Err("Canvas is read-only.".into());
    }
    if current
        .sections
        .iter()
        .find(|s| s.id == draft.section.id)
        .filter(|s| s.editable)
        .map(|s| &s.original)
        != Some(&draft.section.original)
    {
        return Err(
            "Section changed in Slack. Export your draft with :w PATH, then :q! and reopen the canvas."
                .into(),
        );
    }
    write(changes)?;
    // Successful write and failed refresh must not offer a blind retry.
    match load() {
        Ok(doc) => Ok(doc),
        Err(_) => {
            let mut doc = current;
            if let Some(s) = doc.sections.iter_mut().find(|s| s.id == draft.section.id) {
                s.markdown = draft.editor.text.clone();
                s.display = draft.editor.text.clone();
                s.editable = false;
            }
            Ok(doc)
        }
    }
}

enum Loaded {
    Tabs(Vec<Tab>),
    Entries(Vec<Tab>),
    Canvas(Document),
    Saved(Document, bool),
    History(crate::canvas_history::Page),
    Message(crate::file_message::Location),
}
struct Job {
    receive: Receiver<Result<Loaded, String>>,
}
fn spawn(work: impl FnOnce() -> Result<Loaded, String> + Send + 'static) -> Job {
    let (send, receive) = mpsc::channel();
    let log = crate::session_log::JobLog::start("channel_browser");
    std::thread::spawn(move || {
        let result = work();
        let id = log.complete(result.as_ref().err().map(String::as_str));
        if send.send(result).is_err() {
            crate::session_log::record("job_delivery_dropped", serde_json::json!({"id":id}));
        }
    });
    Job { receive }
}

pub struct Picture {
    pub permalink: String,
    pub file: crate::archive::FileInfo,
    pub zoom: u16,
    pub shown: Option<(String, ratatui_image::protocol::StatefulProtocol)>,
}

pub struct Browser {
    pub message: Option<crate::file_message::Location>,
    message_loading: bool,
    pub visible: bool,
    pub channel: String,
    name: String,
    client: Arc<Client>,
    tabs: Vec<Tab>,
    cursor: usize,
    entries: Option<Vec<Tab>>,
    entry_cursor: usize,
    entry_scroll: usize,
    pub previews: bool,
    pub thumbnails: Vec<(Rect, crate::archive::FileInfo)>,
    document: Option<Document>,
    history: Option<crate::canvas_history::History>,
    section: usize,
    document_rows: Vec<(usize, String)>,
    draft: Option<Draft>,
    job: Option<Job>,
    notice: String,
    link: Option<String>,
    pub picture: Option<Picture>,
}
impl Browser {
    pub fn log_state(&self) -> Value {
        serde_json::json!({"channel":self.channel,"tab_cursor":self.cursor,"entry_cursor":self.entry_cursor,
            "entry_scroll":self.entry_scroll,"entries":self.entries.as_ref().map(Vec::len),"canvas":self.document.as_ref().map(|d|&d.id),
            "section":self.section,"editing":self.draft.is_some(),"insert":self.draft.as_ref().is_some_and(|d|d.insert),
            "command":self.draft.as_ref().is_some_and(|d|d.command.is_some()),"history":self.history.is_some(),
            "picture":self.picture.as_ref().map(|p|&p.file.id),"loading":self.job.is_some()})
    }

    pub fn escape_edits(&self) -> bool {
        self.draft.as_ref().is_some_and(|draft| draft.insert || draft.command.is_some())
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.message = None;
        if self.message_loading {
            self.job = None;
            self.message_loading = false;
            self.notice.clear();
        }
    }
    pub fn message_unavailable(&mut self) { self.notice = "Conversation is no longer available.".into(); }
    pub fn can_toggle(&self) -> bool {
        self.draft
            .as_ref()
            .is_none_or(|d| !d.insert && d.command.is_none())
    }
    pub fn busy_or_dirty(&self) -> bool {
        self.job.is_some() || self.draft.as_ref().is_some_and(Draft::dirty)
    }
    pub fn new(client: Arc<Client>, channel: String, name: String) -> Self {
        let api = client.clone();
        let cid = channel.clone();
        Self {
            message: None,
            message_loading: false,
            visible: true,
            channel,
            name,
            client,
            tabs: vec![],
            cursor: 0,
            entries: None,
            entry_cursor: 0,
            entry_scroll: 0,
            previews: true,
            thumbnails: vec![],
            document: None,
            history: None,
            section: 0,
            document_rows: vec![],
            draft: None,
            job: Some(spawn(move || load_tabs(&api, &cid).map(Loaded::Tabs))),
            notice: "Loading channel tabs…".into(),
            link: None,
            picture: None,
        }
    }
    pub fn tick(&mut self) {
        let result = self.job.as_ref().and_then(|j| match j.receive.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("Canvas worker stopped".into())),
            Err(_) => None,
        });
        if let Some(result) = result {
            self.job = None;
            self.message_loading = false;
            self.notice.clear();
            match result {
                Ok(Loaded::Message(location)) => { self.message = Some(location); }
                Ok(Loaded::History(page)) => {
                    if let Some(history) = &mut self.history { history.append(page); }
                }
                Ok(Loaded::Tabs(tabs)) => self.tabs = tabs,
                Ok(Loaded::Entries(entries)) => {
                    self.entries = Some(entries);
                    self.entry_cursor = 0;
                    self.entry_scroll = 0;
                }
                Ok(Loaded::Canvas(doc)) => {
                    self.document = Some(doc);
                    self.section = 0;
                }
                Ok(Loaded::Saved(doc, close)) => {
                    self.document = Some(doc);
                    if close {
                        self.draft = None;
                    } else if let Some(d) = self.draft.take() {
                        if let Some(section) = self
                            .document
                            .as_ref()
                            .and_then(|doc| doc.sections.iter().find(|s| s.id == d.section.id))
                        {
                            let mut next = Draft::new(section.clone());
                            next.insert = false;
                            self.draft = Some(next);
                        }
                    }
                    self.notice = "Saved to Slack".into();
                }
                Err(error) => {
                    if let Some(history) = &mut self.history { history.failed = true; }
                    self.notice = error;
                }
            }
        }
    }
    pub fn key(&mut self, key: KeyEvent) {
        self.message = None;
        if self.message_loading && matches!(key.code, KeyCode::Char('T' | 'h') | KeyCode::Esc | KeyCode::Left) {
            self.job = None;
            self.message_loading = false;
            self.notice.clear();
            if key.code == KeyCode::Char('T') { self.visible = false; }
            return;
        }
        if self.job.is_some() {
            if self.history.is_some() && matches!(key.code, KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc) {
                self.history = None;
                self.job = None;
                self.notice.clear();
                return;
            }
            if matches!(key.code, KeyCode::Char('T')) {
                self.visible = false;
            }
            return;
        }
        if let Some(draft) = &mut self.draft {
            if !draft.insert && draft.command.is_none() && key.code == KeyCode::Char('T') {
                self.visible = false;
                return;
            }
            let Some(command) = draft.key(key) else {
                return;
            };
            match command.as_str() {
                "q!" => self.draft = None,
                "q" if !draft.dirty() => self.draft = None,
                "q" => self.notice = "Unsaved changes; :w to save, :q! to discard".into(),
                "w" | "wq" => {
                    if !draft.dirty() {
                        if command == "wq" {
                            self.draft = None;
                        }
                        return;
                    }
                    let draft = draft.clone();
                    let doc = self.document.clone().unwrap();
                    let api = self.client.clone();
                    self.job = Some(spawn(move || {
                        save_document(&api, &doc, &draft)
                            .map(|doc| Loaded::Saved(doc, command == "wq"))
                    }));
                    self.notice = "Saving section…".into();
                }
                command if command.starts_with("w ") => {
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    let path = command[2..].trim();
                    self.notice = match std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(path)
                        .and_then(|mut f| f.write_all(draft.editor.text.as_bytes()))
                    {
                        Ok(()) => format!(
                            "Draft exported to {path}; :q! can now discard the local buffer"
                        ),
                        Err(error) => format!("Draft export: {error}"),
                    };
                }
                _ => self.notice = "Commands: :w, :w PATH, :q, :wq, :q!".into(),
            }
            return;
        }
        if key.code == KeyCode::Char('T') {
            self.visible = false;
            return;
        }
        if let Some(history) = &mut self.history {
            match key.code {
                KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc => {
                    if history.details { history.details = false; } else { self.history = None; }
                }
                KeyCode::Char('j') | KeyCode::Down if history.details => history.scroll = history.scroll.saturating_add(1),
                KeyCode::Char('k') | KeyCode::Up if history.details => history.scroll = history.scroll.saturating_sub(1),
                KeyCode::Char('g') | KeyCode::Home if history.details => history.scroll = 0,
                KeyCode::Char('G') | KeyCode::End if history.details => history.scroll = usize::MAX,
                KeyCode::Char('j') | KeyCode::Down if !history.details => history.cursor = (history.cursor + 1).min(history.count().saturating_sub(1)),
                KeyCode::Char('k') | KeyCode::Up if !history.details => history.cursor = history.cursor.saturating_sub(1),
                KeyCode::Char('g') | KeyCode::Home if !history.details => history.cursor = 0,
                KeyCode::Char('G') | KeyCode::End if !history.details => history.cursor = history.count().saturating_sub(1),
                KeyCode::Char('r') if !history.details => {
                    self.history = Some(crate::canvas_history::History::default());
                    self.load_history(None);
                }
                KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter if !history.details => {
                    if history.cursor < history.revisions.len() { history.details = true; history.scroll = 0; }
                    else if let Some(older) = history.older { self.load_history(Some(older)); }
                    else if !history.loaded { self.load_history(None); }
                }
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('H') && self.document.is_some() {
            self.history = Some(crate::canvas_history::History::default());
            self.load_history(None);
            return;
        }
        if matches!(key.code, KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc) {
            if self.picture.take().is_some() {
                return;
            }
            if self.link.take().is_some() {
                return;
            }
            if self.document.take().is_some() {
                return;
            }
            if self.entries.take().is_some() {
                return;
            }
            self.visible = false;
            return;
        }
        if key.code == KeyCode::Char('m') && self.entries.is_some() && self.document.is_none() {
            if let Some(file) = self.entries.as_ref().and_then(|entries| entries.get(self.entry_cursor)).and_then(|entry| entry.file.clone()) {
                let api = self.client.clone();
                let channel = self.channel.clone();
                self.message_loading = true;
                self.job = Some(spawn(move || crate::file_message::load(&api, &channel, &file.id).map(Loaded::Message)));
                self.notice = "Finding latest sharing message in this channel…".into();
            } else { self.notice = "This entry has no file sharing message.".into(); }
            return;
        }
        if let Some(picture) = &mut self.picture {
            match key.code {
                KeyCode::Char('+' | '=') => picture.zoom = (picture.zoom + 25).min(800),
                KeyCode::Char('-' | '_') => picture.zoom = picture.zoom.saturating_sub(25).max(25),
                KeyCode::Char('0') => picture.zoom = 100,
                _ => {}
            }
            return;
        }
        if self.link.is_some() {
            return;
        }
        let (cursor, count) = if let Some(doc) = &self.document {
            (
                &mut self.section,
                self.document_rows.len().max(doc.sections.len()),
            )
        } else if let Some(entries) = &self.entries {
            (&mut self.entry_cursor, entries.len())
        } else {
            (&mut self.cursor, self.tabs.len())
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                *cursor = (*cursor + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => *cursor = cursor.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => *cursor = 0,
            KeyCode::Char('G') | KeyCode::End => *cursor = count.saturating_sub(1),
            _ => {}
        }
        if let Some(doc) = &self.document {
            if key.code == KeyCode::Char('i') {
                if !doc.writable {
                    self.notice = "Canvas is read-only".into();
                    return;
                }
                if let Some(section) = doc.sections.get(
                    self.document_rows
                        .get(self.section)
                        .map(|r| r.0)
                        .unwrap_or(self.section),
                ) {
                    if !section.editable {
                        self.notice =
                            "This section contains unsupported formatting; edit it in Slack."
                                .into();
                    } else {
                        self.draft = Some(Draft::new(section.clone()));
                        self.notice.clear();
                    }
                }
            }
            return;
        }
        if matches!(
            key.code,
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter
        ) {
            let target = if let Some(entries) = &self.entries {
                entries.get(self.entry_cursor)
            } else {
                self.tabs.get(self.cursor)
            }
            .map(|t| t.target.clone());
            let api = self.client.clone();
            let cid = self.channel.clone();
            match target {
                Some(Target::Messages) => self.visible = false,
                Some(Target::Canvas(id)) => {
                    self.job = Some(spawn(move || load_document(&api, &id).map(Loaded::Canvas)));
                    self.notice = "Loading canvas…".into();
                }
                Some(Target::Files) => {
                    self.job = Some(spawn(move || {
                        load_entries(&api, &cid, true).map(Loaded::Entries)
                    }));
                    self.notice = "Loading files & links…".into();
                }
                Some(Target::Bookmarks) => {
                    self.job = Some(spawn(move || {
                        load_entries(&api, &cid, false).map(Loaded::Entries)
                    }));
                    self.notice = "Loading bookmarks…".into();
                }
                Some(Target::Image(file, permalink)) => self.picture = Some(Picture { file, permalink, zoom: 100, shown: None }),
                Some(Target::Link(url)) => self.link = Some(url),
                None => {}
            }
        }
    }
    fn load_history(&mut self, older: Option<i64>) {
        let Some(document) = &self.document else { return; };
        let id = document.id.clone();
        let api = self.client.clone();
        let names = self.history.as_ref().map(|history| history.names.clone()).unwrap_or_default();
        let Some(history) = &self.history else { return; };
        let cancelled = history.cancelled.clone();
        self.job = Some(spawn(move || crate::canvas_history::load(&api, &id, older, names, &cancelled).map(Loaded::History)));
        self.notice = "Loading canvas history…".into();
    }
    pub fn title(&self) -> String {
        let mut title = format!("{} · channel tabs", self.name);
        if self.entries.is_some() {
            if let Some(tab) = self.tabs.get(self.cursor) {
                title.push_str(&format!(" · {}", tab.label));
            }
        }
        if let Some(picture) = &self.picture {
            title.push_str(&format!(" · {}", picture.file.name));
        } else if let Some(document) = &self.document {
            title.push_str(&format!(" · {}", document.title));
        } else if self.link.is_some() {
            if let Some(entry) = self.entries.as_ref().and_then(|entries| entries.get(self.entry_cursor)) {
                title.push_str(&format!(" · {}", entry.label));
            }
        }
        if let Some(history) = &self.history {
            title.push_str(if history.details { " · edit history · revision details" } else { " · edit history" });
        }
        title
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, palette: &Palette) {
        self.thumbnails.clear();
        let block = Block::bordered()
            .title(format!(" {} ", self.title()))
            .border_style(Style::new().fg(palette.get(Role::Accent)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 3 || inner.width < 4 {
            return;
        }
        let body = Rect {
            height: inner.height - 2,
            ..inner
        };
        let footer = Rect {
            y: inner.y + inner.height - 2,
            height: 2,
            ..inner
        };
        let hint = if let Some(d) = &self.draft {
            format!(
                "{}{} · Esc normal · :w save · :q close · :q! discard",
                if d.insert { "INSERT" } else { "NORMAL" },
                if d.dirty() { " [+]" } else { "" }
            )
        } else if self.picture.is_some() {
            "+/- zoom · 0 fit · m message · h back · Esc home · T hide".into()
        } else if let Some(history) = &self.history {
            if history.details { "j/k scroll · h back · Esc home · T hide".into() } else { "j/k select · l/Enter details/load older · r reload · h back · Esc home · T hide".into() }
        } else if self.document.is_some() {
            "j/k lines · i edit section · H edit history · h back · Esc home · T hide".into()
        } else {
            "j/k select · l/Enter open · m message · h back · Esc home · T hide".into()
        };
        let status = if self.history.as_ref().is_some_and(|history| history.limited) {
            format!("Slack limits older history. {}", self.notice)
        } else { self.notice.clone() };
        frame.render_widget(Paragraph::new(format!("{hint}\n{status}")), footer);
        if let Some(history) = &mut self.history {
            history.draw(frame, body, palette, self.document.as_ref().map(|doc| doc.permalink.as_str()).unwrap_or(""));
            return;
        }
        if self.picture.is_some() { return; }
        if let Some(draft) = &self.draft {
            let rows = draft.editor.rows();
            let row = rows.iter().position(|(_, c)| c.is_some()).unwrap_or(0);
            let top = row.saturating_sub(body.height as usize - 1);
            let column = rows
                .get(row)
                .and_then(|(s, c)| c.map(|c| unicode_width::UnicodeWidthStr::width(&s[..c])))
                .unwrap_or(0);
            let horizontal = column.saturating_sub(body.width as usize - 1);
            let lines: Vec<_> = rows
                .iter()
                .skip(top)
                .take(body.height as usize)
                .map(|(s, _)| Line::from(s.clone()))
                .collect();
            frame.render_widget(
                Paragraph::new(lines).scroll((0, horizontal.min(u16::MAX as usize) as u16)),
                body,
            );
            if let Some(command) = &draft.command {
                frame.render_widget(Paragraph::new(format!(":{}", command.text)), footer);
                frame.set_cursor_position((
                    footer.x
                        + 1
                        + command.cursor.min(footer.width.saturating_sub(2) as usize) as u16,
                    footer.y,
                ));
            } else {
                frame.set_cursor_position((
                    body.x + (column - horizontal) as u16,
                    body.y + (row - top) as u16,
                ));
            }
            return;
        }
        if let Some(link) = &self.link {
            frame.render_widget(
                Paragraph::new(link.as_str()).wrap(ratatui::widgets::Wrap { trim: false }),
                body,
            );
            return;
        }
        if self.entries.is_some() && self.document.is_none() {
            self.draw_entries(frame, body, palette);
            return;
        }
        let (rows, cursor) = if let Some(doc) = &self.document {
            self.document_rows.clear();
            for (index, section) in doc.sections.iter().enumerate() {
                for row in wrap_lines(&section.display, body.width as usize) {
                    self.document_rows.push((index, row));
                }
            }
            self.section = self.section.min(self.document_rows.len().saturating_sub(1));
            (
                self.document_rows
                    .iter()
                    .map(|r| r.1.clone())
                    .collect::<Vec<_>>(),
                self.section,
            )
        } else {
            (
                self.tabs.iter().map(|t| t.label.clone()).collect(),
                self.cursor,
            )
        };
        let items: Vec<_> = rows.into_iter().map(|s| ListItem::new(s)).collect();
        let mut state = ListState::default().with_selected(Some(cursor));
        frame.render_stateful_widget(
            List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            body,
            &mut state,
        );
    }
    fn draw_entries(&mut self, frame: &mut Frame, body: Rect, palette: &Palette) {
        let entries = self.entries.as_ref().expect("entry list");
        if entries.is_empty() { return; }
        let preview_rows = body.height.saturating_sub(1).min(8);
        let height = |entry: &Tab| -> u16 {
            if self.previews && matches!(entry.target, Target::Image(..)) {
                preview_rows + 1
            } else { 1 }
        };
        self.entry_cursor = self.entry_cursor.min(entries.len() - 1);
        self.entry_scroll = self.entry_scroll.min(self.entry_cursor);
        let mut visible_rows: usize = entries[self.entry_scroll..=self.entry_cursor]
            .iter().map(|entry| usize::from(height(entry))).sum();
        while visible_rows > usize::from(body.height) {
            visible_rows -= usize::from(height(&entries[self.entry_scroll]));
            self.entry_scroll += 1;
        }
        let mut y = body.y;
        for (index, entry) in entries.iter().enumerate().skip(self.entry_scroll) {
            let rows = height(entry);
            if y + rows > body.bottom() { break; }
            let style = if index == self.entry_cursor {
                Style::new().bg(palette.get(Role::SelectionBackground)).fg(palette.get(Role::SelectionText))
            } else { Style::new() };
            let label = match &entry.file {
                Some(file) => format!("{} · {}", file.date(), entry.label),
                None => entry.label.clone(),
            };
            frame.render_widget(Paragraph::new(label).style(style), Rect::new(body.x, y, body.width, 1));
            if rows > 1 {
                if let Target::Image(file, _) = &entry.target {
                    self.thumbnails.push((Rect::new(body.x, y + 1, body.width.min(64), preview_rows), file.clone()));
                }
            }
            y += rows;
        }
    }

}

pub(crate) fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for c in line.chars() {
            let size = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if used + size > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(c);
            used += size;
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    #[test]
    fn escape_leaves_canvas_insert_mode_then_goes_home_preserving_draft() {
        let mut app = crate::app::tests::mute_test_app();
        let mut browser = browser();
        let section = browser.document.as_ref().unwrap().sections[0].clone();
        let mut draft = Draft::new(section);
        draft.editor.text.push_str(" unsaved");
        browser.draft = Some(draft);
        app.channel_browser = Some(browser);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let browser = app.channel_browser.as_ref().unwrap();
        assert!(browser.visible && !browser.draft.as_ref().unwrap().insert);
        app.channel_browser.as_mut().unwrap().draft.as_mut().unwrap().command = Some(Editor::with("w".into()));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.channel_browser.as_ref().unwrap().visible);
        assert!(app.channel_browser.as_ref().unwrap().draft.as_ref().unwrap().command.is_none());
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let browser = app.channel_browser.as_ref().unwrap();
        assert!(!browser.visible && browser.draft.as_ref().unwrap().dirty());
        assert!(app.open.is_none() && app.stack.is_empty());
        app.channel_browser.as_mut().unwrap().visible = true;
        app.channel_browser.as_mut().unwrap().draft = None;
        app.channel_browser.as_mut().unwrap().history = Some(crate::canvas_history::History::default());
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.channel_browser.as_ref().unwrap().visible);
    }

    #[test]
    fn completed_file_lookup_reaches_application_and_navigation_cancels_pending_result() {
        use crate::archive::Msg;
        let message = || Msg::from_api("C1".into(),json!({"ts":"1.000000","files":[{"id":"F1"}]})).unwrap();
        let location = || crate::file_message::Location { channel: "C1".into(), root:1000000, focus:1000000, timeline:vec![message()], replies:vec![], has_older:false,has_newer:false };
        let mut browser = browser();
        browser.document = None;
        browser.entries = Some(vec![]);
        browser.message = Some(location());
        browser.key(key('j'));
        assert!(browser.message.is_none());
        let (sender,receive) = mpsc::channel();
        browser.job = Some(Job {receive});
        browser.message_loading = true;
        browser.key(key('h'));
        assert!(sender.send(Ok(Loaded::Message(location()))).is_err());
        let (sender,receive) = mpsc::channel();
        browser.job = Some(Job {receive});
        browser.message_loading = true;
        sender.send(Ok(Loaded::Message(location()))).unwrap();
        let mut app = crate::app::tests::mute_test_app();
        app.channel_browser = Some(browser);
        app.tick();
        assert!(!app.channel_browser.as_ref().unwrap().visible);
        assert_eq!(app.open.as_ref().unwrap().list.selected().unwrap().id,1000000);
        let browser = app.channel_browser.as_mut().unwrap();
        browser.visible = true;
        browser.message = Some(location());
        app.on_key(key('T'));
        let browser = app.channel_browser.as_ref().unwrap();
        assert!(!browser.visible);
        assert!(browser.message.is_none());
    }

    #[test]
    fn file_rows_display_dates_and_message_key_starts_read_only_lookup() {
        let mut browser = browser();
        browser.document = None;
        browser.entries = Some(vec![Tab { label: "example.png".into(), target: Target::Link("https://example.invalid".into()), file: Some(crate::file_message::Metadata { id: "F1".into(), created: Some(0) }) }]);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90,12)).unwrap();
        terminal.draw(|frame| browser.draw(frame,frame.area(),&Palette::default())).unwrap();
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("1970-01-01 00:00 UTC · example.png"));
        browser.client = Client::for_test(|method, params| {
            assert_eq!(method,"files.info");
            assert_eq!(params,&[("file","F1")]);
            Ok(json!({"file":{}}))
        }).into();
        browser.key(key('m'));
        let result = browser.job.take().unwrap().receive.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert!(matches!(result,Err(error) if error.contains("no sharing message")));
        assert!(browser.visible);
    }

    #[test]
    fn history_navigation_is_read_only_and_returns_one_level_at_a_time() {
        let mut browser = browser();
        let page = crate::canvas_history::Page {
            revisions: vec![crate::canvas_history::Revision { id: "revision".into(), sequence: 1, created_ms: 1000, author: "U1".into(), comment: false }],
            older: None, limited: false, names: Default::default(),
        };
        browser.client = Client::for_test(|method, _| {
            assert_eq!(method, "quip.history.getVersions");
            Ok(json!({"versions":[],"oldest_created_usec":-1}))
        }).into();
        browser.key(key('H'));
        assert!(browser.history.is_some());
        let result = browser.job.take().unwrap().receive.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert!(matches!(result, Ok(Loaded::History(_))));
        let (sender, receive) = mpsc::channel();
        browser.job = Some(Job { receive });
        sender.send(Ok(Loaded::History(page.clone()))).unwrap();
        browser.tick();
        browser.history.as_mut().unwrap().limited = true;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
        terminal.draw(|frame| browser.draw(frame, frame.area(), &Palette::default())).unwrap();
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Slack limits older history."));
        assert!(text.contains("edit history"));
        browser.key(key('i'));
        assert!(browser.draft.is_none());
        browser.key(key('l'));
        assert!(browser.history.as_ref().unwrap().details);
        browser.key(key('i'));
        assert!(browser.draft.is_none());
        browser.key(key('h'));
        assert!(!browser.history.as_ref().unwrap().details);
        browser.key(key('h'));
        assert!(browser.history.is_none());
        assert!(browser.document.is_some());
        // Leaving an in-flight history request discards its late completion.
        browser.history = Some(crate::canvas_history::History::default());
        let (sender, receive) = mpsc::channel();
        browser.job = Some(Job { receive });
        browser.key(key('h'));
        let _ = sender.send(Ok(Loaded::History(page)));
        browser.tick();
        assert!(browser.history.is_none());
        assert!(browser.document.is_some());
    }

    #[test]
    fn parses_sections_without_overlapping_and_refuses_unsupported_edits() {
        let doc=parse_document(&json!({"id":"F1","editable":true}),r#"<div class="quip-canvas-content"><h1 class="default-content" id="title">Title</h1><p id="one"><b>Version:</b> <code>1~x</code></p><ul><li id="two"><span id="two">Hello <a>@U123</a></span></li></ul><p id="three"><img src="x"></p></div>"#).unwrap();
        assert_eq!(doc.sections.len(), 4);
        assert!(!doc.sections[0].editable);
        assert!(doc.sections[1].editable);
        assert!(doc.sections[2].markdown.contains("![](@U123)"));
        assert!(!doc.sections[3].editable);
        let change = replacement(&doc.sections[1], "new").unwrap();
        assert_eq!(change[0]["section_id"], "one");
        assert_eq!(change[0]["operation"], "replace");
        assert!(replacement(&doc.sections[3], "new").is_err());
    }
    #[test]
    fn modal_editor_retains_unicode_and_requires_save_command() {
        let mut d = Draft::new(Section {
            id: "one".into(),
            markdown: "héllo\n世界".into(),
            display: "héllo\n世界".into(),
            original: "".into(),
            editable: true,
        });
        d.key(key('X'));
        assert!(d.dirty());
        d.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!d.insert);
        d.key(key('j'));
        assert!(d.editor.text.is_char_boundary(d.editor.cursor));
        d.key(key('h'));
        d.key(key('l'));
        d.key(key(':'));
        d.key(key('w'));
        assert_eq!(
            d.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some("w".into())
        );
        d.key(key('u'));
        assert_eq!(d.editor.text, "héllo\n世界");
    }
    #[test]
    fn channel_tabs_follow_slack_order_and_skip_disabled() {
        let tabs = tabs_from(
            &json!({"channel":{"properties":{"tabs":[{"type":"canvas","label":"Deploy","data":{"file_id":"F1"}},{"type":"files"},{"type":"folder"},{"type":"channel_canvas","is_disabled":true}]}}}),
        );
        assert_eq!(
            tabs.iter().map(|t| t.label.as_str()).collect::<Vec<_>>(),
            ["Messages", "Deploy", "Files & links", "Bookmarks"]
        );
    }
    fn document() -> Document {
        parse_document(
            &json!({"id":"F1","editable":true}),
            r#"<div class="quip-canvas-content"><p id="first">one</p><p id="second">two</p></div>"#,
        )
        .unwrap()
    }
    #[test]
    fn save_checks_remote_section_and_never_replaces_whole_canvas() {
        let doc = document();
        let mut draft = Draft::new(doc.sections[0].clone());
        draft.editor.text = "changed".into();
        let mut calls = 0;
        save_with(
            &draft,
            || Ok(doc.clone()),
            |value| {
                calls += 1;
                assert_eq!(value.as_array().unwrap().len(), 1);
                assert_eq!(value[0]["section_id"], "first");
                assert_eq!(value[0]["document_content"]["markdown"], "changed");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        let mut changed = doc.clone();
        changed.sections[0].original = "remote changed".into();
        assert!(save_with(
            &draft,
            || Ok(changed.clone()),
            |_| panic!("conflict must not write")
        )
        .is_err());
        let mut unrelated = doc.clone();
        unrelated.sections[1].original = "unrelated change".into();
        assert!(save_with(&draft, || Ok(unrelated.clone()), |_| Ok(())).is_ok());
        assert!(save_with(&draft, || Ok(doc.clone()), |_| Err("write failed".into())).is_err());
        assert!(draft.dirty());
    }
    #[test]
    fn confirmed_save_with_refresh_failure_cannot_resubmit_stale_section() {
        let doc = document();
        let mut draft = Draft::new(doc.sections[0].clone());
        draft.editor.text = "changed".into();
        let mut reads = 0;
        let saved = save_with(
            &draft,
            || {
                reads += 1;
                if reads == 1 {
                    Ok(doc.clone())
                } else {
                    Err("offline".into())
                }
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(saved.sections[0].markdown, "changed");
        assert!(!saved.sections[0].editable);
    }
    fn browser() -> Browser {
        Browser {
            message: None,
            message_loading: false,
            visible: true,
            channel: "C1".into(),
            name: "channel".into(),
            client: Arc::new(Client::new(crate::auth::Auth {
                token: "test".into(),
                cookie: "test".into(),
                source: "test".into(),
            })),
            tabs: vec![],
            cursor: 0,
            entries: None,
            entry_cursor: 0,
            entry_scroll: 0,
            previews: true,
            thumbnails: vec![],
            document: Some(document()),
            history: None,
            section: 0,
            document_rows: vec![],
            draft: None,
            job: None,
            notice: String::new(),
            link: None,
            picture: None,
        }
    }
    #[test]
    fn browser_back_and_toggle_preserve_drafts_and_editor_escape_is_modal() {
        let mut b = browser();
        b.key(key('i'));
        b.key(key('X'));
        b.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!b.draft.as_ref().unwrap().insert);
        assert!(b.draft.as_ref().unwrap().dirty());
        b.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(b.draft.is_some());
        assert!(b.notice.contains("Unsaved"));
        b.key(key('T'));
        assert!(!b.visible);
        assert!(b.busy_or_dirty());
        b.visible = true;
        for c in ":q!".chars() {
            b.key(key(c));
        }
        b.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(b.draft.is_none());
        b.key(key('h'));
        assert!(b.document.is_none());
        assert!(b.visible);
        b.key(key('h'));
        assert!(!b.visible);
    }
    #[test]
    fn narrow_render_keeps_all_canvas_lines_reachable() {
        let mut b = browser();
        b.document.as_mut().unwrap().sections[0].display = "世界".repeat(60);
        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| b.draw(f, f.area(), &Palette::default()))
            .unwrap();
        assert!(b.document_rows.len() > 2);
        let reconstructed = b
            .document_rows
            .iter()
            .filter(|r| r.0 == 0)
            .map(|r| r.1.as_str())
            .collect::<String>();
        assert_eq!(reconstructed, "世界".repeat(60));
        b.key(key('G'));
        terminal
            .draw(|f| b.draw(f, f.area(), &Palette::default()))
            .unwrap();
        b.key(key('i'));
        assert_eq!(b.draft.as_ref().unwrap().section.id, "second");
    }
    #[test]
    fn unsupported_structures_are_readable_and_cannot_be_resubmitted() {
        let doc=parse_document(&json!({"id":"F1","editable":true}),r#"<div class="quip-canvas-content"><blockquote id="quote"><p>one</p><p>two</p></blockquote><pre id="code"><code class="language-rust">let a = 1;</code></pre><div style="color:red"><p id="styled">red text</p></div><h4 id="four">small heading</h4><img alt="diagram" src="https://example.com/image"><div>loose text</div></div>"#).unwrap();
        assert_eq!(doc.sections.len(), 6);
        assert!(doc.sections[0].markdown.contains("> one\n> \n> two"));
        assert!(doc.sections[0].display.contains("one\ntwo"));
        assert!(doc.sections[1..].iter().all(|s| !s.editable));
        assert!(doc.sections[4].display.contains("diagram"));
        assert!(doc.sections[5].display.contains("loose text"));
    }
    #[test]
    fn successful_save_reloads_editor_instead_of_pairing_stale_text_with_new_baseline() {
        let mut b = browser();
        b.key(key('i'));
        b.key(key('X'));
        let mut changed = document();
        changed.sections[0].markdown = "remote change after save".into();
        changed.sections[0].original = "new html".into();
        let (sender, receive) = mpsc::channel();
        b.job = Some(Job { receive });
        sender.send(Ok(Loaded::Saved(changed, false))).unwrap();
        b.tick();
        let draft = b.draft.as_ref().unwrap();
        assert_eq!(draft.editor.text, "remote change after save");
        assert!(!draft.dirty());
        assert!(!draft.insert);
        let mut split = document();
        split.sections[0].id = "replacement id".into();
        let (sender, receive) = mpsc::channel();
        b.job = Some(Job { receive });
        sender.send(Ok(Loaded::Saved(split, false))).unwrap();
        b.tick();
        assert!(b.draft.is_none());
    }
    #[test]
    fn export_preserves_dirty_buffer_and_refuses_existing_files() {
        let mut b = browser();
        b.key(key('i'));
        b.key(key('X'));
        b.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let path =
            std::env::temp_dir().join(format!("slack-tui-canvas-export-{}.md", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let command = format!(":w {}", path.display());
        for c in command.chars() {
            b.key(key(c));
        }
        b.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Xone");
        assert!(b.draft.as_ref().unwrap().dirty());
        for c in command.chars() {
            b.key(key(c));
        }
        b.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(b.notice.starts_with("Draft export:"));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn undo_memory_is_bounded() {
        let mut d = Draft::new(document().sections[0].clone());
        for _ in 0..200 {
            d.remember(Editor::with("x".repeat(100_000)));
        }
        assert!(d.undo.len() <= 100);
        assert!(d.undo.iter().map(|e| e.text.len()).sum::<usize>() <= 8 * 1024 * 1024);
    }
    #[test]
    fn ancestor_semantics_are_checked_before_saving() {
        let file = json!({"id":"F1","editable":true});
        let base = parse_document(
            &file,
            r#"<div class="quip-canvas-content"><div id="group"><p id="one">same</p></div></div>"#,
        )
        .unwrap();
        let remote=parse_document(&file,r#"<div class="quip-canvas-content"><div id="group" style="color:red"><p id="one">same</p></div></div>"#).unwrap();
        assert!(base.sections[0].editable);
        assert!(!remote.sections[0].editable);
        let mut draft = Draft::new(base.sections[0].clone());
        draft.editor.text = "local".into();
        assert!(save_with(
            &draft,
            || Ok(remote.clone()),
            |_| panic!("ancestor change must not write")
        )
        .is_err());
        let numbered = parse_document(
            &file,
            r#"<div class="quip-canvas-content"><ol start="7"><li id="one">seven</li></ol></div>"#,
        )
        .unwrap();
        assert!(!numbered.sections[0].editable);
    }
    #[test]
    fn file_list_renders_visible_thumbnails_without_opening_a_picture() {
        use crate::app::{App, ImageState};
        use crate::archive::{Corpus, FileInfo};
        use crate::render::Tz;
        use std::path::PathBuf;
        let mut browser = browser();
        browser.document = None;
        browser.tabs = vec![Tab { file: None, label: "Files & links".into(), target: Target::Files }];
        browser.entries = Some((0..20).map(|index| {
            let id = format!("F{index}");
            let file = FileInfo::from_slack(&json!({"id":id,"title":format!("image{index}.png"),
                "mimetype":"image/png","thumb_360":"https://example.invalid/thumb.png"}), "C1");
            Tab { file: None, label: file.name.clone(), target: Target::Image(file, String::new()) }
        }).collect());
        let mut app = App::new(Corpus::stub(&[]), Tz::Utc, 30.0, false, false,
            PathBuf::new(), PathBuf::new(), 0, None, None);
        app.channel_browser = Some(browser);
        app.picker = Some(ratatui_image::picker::Picker::halfblocks());
        app.images.insert("F0".into(), ImageState::Ready(image::DynamicImage::ImageRgb8(
            image::RgbImage::from_pixel(128, 128, image::Rgb([255, 0, 0])))));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        assert!(terminal.backend().buffer().content.iter().any(|cell| cell.fg == ratatui::style::Color::Rgb(255, 0, 0)));
        let browser = app.channel_browser.as_ref().unwrap();
        assert!(browser.picture.is_none());
        assert_eq!(browser.thumbnails.len(), 2);
        assert!(!app.images.contains_key("F2"));
        app.on_key(key('G'));
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        let browser = app.channel_browser.as_ref().unwrap();
        assert_eq!(browser.entry_cursor, 19);
        assert_eq!(browser.thumbnails.last().unwrap().1.id, "F19");
        assert!(browser.thumbnails.iter().all(|(area, _)| area.bottom() <= 26));
        app.images.insert("F19".into(), ImageState::Failed("denied".into()));
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Image unavailable"));
        app.on_key(key('g'));
        terminal.backend_mut().resize(80, 10);
        terminal.resize(Rect::new(0, 0, 80, 10)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        assert_eq!(app.channel_browser.as_ref().unwrap().thumbnails.len(), 1);
        app.picker = None;
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        assert!(app.channel_browser.as_ref().unwrap().thumbnails.is_empty());
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("image0.png"));
        assert!(text.contains("image1.png"));
    }

    #[test]
    fn files_breadcrumb_and_picture_rendering_return_one_level_at_a_time() {
        use crate::app::{App, ImageState};
        use crate::archive::Corpus;
        use crate::render::Tz;
        use std::path::PathBuf;
        let client = Client::for_test(|method, _| match method {
            "files.list" => Ok(json!({"files":[
                {"id":"FIMAGE","title":"diagram.png","permalink":"https://slack.com/diagram","mimetype":"image/png","url_private":"https://files.slack.com/diagram.png","original_w":128,"original_h":128},
                {"id":"FTEXT","title":"notes.txt","mimetype":"text/plain","permalink":"https://slack.com/notes"}
            ]})),
            "bookmarks.list" => Ok(json!({"bookmarks":[]})),
            _ => panic!("unexpected API call"),
        });
        let mut browser = browser();
        browser.name = "#team-cdn-alpha".into();
        browser.document = None;
        browser.tabs = vec![Tab { file: None, label: "Files & links".into(), target: Target::Files }];
        browser.entries = Some(load_entries(&client, "C1", true).unwrap());
        assert!(matches!(browser.entries.as_ref().unwrap()[1].target, Target::Link(_)));
        assert_eq!(browser.title(), "#team-cdn-alpha · channel tabs · Files & links");
        browser.key(key('l'));
        assert_eq!(browser.title(), "#team-cdn-alpha · channel tabs · Files & links · diagram.png");
        let file = &browser.picture.as_ref().unwrap().file;
        assert_eq!(file.channel, "C1");
        assert_eq!(file.url.as_deref(), Some("https://files.slack.com/diagram.png"));
        let mut app = App::new(Corpus::stub(&[]), Tz::Utc, 30.0, false, false,
            PathBuf::new(), PathBuf::new(), 0, None, None);
        app.channel_browser = Some(browser);
        app.picker = Some(ratatui_image::picker::Picker::halfblocks());
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        let image_key = "FIMAGE:full".to_string();
        for (state, expected) in [(ImageState::Loading, "loading the original"),
            (ImageState::Failed("download denied".into()), "image failed: download denied")] {
            app.images.insert(image_key.clone(), state);
            terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
            let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains(expected));
            assert!(text.contains("https://slack.com/diagram"));
        }
        app.picker = None;
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Image display is disabled"));
        assert!(text.contains("https://slack.com/diagram"));
        app.picker = Some(ratatui_image::picker::Picker::halfblocks());
        app.images.insert(image_key.clone(), ImageState::Ready(image::DynamicImage::ImageRgb8(
            image::RgbImage::from_pixel(128, 128, image::Rgb([255, 0, 0])))));
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        assert!(terminal.backend().buffer().content.iter().any(|cell| cell.fg == ratatui::style::Color::Rgb(255, 0, 0)));
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("#team-cdn-alpha · channel tabs · Files & links · diagram.png"));
        app.on_key(key('+'));
        assert_eq!(app.channel_browser.as_ref().unwrap().picture.as_ref().unwrap().zoom, 125);
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        assert!(app.channel_browser.as_ref().unwrap().picture.as_ref().unwrap().shown.as_ref().unwrap().0.contains(":125:"));
        app.on_key(key('h'));
        assert!(!app.images.contains_key(&image_key));
        let browser = app.channel_browser.as_ref().unwrap();
        assert!(browser.picture.is_none());
        assert!(browser.entries.is_some());
        assert_eq!(browser.entry_cursor, 0);
        assert_eq!(browser.title(), "#team-cdn-alpha · channel tabs · Files & links");
        app.on_key(key('h'));
        assert_eq!(app.channel_browser.as_ref().unwrap().title(), "#team-cdn-alpha · channel tabs");
    }

}
