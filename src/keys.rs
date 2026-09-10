//! What a key does in the lists, and the file that says so. The editors that
//! run inside a view — the color palette, reaction details, the image viewer,
//! the prompts — keep their own fixed keys.

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};

/// Something a key can ask for in the conversation list or the messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Down,
    Up,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    First,
    Last,
    Open,
    Back,
    Close,
    OtherPane,
    ToggleConversations,
    Command,
    Keys,
    ConversationsPane,
    GoToDate,
    MyThreads,
    ChannelTabs,
    UnreadsFirst,
    Compose,
    QuoteReply,
    Delete,
    Save,
    Unsave,
    React,
    Images,
    InlineImages,
    MarkRead,
    MarkUnread,
    ShowInChannel,
    RawJson,
    Reload,
    Refresh,
    Archive,
    Sort,
    Help,
    Quit,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Down => "move down",
            Action::Up => "move up",
            Action::HalfPageDown => "half a page down",
            Action::HalfPageUp => "half a page up",
            Action::PageDown => "a page down",
            Action::PageUp => "a page up",
            Action::First => "oldest / first",
            Action::Last => "newest / last",
            Action::Open => "open: conversation, thread, raw JSON",
            Action::Back => "back one view, then the list",
            Action::Close => "home; again: first conversation",
            Action::OtherPane => "the other pane",
            Action::ToggleConversations => "conversations pane: shown, hidden, auto-hide",
            Action::Command => "a command line",
            Action::Keys => "this key editor",
            Action::ConversationsPane => "choose visible conversations",
            Action::GoToDate => "go to a date",
            Action::MyThreads => "threads I took part in or was mentioned in",
            Action::ChannelTabs => "channel tabs and canvases",
            Action::UnreadsFirst => "unread conversations on top",
            Action::Compose => "write a message",
            Action::QuoteReply => "quote the message and reply",
            Action::Delete => "delete your own message",
            Action::Save => "save message for later in Slack",
            Action::Unsave => "remove message from Slack Later",
            Action::React => "View reactions",
            Action::Images => "the message's images",
            Action::InlineImages => "inline thumbnails on/off",
            Action::MarkRead => "mark read",
            Action::MarkUnread => "mark unread from here",
            Action::ShowInChannel => "show a hit or thread root in the channel",
            Action::RawJson => "raw JSON of the message or conversation",
            Action::Reload => "reload from the archive",
            Action::Refresh => "refresh from Slack",
            Action::Archive => "archive a conversation",
            Action::Sort => "sort the conversations",
            Action::Help => "the key guide",
            Action::Quit => "quit",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Action::Down => "down",
            Action::Up => "up",
            Action::HalfPageDown => "half_page_down",
            Action::HalfPageUp => "half_page_up",
            Action::PageDown => "page_down",
            Action::PageUp => "page_up",
            Action::First => "first",
            Action::Last => "last",
            Action::Open => "open",
            Action::Back => "back",
            Action::Close => "close",
            Action::OtherPane => "other_pane",
            Action::ToggleConversations => "toggle_conversations",
            Action::Command => "command",
            Action::Keys => "keys",
            Action::ConversationsPane => "conversations_pane",
            Action::GoToDate => "go_to_date",
            Action::MyThreads => "my_threads",
            Action::ChannelTabs => "channel_tabs",
            Action::UnreadsFirst => "unreads_first",
            Action::Compose => "compose",
            Action::QuoteReply => "quote_reply",
            Action::Delete => "delete",
            Action::Save => "save",
            Action::Unsave => "unsave",
            Action::React => "react",
            Action::Images => "images",
            Action::InlineImages => "inline_images",
            Action::MarkRead => "mark_read",
            Action::MarkUnread => "mark_unread",
            Action::ShowInChannel => "show_in_channel",
            Action::RawJson => "raw_json",
            Action::Reload => "reload",
            Action::Refresh => "refresh",
            Action::Archive => "archive",
            Action::Sort => "sort",
            Action::Help => "help",
            Action::Quit => "quit",
        }
    }
}

