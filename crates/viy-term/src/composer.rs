//! The input box (ui-plan 5, flags F33), after Codex's
//! `bottom_pane/chat_composer.rs`: the text area, its keys (Codex's default
//! editor keymap, `keymap.rs`), the history, slash completion, and Codex's
//! paste-burst guard (`paste_burst.rs`).
//!
//! What rp's readline did: Enter sends; Ctrl-J, Alt-Enter and a line ending
//! in `\` insert a newline (rpctl kb f087); a bracketed paste arrives as one
//! block whose newlines never send (`test_paste.py`); a sent line is
//! trimmed, as rp's `_read_input(...).strip()`; Shift+Tab steps the chat
//! mode. A terminal without bracketed paste types a paste as keys: Enter
//! within 120 ms of a burst of keys under 8 ms apart is a newline, not a
//! send (Codex's direct-insert path; no person types that fast).

use std::time::Duration;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use tokio::time::Instant;

use crate::history::History;
use crate::slash::Completion;
use crate::slash::completions;
use crate::textarea::TextArea;

/// Codex's `PASTE_BURST_CHAR_INTERVAL` (Linux and macOS).
const BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);
/// Codex's `PASTE_BURST_MIN_CHARS`.
const BURST_MIN_CHARS: u32 = 3;
/// Codex's `PASTE_ENTER_SUPPRESS_WINDOW`.
const BURST_ENTER_WINDOW: Duration = Duration::from_millis(120);
/// Rows the slash popup shows at most.
pub const POPUP_ROWS: usize = 6;

/// What a key asks of the chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    /// Send this, trimmed and not empty: a message or a slash command.
    Submit(String),
    /// Shift+Tab: the next chat mode.
    CycleMode,
}

/// Keys arriving faster than anyone types.
#[derive(Debug, Default)]
struct PasteBurst {
    last: Option<Instant>,
    run: u32,
    /// Until when Enter inserts a newline.
    window: Option<Instant>,
}

impl PasteBurst {
    fn on_char(&mut self, now: Instant) {
        let close = self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) <= BURST_CHAR_INTERVAL);
        self.run = if close { self.run + 1 } else { 1 };
        self.last = Some(now);
        if self.run >= BURST_MIN_CHARS {
            self.window = Some(now + BURST_ENTER_WINDOW);
        }
    }

    /// Whether Enter now is part of a paste; it keeps the burst going.
    fn enter_is_newline(&mut self, now: Instant) -> bool {
        if self.window.is_some_and(|until| now <= until) {
            self.window = Some(now + BURST_ENTER_WINDOW);
            self.last = Some(now);
            true
        } else {
            false
        }
    }

    /// Any other key ends a burst.
    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Default)]
pub struct Composer {
    area: TextArea,
    history: History,
    burst: PasteBurst,
    /// The popup's choice, and the input it was offered for.
    selected: usize,
    /// Esc closed the popup for this input.
    dismissed: Option<String>,
    /// The width the input was last laid out at, for Up and Down.
    width: usize,
}

impl Composer {
    pub fn new(history: History) -> Self {
        Self {
            history,
            width: 80,
            ..Self::default()
        }
    }

    pub fn text(&self) -> &str {
        self.area.text()
    }

    pub fn is_empty(&self) -> bool {
        self.area.is_empty()
    }

    /// Empties the box (Ctrl-C with a draft).
    pub fn clear(&mut self) {
        self.area.take();
    }

