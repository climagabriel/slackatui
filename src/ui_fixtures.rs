//! Drawn-buffer fixtures for every element the interface puts on screen.
//!
//! Each fixture builds one application state, draws it through a `TestBackend`
//! and compares the whole buffer — every cell's symbol and its style — against
//! a file on disk. The set covers the conversations pane and its top rows, an
//! open conversation down to the parts of a single message, the THREADS and
//! UNREADS cards, every overlay and editor, and the status line: the
//! inventory `/labels` names.
//!
//! They exist to pin the drawing as it was before that mode: with `/labels`
//! off the buffers have to stay identical, cell for cell, and "identical to
//! today" is not a claim a test can make about itself.
//!
//! `SLACK_TUI_UPDATE_FIXTURES=1 cargo test` rewrites the files. Rewriting one
//! asserts the drawing changed on purpose; read the diff before committing it.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Style;
use ratatui::Terminal;
use serde_json::json;

use crate::app::{
    App, ConversationsPaneVisibility, Focus, MsgList, Mode, PromptKind, ScanOverlay, View,
};
use crate::archive::{FileInfo, Msg};
use crate::render::Card;

/// One drawn state: what it is called on disk, how big the terminal is, and
/// the application state it draws.
pub(crate) struct Fixture {
    pub name: &'static str,
    pub width: u16,
    pub height: u16,
    pub build: fn() -> App,
}

pub(crate) const FIXTURES: &[Fixture] = &[
    Fixture { name: "conversations", width: 120, height: 24, build: conversations },
    Fixture { name: "conversation", width: 120, height: 30, build: conversation },
    Fixture { name: "collapsed", width: 100, height: 18, build: collapsed },
    Fixture { name: "threads", width: 120, height: 30, build: threads },
    Fixture { name: "unreads", width: 120, height: 30, build: unreads },
    Fixture { name: "compose", width: 120, height: 14, build: compose },
    Fixture { name: "compose-narrow", width: 50, height: 12, build: compose },
    Fixture { name: "scan-overlay", width: 100, height: 24, build: scan_overlay },
    Fixture { name: "channel-tabs", width: 100, height: 20, build: channel_tabs },
    Fixture { name: "pane-picker", width: 100, height: 20, build: pane_picker },
    Fixture { name: "key-guide", width: 110, height: 30, build: key_guide },
    Fixture { name: "keys-editor", width: 100, height: 20, build: keys_editor },
    Fixture { name: "color-palette", width: 100, height: 20, build: color_palette },
    Fixture { name: "raw-json", width: 100, height: 20, build: raw_json },
    Fixture { name: "images", width: 100, height: 16, build: images },
    Fixture { name: "narrow", width: 24, height: 10, build: narrow },
];

/// A message of the fixture conversation. Timestamps are in 1970 so no date
/// label reads "Today" and the fixtures do not expire overnight.
fn msg(secs: i64, text: &str) -> Msg {
    Msg::from_api(
        "C1".to_string(),
        json!({ "ts": format!("{secs}.000000"), "user": "U1", "text": text }),
    )
    .expect("fixture message")
}

fn base() -> App {
    let mut app = crate::app::tests::mute_test_app();
    app.status = "archive loaded".to_string();
    app
}

/// The home view: the conversations pane with its four top rows, the empty
/// messages pane and the status line.
fn conversations() -> App {
    base()
}

/// One conversation whose messages carry every part a message can have, with
/// a day divider between them and the unread divider inside a day.
fn conversation() -> App {
    let mut app = base();
    app.conversations_pane = ConversationsPaneVisibility::AlwaysHidden;
    app.open_conv(0);
    app.focus = Focus::Msgs;
    let mut first = Msg::from_api(
        "C1".to_string(),
        json!({
            "ts": "1000.000000",
            "user": "U1",
            "text": "the first message of the day",
            "files": [{
                "id": "F1", "title": "diagram.png", "mimetype": "image/png",
                "filetype": "png", "size": 2048
            }],
            "reactions": [{ "name": "eyes", "count": 2 }],
            "reply_count": 3,
            "thread_ts": "1000.000000"
        }),
    )
    .expect("fixture message");
    first.archived_replies = 3;
    let messages = vec![first, msg(2000, "the second message of the day"), msg(90000, "the next day")];
    app.corpus.convs[0].last_read = 1_500_000_000;
    app.corpus.convs[0].unread_count = Some(2);
    app.open.as_mut().expect("open conversation").list = MsgList::new(messages, false);
    app
}