/// The actions in the order the editor lists them, with the keys they carry
/// out of the box.
pub const DEFAULTS: &[(Action, &[&str])] = &[
    (Action::Down, &["j", "down"]),
    (Action::Up, &["k", "up"]),
    (Action::HalfPageDown, &["ctrl-d", "f"]),
    (Action::HalfPageUp, &["ctrl-u", "b"]),
    (Action::PageDown, &["ctrl-f", "page-down"]),
    (Action::PageUp, &["page-up"]),
    (Action::First, &["g", "home"]),
    (Action::Last, &["G", "end"]),
    (Action::Open, &["enter", "l", "right"]),
    (Action::Back, &["h", "left"]),
    (Action::Close, &["esc"]),
    (Action::OtherPane, &["tab"]),
    (Action::ToggleConversations, &["ctrl-b"]),
    (Action::Command, &["/"]),
    (Action::Keys, &[]),
    (Action::ConversationsPane, &["ctrl-shift-p"]),
    (Action::GoToDate, &["d"]),
    (Action::MyThreads, &["ctrl-t"]),
    (Action::ChannelTabs, &["T"]),
    (Action::UnreadsFirst, &["U"]),
    (Action::Compose, &["c"]),
    (Action::QuoteReply, &[">"]),
    (Action::Delete, &["D"]),
    (Action::Save, &["ctrl-s"]),
    (Action::Unsave, &["ctrl-shift-s"]),
    (Action::React, &["e"]),
    (Action::Images, &["i"]),
    (Action::InlineImages, &["I"]),
    (Action::MarkRead, &["m"]),
    (Action::MarkUnread, &["M"]),
    (Action::ShowInChannel, &["o"]),
    (Action::RawJson, &["v"]),
    (Action::Reload, &["r"]),
    (Action::Refresh, &["R"]),
    (Action::Archive, &["a"]),
    (Action::Sort, &["s"]),
    (Action::Help, &["?", "H"]),
    (Action::Quit, &["q", "ctrl-c"]),
];

/// One key, with Control the only modifier a binding carries: Shift lives in
/// the character itself, and Alt belongs to the prompt's editing keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub code: KeyCode,
    pub ctrl: bool,
}

impl Chord {
    pub fn of(event: KeyEvent) -> Option<Chord> {
        let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
        if event.modifiers.contains(KeyModifiers::ALT) {
            return None;
        }
        matches!(
            event.code,
            KeyCode::Char(_)
                | KeyCode::Enter
                | KeyCode::Esc
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Insert
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::F(_)
        )
        .then_some(Chord {
            code: match event.code {
                KeyCode::Char(c) if ctrl && event.modifiers.contains(KeyModifiers::SHIFT) => {
                    KeyCode::Char(c.to_ascii_uppercase())
                }
                code => code,
            },
            ctrl,
        })
    }

    pub fn text(self) -> String {
        let name = match self.code {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Esc => "esc".to_string(),
            KeyCode::Tab => "tab".to_string(),
            KeyCode::BackTab => "shift-tab".to_string(),
            KeyCode::Backspace => "backspace".to_string(),
            KeyCode::Delete => "delete".to_string(),
            KeyCode::Insert => "insert".to_string(),
            KeyCode::Home => "home".to_string(),
            KeyCode::End => "end".to_string(),
            KeyCode::PageUp => "page-up".to_string(),
            KeyCode::PageDown => "page-down".to_string(),
            KeyCode::Up => "up".to_string(),
            KeyCode::Down => "down".to_string(),
            KeyCode::Left => "left".to_string(),
            KeyCode::Right => "right".to_string(),
            KeyCode::F(n) => format!("f{n}"),
            other => format!("{other:?}").to_lowercase(),
        };
        if self.ctrl {
            if matches!(self.code, KeyCode::Char(c) if c.is_ascii_uppercase()) {
                format!("ctrl-shift-{}", name.to_ascii_lowercase())
            } else {
                format!("ctrl-{name}")
            }
        } else {
            name
        }
    }

