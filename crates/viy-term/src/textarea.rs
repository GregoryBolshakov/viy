//! The input box's text: a string with a cursor, laid out in rows for the
//! live region. The spike's version (WP 0.8); the composer of WP 3.x follows
//! Codex's `bottom_pane/textarea.rs` and `chat_composer.rs` (flags F33).

use ratatui::layout::Position;
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TextArea {
    text: String,
    /// A byte offset on a char boundary.
    cursor: usize,
}

impl TextArea {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replaces the text and puts the cursor at its end: the prefill of
    /// `/rewind`.
    pub fn set_text(&mut self, text: &str) {
        self.text = normalize_newlines(text);
        self.cursor = self.text.len();
    }

    /// Empties the box and returns what it held.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Inserts typed or pasted text at the cursor; a paste's `\r\n` and `\r`
    /// become newlines and never submit.
    pub fn insert_str(&mut self, text: &str) {
        let text = normalize_newlines(text);
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn move_right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    /// To the start of the cursor's line.
    pub fn move_home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    /// To the end of the cursor's line.
    pub fn move_end(&mut self) {
        self.cursor += self.text[self.cursor..]
            .find('\n')
            .unwrap_or(self.text.len() - self.cursor);
    }

    /// The text in rows at most `width` cells wide, and the cursor's column
    /// and row in them.
    pub fn layout(&self, width: u16) -> (Vec<String>, Position) {
        let width = usize::from(width.max(1));
        let mut rows = vec![String::new()];
        let mut used = 0;
        let mut cursor = Position::ORIGIN;
        for (i, c) in self.text.char_indices() {
            if i == self.cursor {
                cursor = position(rows.len() - 1, used);
            }
            if c == '\n' {
                rows.push(String::new());
                used = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if used + w > width {
                rows.push(String::new());
                used = 0;
            }
            if let Some(row) = rows.last_mut() {
                row.push(c);
            }
            used += w;
        }
        if self.cursor == self.text.len() {
            // A cursor after a full row goes to the start of the next.
            if used >= width {
                rows.push(String::new());
                used = 0;
            }
            cursor = position(rows.len() - 1, used);
        }
        (rows, cursor)
    }
}

fn position(row: usize, column: usize) -> Position {
    Position::new(
        u16::try_from(column).unwrap_or(u16::MAX),
        u16::try_from(row).unwrap_or(u16::MAX),
    )
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
#[path = "textarea_tests.rs"]
mod tests;
