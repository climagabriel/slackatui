//! Drawing: conversations on the left, the active view on the right, two
//! status lines below, a help overlay on demand.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Focus, ImageState, Mode, PromptKind, Sort, View};
use crate::render::{self, Ctx};
use ratatui_image::{Image, StatefulImage};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let rows = app.prompt_rows();
    let [main, status] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(rows)]).areas(area);
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
        format!(
            " {} · {}by {} ",
            app.filtered.len(),
            if app.unreads_first {
                "unread first · "
            } else {
                ""
            },
            app.sort_label()
        )
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
            if c.unread && !c.muted {
                // The marker carries the mention count; the name itself lights up.
                name.insert_str(
                    0,
                    &if c.mentions > 0 {
                        format!("●{} ", c.mentions)
                    } else {
                        "● ".to_string()
                    },
                );
            }
            let name = clip(&name, room);
            let pad = room.saturating_sub(name.width());
            let mut name_style = if c.unread && !c.muted {
                Style::new()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD)
            } else if app.highlight_cached && !c.live_only {
                Style::new().fg(Color::LightGreen)
            } else if c.live_only || c.left {
                Style::new().add_modifier(Modifier::DIM)
            } else {
                Style::new()
            };
            if Some(i) == open_idx {
                name_style = name_style.add_modifier(Modifier::BOLD);
            }
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
    if let Some(View::Emoji { .. }) = app.stack.last() {
        draw_emoji_picker(frame, app, inner);
        return;
    }
    if let Some(View::Image { .. }) = app.stack.last() {
        draw_image_view(frame, app, inner);
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
    let image_font = app.image_font();
    // The read marker applies to the conversation's own timeline only.
    let last_read = match (
        app.stack.iter().rev().find(|v| {
            !matches!(
                v,
                View::Raw { .. } | View::Image { .. } | View::Emoji { .. }
            )
        }),
        app.open.as_ref(),
    ) {
        (None, Some(o)) => Some(app.corpus.convs[o.conv].last_read).filter(|v| *v > 0),
        _ => None,
    };
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
        Some(View::Search { list, .. }) | Some(View::Threads { list }) => (list, conv_archive),
        _ => (&mut open.list, conv_archive),
    };
    let ctx = Ctx {
        archive,
        corpus,
        tz: *tz,
        image_font,
        last_read,
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
    // Inline images: one rect per slot whose first row is on screen.
    let slots: Vec<(u16, crate::render::ImageSlot)> = list
        .flat
        .iter()
        .enumerate()
        .skip(list.scroll)
        .take(inner.height as usize)
        .filter_map(|(i, fl)| fl.image.clone().map(|s| ((i - list.scroll) as u16, s)))
        .collect();
    for (row, slot) in slots {
        app.ensure_image(&slot.file, false);
        let x = inner.x + 3;
        let y = inner.y + row;
        let width = slot.cols.min(inner.width.saturating_sub(3));
        let height = slot.rows.min(inner.bottom().saturating_sub(y));
        if width == 0 || height == 0 {
            continue;
        }
        let area = Rect {
            x,
            y,
            width,
            height,
        };
        let note = match app.images.get(&slot.file.id) {
            Some(ImageState::Ready(_)) => None,
            Some(ImageState::Failed(e)) => Some(format!("(image: {e})")),
            Some(_) => Some("(loading image)".to_string()),
            None => Some("(image)".to_string()),
        };
        match note {
            Some(text) => frame.render_widget(
                Paragraph::new(Span::styled(text, Style::new().add_modifier(Modifier::DIM))),
                Rect { height: 1, ..area },
            ),
            None => {
                if let Some(proto) = app.inline_protocol(&slot.file.id, slot.cols, slot.rows) {
                    frame.render_widget(Image::new(proto).allow_clipping(true), area);
                }
            }
        }
    }
}