    pub fn parse(text: &str) -> Option<Chord> {
        let text = text.trim();
        if let Some(name) = text.to_ascii_lowercase().strip_prefix("ctrl-shift-") {
            let mut chars = name.chars();
            let c = chars.next()?;
            return (c.is_ascii_alphabetic() && chars.next().is_none()).then_some(Chord {
                code: KeyCode::Char(c.to_ascii_uppercase()),
                ctrl: true,
            });
        }
        let (ctrl, name) = match text.to_lowercase().strip_prefix("ctrl-") {
            // The name keeps its case: G and g are different keys.
            Some(_) => (true, &text[5..]),
            None => (false, text),
        };
        let code = match name.to_lowercase().as_str() {
            "space" => KeyCode::Char(' '),
            "enter" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "shift-tab" => KeyCode::BackTab,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" => KeyCode::Insert,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "page-up" | "pageup" => KeyCode::PageUp,
            "page-down" | "pagedown" => KeyCode::PageDown,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            other => {
                if let Some(digits) = other.strip_prefix('f') {
                    if let Ok(n) = digits.parse::<u8>() {
                        return (1..=24).contains(&n).then_some(Chord {
                            code: KeyCode::F(n),
                            ctrl,
                        });
                    }
                }
                let mut chars = name.chars();
                let first = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                KeyCode::Char(first)
            }
        };
        Some(Chord { code, ctrl })
    }
}

/// Describe the event delivered by the terminal, including modifiers used by editors.
pub fn received_key(event: KeyEvent) -> String {
    let mut parts = Vec::new();
    for (modifier, name) in [(KeyModifiers::CONTROL,"Ctrl"),(KeyModifiers::ALT,"Alt"),
        (KeyModifiers::SHIFT,"Shift"),(KeyModifiers::SUPER,"Super"),
        (KeyModifiers::HYPER,"Hyper"),(KeyModifiers::META,"Meta")] {
        if event.modifiers.contains(modifier) { parts.push(name.to_string()); }
    }
    parts.push(match event.code {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) if c.is_control() => c.escape_default().to_string(),
        KeyCode::Char(c) => c.to_string(),
        code => code.to_string(),
    });
    parts.join("+")
}

/// The bindings in force, first match wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    bindings: Vec<(Chord, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        let mut bindings = Vec::new();
        for (action, chords) in DEFAULTS {
            for text in *chords {
                if let Some(chord) = Chord::parse(text) {
                    bindings.push((chord, *action));
                }
            }
        }
        Keymap { bindings }
    }
}

impl Keymap {
    pub fn action(&self, event: KeyEvent) -> Option<Action> {
        let chord = Chord::of(event)?;
        self.bindings
            .iter()
            .find_map(|(c, a)| (*c == chord).then_some(*a))
    }

    pub fn chords(&self, action: Action) -> Vec<Chord> {
        self.bindings
            .iter()
            .filter_map(|(c, a)| (*a == action).then_some(*c))
            .collect()
    }

    /// The keys of `action`, as the editor and the file spell them.
    pub fn text(&self, action: Action) -> String {
        let chords: Vec<String> = self.chords(action).iter().map(|c| c.text()).collect();
        if chords.is_empty() {
            "—".to_string()
        } else {
            chords.join(", ")
        }
    }

    /// Gives `chord` to `action`, alone unless `keep` asks for an alternate.
    /// Returns the action it was taken from, when it had one.
    pub fn bind(&mut self, action: Action, chord: Chord, keep: bool) -> Option<Action> {
        let stolen = self
            .bindings
            .iter()
            .find_map(|(c, a)| (*c == chord && *a != action).then_some(*a));
        self.bindings
            .retain(|(c, a)| *c != chord && (keep || *a != action));
        self.bindings.push((chord, action));
        stolen
    }

