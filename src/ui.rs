//! Drawing: conversations on the left, the active view on the right, two
//! status lines below, a help overlay on demand.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Focus, ImageState, Mode, PromptKind, Sort, View};
use crate::complete;
use crate::keys::{Action, DEFAULTS};
use crate::palette::{Palette, Role, ROLES};
use crate::render::{self, Ctx};
use ratatui_image::{Image, StatefulImage};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    // Painted first: widgets that set no background of their own keep it.
    let background = app.palette.get(Role::Background);
    if background != Color::Reset {
        frame.render_widget(Block::default().style(Style::new().bg(background)), area);
    }
    let rows = app.prompt_rows();
    let [main, status] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(rows)]).areas(area);
    let conv_w = (area.width / 4).clamp(22, 40);
    let [left, right] =
        Layout::horizontal([Constraint::Length(conv_w), Constraint::Min(20)]).areas(main);
    draw_convs(frame, app, left);
    draw_msgs(frame, app, right);
    draw_status(frame, app, status);
    draw_suggestions(frame, app, main);
    if app.help {
        draw_help(frame, area, app);
    }
}

/// The palette's background, or nothing when the terminal keeps its own.
fn background_style(palette: &Palette) -> Style {
    match palette.get(Role::Background) {
        Color::Reset => Style::new(),
        color => Style::new().bg(color),
    }
}

fn border(focused: bool, palette: &Palette) -> Style {
    if focused {
        Style::new().fg(palette.get(Role::Accent))
    } else {
        Style::new()
            .fg(palette.get(Role::InactiveAccent))
            .add_modifier(Modifier::DIM)
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
    let block = Block::bordered()
        .title(title)
        .border_style(border(focused, &app.palette));
    let inner = block.inner(area);
    let width = inner.width as usize;
    let dim = Style::new().add_modifier(Modifier::DIM);
    let open_idx = app.open.as_ref().map(|o| o.conv);
    let items: Vec<Line> = app
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
            let mut name_style = if c.unread {
                Style::new()
                    .fg(app.palette.get(Role::Unread))
                    .add_modifier(Modifier::BOLD)
            } else if app.highlight_cached && !c.live_only {
                Style::new().fg(app.palette.get(Role::Cached))
            } else if c.live_only || c.left {
                Style::new().add_modifier(Modifier::DIM)
            } else {
                Style::new()
            };
            if Some(i) == open_idx {
                name_style = name_style.add_modifier(Modifier::BOLD);
            }
            Line::from(vec![
                Span::styled(name, name_style),
                Span::raw(" ".repeat(pad + 1)),
                Span::styled(count, dim),
            ])
        })
        .collect();
    // The cursor's row is rewritten with the palette's selection colors rather
    // than styled through `highlight_style`: a span's own color would win.
    let items: Vec<ListItem> = items
        .into_iter()
        .enumerate()
        .map(|(k, line)| {
            ListItem::new(if k == app.conv_cursor {
                on_cursor(line, focused, &app.palette)
            } else {
                line
            })
        })
        .collect();
    let list = List::new(items).block(block);
    // The offset carries over from the last frame: rebuilt at zero, ratatui
    // would rescroll to the minimum that shows the cursor, pinning it to the
    // bottom row and moving the whole pane on every step upward.
    let mut state = ListState::default()
        .with_offset(app.conv_offset.min(app.filtered.len().saturating_sub(1)))
        .with_selected(if app.filtered.is_empty() {
            None
        } else {
            Some(app.conv_cursor)
        });
    frame.render_stateful_widget(list, area, &mut state);
    app.conv_offset = state.offset();
}

