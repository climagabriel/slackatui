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
    if let Some(menu) = &app.pane_menu {
        let block = Block::bordered()
            .title(" conversations-pane ")
            .border_style(border(true, &app.palette));
        let inner = block.inner(main);
        frame.render_widget(Clear, main);
        frame.render_widget(block, main);
        let [help, list] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).areas(inner);
        frame.render_widget(Paragraph::new(
            "Outside Search: j/k or ↑/↓ select · h/l unset/set · Space cycle · Enter save · Esc cancel/home\nSearch row: type · Backspace erase · Ctrl-U clear · ↓/Tab/Enter leave\nMuted and Number: h/l previous/next. Individuals: h hide, l show; Space cycles category/show/hide.\nReset: select Reset, Space, then Enter."), help);
        let items: Vec<_> = menu.rows().into_iter().map(ListItem::new).collect();
        let mut state = ListState::default().with_selected(Some(menu.cursor));
        frame.render_stateful_widget(
            List::new(items).highlight_style(
                Style::new()
                    .fg(Color::Black)
                    .bg(app.palette.get(Role::Accent)),
            ),
            list,
            &mut state,
        );
    }
    if let Some((key, received)) = &app.last_key {
        if received.elapsed() < std::time::Duration::from_secs(3) && main.height >= 3 && main.width >= 4 {
            let width=(key.width().saturating_add(4)).min(main.width as usize) as u16;
            let popup=Rect::new(main.right()-width,main.bottom()-3,width,3);
            frame.render_widget(Clear,popup);
            frame.render_widget(Paragraph::new(format!(" {key} "))
                .style(background_style(&app.palette).fg(Color::Gray))
                .block(Block::bordered().border_type(ratatui::widgets::BorderType::Rounded)
                    .border_style(Style::new().fg(Color::DarkGray))),popup);
        }
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
            " {} · {} · {}by {} ",
            app.filtered.len(),
            app.pane_settings.number.heading(),
            if app.unreads_first {
                "unread first · "
            } else {
                ""
            },
            app.sort_label()
        )
    } else {
        format!(
            " {} · {} · '{}' ",
            app.filtered.len(),
            app.pane_settings.number.heading(),
            app.filter
        )
    };
    let block = Block::bordered()
        .title(app.palette.highlight_line(Line::from(title)))
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
            let hidden =
                app.pane_settings.number == crate::conversations_pane::NumberColumn::Hidden;
            let count = if hidden || (app.pane_settings.number == crate::conversations_pane::NumberColumn::Unread && !c.unread) {
                String::new()
            } else {
                app.pane_settings
                    .number
                    .value(c, app.sort == Sort::Mine)
                    .map(|count| if app.pane_settings.number == crate::conversations_pane::NumberColumn::Unread && count > 9 { "9+".into() } else { human_count(count) })
                    .unwrap_or_else(|| "—".into())
            };
            let gap = usize::from(!hidden);
            let room = width.saturating_sub(count.width() + gap);
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
            let pad = room.saturating_sub(clip(&name, room).width());
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
            let selected = app.top_section.is_none() && app.filtered.get(app.conv_cursor) == Some(&i);
            let mut name_line = Line::from(Span::styled(name, name_style));
            if selected { name_line = on_cursor(name_line, focused, &app.palette); }
            let mut line = clip_line(app.palette.highlight_line(name_line), room);
            let mut tail = Line::from(vec![Span::raw(" ".repeat(pad + gap)), Span::styled(count, dim)]);
            if selected { tail = on_cursor(tail, focused, &app.palette); }
            line.spans.extend(tail.spans);
            line
        })
        .collect();
    // The cursor's row is rewritten with the palette's selection colors rather
    // than styled through `highlight_style`: a span's own color would win.
    let items: Vec<ListItem> = items
        .into_iter()
        .enumerate()
        .map(|(k, line)| {
            let boundary = k > 0
                && app.starred.contains(&app.conv(app.filtered[k - 1]).id)
                && !app.starred.contains(&app.conv(app.filtered[k]).id);
            if boundary {
                ListItem::new(vec![Line::from(Span::styled("─".repeat(width), dim)), line])
            } else {
                ListItem::new(line)
            }
        })
        .collect();
    let mut items = items;
    for section in [crate::app::TopSection::Sent, crate::app::TopSection::Saved] {
        let mut line = Line::from(Span::styled(section.label(), Style::new().add_modifier(Modifier::BOLD)));
        if app.top_section == Some(section) { line = on_cursor(line, focused, &app.palette); }
        let mut lines = vec![line];
        if section == crate::app::TopSection::Sent { lines.push(Line::from(Span::styled("─".repeat(width), dim))); }
        items.insert(0, ListItem::new(lines));
    }
    let list = List::new(items).block(block);
    // The offset carries over from the last frame: rebuilt at zero, ratatui
    // would rescroll to the minimum that shows the cursor, pinning it to the
    // bottom row and moving the whole pane on every step upward.
    let mut state = ListState::default()
        .with_offset(app.conv_offset.min(app.filtered.len() + 1))
        .with_selected(Some(app.top_section.map(crate::app::TopSection::row).unwrap_or(if app.filtered.is_empty() { 0 } else { app.conv_cursor + 2 })));
    frame.render_stateful_widget(list, area, &mut state);
    app.conv_offset = state.offset();
}

