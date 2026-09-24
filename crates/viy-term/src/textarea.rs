//! The input box's text (flags F33), after Codex's `bottom_pane/textarea.rs`:
//! the text with a cursor on a grapheme boundary, word-wrapped rows that
//! keep the byte ranges they show (cached per width), moves by grapheme,
//! word, line and screen row with a remembered column, and one kill buffer
//! for Ctrl-K, Ctrl-U and the word deletions, which Ctrl-Y yanks back.
//!
//! The text is kept as typed or pasted, and sent so; what is drawn goes
//! through the sanitizer (ui-plan 7, kb d076): a pasted escape shows as
//! `␛`, and the rows measure what is drawn. Codex keeps only text that is
//! safe to draw, so its rows index drawn and stored text alike; here the
//! rows index the stored text and their cells carry what is drawn.

use std::cell::Ref;
use std::cell::RefCell;

use ratatui::layout::Position;
use unicode_segmentation::UnicodeSegmentation;

use crate::wrap::TextRow;
use crate::wrap::row_of;
use crate::wrap::word_wrap;

#[derive(Debug, Default, Clone)]
pub struct TextArea {
    text: String,
    /// A byte offset on a grapheme boundary.
    cursor: usize,
    /// The column Up and Down aim for, kept while they repeat.
    preferred: Option<usize>,
    /// What the last kill took.
    killed: String,
    /// The rows for the last width laid out.
    rows: RefCell<Option<(usize, Vec<TextRow>)>>,
}

impl PartialEq for TextArea {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.cursor == other.cursor
    }
}

impl Eq for TextArea {}

/// What a char is for word moves: Codex's and readline's classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

impl TextArea {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The cursor's byte offset.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replaces the text and puts the cursor at its end: a prefill or a
    /// recalled history entry.
    pub fn set_text(&mut self, text: &str) {
        self.text = normalize_newlines(text);
        self.cursor = self.text.len();
        self.changed();
    }