/// The full-pane viewer: the original file, fitted to the pane.
/// The reaction picker: the query on the first row, matches below it, the
/// cursor row reversed. With no match, Enter sends the query as typed.
fn draw_emoji_picker(frame: &mut Frame, app: &mut App, inner: Rect) {
    let Some(View::Emoji {
        target,
        query,
        cursor,
        matches,
    }) = app.stack.last()
    else {
        return;
    };
    let mut lines: Vec<Line> = editor_lines(
        query,
        Span::styled(
            format!(" {} with: ", target.label),
            Style::new().fg(Color::Cyan),
        ),
    );
    let rows = inner.height.saturating_sub(1) as usize;
    if matches.is_empty() {
        let q = query.text.trim().trim_matches(':');
        let hint = if q.is_empty() {
            "  type to search; Enter sends the name as typed".to_string()
        } else {
            format!("  no match; Enter sends :{q}: as typed")
        };
        lines.push(Line::from(Span::styled(
            hint,
            Style::new().add_modifier(Modifier::DIM),
        )));
    } else {
        let first = cursor
            .saturating_sub(rows / 2)
            .min(matches.len().saturating_sub(rows));
        for (k, &i) in matches.iter().enumerate().skip(first).take(rows) {
            let (name, glyph) = &app.emoji_table[i];
            let style = if k == *cursor {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            lines.push(Line::from(Span::styled(
                format!("  {glyph:<4} {name}"),
                style,
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// An editor's rows, the first behind `prefix`, the cursor cell reversed.
fn editor_lines(ed: &crate::edit::Editor, prefix: Span<'static>) -> Vec<Line<'static>> {
    let cursor = Style::new().add_modifier(Modifier::REVERSED);
    let mut out = Vec::new();
    for (i, (text, at)) in ed.rows().into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if i == 0 {
            spans.push(prefix.clone());
        } else {
            spans.push(Span::raw("   "));
        }
        match at {
            Some(p) => {
                let (before, rest) = text.split_at(p);
                let mut it = rest.chars();
                let under = it
                    .next()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| " ".to_string());
                let after: String = it.collect();
                spans.push(Span::raw(before.to_string()));
                spans.push(Span::styled(under, cursor));
                spans.push(Span::raw(after));
            }
            None => spans.push(Span::raw(text)),
        }
        out.push(Line::from(spans));
    }
    out
}

fn draw_image_view(frame: &mut Frame, app: &mut App, inner: Rect) {
    let Some(View::Image { files, index, .. }) = app.stack.last() else {
        return;
    };
    let file = files[*index].clone();
    app.ensure_image(&file, true);
    let key = format!("{}:full", file.id);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let note = match app.images.get(&key) {
        Some(ImageState::Ready(_)) => None,
        Some(ImageState::Failed(e)) => Some(format!("  image failed: {e}")),
        _ => Some("  loading the original from Slack".to_string()),
    };
    if let Some(text) = note {
        frame.render_widget(Paragraph::new(Span::styled(text, dim)), inner);
        return;
    }
    // Encode once per image: the fitted protocol is kept on the view and
    // rebuilt only when the shown image changes.
    let stale = match app.stack.last() {
        Some(View::Image { shown, .. }) => shown.as_ref().map(|(k, _)| k != &key).unwrap_or(true),
        _ => return,
    };
    if stale {
        let img = match app.images.get(&key) {
            Some(ImageState::Ready(img)) => img.clone(),
            _ => return,
        };
        let Some(picker) = app.picker.as_ref() else {
            return;
        };
        let fresh = picker.new_resize_protocol(img);
        if let Some(View::Image { shown, .. }) = app.stack.last_mut() {
            *shown = Some((key.clone(), fresh));
        }
    }
    if let Some(View::Image {
        shown: Some((_, proto)),
        ..
    }) = app.stack.last_mut()
    {
        frame.render_stateful_widget(StatefulImage::new(), inner, proto);
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    // One line: a prompt when one is open; else the clock, a running job,
    // the last status, and the selected message's permalink. H lists the keys.
    if let Mode::Prompt { kind, buf, .. } = &app.mode {
        let label = match kind {
            PromptKind::Command => "",
            PromptKind::Date => "go to date (YYYY-MM-DD)",
            PromptKind::Archive => "archive a conversation from Slack, last 90 days (URL or id)",
            PromptKind::Compose => app
                .compose
                .as_ref()
                .map(|c| c.label.as_str())
                .unwrap_or("message"),
        };
        let prefix = Span::styled(
            if label.is_empty() {
                " /".to_string()
            } else {
                format!(" {label}: ")
            },
            Style::new().fg(Color::Cyan),
        );
        let lines = editor_lines(buf, prefix);
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    let mut spans = vec![Span::styled(format!(" {} ", app.tz.label()), dim)];
    if let Some(job) = &app.job {
        let frame_char = crate::live::SPINNER[app.spinner % crate::live::SPINNER.len()];
        spans.push(Span::styled(
            format!(
                "{frame_char} {} ({}s)  ",
                job.label,
                job.started.elapsed().as_secs()
            ),
            Style::new().fg(Color::Yellow),
        ));
    }
    if !app.status.is_empty() {
        spans.push(Span::styled(
            format!("{}  ", app.status),
            Style::new().fg(Color::Yellow),
        ));
    }
    if let Some(m) = app.selected() {
        spans.push(Span::styled(
            m.permalink(&app.corpus.workspace_url),
            Style::new().fg(Color::Blue),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
        "a command, each taking an optional #name: find|search TEXT filters the list or searches the open conversation; leave; mute|unmute (never shown as unread); cache start|stop|wipe (archive it, pause its hourly refresh, delete its archive)",
    ),
    (
        "o",
        "from a search hit or a thread: show the message in the channel",
    ),
    ("d", "go to a date (YYYY-MM-DD)"),
    ("T", "threads you took part in, newest reply first"),
    (
        "i",
        "the selected message's images, full pane; j/k between them",
    ),
    ("I", "inline image thumbnails on/off"),
    ("C", "highlight cached conversations in light green on/off"),
    ("U", "unread conversations on top on/off"),
    (
        "c",
        "write a message: to the open conversation, into the open thread, or into the selected hit's thread; Enter sends, Esc keeps the draft",
    ),
    (
        "e",
        "react to the selected message: a picker opens; type to search, Up/Down or Ctrl-n/Ctrl-p to move, Enter reacts (the name as typed when nothing matches); your own reaction again removes it",
    ),
    (
        "m",
        "mark read: the highlighted conversation, or the open one at its newest message",
    ),
    (
        "M",
        "mark unread from the message under the cursor (the highlighted conversation in the list)",
    ),
    (
        "Esc in the list",
        "drop the filter, then close the conversation: the home view",
    ),
    ("?, H", "this guide"),
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
    (
        "in a prompt",
        "Ctrl-a/e line start/end, Ctrl-b/f and Alt-b/f by char and word, Ctrl-k/u kill to line end/start, Ctrl-w and Alt-d kill a word, Ctrl-y yank, Ctrl-d delete under the cursor; Ctrl-j a newline in a message",
    ),
];

const HELP_NOTE: &str = "The unread part of a conversation starts at the highlighted day divider; the list marks unread conversations with ● and the mention count.";

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
        format!("  {HELP_NOTE}"),
        Style::new().add_modifier(Modifier::DIM),
    )));
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
        image_font: None,
        last_read: None,
    };
    open.list.rebuild(&ctx, width);
    open.list
        .flat
        .iter()
        .map(|fl| render::line_text(&fl.line))
        .collect::<Vec<_>>()
        .join("\n")
}