    /// Puts `text` in the box to edit: `/rewind`'s prefill.
    pub fn set_text(&mut self, text: &str) {
        self.area.set_text(text);
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    /// A bracketed paste: one block, its newlines kept.
    pub fn paste(&mut self, text: &str) {
        self.burst.clear();
        self.area.insert_str(text);
    }

    /// The popup's offers for the input now; none once Esc closed it.
    pub fn popup(&self) -> Vec<Completion> {
        if self.area.cursor() != self.area.text().len()
            || self.dismissed.as_deref() == Some(self.area.text())
        {
            return Vec::new();
        }
        completions(self.area.text())
    }

    /// The popup's choice.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The input in rows `width` cells wide, and the cursor in them.
    pub fn layout(&mut self, width: u16) -> (Vec<String>, ratatui::layout::Position) {
        self.width = usize::from(width.max(1));
        self.area.layout(width)
    }

    /// Takes one key, at `now`.
    pub fn key(&mut self, key: KeyEvent, now: Instant) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let before = self.area.text().to_string();
        if !matches!(key.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab) || ctrl || alt {
            self.burst.clear();
        }
        let popup = self.popup();
        let action = match key.code {
            KeyCode::Enter if !alt && self.burst.enter_is_newline(now) => self.newline(),
            KeyCode::Enter if alt => self.newline(),
            KeyCode::Enter if self.area.text().ends_with('\\') => {
                let text = self.area.text();
                let kept = text[..text.len() - 1].to_string();
                self.area.set_text(&kept);
                self.newline()
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Char('j') if ctrl => self.newline(),
            KeyCode::BackTab => Action::CycleMode,
            KeyCode::Tab if !popup.is_empty() => {
                let choice = &popup[self.selected.min(popup.len() - 1)];
                self.area.set_text(&choice.text);
                Action::None
            }
            KeyCode::Tab => {
                self.area.insert_char('\t');
                Action::None
            }
            KeyCode::Esc if !popup.is_empty() => {
                self.dismissed = Some(self.area.text().to_string());
                Action::None
            }
            KeyCode::Up if !popup.is_empty() => {
                self.selected = self.selected.checked_sub(1).unwrap_or(popup.len() - 1);
                return Action::None;
            }
            KeyCode::Down if !popup.is_empty() => {
                self.selected = (self.selected + 1) % popup.len();
                return Action::None;
            }
            KeyCode::Up => self.up(),
            KeyCode::Char('p') if ctrl => self.up(),
            KeyCode::Down => self.down(),
            KeyCode::Char('n') if ctrl => self.down(),
            KeyCode::Left if ctrl || alt => self.edit(TextArea::move_word_left),
            KeyCode::Right if ctrl || alt => self.edit(TextArea::move_word_right),
            KeyCode::Char('b') if alt => self.edit(TextArea::move_word_left),
            KeyCode::Char('f') if alt => self.edit(TextArea::move_word_right),
            KeyCode::Left => self.edit(TextArea::move_left),
            KeyCode::Char('b') if ctrl => self.edit(TextArea::move_left),
            KeyCode::Right => self.edit(TextArea::move_right),
            KeyCode::Char('f') if ctrl => self.edit(TextArea::move_right),
            KeyCode::Home => self.edit(TextArea::move_home),
            KeyCode::Char('a') if ctrl => self.edit(TextArea::move_home),
            KeyCode::End => self.edit(TextArea::move_end),
            KeyCode::Char('e') if ctrl => self.edit(TextArea::move_end),
            KeyCode::Backspace if ctrl || alt => self.edit(TextArea::delete_word_left),
            KeyCode::Char('w') if ctrl => self.edit(TextArea::delete_word_left),
            KeyCode::Char('h') if ctrl && alt => self.edit(TextArea::delete_word_left),
            KeyCode::Backspace => self.edit(TextArea::backspace),
            KeyCode::Char('h') if ctrl => self.edit(TextArea::backspace),
            KeyCode::Delete if ctrl || alt => self.edit(TextArea::delete_word_right),
            KeyCode::Char('d') if alt => self.edit(TextArea::delete_word_right),
            KeyCode::Delete => self.edit(TextArea::delete),
            KeyCode::Char('d') if ctrl => self.edit(TextArea::delete),
            KeyCode::Char('u') if ctrl => self.edit(TextArea::kill_to_line_start),
            KeyCode::Char('k') if ctrl => self.edit(TextArea::kill_to_line_end),
            KeyCode::Char('y') if ctrl => self.edit(TextArea::yank),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.burst.on_char(now);
                self.area.insert_char(c);
                Action::None
            }
            _ => Action::None,
        };
        if self.area.text() != before {
            self.selected = 0;
        }
        action
    }

    fn edit(&mut self, f: fn(&mut TextArea)) -> Action {
        f(&mut self.area);
        Action::None
    }

    fn newline(&mut self) -> Action {
        self.area.insert_char('\n');
        Action::None
    }

    fn up(&mut self) -> Action {
        if self.history.browses(self.area.text(), self.area.cursor()) {
            if let Some(text) = self.history.older() {
                self.area.set_text(&text);
            }
        } else {
            self.area.move_up(self.width);
        }
        Action::None
    }

    fn down(&mut self) -> Action {
        if self.history.browses(self.area.text(), self.area.cursor()) {
            if let Some(text) = self.history.newer() {
                self.area.set_text(&text);
            }
        } else {
            self.area.move_down(self.width);
        }
        Action::None
    }

    /// The input, trimmed, to send; nothing when it is blank. It goes into
    /// the history; a history file that cannot be written loses the entry,
    /// never the message (rp ignores the same failure).
    fn submit(&mut self) -> Action {
        let text = self.area.text().trim().to_string();
        if text.is_empty() {
            return Action::None;
        }
        self.area.take();
        self.dismissed = None;
        let _ = self.history.record(&text);
        Action::Submit(text)
    }
}

#[cfg(test)]
#[path = "composer_tests.rs"]
mod tests;