/// A message taller than its share of the pane, shown as its first row, the
/// count of what is hidden and its last row.
fn collapsed() -> App {
    let mut app = base();
    app.conversations_pane = ConversationsPaneVisibility::AlwaysHidden;
    app.open_conv(0);
    app.focus = Focus::Msgs;
    let body = (0..30).map(|n| format!("body {n}")).collect::<Vec<_>>().join("\n");
    app.open.as_mut().expect("open conversation").list =
        MsgList::new(vec![msg(1000, &body), msg(2000, "short")], false);
    app
}

/// The THREADS view: one card, its header, the root, the elided replies and
/// the thread's newest reply.
fn threads() -> App {
    let mut app = base();
    app.conversations_pane = ConversationsPaneVisibility::AlwaysHidden;
    app.focus = Focus::Msgs;
    let card = Card {
        conversation: "#one".to_string(),
        participants: "alice, bob, and 2 others".to_string(),
        hidden: 5,
        counted_from: 1_000_000,
        counted_through: 3_000_000,
        tail: vec![msg(3000, "the newest reply")],
        elision: crate::render::Elision::Replies,
    };
    app.stack.push(View::Threads {
        list: MsgList::with_cards(vec![(msg(1000, "the thread root"), card)]),
    });
    app
}

/// The UNREADS view: one card, the conversation and its unread count, the
/// first unread message, the elided count and the newest unread messages.
fn unreads() -> App {
    let mut app = base();
    app.conversations_pane = ConversationsPaneVisibility::AlwaysHidden;
    app.focus = Focus::Msgs;
    let card = Card {
        conversation: "#one".to_string(),
        participants: "6 unread".to_string(),
        hidden: 2,
        counted_from: 1_000_000,
        counted_through: 4_000_000,
        tail: vec![msg(3000, "the newest but one"), msg(4000, "the newest unread")],
        elision: crate::render::Elision::Messages,
    };
    app.stack.push(View::Unreads {
        list: MsgList::with_cards(vec![(msg(1000, "the first unread message"), card)]),
        deleted: Vec::new(),
    });
    app
}

/// The compose box over the status line.
fn compose() -> App {
    let mut app = base();
    app.open_conv(0);
    app.compose = Some(crate::app::Compose {
        conv: 0,
        cid: "C1".to_string(),
        thread: None,
        label: "message to #one".to_string(),
    });
    app.mode = Mode::Prompt {
        kind: PromptKind::Compose,
        buf: crate::edit::Editor::with("a draft".to_string()),
        previous: String::new(),
    };
    app
}

/// The `/find` progress box, over everything else.
fn scan_overlay() -> App {
    let mut app = base();
    app.scan_overlay = Some(ScanOverlay::for_test(
        "/find nginx",
        vec![
            crate::live::ScanLine::plain("scanning #one"),
            crate::live::ScanLine { text: "3 hits".to_string(), dim: true },
        ],
    ));
    app
}

/// The channel-tabs menu over the messages pane.
fn channel_tabs() -> App {
    let mut app = base();
    app.open_conv(0);
    let client = std::sync::Arc::new(crate::api::Client::for_test(|_, _| Err("test".to_string())));
    app.channel_browser = Some(crate::canvas::Browser::new(
        client,
        "C1".to_string(),
        "one".to_string(),
    ));
    app
}

/// The conversations-pane picker, over the whole main area.
fn pane_picker() -> App {
    let mut app = base();
    app.pane_menu = Some(crate::conversations_pane::Menu::new(
        app.pane_settings.clone(),
        &app.corpus.convs,
    ));
    app
}

/// The key guide.
fn key_guide() -> App {
    let mut app = base();
    app.help = true;
    app
}

/// The `/keys` editor.
fn keys_editor() -> App {
    let mut app = base();
    app.open_conv(0);
    app.focus = Focus::Msgs;
    app.stack.push(View::Keys {
        cursor: 0,
        original: app.keymap.clone(),
        return_focus: Focus::Msgs,
        capture: None,
    });
    app
}

/// The `/colorpalette` editor.
fn color_palette() -> App {
    let mut app = base();
    app.open_conv(0);
    app.focus = Focus::Msgs;
    app.stack.push(View::ColorPalette {
        highlights: None,
        cursor: 0,
        original: app.palette.clone(),
        return_focus: Focus::Msgs,
    });
    app
}

/// The raw JSON view of one message.
fn raw_json() -> App {
    let mut app = base();
    app.open_conv(0);
    app.focus = Focus::Msgs;
    app.stack.push(View::Raw {
        title: "raw".to_string(),
        browser: crate::raw::Browser::new(&json!({ "ts": "1000.000000", "text": "hi" })),
        entry_focus: Focus::Msgs,
    });
    app
}

