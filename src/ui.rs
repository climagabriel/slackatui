//! Drawing: conversations on the left, the active view on the right, two
//! status lines below, a help overlay on demand.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Focus, Mode, PromptKind, Sort, View};
use crate::render::{self, Ctx};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let [main, status] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(area);
    let conv_w = (area.width / 4).clamp(22, 40);
    let [left, right] =
        Layout::horizontal([Constraint::Length(conv_w), Constraint::Min(20)]).areas(main);
    draw_convs(frame, app, left);
    draw_msgs(frame, app, right);
    draw_status(frame, app, status);
    if app.help {
        draw_help(frame, area);
    }
}

fn border(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().add_modifier(Modifier::DIM)
    }
}

fn draw_convs(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Convs;
    let title = if app.filter.is_empty() {
        format!(" {} · by {} ", app.filtered.len(), app.sort_label())
    } else {
        format!(" {} · '{}' ", app.filtered.len(), app.filter)
    };
    let block = Block::bordered().title(title).border_style(border(focused));
    let inner = block.inner(area);
    let width = inner.width as usize;
    let dim = Style::new().add_modifier(Modifier::DIM);
    let open_idx = app.open.as_ref().map(|o| o.conv);
    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|&i| {
            let c = app.conv(i);
            // The number that ordered the list: the owner's recency-weighted
            // messages under "my activity", the conversation's total otherwise.
            let count = human_count(if app.sort == Sort::Mine {
                c.score.round() as i64
            } else {
                c.msgs
            });
            let room = width.saturating_sub(count.len() + 1);
            let mut name = c.name.clone();
            if c.archived {
                name.push('†');
            }
            if c.unread {
                name.insert_str(0, "● ");
            }
            let name = clip(&name, room);
            let pad = room.saturating_sub(name.width());
            let name_style = if Some(i) == open_idx {
                Style::new().add_modifier(Modifier::BOLD)
            } else if c.live_only {
                Style::new().add_modifier(Modifier::DIM)
            } else {
                Style::new()
            };
            ListItem::new(Line::from(vec![
                Span::styled(name, name_style),
                Span::raw(" ".repeat(pad + 1)),
                Span::styled(count, dim),
            ]))
        })
        .collect();
    let highlight = if focused {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    };
    let list = List::new(items).block(block).highlight_style(highlight);
    let mut state = ListState::default().with_selected(if app.filtered.is_empty() {
        None
    } else {
        Some(app.conv_cursor)
    });
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_msgs(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Msgs;
    let title = format!(
        " {} ",
        clip(&app.title(), area.width.saturating_sub(4) as usize)
    );
    let block = Block::bordered().title(title).border_style(border(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.msgs_height = inner.height as usize;
    if inner.width < 4 || inner.height == 0 {
        return;
    }
    if let Some(View::Raw { lines, scroll, .. }) = app.stack.last() {
        let shown: Vec<Line> = lines
            .iter()
            .skip(*scroll)
            .take(inner.height as usize)
            .map(|s| Line::from(s.as_str()))
            .collect();
        frame.render_widget(Paragraph::new(Text::from(shown)), inner);
        return;
    }
    let App {
        corpus,
        open,
        stack,
        tz,
        ..
    } = app;
    let Some(open) = open.as_mut() else {
        let hint = Line::from(Span::styled(
            "  select a conversation and press Enter",
            Style::new().add_modifier(Modifier::DIM),
        ));
        frame.render_widget(Paragraph::new(hint), inner);
        return;
    };
    let conv = &corpus.convs[open.conv];
    let conv_archive = &corpus.archives[conv.archive];
    let (list, archive) = match stack
        .iter_mut()
        .rev()
        .find(|v| !matches!(v, View::Raw { .. }))
    {
        Some(View::Thread { list, live, .. }) => (list, live.as_deref().unwrap_or(conv_archive)),
        Some(View::Search { list, .. }) => (list, conv_archive),
        _ => (&mut open.list, conv_archive),
    };
    let ctx = Ctx {
        archive,
        corpus,
        tz: *tz,
    };
    let text_w = inner.width as usize - 1;
    list.rebuild(&ctx, text_w);
    list.ensure_visible(inner.height as usize);
    let cursor = list.cursor;
    let gutter_on = Span::styled(
        "▎",
        Style::new().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }),
    );
    let header_line = list.first.get(cursor).copied();
    let mut shown: Vec<Line> = Vec::with_capacity(inner.height as usize);
    for (i, fl) in list
        .flat
        .iter()
        .enumerate()
        .skip(list.scroll)
        .take(inner.height as usize)
    {
        let selected = fl.msg == Some(cursor);
        let mut spans: Vec<Span> = Vec::with_capacity(fl.line.spans.len() + 1);
        spans.push(if selected {
            gutter_on.clone()
        } else {
            Span::raw(" ")
        });
        spans.extend(fl.line.spans.iter().cloned());
        let mut line = Line::from(spans);
        if selected && header_line == Some(i) {
            line.style = Style::new().add_modifier(Modifier::REVERSED);
        }
        shown.push(line);
    }
    frame.render_widget(Paragraph::new(Text::from(shown)), inner);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let [top, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let first = match &app.mode {
        Mode::Prompt { kind, buf, .. } => {
            let label = match kind {
                PromptKind::Filter => "filter conversations",
                PromptKind::Search => "search this conversation",
                PromptKind::Date => "go to date (YYYY-MM-DD)",
                PromptKind::Archive => {
                    "archive a conversation from Slack, last 90 days (URL or id)"
                }
            };
            Line::from(vec![
                Span::styled(format!(" {label}: "), Style::new().fg(Color::Cyan)),
                Span::raw(buf.clone()),
                Span::styled("▏", Style::new().fg(Color::Cyan)),
            ])
        }
        Mode::Normal => Line::from(Span::styled(format!(" {}", app.hints()), dim)),
    };
    frame.render_widget(Paragraph::new(first), top);
    // Status first: an error must not hide behind a long permalink.
    let mut second = vec![Span::styled(format!(" {} ", app.tz.label()), dim)];
    if !app.status.is_empty() {
        second.push(Span::styled(
            format!("{}  ", app.status),
            Style::new().fg(Color::Yellow),
        ));
    }
    if let Some(job) = &app.job {
        let frame_char = crate::live::SPINNER[app.spinner % crate::live::SPINNER.len()];
        second.push(Span::styled(
            format!(
                "{frame_char} {} ({}s)  ",
                job.label,
                job.started.elapsed().as_secs()
            ),
            Style::new().fg(Color::Yellow),
        ));
    }
    if let Some(m) = app.selected() {
        second.push(Span::styled(
            m.permalink(&app.corpus.workspace_url),
            Style::new().fg(Color::Blue),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(second)), bottom);
}

const HELP: &[(&str, &str)] = &[
    (
        "j / k, ↓ / ↑",
        "move; k at the top of the channel loads older messages",
    ),
    ("Ctrl-d / Ctrl-u", "half a page"),
    ("Ctrl-f / Ctrl-b, PgDn / PgUp", "a full page"),
    ("g / G", "oldest / newest message of the channel"),
    ("h / l, Tab", "conversations pane / messages pane"),
    (
        "Enter, l",
        "the selected message's thread; inside a thread, its raw JSON",
    ),
    (
        "Esc, h",
        "back: close the thread, search or raw view; then the pane",
    ),
    (
        "/",
        "conversations: filter by name; messages: search the channel",
    ),
    (
        "o",
        "from a search hit or a thread: show the message in the channel",
    ),
    ("d", "go to a date (YYYY-MM-DD)"),
    ("v", "raw JSON of the selected message"),
    ("r", "reload the conversation from the archive"),
    (
        "R",
        "refresh the conversation from Slack now (a slackdump resume)",
    ),
    ("a", "archive a conversation not cached yet (URL or id)"),
    (
        "s",
        "sort conversations: my activity (messages you wrote), name, recent, size",
    ),
    ("q, Ctrl-c", "quit"),
];

fn draw_help(frame: &mut Frame, area: Rect) {
    let w = 96.min(area.width.saturating_sub(2));
    let h = (HELP.len() as u16 + 4).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: (area.width - w) / 2,
        y: (area.height - h) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    let mut lines = Vec::new();
    for (k, v) in HELP {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<28} "), Style::new().fg(Color::Cyan)),
            Span::raw(*v),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  any key closes this",
        Style::new().add_modifier(Modifier::DIM),
    )));
    let block = Block::bordered()
        .title(" keys ")
        .border_style(Style::new().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), rect);
}

pub fn human_count(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

pub fn clip(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Plain-text rendering of a message list, for `--dump`.
pub fn dump(app: &mut App, width: usize) -> String {
    let Some(open) = app.open.as_mut() else {
        return String::new();
    };
    let conv = &app.corpus.convs[open.conv];
    let ctx = Ctx {
        archive: &app.corpus.archives[conv.archive],
        corpus: &app.corpus,
        tz: app.tz,
    };
    open.list.rebuild(&ctx, width);
    open.list
        .flat
        .iter()
        .map(|fl| render::line_text(&fl.line))
        .collect::<Vec<_>>()
        .join("\n")
}