fn draw_msgs(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Msgs;
    let title = format!(
        " {} ",
        clip(&app.title(), area.width.saturating_sub(4) as usize)
    );
    let block = Block::bordered()
        .title(title)
        .border_style(border(focused, &app.palette));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.msgs_height = inner.height as usize;
    if inner.width < 4 || inner.height == 0 {
        return;
    }
    if let Some(View::Keys { .. }) = app.stack.last() {
        draw_keys(frame, app, inner);
        return;
    }
    if let Some(View::ColorPalette { .. }) = app.stack.last() {
        draw_color_palette(frame, app, inner);
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
        palette,
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
        palette,
    };
    let text_w = inner.width as usize - 1;
    list.rebuild(&ctx, text_w);
    list.ensure_visible(inner.height as usize);
    let cursor = list.cursor;
    let gutter_on = Span::styled(
        "▎",
        Style::new().fg(if focused {
            palette.get(Role::Accent)
        } else {
            palette.get(Role::InactiveAccent)
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
            line = on_cursor(line, focused, palette);
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
/// cursor row selected. With no match, Enter sends the query as typed.
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
            Style::new().fg(app.palette.get(Role::Accent)),
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
                Style::new()
                    .bg(app.palette.get(Role::SelectionBackground))
                    .fg(app.palette.get(Role::SelectionText))
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

/// The cursor's row uses the palette's active or inactive selection background,
/// with each span's own color dropped so the text stays readable. Bold, italic
/// and underline survive. A row is rewritten because a span's color would win.
fn on_cursor<'a>(line: Line<'a>, focused: bool, palette: &Palette) -> Line<'a> {
    let keep = Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED;
    let bg = palette.get(if focused {
        Role::SelectionBackground
    } else {
        Role::InactiveSelectionBackground
    });
    let spans: Vec<Span> = line
        .spans
        .into_iter()
        .map(|s| {
            let m = s.style.add_modifier & keep;
            let style = Style::new()
                .bg(bg)
                .fg(palette.get(Role::SelectionText))
                .add_modifier(m);
            Span::styled(s.content, style)
        })
        .collect();
    Line::from(spans)
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

/// `/keys`: one action per row with the keys that reach it.
fn draw_keys(frame: &mut Frame, app: &App, inner: Rect) {
    let Some(View::Keys {
        cursor, capture, ..
    }) = app.stack.last()
    else {
        return;
    };
    let visible = inner.height.saturating_sub(3) as usize;
    let first = cursor
        .saturating_sub(visible / 2)
        .min(DEFAULTS.len().saturating_sub(visible));
    let mut lines = vec![Line::from(Span::styled(
        " action                                     keys",
        Style::new().add_modifier(Modifier::DIM),
    ))];
    for (index, (action, _)) in DEFAULTS.iter().enumerate().skip(first).take(visible) {
        let selected = index == *cursor;
        let marker = if selected { "›" } else { " " };
        let label_style = if selected {
            Style::new()
                .fg(app.palette.get(Role::Accent))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let bound = app.keymap.text(*action);
        let keys_style = if selected && capture.is_some() {
            Style::new()
                .fg(app.palette.get(Role::Status))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(app.palette.get(Role::Code))
        };
        let shown = if selected && capture.is_some() {
            "press a key…".to_string()
        } else {
            bound
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} {:<40}", action.label()), label_style),
            Span::styled(shown, keys_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " j/k action · e bind · A add · d reset action · D reset all · Enter save · Esc cancel",
        Style::new().add_modifier(Modifier::DIM),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_color_palette(frame: &mut Frame, app: &App, inner: Rect) {
    let Some(View::ColorPalette { cursor, .. }) = app.stack.last() else {
        return;
    };
    let visible = inner.height.saturating_sub(3) as usize;
    let first = cursor
        .saturating_sub(visible / 2)
        .min(ROLES.len().saturating_sub(visible));
    let mut lines = vec![Line::from(Span::styled(
        " semantic role                 preview   color",
        Style::new().add_modifier(Modifier::DIM),
    ))];
    for (index, role) in ROLES.iter().enumerate().skip(first).take(visible) {
        let selected = index == *cursor;
        let marker = if selected { "›" } else { " " };
        let label_style = if selected {
            Style::new()
                .fg(app.palette.get(Role::Accent))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let sample_style = match role {
            Role::SelectionText => Style::new()
                .fg(app.palette.get(*role))
                .bg(app.palette.get(Role::SelectionBackground)),
            Role::SelectionBackground => Style::new()
                .fg(app.palette.get(Role::SelectionText))
                .bg(app.palette.get(*role)),
            Role::InactiveSelectionBackground => Style::new()
                .fg(app.palette.get(Role::SelectionText))
                .bg(app.palette.get(*role)),
            _ => Style::new()
                .fg(app.palette.get(*role))
                .add_modifier(Modifier::BOLD),
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} {:<28}", role.label()), label_style),
            Span::styled(" sample ", sample_style),
            Span::raw("  "),
            Span::styled(
                app.palette.color_name(*role),
                Style::new().fg(app.palette.get(*role)),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " j/k role · h/l color · d reset role · D reset all · Enter save · Esc cancel",
        Style::new().add_modifier(Modifier::DIM),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
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

/// The commands, cache operations or conversation names the open command
/// line can still become, above the prompt.
fn draw_suggestions(frame: &mut Frame, app: &App, area: Rect) {
    let Mode::Prompt {
        kind: PromptKind::Command,
        buf,
        ..
    } = &app.mode
    else {
        return;
    };
    let found = complete::complete(&buf.text, &app.conv_names());
    if found.items.is_empty() || area.height < 4 {
        return;
    }
    let room = (area.height as usize).saturating_sub(3).min(8);
    let more = found.items.len().saturating_sub(room);
    let shown = &found.items[..room.min(found.items.len())];
    let column = shown.iter().map(|i| i.text.width()).max().unwrap_or(0);
    let mut lines: Vec<Line> = shown
        .iter()
        .map(|i| {
            let mut spans = vec![Span::styled(
                i.text.clone(),
                Style::new().fg(app.palette.get(Role::Accent)),
            )];
            if !i.help.is_empty() {
                spans.push(Span::styled(
                    format!("{} {}", " ".repeat(column - i.text.width()), i.help),
                    Style::new().add_modifier(Modifier::DIM),
                ));
            }
            Line::from(spans)
        })
        .collect();
    if more > 0 {
        lines.push(Line::styled(
            format!("… {more} more"),
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    let widest = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let w = (widest + 2).min(area.width);
    let h = lines.len() as u16 + 2;
    let rect = Rect {
        x: area.x,
        y: area.bottom().saturating_sub(h),
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(" Tab ")
                    .border_style(border(true, &app.palette)),
            )
            .style(background_style(&app.palette)),
        rect,
    );
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    // One line: a prompt when one is open; else the clock, a running job,
    // the last status, and the selected message's permalink. H lists the keys.
    if let Mode::Prompt { kind, buf, .. } = &app.mode {
        let label = match kind {
            PromptKind::Command => "",
            PromptKind::PaletteColor => "color (a name, or #rrggbb)",
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
            Style::new().fg(app.palette.get(Role::Accent)),
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
            Style::new().fg(app.palette.get(Role::Status)),
        ));
    }
    if !app.status.is_empty() {
        spans.push(Span::styled(
            format!("{}  ", app.status),
            Style::new().fg(app.palette.get(Role::Status)),
        ));
    }
    if let Some(m) = app.selected() {
        spans.push(Span::styled(
            m.permalink(&app.corpus.workspace_url),
            Style::new().fg(app.palette.get(Role::Link)),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The guide's rows, in order: an action shows the keys bound to it now, a
/// literal row the keys that view holds fixed.
enum HelpRow {
    Bound(Action, &'static str),
    Fixed(&'static str, &'static str),
}

const HELP: &[HelpRow] = &[
    HelpRow::Bound(
        Action::Down,
        "move; at the top of the channel, moving up loads older messages",
    ),
    HelpRow::Bound(Action::Up, "move up"),
    HelpRow::Bound(Action::HalfPageDown, "half a page down"),
    HelpRow::Bound(Action::HalfPageUp, "half a page up"),
    HelpRow::Bound(Action::PageDown, "a full page down"),
    HelpRow::Bound(Action::PageUp, "a full page up"),
    HelpRow::Bound(Action::First, "oldest message of the channel, top of the list"),
    HelpRow::Bound(Action::Last, "newest message of the channel, end of the list"),
    HelpRow::Bound(Action::OtherPane, "the other pane"),
    HelpRow::Bound(
        Action::Open,
        "open: the conversation, the selected message's thread, and inside a thread its raw JSON",
    ),
    HelpRow::Bound(
        Action::Back,
        "back: close the thread, search or raw view; then the pane",
    ),
    HelpRow::Bound(
        Action::Close,
        "in the list, drop the filter and then close the conversation: the home view",
    ),
    HelpRow::Bound(
        Action::Command,
        "a command, Tab completes it and its argument: keys rebinds what the keys in this guide do; colorpalette [name] edits UI colors, from the vintage or default palette when named (h/l cycles, e types a name, #rrggbb or terminal, d and D reset, Enter saves); find|search TEXT filters the list or searches the open conversation; leave, mute|unmute and cache start|stop|wipe take an optional #name; cache highlight on|off colors the cached conversations",
    ),
    HelpRow::Bound(Action::Keys, "rebind these keys (also /keys)"),
    HelpRow::Bound(
        Action::ShowInChannel,
        "from a search hit or a thread: show the message in the channel",
    ),
    HelpRow::Bound(Action::GoToDate, "go to a date (YYYY-MM-DD)"),
    HelpRow::Bound(Action::MyThreads, "threads you took part in, newest reply first"),
    HelpRow::Bound(
        Action::Images,
        "the selected message's images, full pane; j/k between them",
    ),
    HelpRow::Bound(Action::InlineImages, "inline image thumbnails on/off"),
    HelpRow::Bound(Action::UnreadsFirst, "unread conversations on top on/off"),
    HelpRow::Bound(
        Action::Compose,
        "write a message: to the open conversation, into the open thread, or into the selected hit's thread; Enter sends, Esc keeps the draft",
    ),
    HelpRow::Bound(
        Action::React,
        "react to the selected message: a picker opens; type to search, Up/Down or Ctrl-n/Ctrl-p to move, Enter reacts (the name as typed when nothing matches); your own reaction again removes it",
    ),
    HelpRow::Bound(
        Action::MarkRead,
        "mark read: the highlighted conversation, or the open one at its newest message",
    ),
    HelpRow::Bound(
        Action::MarkUnread,
        "mark unread from the message under the cursor (the highlighted conversation in the list)",
    ),
    HelpRow::Bound(Action::Help, "this guide"),
    HelpRow::Bound(Action::RawJson, "raw JSON of the selected message"),
    HelpRow::Bound(Action::Reload, "reload the conversation from the archive"),
    HelpRow::Bound(
        Action::Refresh,
        "refresh the conversation from Slack now (a slackdump resume)",
    ),
    HelpRow::Bound(Action::Archive, "archive a conversation not cached yet (URL or id)"),
    HelpRow::Bound(
        Action::Sort,
        "sort conversations: my activity (messages you wrote), name, recent, size",
    ),
    HelpRow::Bound(Action::Quit, "quit"),
    HelpRow::Fixed(
        "in a prompt",
        "Ctrl-a/e line start/end, Ctrl-b/f and Alt-b/f by char and word, Ctrl-k/u kill to line end/start, Ctrl-w and Alt-d kill a word, Ctrl-y yank, Ctrl-d delete under the cursor; Ctrl-j a newline in a message",
    ),
];

const HELP_NOTE: &str = "The unread part of a conversation starts at the highlighted day divider; the list marks unread conversations with ● and the mention count.";

fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    let palette = &app.palette;
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
    for row in HELP {
        let (keys, text) = match row {
            HelpRow::Bound(action, text) => (app.keymap.text(*action), *text),
            HelpRow::Fixed(keys, text) => (keys.to_string(), *text),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {keys:<28} "),
                Style::new().fg(palette.get(Role::Accent)),
            ),
            Span::raw(text),
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
        .border_style(Style::new().fg(palette.get(Role::Accent)));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .style(background_style(palette)),
        rect,
    );
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
        palette: &app.palette,
    };
    open.list.rebuild(&ctx, width);
    open.list
        .flat
        .iter()
        .map(|fl| render::line_text(&fl.line))
        .collect::<Vec<_>>()
        .join("\n")
}