fn muted_message_style(mut style: Style) -> Style {
    style.fg = Some(Color::Rgb(160, 160, 160));
    style.bg = Some(Color::Reset);
    style.underline_color = Some(Color::Rgb(160, 160, 160));
    style.sub_modifier |= Modifier::REVERSED;
    style.add_modifier.remove(Modifier::REVERSED);
    style
}

fn draw_msgs(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.channel_browser.as_ref().is_some_and(|browser| browser.visible) {
        let mut browser = app.channel_browser.take().expect("visible browser");
        browser.previews = app.picker.is_some();
        browser.draw(frame, area, &app.palette);
        for (thumbnail_area, file) in &browser.thumbnails {
            app.ensure_image(file, false);
            if let Some(protocol) = app.inline_protocol(&file.id, thumbnail_area.width, thumbnail_area.height) {
                frame.render_widget(Image::new(protocol).allow_clipping(true), *thumbnail_area);
            } else {
                let message = if matches!(app.images.get(&file.id), Some(ImageState::Failed(_))) {
                    "Image unavailable · l/Enter for details"
                } else { "Loading image…" };
                frame.render_widget(Paragraph::new(message), *thumbnail_area);
            }
        }
        if let Some(picture) = &mut browser.picture {
            let inner = Block::bordered().inner(area);
            let body = Rect { height: inner.height.saturating_sub(2), ..inner };
            draw_picture(frame, app, body, &picture.file, picture.zoom, &mut picture.shown, Some(&picture.permalink));
        }
        app.channel_browser = Some(browser);
        return;
    }

    let focused = app.focus == Focus::Msgs;
    let reading = !matches!(
        app.stack.last(),
        Some(
            View::Raw { .. }
                | View::Image { .. }
                | View::Reactions { .. }
                | View::Keys { .. }
                | View::ColorPalette { .. }
        )
    ) && app.active_list().is_some_and(|list| list.line_scroll);
    let label = if reading {
        format!("read lines · l: raw · {}", app.title())
    } else {
        app.title()
    };
    let title = clip_line(app.palette.highlight_line(Line::from(format!(" {label} "))), area.width.saturating_sub(2) as usize);
    let block = Block::bordered()
        .title(app.palette.highlight_line(Line::from(title)))
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
    if let Some(View::Image { .. }) = app.stack.last() {
        draw_image_view(frame, app, inner);
        return;
    }
    if let Some(View::Reactions { lines, scroll, .. }) = app.stack.last() {
        let shown: Vec<Line> = lines.iter().skip(*scroll).take(inner.height as usize)
            .map(|line| Line::raw(line.clone())).collect();
        frame.render_widget(Paragraph::new(shown), inner);
        return;
    }
    if let Some(View::Raw { browser, .. }) = app.stack.last_mut() {
        let height = inner.height.saturating_sub(2);
        let rows = browser.rows(inner.width as usize, height as usize, &app.palette);
        frame.render_widget(Paragraph::new(rows), Rect { height, ..inner });
        let help = vec![Line::raw(browser.label()),
            Line::raw("j/k: leaf/link · Enter: follow · h: back · PgUp/PgDn: scroll · Esc: home")];
        frame.render_widget(Paragraph::new(help), Rect { y: inner.y + height, height: inner.height - height, ..inner });
        return;
    }
    let image_font = app.image_font();
    // The read marker applies to the conversation's own timeline only.
    let last_read = match (
        app.stack.iter().rev().find(|v| {
            !matches!(
                v,
                View::Raw { .. } | View::Image { .. } | View::Reactions { .. }
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
    if open.is_none() && !stack.iter().any(|v| matches!(v, View::Saved { .. } | View::Sent { .. } | View::Search { .. })) {
        let hint = Line::from(Span::styled(
            "  select a conversation and press Enter",
            Style::new().add_modifier(Modifier::DIM),
        ));
        frame.render_widget(Paragraph::new(hint), inner);
        return;
    };
    let conv = open.as_ref().map(|open| &corpus.convs[open.conv]);
    let conv_archive = conv.and_then(|conv| corpus.conv_archive(conv));
    let (list, archive) = match stack
        .iter_mut()
        .rev()
        .find(|v| !matches!(v, View::Raw { .. }))
    {
        Some(View::Thread { list, live, .. }) => (list, live.as_deref().or(conv_archive)),
        Some(View::Saved { list }) | Some(View::Sent { list, .. }) => (list, None),
        Some(View::Search { list, .. }) | Some(View::Threads { list }) => (list, conv_archive),
        _ => { let Some(open) = open.as_mut() else { return }; (&mut open.list, conv_archive) },
    };
    let ctx = Ctx {
        archive,
        corpus,
        tz: *tz,
        image_font,
        last_read,
        palette,
    };
    let text_w = inner.width as usize - 2;
    if inner.height < 6 {
        frame.render_widget(Paragraph::new("Enlarge pane to show a whole message"), inner);
        return;
    }
    list.set_unread_count(last_read.and(conv.and_then(|conv| conv.unread_count)));
    list.rebuild_for_pane(&ctx, text_w, inner.height as usize);
    let end = if list.line_scroll {
        list.ensure_visible(inner.height as usize);
        (list.scroll + inner.height as usize).min(list.flat.len())
    } else {
        list.whole_message_viewport(inner.height as usize)
    };
    let cursor = list.cursor;
    let first = list.first.get(cursor).copied();
    let last = list.last.get(cursor).copied();
    let scroll = list.scroll;
    let outline = Style::reset().fg(Color::Rgb(112, 112, 112)).bg(palette.get(Role::Background));
    let mut shown: Vec<Line> = Vec::with_capacity(inner.height as usize);
    for (i, fl) in list.flat.iter().enumerate().skip(list.scroll).take(end - list.scroll) {
        let selected = fl.msg == Some(cursor);
        if selected && (first == Some(i) || last == Some(i)) {
            let (left, right) = if first == Some(i) { ("╭", "╮") } else { ("╰", "╯") };
            shown.push(Line::styled(format!("{left}{}{right}", "─".repeat(text_w)), outline));
            continue;
        }
        let mut spans = vec![Span::styled(if selected { "│" } else { " " }, outline)];
        spans.extend(clip_line(fl.line.clone(), text_w).spans);
        let mut line = Line::from(spans).style(fl.line.style);
        if fl.msg.is_some() && !selected {
            frame.buffer_mut().set_style(
                Rect::new(inner.x, inner.y + (i - scroll) as u16, inner.width, 1),
                muted_message_style(Style::default()),
            );
            // Apply after word highlighting, so no message color leaks through.
            line.style = muted_message_style(line.style);
            for span in &mut line.spans { span.style = muted_message_style(span.style); }
        }
        shown.push(line);
    }
    frame.render_widget(Paragraph::new(Text::from(shown)), inner);
    // Inline images: one rect per slot whose first row is on screen.
    let slots: Vec<(u16, crate::render::ImageSlot, bool)> = list
        .flat
        .iter()
        .enumerate()
        .skip(list.scroll)
        .take(end - list.scroll)
        .filter_map(|(i, fl)| fl.image.clone().map(|s| ((i - list.scroll) as u16, s, fl.msg == Some(cursor))))
        .collect();
    for (row, slot, selected) in slots {
        let key = match &slot.source {
            render::ImageSource::File(file) => {
                app.ensure_image(file, false);
                Some(file.id.clone())
            }
        };
        let x = inner.x + 3;
        let y = inner.y + row;
        let width = slot.cols.min(inner.width.saturating_sub(4));
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
        let note = match key.as_ref().and_then(|key| app.images.get(key)) {
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
                if let Some(proto) = app.message_protocol(key.as_deref().expect("ready image key"), slot.cols, slot.rows, !selected) {
                    frame.render_widget(Image::new(proto).allow_clipping(true), area);
                }
            }
        }
    }
    if let (Some(first), Some(last)) = (first, last) {
        for row in 0..inner.height {
            let index = scroll + row as usize;
            if index < end && index > first && index < last {
                for x in [inner.x, inner.right() - 1] {
                    frame.buffer_mut()[(x, inner.y + row)].set_symbol("│").set_style(outline);
                }
            }
        }
    }
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
        " j/k action · e bind · A add · d reset action · D reset all · Enter save · Esc cancel/home",
        Style::new().add_modifier(Modifier::DIM),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_color_palette(frame: &mut Frame, app: &App, inner: Rect) {
    let Some(View::ColorPalette { cursor, highlights, .. }) = app.stack.last() else {
        return;
    };
    if let Some(menu) = highlights { menu.draw(frame, inner, &app.palette); return; }
    let visible = inner.height.saturating_sub(3) as usize;
    let first = cursor
        .saturating_sub(visible / 2)
        .min((ROLES.len() + 1).saturating_sub(visible));
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
    if first + visible > ROLES.len() {
        lines.push(Line::from(Span::styled(
            format!(" {} Word highlights →", if *cursor == ROLES.len() { "›" } else { " " }),
            Style::new().fg(app.palette.get(Role::Accent)),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " j/k select · h/l color · W word highlights · e type color · d/D reset · Enter save · Esc cancel/home",
        Style::new().add_modifier(Modifier::DIM),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_image_view(frame: &mut Frame, app: &mut App, inner: Rect) {
    let Some(View::Image { files, index, zoom, shown }) = app.stack.last_mut() else { return; };
    let file = files[*index].clone();
    let zoom = *zoom;
    let mut protocol = shown.take();
    draw_picture(frame, app, inner, &file, zoom, &mut protocol, None);
    if let Some(View::Image { shown, .. }) = app.stack.last_mut() { *shown = protocol; }
}

fn draw_picture(
    frame: &mut Frame,
    app: &mut App,
    inner: Rect,
    file: &crate::archive::FileInfo,
    zoom: u16,
    shown: &mut Option<(String, ratatui_image::protocol::StatefulProtocol)>,
    permalink: Option<&str>,
) {
    if inner.is_empty() { return; }
    let fallback = |message: &str| match permalink.filter(|link| !link.is_empty()) {
        Some(link) => format!("{message}\n{link}"),
        None => message.to_string(),
    };
    if app.picker.is_none() {
        frame.render_widget(Paragraph::new(fallback("Image display is disabled; restart without --no-images.")), inner);
        return;
    }
    app.ensure_image(file, true);
    let key = format!("{}:full", file.id);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let note = match app.images.get(&key) {
        Some(ImageState::Ready(_)) => None,
        Some(ImageState::Failed(e)) => Some(format!("  image failed: {e}")),
        _ => Some("  loading the original from Slack".to_string()),
    };
    if let Some(text) = note {
        frame.render_widget(Paragraph::new(Span::styled(fallback(&text), dim)), inner);
        return;
    }
    let render_key = format!("{key}:{zoom}:{}:{}", inner.width, inner.height);
    // Rebuild for zoom or terminal-size changes as well as image changes.
    let stale = shown.as_ref().is_none_or(|(key, _)| key != &render_key);
    if stale {
        let img = match app.images.get(&key) {
            Some(ImageState::Ready(img)) => img.clone(),
            _ => return,
        };
        let Some(picker) = app.picker.as_ref() else {
            return;
        };
        let font = picker.font_size();
        let img = zoom_image(
            &img,
            u32::from(inner.width) * u32::from(font.width),
            u32::from(inner.height) * u32::from(font.height),
            zoom,
        );
        let fresh = picker.new_resize_protocol(img);
        *shown = Some((render_key, fresh));
    }
    if let Some((_, proto)) = shown {
        frame.render_stateful_widget(StatefulImage::new(), inner, proto);
    }
}

/// Center-crop before resizing, so large zoom factors do not allocate an
/// enormous intermediate bitmap. 100% is fit-to-pane, not native pixels.
fn zoom_image(
    img: &image::DynamicImage,
    width: u32,
    height: u32,
    zoom: u16,
) -> image::DynamicImage {
    let width = width.clamp(1, 4096);
    let height = height.clamp(1, 4096);
    let scale = (width as f64 / img.width().max(1) as f64)
        .min(height as f64 / img.height().max(1) as f64)
        .min(1.0)
        * f64::from(zoom)
        / 100.0;
    let crop_w = ((width as f64 / scale).round() as u32).clamp(1, img.width().max(1));
    let crop_h = ((height as f64 / scale).round() as u32).clamp(1, img.height().max(1));
    let cropped = img.crop_imm(
        img.width().saturating_sub(crop_w) / 2,
        img.height().saturating_sub(crop_h) / 2,
        crop_w,
        crop_h,
    );
    cropped.resize_exact(
        ((crop_w as f64 * scale).round() as u32).clamp(1, width),
        ((crop_h as f64 * scale).round() as u32).clamp(1, height),
        image::imageops::FilterType::Triangle,
    )
}

#[cfg(test)]
mod zoom_tests {
    #[test]
    fn zoom_scales_and_center_crops_within_viewport() {
        use image::GenericImageView;
        let img = image::DynamicImage::ImageRgba8(image::ImageBuffer::from_fn(400, 200, |x, _| {
            image::Rgba([if (150..250).contains(&x) { 255 } else { 0 }, 0, 0, 255])
        }));
        assert_eq!(
            super::zoom_image(&img, 200, 100, 100).dimensions(),
            (200, 100)
        );
        assert_eq!(
            super::zoom_image(&img, 200, 100, 50).dimensions(),
            (100, 50)
        );
        let close = super::zoom_image(&img, 200, 100, 800);
        assert_eq!(close.dimensions(), (200, 100));
        assert_eq!(close.get_pixel(0, 0)[0], 255);
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
    let found = complete::with_authors(&buf.text, &app.conv_names(), &app.corpus.author_names());
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
        let compose_label = match (app.compose.as_ref(), app.attachment.as_deref()) {
            (Some(c), Some(file)) => format!(
                "{} with {}",
                c.label,
                file.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("a file")
            ),
            (Some(c), None) => match app.attach_note.as_deref() {
                Some(why) => format!("{} · {why}", c.label),
                None => c.label.clone(),
            },
            (None, _) => "message".to_string(),
        };
        let label = match kind {
            PromptKind::Command => "",
            PromptKind::PaletteColor => "color (a name, or #rrggbb)",
            PromptKind::Date => "go to date (YYYY-MM-DD)",
            PromptKind::Archive => "archive a conversation from Slack, last 90 days (URL or id)",
            PromptKind::Compose => &compose_label,
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
    // The version keeps the right corner; the rest of the line shortens.
    let mut area = area;
    if app.show_version {
        let label = format!(" {} ", crate::version());
        let width = label.width() as u16;
        if area.width > width {
            let [rest, corner] =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
            frame.render_widget(Paragraph::new(Span::styled(label, dim)), corner);
            area = rest;
        }
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
        "open conversation; l/Right opens a known thread, expands a collapsed message, or opens raw JSON; Enter opens the thread, or raw JSON inside a thread",
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
        "a command, Tab completes it and its argument: keys rebinds what the keys in this guide do; upload [path] sends a file with the next message, the clipboard's image when no path is given; colorpalette [name] edits UI colors, from the vintage or default palette when named (h/l cycles, e types a name, #rrggbb or terminal, d and D reset, Enter saves); find|search TEXT filters the list or searches the open conversation; leave, mute|unmute and cache start|stop|wipe take an optional #name; cache highlight on|off colors the cached conversations; version shows the version in the corner",
    ),
    HelpRow::Bound(Action::Keys, "rebind these keys (also /keys)"),
    HelpRow::Bound(
        Action::ShowInChannel,
        "from a search hit or a thread: show the message in the channel",
    ),
    HelpRow::Bound(Action::GoToDate, "go to a date (YYYY-MM-DD)"),
    HelpRow::Bound(Action::ChannelTabs, "channel tabs: canvases, files and bookmarks"),
    HelpRow::Bound(Action::MyThreads, "threads you took part in, newest reply first"),
    HelpRow::Bound(
        Action::Images,
        "the selected message's images, full pane; j/k between them",
    ),
    HelpRow::Bound(Action::InlineImages, "inline image thumbnails on/off"),
    HelpRow::Bound(Action::UnreadsFirst, "unread conversations on top on/off"),
    HelpRow::Bound(
        Action::Compose,
        "write a message: to the open conversation, into the open thread, or into the selected hit's thread; Ctrl-v attaches the clipboard's image, Ctrl-j and Alt-Enter break the line, Enter sends, Esc keeps the draft and returns home",
    ),
    HelpRow::Bound(
        Action::Delete,
        "delete the selected message, which Slack allows only for your own; the same key again confirms, any other cancels",
    ),
    HelpRow::Bound(
        Action::React,
        "view reactions on the selected message: cached counts and known users; partial lists show missing counts; j/k scroll, h back, Esc home",
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
    HelpRow::Bound(Action::Save, "save selected message to Slack Later; /save"),
    HelpRow::Bound(Action::Unsave, "remove selected message from Slack Later; /unsave"),
    HelpRow::Fixed("in raw JSON", "j/k select leaf values or Slack links; Enter follows the selected link; h returns; PgUp/PgDn scroll long values; g/G select first/last value or link"),
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
        "Ctrl-a/e line start/end, Ctrl-b/f and Alt-b/f by char and word, Ctrl-k/u kill to line end/start, Ctrl-w and Alt-d kill a word, Ctrl-y yank, Ctrl-d delete under the cursor; Ctrl-j or Alt-Enter a newline in a message",
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

fn clip_line(mut line: Line<'static>, width: usize) -> Line<'static> {
    let text = line.to_string();
    let clipped = clip(&text, width);
    if clipped == text { return line; }
    let mut remaining = clipped.strip_suffix('…').unwrap_or(&clipped).len();
    let mut spans = Vec::new();
    for span in line.spans {
        if remaining == 0 { break; }
        let count = remaining.min(span.content.len());
        spans.push(Span::styled(span.content[..count].to_string(), span.style));
        remaining -= count;
    }
    let style = spans.last().map(|span| span.style).unwrap_or_default();
    spans.push(Span::styled("…", style));
    line.spans = spans;
    line
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
        archive: app.corpus.conv_archive(conv),
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

#[cfg(test)]
mod highlight_tests {
    use super::*;
    #[test]
    fn a_clipped_match_keeps_its_color_and_selection_background() {
        let palette = Palette::default();
        let line = on_cursor(Line::from("#nginx"), true, &palette);
        let line = clip_line(palette.highlight_line(line), 4);
        assert_eq!(line.to_string(), "#ng…");
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(4, 1)).unwrap();
        terminal.draw(|frame| frame.render_widget(Paragraph::new(line.clone()), frame.area())).unwrap();
        assert_eq!(terminal.backend().buffer()[(1,0)].fg, Color::Green);
        assert_eq!(terminal.backend().buffer()[(1,0)].bg, palette.get(Role::SelectionBackground));
    }
}

#[cfg(test)]
mod message_focus_tests {
    use super::*;
    use crate::app::{MsgList, tests::mute_test_app};
    use crate::archive::Msg;
    use serde_json::json;

    #[test]
    fn opening_at_the_end_backfills_whole_collapsed_messages() {
        let mut app = mute_test_app();
        app.open_conv(0);
        let body = (0..50).map(|n| format!("body {n}")).collect::<Vec<_>>().join("\n");
        let messages = (1..=20).map(|second| Msg::from_api("C1".into(), json!({
            "ts":format!("{second}.000000"), "user":"U1", "text":if second == 20 { "final" } else { &body }
        })).unwrap()).collect();
        app.open.as_mut().unwrap().list = MsgList::new(messages, false);
        app.open.as_mut().unwrap().list.cursor = 19;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 40)).unwrap();
        terminal.draw(|frame| draw_msgs(frame, &mut app, frame.area())).unwrap();
        let list = app.active_list().unwrap();
        let visible = list.first.iter().filter(|&&first| first >= list.scroll && first < list.scroll + 38).count();
        assert_eq!(visible, 6);
        assert!(list.flat.len() - list.scroll >= 34);
        assert_eq!(list.cursor, 19);
        let selected_last = list.last[19] - list.scroll + 1;
        assert_eq!(terminal.backend().buffer()[(78, selected_last as u16)].symbol(), "╯");
        // There is not enough spare space for another whole preceding message.
        let previous = list.flat[list.scroll - 1].msg.unwrap();
        assert!(list.flat.len() - list.first[previous] > 38);
    }

    #[test]
    fn unread_divider_updates_count_without_message_changes() {
        let mut app = mute_test_app();
        app.open_conv(0);
        let messages = [1, 2, 86401].into_iter().map(|second| Msg::from_api("C1".into(), json!({
            "ts":format!("{second}.000000"), "user":"U1", "text":"body"
        })).unwrap()).collect();
        app.open.as_mut().unwrap().list = MsgList::new(messages, false);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 25)).unwrap();
        for marker in [1_000_000, 2_000_000] {
            app.corpus.convs[0].last_read = marker;
            app.active_list_mut().unwrap().mark_dirty();
            for (count, label) in [(None, "new (—)"), (Some(3), "new (3)"), (Some(10), "new (9+)")] {
                app.corpus.convs[0].unread_count = count;
                terminal.draw(|frame| draw_msgs(frame, &mut app, frame.area())).unwrap();
                let buffer = terminal.backend().buffer();
                let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
                assert!(text.contains(label));
                let list = app.active_list().unwrap();
                let divider = list.flat.iter().position(|line| line.msg.is_none() && line.line.to_string().contains(label)).unwrap();
                assert_eq!(buffer[(3, (divider - list.scroll + 1) as u16)].fg, app.palette.get(Role::Unread));
            }
        }
    }

    #[test]
    fn whole_messages_collapse_resize_and_preserve_unread_divider() {
        let mut app = mute_test_app();
        app.open_conv(0);
        app.corpus.convs[0].last_read = 1_000_000;
        let messages = (1..=8).map(|second| Msg::from_api("C1".into(), json!({
            "ts": format!("{second}.000000"), "user": "U1",
            "text": if second == 2 { (0..20).map(|n| format!("body {n}")).collect::<Vec<_>>().join("\n") } else { "short".into() }
        })).unwrap()).collect();
        app.open.as_mut().unwrap().list = MsgList::new(messages, false);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        for height in [20, 8, 12, 50, 20] {
            terminal.backend_mut().resize(80, height);
            terminal.resize(Rect::new(0, 0, 80, height)).unwrap();
            for cursor in [0, 1, 2, 7, 6, 0] {
                app.focus = Focus::Msgs;
                let current = app.active_list().unwrap().cursor;
                let direction = if cursor > current { 'j' } else { 'k' };
                for _ in 0..cursor.abs_diff(current) {
                    app.on_key(crate::event::KeyEvent::new(crate::event::KeyCode::Char(direction), crate::event::KeyModifiers::NONE));
                }
                terminal.draw(|frame| draw_msgs(frame, &mut app, frame.area())).unwrap();
                let list = app.active_list().unwrap();
                let buffer = terminal.backend().buffer();
                let first = list.first[cursor] - list.scroll + 1;
                let last = list.last[cursor] - list.scroll + 1;
                assert_eq!(buffer[(1, first as u16)].symbol(), "╭");
                assert_eq!(buffer[(78, last as u16)].symbol(), "╯");
                let end = list.scroll + height as usize - 2;
                for index in 0..list.msgs.len() {
                    if list.first[index] < list.scroll { assert!(list.last[index] < list.scroll); }
                    if list.first[index] >= list.scroll && list.last[index] >= end {
                        let row = list.first[index] - list.scroll + 1;
                        if row + 1 < height as usize - 1 {
                            assert!((2..78).all(|column| buffer[(column, row as u16 + 1)].symbol() == " "));
                        }
                    }
                }
                let preview: Vec<_> = list.flat[list.first[1]..=list.last[1]].iter().map(|line| line.line.to_string()).collect();
                if height < 50 {
                    assert_eq!(preview.len(), 6);
                    assert!(preview[2].contains("body 0"));
                    assert!(preview[3].contains("body 1"));
                    assert!(preview[4].contains("(18 more lines)"));
                } else { assert!(preview.len() > 6); }
                for (row, line) in list.flat.iter().enumerate().skip(list.scroll).take(height as usize - 2) {
                    if line.msg.is_none() && line.line.to_string().contains("new") {
                        assert_eq!(buffer[(3, (row - list.scroll + 1) as u16)].fg, app.palette.get(Role::Unread));
                    }
                }
            }
        }
    }

    #[test]
    fn long_wide_header_cannot_cover_border_or_color_nonselected_background() {
        let mut app = mute_test_app();
        app.open_conv(0);
        app.palette.set(Role::Background, Color::Blue);
        app.palette.set(Role::OtherUsername, Color::Red);
        let messages = [1, 2].into_iter().map(|second| Msg::from_api("C1".into(), json!({
            "ts": format!("{second}.000000"), "user": "界界界界界界界界界", "text": "nginx"
        })).unwrap()).collect();
        app.open.as_mut().unwrap().list = MsgList::new(messages, false);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, 20)).unwrap();
        terminal.draw(|frame| {
            frame.render_widget(Block::default().style(Style::new().bg(Color::Blue)), frame.area());
            draw_msgs(frame, &mut app, frame.area());
        }).unwrap();
        let list = app.active_list().unwrap();
        let first = list.first[0] - list.scroll + 1;
        let header = first + 1;
        let buffer = terminal.backend().buffer();
        let cell = &buffer[(32, header as u16)];
        assert_eq!(cell.symbol(), "│");
        assert_eq!(cell.modifier, Modifier::empty());
        assert_eq!(cell.fg, Color::Rgb(112,112,112));
        assert_eq!(cell.bg, Color::Blue);
        assert!(buffer[(31, header as u16)].symbol().width() <= 1);
        let other_header = list.first[1] - list.scroll + 2;
        for column in 1..33 {
            assert_ne!(buffer[(column, other_header as u16)].bg, Color::Blue);
        }
    }

    #[test]
    fn focus_box_and_grayscale_follow_cursor_for_text_and_files() {
        let mut app = mute_test_app();
        app.open_conv(0);
        app.picker = Some(ratatui_image::picker::Picker::halfblocks());
        app.inline_images = true;
        let pixels = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(128, 128, image::Rgba([255, 0, 0, 255])));
        app.images.insert("F1".into(), ImageState::Ready(pixels));
        let messages: Vec<Msg> = [1, 2].into_iter().map(|second| Msg::from_api("C1".into(), json!({
            "ts": format!("{second}.000000"), "user": "U1", "text": "nginx colored text",
            "files": [{"id":"F1", "name":"test.png", "mimetype":"image/png", "filetype":"png", "original_w":64, "original_h":32}],
            "reactions": [{"name":"focus-test", "count":3}]
        })).unwrap()).collect();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 40)).unwrap();
        for thread in [false, true] {
            if thread {
                app.stack.push(View::Thread { root: messages[0].id, list: MsgList::new(messages.clone(), true), live: None, place: None });
            } else {
                app.open.as_mut().unwrap().list = MsgList::new(messages.clone(), false);
            }
            for selected in [0, 1, 0] {
                app.active_list_mut().unwrap().cursor = selected;
                terminal.draw(|frame| draw_msgs(frame, &mut app, frame.area())).unwrap();
                let list = app.active_list().unwrap();
                let buffer = terminal.backend().buffer();
                for index in 0..2 {
                    let first = list.first[index] - list.scroll + 1;
                    let last = list.last[index] - list.scroll + 1;
                    let rows = first..=last;
                    let mut red = 0;
                    let mut gray_pixels = 0;
                    for row in rows {
                        for x in 1..79 {
                            let cell = &buffer[(x, row as u16)];
                            if cell.fg == Color::Rgb(255, 0, 0) || cell.bg == Color::Rgb(255, 0, 0) { red += 1; }
                            if matches!(cell.bg, Color::Rgb(r,g,b) if r == g && g == b && r > 0) { gray_pixels += 1; }
                            if index != selected {
                                assert!(!matches!(cell.fg, Color::Rgb(r,g,b) if r != g || g != b), "nonselected color: {cell:?}");
                            }
                        }
                    }
                    if index == selected {
                        assert_eq!(buffer[(1, first as u16)].symbol(), "╭");
                        assert_eq!(buffer[(78, first as u16)].symbol(), "╮");
                        assert_eq!(buffer[(1, last as u16)].symbol(), "╰");
                        assert_eq!(buffer[(78, last as u16)].symbol(), "╯");
                        assert!(red > 0);
                    } else {
                        assert_eq!(red, 0);
                        assert!(gray_pixels > 0);
                        assert_eq!(buffer[(1, first as u16)].symbol(), " ");
                    }
                    let text_row = first + 2;
                    assert_eq!(buffer[(4, text_row as u16)].fg, if index == selected { Color::Green } else { Color::Rgb(160, 160, 160) });
                }
            }
        }
        // A tiny pane shows a size hint rather than a partial message.
        terminal.backend_mut().resize(24, 6);
        terminal.resize(Rect::new(0, 0, 24, 6)).unwrap();
        terminal.draw(|frame| draw_msgs(frame, &mut app, frame.area())).unwrap();
        assert_eq!(terminal.backend().buffer()[(1, 1)].symbol(), "E");
        // Originals remain colored after encoding both variants.
        let ImageState::Ready(original) = &app.images["F1"] else { panic!() };
        assert_eq!(original.to_rgba8().get_pixel(0,0).0, [255,0,0,255]);
    }
}
