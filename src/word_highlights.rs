//! Palette-owned word rules; edits remain a draft until the parent menu saves.
use crate::{
    edit::Editor,
    palette::{color_name, Highlight, Palette, Role, COLORS},
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

#[derive(Default)]
pub struct Menu {
    cursor: usize,
    editing: Option<Editor>,
    color: Option<usize>,
}

impl Menu {
    pub fn editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Return true to return to the palette menu, retaining the draft.
    pub fn key(&mut self, key: KeyEvent, palette: &mut Palette) -> bool {
        if let Some(editor) = &mut self.editing {
            match key.code {
                KeyCode::Esc => self.editing = None,
                KeyCode::Enter => {
                    let word = editor.text.trim().to_string();
                    if !word.is_empty() {
                        if let Some(rule) = palette.highlights.get_mut(self.cursor) {
                            rule.word = word;
                        } else {
                            palette.highlights.push(Highlight {
                                word,
                                color: ratatui::style::Color::Green,
                            });
                        }
                        self.editing = None;
                    }
                }
                _ => {
                    editor.key(key, false);
                }
            }
            return false;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        if let Some(cursor) = &mut self.color {
            match key.code {
                KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => self.color = None,
                KeyCode::Char('j') | KeyCode::Down => *cursor = (*cursor + 1).min(COLORS.len() - 1),
                KeyCode::Char('k') | KeyCode::Up => *cursor = cursor.saturating_sub(1),
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                    if let Some(rule) = palette.highlights.get_mut(self.cursor) {
                        rule.color = COLORS[*cursor].1;
                    }
                    self.color = None;
                }
                _ => {}
            }
            return false;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => return true,
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(palette.highlights.len())
            }
            KeyCode::Char('k') | KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => self.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.cursor = palette.highlights.len(),
            KeyCode::Char('a') => {
                self.cursor = palette.highlights.len();
                self.editing = Some(Editor::default());
            }
            KeyCode::Char('i') => {
                self.editing = Some(Editor::with(
                    palette
                        .highlights
                        .get(self.cursor)
                        .map(|r| r.word.clone())
                        .unwrap_or_default(),
                ))
            }
            KeyCode::Char('d') => {
                if self.cursor < palette.highlights.len() {
                    palette.highlights.remove(self.cursor);
                }
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                if let Some(rule) = palette.highlights.get(self.cursor) {
                    self.color = Some(
                        COLORS
                            .iter()
                            .position(|(_, color)| *color == rule.color)
                            .unwrap_or(0),
                    );
                } else {
                    self.editing = Some(Editor::default());
                }
            }
            _ => {}
        }
        false
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, palette: &Palette) {
        let heading = Style::new().fg(palette.get(Role::Accent));
        if let Some(editor) = &self.editing {
            let (before, after) = editor.text.split_at(editor.cursor);
            let mut characters = after.chars();
            let cursor = characters.next().unwrap_or(' ');
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled("Word highlights · edit word", heading),
                    Line::from(vec![
                        Span::raw(before.to_string()),
                        Span::styled(
                            cursor.to_string(),
                            Style::new().add_modifier(Modifier::REVERSED),
                        ),
                        Span::raw(characters.as_str().to_string()),
                    ]),
                    Line::from("Enter accept · Esc cancel editing"),
                ]),
                area,
            );
            return;
        }
        let (title, cursor, rows, footer) = if let Some(cursor) = self.color {
            (
                format!(
                    "Word highlights · color for {}",
                    palette.highlights[self.cursor].word
                ),
                cursor,
                COLORS
                    .iter()
                    .map(|(name, color)| (name.to_string(), *color))
                    .collect::<Vec<_>>(),
                "j/k select · l/Enter apply color · h/Esc back",
            )
        } else {
            let mut rows: Vec<_> = palette
                .highlights
                .iter()
                .map(|r| (format!("{}  ·  {}", r.word, color_name(r.color)), r.color))
                .collect();
            rows.push(("Add word…".into(), palette.get(Role::Accent)));
            ("Word highlights · case insensitive · first rule wins overlaps".into(), self.cursor, rows,
             "j/k select · l/Enter color · a add · i edit · d remove · h/Esc back; Enter in palette saves")
        };
        let visible = area.height.saturating_sub(2) as usize;
        let first = cursor
            .saturating_sub(visible.saturating_sub(1))
            .min(rows.len().saturating_sub(visible));
        let mut lines = vec![Line::styled(title, heading)];
        for (index, (text, color)) in rows.into_iter().enumerate().skip(first).take(visible) {
            let style = if index == cursor {
                Style::new()
                    .fg(color)
                    .bg(palette.get(Role::SelectionBackground))
            } else {
                Style::new().fg(color)
            };
            lines.push(Line::styled(
                format!("{} {text}", if index == cursor { "›" } else { " " }),
                style,
            ));
        }
        lines.push(Line::from(footer));
        frame.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_select_color_and_scroll_menu() {
        let mut palette = Palette::default();
        let mut menu = Menu::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        menu.key(key(KeyCode::Char('a')), &mut palette);
        for character in "hjkL".chars() {
            menu.key(key(KeyCode::Char(character)), &mut palette);
        }
        menu.key(key(KeyCode::Enter), &mut palette);
        assert_eq!(palette.highlights[1].word, "hjkL");
        menu.key(key(KeyCode::Char('l')), &mut palette);
        menu.key(key(KeyCode::Char('j')), &mut palette);
        menu.key(key(KeyCode::Enter), &mut palette);
        assert_eq!(
            palette.highlights[1].color,
            ratatui::style::Color::LightGreen
        );
        menu.key(key(KeyCode::Char('i')), &mut palette);
        menu.key(key(KeyCode::Char('x')), &mut palette);
        menu.key(key(KeyCode::Esc), &mut palette);
        assert_eq!(palette.highlights[1].word, "hjkL");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 5)).unwrap();
        terminal
            .draw(|frame| menu.draw(frame, frame.area(), &palette))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 2)].symbol(), "h");
        assert_eq!(buffer[(2, 2)].fg, ratatui::style::Color::LightGreen);
        menu.key(key(KeyCode::Char('G')), &mut palette);
        menu.key(key(KeyCode::Char('l')), &mut palette);
        menu.key(key(KeyCode::Esc), &mut palette);
        assert_eq!(palette.highlights.len(), 2);
        assert!(menu.key(key(KeyCode::Char('h')), &mut palette));
    }
}