/// The images view, without an image protocol: the note it draws instead.
fn images() -> App {
    let mut app = base();
    app.open_conv(0);
    app.focus = Focus::Msgs;
    app.stack.push(View::Image {
        files: vec![FileInfo::from_slack(
            &json!({ "id": "F1", "title": "diagram.png", "mimetype": "image/png" }),
            "C1",
        )],
        index: 0,
        zoom: 100,
        shown: None,
    });
    app
}

/// A terminal too narrow to carry a tag anywhere.
fn narrow() -> App {
    let mut app = base();
    app.status = "loaded from the archive".to_string();
    app.open_conv(0);
    app.focus = Focus::Msgs;
    app.open.as_mut().expect("open conversation").list =
        MsgList::new(vec![msg(1000, "a message")], false);
    app
}

/// The application state one fixture builds, for a test that wants to draw it
/// at another size or with another mode on.
pub(crate) fn state(name: &str) -> App {
    let fixture = FIXTURES
        .iter()
        .find(|fixture| fixture.name == name)
        .unwrap_or_else(|| panic!("no fixture named {name}"));
    (fixture.build)()
}

/// One fixture drawn, through the same `ui::draw` the application runs.
pub(crate) fn drawn(fixture: &Fixture) -> Buffer {
    let mut app = (fixture.build)();
    let mut terminal = Terminal::new(TestBackend::new(fixture.width, fixture.height))
        .expect("test terminal");
    terminal
        .draw(|frame| crate::ui::draw(frame, &mut app))
        .expect("draw");
    terminal.backend().buffer().clone()
}

/// A style as one short token, so a row of identical cells costs one run
/// rather than one description per cell.
fn describe(style: &Style) -> String {
    let mut parts = Vec::new();
    if let Some(color) = style.fg {
        parts.push(format!("fg={color:?}"));
    }
    if let Some(color) = style.bg {
        parts.push(format!("bg={color:?}"));
    }
    if let Some(color) = style.underline_color {
        parts.push(format!("ul={color:?}"));
    }
    if !style.add_modifier.is_empty() {
        parts.push(format!("+{:?}", style.add_modifier));
    }
    if !style.sub_modifier.is_empty() {
        parts.push(format!("-{:?}", style.sub_modifier));
    }
    if parts.is_empty() {
        "plain".to_string()
    } else {
        parts.join(",")
    }
}

/// The buffer as text: one line of symbols per row, and under it the styles as
/// column runs. A cell continuing a double-width character has no symbol of
/// its own and is written `␀`.
pub(crate) fn snapshot(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        let mut symbols = String::new();
        let mut runs: Vec<String> = Vec::new();
        let mut run: Option<(u16, Style)> = None;
        for x in area.left()..area.right() {
            let cell = &buffer[(x, y)];
            symbols.push_str(match cell.symbol() {
                "" => "␀",
                symbol => symbol,
            });
            let style = cell.style();
            match run {
                Some((_, previous)) if previous == style => {}
                Some((start, previous)) => {
                    runs.push(format!("{start}..{x}:{}", describe(&previous)));
                    run = Some((x, style));
                }
                None => run = Some((x, style)),
            }
        }
        if let Some((start, previous)) = run {
            runs.push(format!("{start}..{}:{}", area.right(), describe(&previous)));
        }
        out.push_str(&format!("{y:3} |{symbols}|\n    {}\n", runs.join(" ")));
    }
    out
}

fn path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(format!("{name}.txt"))
}

/// Every element of the inventory, drawn and compared cell for cell against
/// the file committed for it.
#[test]
fn every_element_draws_as_its_fixture() {
    let update = std::env::var_os("SLACK_TUI_UPDATE_FIXTURES").is_some();
    let mut missing = Vec::new();
    for fixture in FIXTURES {
        let drawn = snapshot(&drawn(fixture));
        let file = path(fixture.name);
        if update {
            std::fs::create_dir_all(file.parent().expect("fixtures directory"))
                .expect("fixtures directory");
            std::fs::write(&file, &drawn).expect("write fixture");
            continue;
        }
        let Ok(stored) = std::fs::read_to_string(&file) else {
            missing.push(fixture.name);
            continue;
        };
        if stored == drawn {
            continue;
        }
        let at = stored
            .lines()
            .zip(drawn.lines())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        panic!(
            "fixture {} changed at line {at}\n  stored: {:?}\n   drawn: {:?}",
            fixture.name,
            stored.lines().nth(at),
            drawn.lines().nth(at)
        );
    }
    assert!(missing.is_empty(), "no fixture file for {missing:?}");
}
