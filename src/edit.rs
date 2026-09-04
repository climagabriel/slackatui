//! A text field with the Emacs keys Claude Code's input uses: Ctrl-a/e,
//! Ctrl-b/f, Alt-b/f, Ctrl-k/u/w, Alt-d, Ctrl-y, Ctrl-d, and Ctrl-j for a
//! newline where a field allows several lines.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Default, Clone, Debug)]
pub struct Editor {
    pub text: String,
    /// Byte offset of the cursor, always on a char boundary.
    pub cursor: usize,
    kill: String,
}

impl Editor {
    pub fn with(text: String) -> Editor {
        let cursor = text.len();
        Editor {
            text,
            cursor,
            kill: String::new(),
        }
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor)
    }

    fn word_start(&self) -> usize {
        let s = &self.text[..self.cursor];
        let s = s.trim_end_matches(|c: char| !c.is_alphanumeric());
        s.trim_end_matches(|c: char| c.is_alphanumeric()).len()
    }

    fn word_end(&self) -> usize {
        let s = &self.text[self.cursor..];
        let s = s.trim_start_matches(|c: char| !c.is_alphanumeric());
        let s = s.trim_start_matches(|c: char| c.is_alphanumeric());
        self.text.len() - s.len()
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len())
    }

    fn kill_range(&mut self, a: usize, b: usize) {
        if a < b {
            self.kill = self.text[a..b].to_string();
            self.text.replace_range(a..b, "");
            self.cursor = a;
        }
    }

    /// Applies an editing key; false when the key is not one.
    pub fn key(&mut self, k: KeyEvent, multiline: bool) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match (k.code, ctrl, alt) {
            (KeyCode::Char(c), false, false) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            (KeyCode::Backspace, _, _) | (KeyCode::Char('h'), true, _) => {
                let p = self.prev_boundary();
                if p < self.cursor {
                    self.text.replace_range(p..self.cursor, "");
                    self.cursor = p;
                }
            }
            (KeyCode::Delete, _, _) | (KeyCode::Char('d'), true, false) => {
                let n = self.next_boundary();
                self.text.replace_range(self.cursor..n, "");
            }
            (KeyCode::Left, _, false) | (KeyCode::Char('b'), true, _) => {
                self.cursor = self.prev_boundary()
            }
            (KeyCode::Right, _, false) | (KeyCode::Char('f'), true, _) => {
                self.cursor = self.next_boundary()
            }
            (KeyCode::Home, _, _) | (KeyCode::Char('a'), true, _) => {
                self.cursor = self.line_start()
            }
            (KeyCode::End, _, _) | (KeyCode::Char('e'), true, _) => self.cursor = self.line_end(),
            (KeyCode::Char('k'), true, _) => {
                let e = self.line_end();
                if e == self.cursor && e < self.text.len() {
                    self.kill_range(e, e + 1);
                } else {
                    self.kill_range(self.cursor, e);
                }
            }
            (KeyCode::Char('u'), true, _) => {
                let s = self.line_start();
                self.kill_range(s, self.cursor);
            }
            (KeyCode::Char('w'), true, _) => {
                let s = self.word_start();
                self.kill_range(s, self.cursor);
            }
            (KeyCode::Char('d'), false, true) => {
                let e = self.word_end();
                self.kill_range(self.cursor, e);
            }
            (KeyCode::Char('b'), false, true) | (KeyCode::Left, _, true) => {
                self.cursor = self.word_start()
            }
            (KeyCode::Char('f'), false, true) | (KeyCode::Right, _, true) => {
                self.cursor = self.word_end()
            }
            (KeyCode::Char('y'), true, _) => {
                let k = self.kill.clone();
                self.text.insert_str(self.cursor, &k);
                self.cursor += k.len();
            }
            (KeyCode::Char('j'), true, _) if multiline => self.newline(),
            _ => return false,
        }
        true
    }

    /// Breaks the line at the cursor, for the keys a caller reads before the
    /// editor sees them.
    pub fn newline(&mut self) {
        self.text.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Lines of text with the cursor as a byte range of one char (or an
    /// empty range at the end): (line text, cursor position in that line).
    pub fn rows(&self) -> Vec<(String, Option<usize>)> {
        let mut out = Vec::new();
        let mut start = 0usize;
        for (i, line) in self.text.split('\n').enumerate() {
            let end = start + line.len();
            let at = (self.cursor >= start && self.cursor <= end).then(|| self.cursor - start);
            let _ = i;
            out.push((line.to_string(), at));
            start = end + 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(e: &mut Editor, code: KeyCode, m: KeyModifiers) {
        assert!(e.key(KeyEvent::new(code, m), true));
    }
    fn ctrl(e: &mut Editor, c: char) {
        press(e, KeyCode::Char(c), KeyModifiers::CONTROL);
    }
    fn alt(e: &mut Editor, c: char) {
        press(e, KeyCode::Char(c), KeyModifiers::ALT);
    }
    fn type_(e: &mut Editor, s: &str) {
        for c in s.chars() {
            press(e, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    #[test]
    fn emacs_keys_edit_the_line() {
        let mut e = Editor::default();
        type_(&mut e, "hello wörld");
        ctrl(&mut e, 'a');
        type_(&mut e, "X ");
        assert_eq!(e.text, "X hello wörld");
        ctrl(&mut e, 'e');
        ctrl(&mut e, 'w');
        assert_eq!(e.text, "X hello ");
        ctrl(&mut e, 'y');
        assert_eq!(e.text, "X hello wörld");
        alt(&mut e, 'b');
        ctrl(&mut e, 'k');
        assert_eq!(e.text, "X hello ");
        ctrl(&mut e, 'u');
        assert_eq!(e.text, "");
        ctrl(&mut e, 'y');
        assert_eq!(e.text, "X hello ");
        press(&mut e, KeyCode::Left, KeyModifiers::NONE);
        press(&mut e, KeyCode::Left, KeyModifiers::NONE);
        ctrl(&mut e, 'd');
        assert_eq!(e.text, "X hell ");
        alt(&mut e, 'f');
        assert_eq!(e.cursor, e.text.len());
        assert!(!e.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), true));
    }

    #[test]
    fn newlines_only_where_allowed_and_rows_carry_the_cursor() {
        let mut e = Editor::default();
        type_(&mut e, "one");
        assert!(!e.key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            false
        ));
        ctrl(&mut e, 'j');
        type_(&mut e, "two");
        assert_eq!(e.text, "one\ntwo");
        ctrl(&mut e, 'a');
        assert_eq!(
            e.rows(),
            vec![("one".to_string(), None), ("two".to_string(), Some(0))]
        );
        ctrl(&mut e, 'k');
        assert_eq!(e.text, "one\n");
        press(&mut e, KeyCode::Backspace, KeyModifiers::NONE);
        ctrl(&mut e, 'k');
        assert_eq!(e.text, "one");
        // What Alt-Enter reaches, the prompt reading the key before the editor.
        ctrl(&mut e, 'a');
        e.newline();
        assert_eq!(e.text, "\none");
        assert_eq!(e.cursor, 1);
    }
}