    /// Empties the box and returns what it held.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.changed();
        std::mem::take(&mut self.text)
    }

    /// Inserts typed or pasted text at the cursor; a paste's `\r\n` and `\r`
    /// become newlines and never submit.
    pub fn insert_str(&mut self, text: &str) {
        let text = normalize_newlines(text);
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.changed();
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.changed();
    }

    pub fn backspace(&mut self) {
        let start = self.prev_boundary(self.cursor);
        self.remove(start..self.cursor);
    }

    pub fn delete(&mut self) {
        let end = self.next_boundary(self.cursor);
        self.remove(self.cursor..end);
    }

    pub fn move_left(&mut self) {
        self.move_to(self.prev_boundary(self.cursor));
    }

    pub fn move_right(&mut self) {
        self.move_to(self.next_boundary(self.cursor));
    }

    /// To the start of the word before the cursor (Alt-B, Ctrl-Left).
    pub fn move_word_left(&mut self) {
        self.move_to(self.word_start(self.cursor));
    }

    /// To the end of the word after the cursor (Alt-F, Ctrl-Right).
    pub fn move_word_right(&mut self) {
        self.move_to(self.word_end(self.cursor));
    }

    /// To the start of the cursor's line.
    pub fn move_home(&mut self) {
        self.move_to(self.line_start(self.cursor));
    }

    /// To the end of the cursor's line.
    pub fn move_end(&mut self) {
        self.move_to(self.line_end(self.cursor));
    }

    /// Deletes the word before the cursor into the kill buffer (Ctrl-W,
    /// Alt-Backspace).
    pub fn delete_word_left(&mut self) {
        let start = self.word_start(self.cursor);
        self.kill(start..self.cursor);
    }

    /// Deletes the word after the cursor into the kill buffer (Alt-D).
    pub fn delete_word_right(&mut self) {
        let end = self.word_end(self.cursor);
        self.kill(self.cursor..end);
    }

    /// Kills to the start of the line (Ctrl-U); at a line's start, the
    /// newline before it.
    pub fn kill_to_line_start(&mut self) {
        let start = self.line_start(self.cursor);
        let start = if start == self.cursor {
            self.cursor.saturating_sub(1)
        } else {
            start
        };
        self.kill(start..self.cursor);
    }

    /// Kills to the end of the line (Ctrl-K); at a line's end, the newline
    /// after it.
    pub fn kill_to_line_end(&mut self) {
        let end = self.line_end(self.cursor);
        let end = if end == self.cursor && end < self.text.len() {
            end + 1
        } else {
            end
        };
        self.kill(self.cursor..end);
    }

    /// Inserts what the last kill took (Ctrl-Y).
    pub fn yank(&mut self) {
        let text = self.killed.clone();
        self.insert_str(&text);
    }

    /// One screen row up at `width`, keeping the column; false on the first
    /// row, where the cursor goes to the start.
    pub fn move_up(&mut self, width: usize) -> bool {
        self.move_row(width, false)
    }

    /// One screen row down at `width`, keeping the column; false on the
    /// last row, where the cursor goes to the end.
    pub fn move_down(&mut self, width: usize) -> bool {
        self.move_row(width, true)
    }

    /// The text in rows at most `width` cells wide, as drawn, and the
    /// cursor's column and row in them. A cursor past a row's last column
    /// (a full row, or spaces hanging past the width) goes to the start of
    /// a row of its own at the end of the text, and onto the last column
    /// elsewhere.
    pub fn layout(&self, width: u16) -> (Vec<String>, Position) {
        let width = usize::from(width.max(1));
        let rows = self.rows(width);
        let mut drawn: Vec<String> = rows.iter().map(TextRow::drawn).collect();
        let row = row_of(&rows, self.cursor);
        let mut column = rows.get(row).map_or(0, |r| r.column_of(self.cursor));
        let mut y = row;
        if column >= width {
            if self.cursor == self.text.len() {
                drawn.push(String::new());
                y += 1;
                column = 0;
            } else {
                column = width - 1;
            }
        }
        (drawn, position(y, column))
    }

    fn rows(&self, width: usize) -> Ref<'_, [TextRow]> {
        let stale = self.rows.borrow().as_ref().is_none_or(|(w, _)| *w != width);
        if stale {
            *self.rows.borrow_mut() = Some((width, word_wrap(&self.text, width)));
        }
        Ref::map(self.rows.borrow(), |rows| match rows {
            Some((_, rows)) => rows.as_slice(),
            None => &[],
        })
    }

    fn move_row(&mut self, width: usize, down: bool) -> bool {
        let width = width.max(1);
        let (target, column) = {
            let rows = self.rows(width);
            let row = row_of(&rows, self.cursor);
            let column = self
                .preferred
                .unwrap_or_else(|| rows.get(row).map_or(0, |r| r.column_of(self.cursor)));
            let target = if down {
                rows.get(row + 1)
            } else {
                row.checked_sub(1).and_then(|r| rows.get(r))
            };
            (target.map(|r| r.pos_at(column)), column)
        };
        match target {
            Some(pos) => {
                self.cursor = pos;
                self.preferred = Some(column);
                true
            }
            None => {
                self.move_to(if down { self.text.len() } else { 0 });
                false
            }
        }
    }

    fn move_to(&mut self, pos: usize) {
        self.cursor = pos;
        self.preferred = None;
    }

    fn remove(&mut self, range: std::ops::Range<usize>) {
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.changed();
    }

    fn kill(&mut self, range: std::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.killed = self.text[range.clone()].to_string();
        self.remove(range);
    }

    fn changed(&mut self) {
        self.preferred = None;
        *self.rows.get_mut() = None;
    }

    fn prev_boundary(&self, pos: usize) -> usize {
        self.text[..pos]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, pos: usize) -> usize {
        self.text[pos..]
            .graphemes(true)
            .next()
            .map_or(pos, |g| pos + g.len())
    }

    /// Back over spaces, then over chars of the class before them.
    fn word_start(&self, pos: usize) -> usize {
        let mut chars = self.text[..pos].char_indices().rev().peekable();
        while chars.next_if(|&(_, c)| class(c) == Class::Space).is_some() {}
        let Some(&(_, first)) = chars.peek() else {
            return 0;
        };
        let kind = class(first);
        let mut start = pos;
        for (i, c) in chars {
            if class(c) != kind {
                break;
            }
            start = i;
        }
        start.min(pos)
    }

    /// Forward over spaces, then over chars of the class after them.
    fn word_end(&self, pos: usize) -> usize {
        let mut chars = self.text[pos..].char_indices().peekable();
        while chars.next_if(|&(_, c)| class(c) == Class::Space).is_some() {}
        let Some(&(_, first)) = chars.peek() else {
            return self.text.len();
        };
        let kind = class(first);
        let mut end = self.text.len();
        for (i, c) in chars {
            if class(c) != kind {
                end = pos + i;
                break;
            }
        }
        end
    }

    fn line_start(&self, pos: usize) -> usize {
        self.text[..pos].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, pos: usize) -> usize {
        self.text[pos..]
            .find('\n')
            .map_or(self.text.len(), |i| pos + i)
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