    pub fn reset(&mut self, action: Action) {
        self.bindings.retain(|(_, a)| *a != action);
        let defaults = Keymap::default();
        for chord in defaults.chords(action) {
            // A default that another action took over stays where it is.
            if !self.bindings.iter().any(|(c, _)| *c == chord) {
                self.bindings.push((chord, action));
            }
        }
    }

    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let value = serde_json::from_str::<Value>(&text)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let object = value
            .as_object()
            .ok_or_else(|| format!("{}: expected a JSON object", path.display()))?;
        let mut keymap = Keymap::default();
        for (action, _) in DEFAULTS {
            let Some(value) = object.get(action.key()) else {
                continue;
            };
            let list = value
                .as_array()
                .ok_or_else(|| format!("{}: {} must be a list", path.display(), action.key()))?;
            // Old saved defaults used T for MyThreads. Migrate that one exact
            // default only; a file already naming ChannelTabs is intentional.
            if *action == Action::MyThreads && !object.contains_key("channel_tabs")
                && list.len() == 1 && list[0].as_str() == Some("T") {
                continue;
            }
            if *action == Action::PageUp && !object.contains_key("toggle_conversations")
                && value == &serde_json::json!(["ctrl-b", "page-up"]) {
                continue;
            }
            keymap.bindings.retain(|(_, a)| a != action);
            for item in list {
                let text = item.as_str().ok_or_else(|| {
                    format!("{}: {} must hold key names", path.display(), action.key())
                })?;
                let chord = Chord::parse(text)
                    .ok_or_else(|| format!("{}: unknown key {text:?}", path.display()))?;
                keymap.bindings.retain(|(c, _)| *c != chord);
                keymap.bindings.push((chord, *action));
            }
        }
        // Add the new aliases to unchanged old defaults, without stealing a custom key.
        for (action, old, alias) in [(Action::HalfPageDown,"ctrl-d","f"),(Action::HalfPageUp,"ctrl-u","b")] {
            if object.get(action.key()) == Some(&serde_json::json!([old])) {
                let chord=Chord::parse(alias).unwrap();
                if !keymap.bindings.iter().any(|(key,_)|*key==chord) { keymap.bind(action,chord,true); }
            }
        }
        Ok(keymap)
    }

    pub fn save(&self, path: Option<&Path>) -> Result<PathBuf, String> {
        let path = path.ok_or_else(|| {
            "no configuration directory; set SLACK_TUI_KEYS to a file".to_string()
        })?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        let mut object = Map::new();
        object.insert("version".to_string(), Value::from(1));
        for (action, _) in DEFAULTS {
            let chords: Vec<Value> = self
                .chords(*action)
                .iter()
                .map(|c| Value::from(c.text()))
                .collect();
            object.insert(action.key().to_string(), Value::Array(chords));
        }
        let text = serde_json::to_string_pretty(&object).map_err(|error| error.to_string())?;
        let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&temporary, format!("{text}\n"))
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!("{}: {error}", path.display()));
        }
        Ok(path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What each action answers to, which is all that a saved file must
    /// preserve: the order bindings sit in never reaches behavior.
    fn bound(keymap: &Keymap) -> Vec<(Action, String)> {
        DEFAULTS
            .iter()
            .map(|(action, _)| (*action, keymap.text(*action)))
            .collect()
    }

    fn press(code: KeyCode, ctrl: bool) -> KeyEvent {
        KeyEvent::new(
            code,
            if ctrl {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            },
        )
    }

    #[test]
    fn old_page_up_default_migrates_to_sidebar_toggle() {
        let path = std::env::temp_dir().join(format!("slack-tui-sidebar-keys-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"page_up":["ctrl-b","page-up"]}"#).unwrap();
        let keymap = Keymap::load(Some(&path)).unwrap();
        assert_eq!(keymap.action(press(KeyCode::Char('b'), true)), Some(Action::ToggleConversations));
        assert_eq!(keymap.action(press(KeyCode::PageUp, false)), Some(Action::PageUp));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn every_default_key_parses_and_reaches_its_action() {
        let keymap = Keymap::default();
        for (action, chords) in DEFAULTS {
            for text in *chords {
                let chord = Chord::parse(text).unwrap_or_else(|| panic!("{text} is not a key"));
                assert_eq!(chord.text(), *text, "{text} does not round trip");
                assert_eq!(
                    keymap.action(press(chord.code, chord.ctrl)),
                    Some(*action),
                    "{text}"
                );
            }
        }
        assert_eq!(keymap.action(press(KeyCode::Char('z'), false)), None);
        // Case is part of the key: G is not g.
        assert_eq!(
            keymap.action(press(KeyCode::Char('G'), false)),
            Some(Action::Last)
        );
        assert_eq!(
            keymap.action(press(KeyCode::Char('g'), false)),
            Some(Action::First)
        );
    }

    #[test]
    fn saved_react_binding_loads_as_view_reactions() {
        let path = std::env::temp_dir().join(format!("slack-tui-reaction-keys-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"react":["f6"]}"#).unwrap();
        let keymap = Keymap::load(Some(&path)).unwrap();
        assert_eq!(keymap.action(press(KeyCode::F(6), false)), Some(Action::React));
        assert_eq!(Action::React.label(), "View reactions");
        keymap.save(Some(&path)).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["react"], serde_json::json!(["f6"]));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn binding_takes_the_key_from_whoever_had_it() {
        let mut keymap = Keymap::default();
        let space = Chord::parse("space").unwrap();
        assert_eq!(keymap.bind(Action::Compose, space, false), None);
        assert_eq!(
            keymap.action(press(KeyCode::Char(' '), false)),
            Some(Action::Compose)
        );
        assert_eq!(keymap.text(Action::Compose), "space");

        let j = Chord::parse("j").unwrap();
        assert_eq!(keymap.bind(Action::Compose, j, true), Some(Action::Down));
        assert_eq!(
            keymap.action(press(KeyCode::Char('j'), false)),
            Some(Action::Compose)
        );
        assert_eq!(keymap.text(Action::Down), "down");

        keymap.reset(Action::Compose);
        assert_eq!(keymap.text(Action::Compose), "c");
        // j went back to being free, so Down takes it again.
        keymap.reset(Action::Down);
        assert_eq!(keymap.text(Action::Down), "j, down");
    }

    #[test]
    fn the_file_round_trips_and_rejects_an_unknown_key() {
        let path = std::env::temp_dir().join(format!(
            "slack-tui-keys-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut keymap = Keymap::default();
        keymap.bind(Action::Refresh, Chord::parse("f5").unwrap(), true);
        keymap.save(Some(&path)).unwrap();
        assert_eq!(bound(&Keymap::load(Some(&path)).unwrap()), bound(&keymap));

        std::fs::write(&path, "{\"quit\":[\"ctrl-q\"]}\n").unwrap();
        let partial = Keymap::load(Some(&path)).unwrap();
        assert_eq!(partial.text(Action::Quit), "ctrl-q");
        assert_eq!(partial.text(Action::Down), "j, down");

        std::fs::write(&path, "{\"command\":[\"ctrl-shift-p\"]}").unwrap();
        let custom = Keymap::load(Some(&path)).unwrap();
        assert_eq!(
            custom.action(KeyEvent::new(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(Action::Command)
        );
        assert!(custom.chords(Action::ConversationsPane).is_empty());
        let mut custom = custom;
        custom.bind(
            Action::ConversationsPane,
            Chord::parse("f6").unwrap(),
            false,
        );
        custom.save(Some(&path)).unwrap();
        assert_eq!(
            Keymap::load(Some(&path))
                .unwrap()
                .action(press(KeyCode::F(6), false)),
            Some(Action::ConversationsPane)
        );

        std::fs::write(&path, r#"{"my_threads":["T"]}"#).unwrap();
        let migrated = Keymap::load(Some(&path)).unwrap();
        assert_eq!(migrated.action(press(KeyCode::Char('T'), false)), Some(Action::ChannelTabs));
        assert_eq!(migrated.action(press(KeyCode::Char('t'), true)), Some(Action::MyThreads));
        std::fs::write(&path, r#"{"my_threads":["T"],"channel_tabs":["f6"]}"#).unwrap();
        let explicit = Keymap::load(Some(&path)).unwrap();
        assert_eq!(explicit.action(press(KeyCode::Char('T'), false)), Some(Action::MyThreads));
        assert_eq!(explicit.action(press(KeyCode::F(6), false)), Some(Action::ChannelTabs));
        std::fs::write(&path, r#"{"half_page_down":["ctrl-d"],"half_page_up":["ctrl-u"]}"#).unwrap();
        let migrated=Keymap::load(Some(&path)).unwrap();
        assert_eq!(migrated.action(press(KeyCode::Char('f'),false)),Some(Action::HalfPageDown));
        assert_eq!(migrated.action(press(KeyCode::Char('b'),false)),Some(Action::HalfPageUp));
        std::fs::write(&path, r#"{"half_page_down":["ctrl-d"],"archive":["f"]}"#).unwrap();
        let custom=Keymap::load(Some(&path)).unwrap();
        assert_eq!(custom.action(press(KeyCode::Char('f'),false)),Some(Action::Archive));
        std::fs::write(&path, "{\"quit\":[\"meta-q\"]}\n").unwrap();
        let error = Keymap::load(Some(&path)).unwrap_err();
        assert!(error.contains("unknown key"), "{error}");
        std::fs::remove_file(path).unwrap();
    }
}
