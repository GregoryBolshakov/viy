//! Question boxes (ui-plan 6, flag F37): every question the engine asks is
//! a box above the input, oldest first, the rest waiting behind it.
//!
//! Type-ahead never answers. A box takes a key only while it is drawn
//! whole, reason and all, and once [`GUARD`] has passed since it was and
//! since the last key that went to the input box: keys typed before it
//! appeared, in the first 300 ms, or while the user is still typing go to
//! the input box, and each one starts the guard again, so a message being
//! typed when a box comes up cannot answer it, however long it is. A box
//! cut short by a resize or a growing draft stops taking keys, and starts
//! its guard again once it is whole. Then `y`, `n`, `a` (when "always" is
//! offered) answer, and Enter alone takes the default, which is no except
//! where rp's is yes (`[Y/n]`), but only with the input box empty: with a
//! draft, Enter sends the draft. Shift+Tab at the box asks for the next
//! mode naming the question, which only the new mode may answer (rp's
//! `_shift_tab`). Ctrl-C at a box that takes keys answers no, as rp's
//! `_confirm` takes a Ctrl-C; the turn goes on, and a second Ctrl-C stops
//! it (F13: rpctl's Ctrl-C table).
//!
//! A pick (rp's numbered lists: `/rewind`, `/resume`) lists its options
//! with one chosen, the default at first: Up, Down, Home, End and a digit
//! choose, Enter picks the one chosen, `q` or Esc picks none, with the
//! same guard.

use std::collections::VecDeque;
use std::time::Duration;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use tokio::time::Instant;
use viy_protocol::Answer;
use viy_protocol::Choices;
use viy_protocol::Question;
use viy_protocol::QuestionKind;

use crate::cells::notice_style;
use crate::transcript::Cell;
use crate::wrap::text_width;
use crate::wrap::wrap_spans;

/// How long a box must have been on screen, with no key typed, before it
/// takes a key.
pub const GUARD: Duration = Duration::from_millis(300);
/// Narrower than this, a box is drawn without its frame.
const FRAMED: usize = 12;
/// A pick's options shown at once.
pub const PICK_ROWS: usize = 8;

/// A question on screen or waiting for its turn.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionBox {
    pub id: String,
    pub question: Question,
    /// Since when it has been drawn whole, without a break.
    shown: Option<Instant>,
    /// The option chosen, for a pick.
    chosen: usize,
}

/// What a box made of a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Taken {
    /// The front question is answered; it has left the queue.
    Answered {
        question: QuestionBox,
        answer: Answer,
    },
    /// Shift+Tab at the front question.
    CycleMode { question_id: String },
    /// Another option of the front pick was chosen.
    Chose,
}

/// The questions asked and not yet answered, oldest first.
#[derive(Debug, Default)]
pub struct Questions {
    queue: VecDeque<QuestionBox>,
    /// When a key or a paste last went to the input box.
    typed: Option<Instant>,
}

impl Questions {
    pub fn push(&mut self, id: String, question: Question) {
        let chosen = match question.default {
            Answer::Pick(n) => n as usize,
            _ => 0,
        };
        self.queue.push_back(QuestionBox {
            id,
            question,
            shown: None,
            chosen,
        });
    }

    /// Takes question `id` away: the engine no longer waits for it.
    pub fn resolve(&mut self, id: &str) -> Option<QuestionBox> {
        let i = self.queue.iter().position(|b| b.id == id)?;
        self.queue.remove(i)
    }

    pub fn front(&self) -> Option<&QuestionBox> {
        self.queue.front()
    }

    /// Questions behind the one shown.
    pub fn waiting(&self) -> usize {
        self.queue.len().saturating_sub(1)
    }

    /// A key or a paste went to the input box at `now`.
    pub fn typed(&mut self, now: Instant) {
        self.typed = Some(now);
    }

    /// The front box was drawn at `now`, `whole` or cut short.
    pub fn drawn(&mut self, now: Instant, whole: bool) {
        if let Some(front) = self.queue.front_mut() {
            if whole {
                front.shown.get_or_insert(now);
            } else {
                front.shown = None;
            }
        }
    }

    /// When the front box starts taking keys, while it is drawn whole.
    pub fn armed_at(&self) -> Option<Instant> {
        let shown = self.queue.front()?.shown?;
        Some(self.typed.map_or(shown, |t| t.max(shown)) + GUARD)
    }

    pub fn armed(&self, now: Instant) -> bool {
        self.armed_at().is_some_and(|at| now >= at)
    }

    /// Offers the key pressed at `now` to the front box; `None` leaves it
    /// to the rest of the app. `draft`: the input box holds text, which
    /// Enter sends rather than answering.
    pub fn key(&mut self, key: KeyEvent, now: Instant, draft: bool) -> Option<Taken> {
        if !self.armed(now) {
            return None;
        }
        let front = self.queue.front()?;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Char('c') && ctrl {
            return self.answer(Answer::No);
        }
        if ctrl || key.modifiers.contains(KeyModifiers::ALT) {
            return None;
        }
        let question = &front.question;
        if question.choices == Choices::Pick {
            return self.pick_key(key, draft);
        }
        let answer = match key.code {
            KeyCode::Char('y' | 'Y') => Answer::Yes,
            KeyCode::Char('n' | 'N') => Answer::No,
            KeyCode::Char('a' | 'A') if question.choices == Choices::YesNoAlways => Answer::Always,
            KeyCode::Enter if key.modifiers.is_empty() && !draft => question.default,
            KeyCode::BackTab => {
                return Some(Taken::CycleMode {
                    question_id: front.id.clone(),
                });
            }
            _ => return None,
        };
        self.answer(answer)
    }

    fn pick_key(&mut self, key: KeyEvent, draft: bool) -> Option<Taken> {
        let front = self.queue.front_mut()?;
        let last = front.question.options.len().saturating_sub(1);
        let chosen = match key.code {
            KeyCode::Up => front.chosen.saturating_sub(1),
            KeyCode::Down => (front.chosen + 1).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            KeyCode::Char(c @ '1'..='9') => {
                let n = c as usize - '1' as usize;
                if n > last {
                    return None;
                }
                n
            }
            KeyCode::Enter if key.modifiers.is_empty() && !draft => {
                let n = u32::try_from(front.chosen).unwrap_or(u32::MAX);
                return self.answer(Answer::Pick(n));
            }
            KeyCode::Char('q' | 'Q') | KeyCode::Esc => return self.answer(Answer::No),
            _ => return None,
        };
        front.chosen = chosen;
        Some(Taken::Chose)
    }

    fn answer(&mut self, answer: Answer) -> Option<Taken> {
        let question = self.queue.pop_front()?;
        Some(Taken::Answered { question, answer })
    }

    /// The front box in at most `max_rows` rows of `width`, and whether
    /// all of it is there. A box that does not fit shows the question's
    /// last rows and says so, and takes no key until it fits.
    pub fn rows(&self, width: usize, max_rows: usize, now: Instant) -> (Vec<Line<'static>>, bool) {
        let Some(front) = self.queue.front() else {
            return (Vec::new(), true);
        };
        let mut framed = width >= FRAMED;
        let inner = |framed: bool| if framed { width - 4 } else { width.max(1) };
        let content = |framed: bool| {
            let inner = inner(framed);
            let mut rows: Vec<(Line<'static>, usize)> = Vec::new();
            for notice in &front.question.reason {
                let style = notice_style(notice.level);
                for line in notice.message.split('\n') {
                    rows.extend(wrapped(&[Span::styled(line.to_string(), style)], inner));
                }
            }
            rows.extend(wrapped(&prompt_spans(&front.question), inner));
            if front.question.choices == Choices::Pick {
                rows.extend(front.option_rows(inner));
            }
            rows.extend(wrapped(
                &keys_spans(&front.question, self.armed(now)),
                inner,
            ));
            rows
        };
        let mut rows = content(framed);
        if framed && rows.len() + 2 > max_rows {
            framed = false;
            rows = content(false);
        }
        let whole = rows.len() + if framed { 2 } else { 0 } <= max_rows;
        if !whole {
            let short = wrapped(
                &[Span::styled(
                    "too little room for this question: make the terminal taller",
                    Style::new().yellow(),
                )],
                inner(false),
            );
            let keep = max_rows.saturating_sub(short.len());
            rows.drain(..rows.len().saturating_sub(keep));
            rows.extend(short);
            rows.truncate(max_rows);
        }
        if !framed {
            return (rows.into_iter().map(|(line, _)| line).collect(), whole);
        }
        let border = Style::new().yellow();
        let inner = inner(true);
        let mut out = vec![top_border(kind_label(front.question.kind), width, border)];
        for (line, used) in rows {
            let mut spans = vec![Span::styled("│ ", border)];
            spans.extend(line.spans);
            spans.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
            spans.push(Span::styled(" │", border));
            out.push(Line::from(spans));
        }
        out.push(Line::from(Span::styled(
            format!("└{}┘", "─".repeat(width - 2)),
            border,
        )));
        (out, whole)
    }
}

impl QuestionBox {
    /// A pick's options in view, the chosen one reversed: the window
    /// follows it.
    fn option_rows(&self, width: usize) -> Vec<(Line<'static>, usize)> {
        let options = &self.question.options;
        let first = self
            .chosen
            .saturating_sub(PICK_ROWS - 1)
            .min(options.len().saturating_sub(PICK_ROWS));
        options
            .iter()
            .enumerate()
            .skip(first)
            .take(PICK_ROWS)
            .map(|(i, option)| {
                let row = format!("{:>3}. {}", i + 1, option.replace('\n', " "));
                let mut cut = wrapped(&[Span::raw(row)], width);
                let (line, used) = cut.swap_remove(0);
                let line = if i == self.chosen {
                    line.cyan().reversed()
                } else {
                    line
                };
                (line, used)
            })
            .collect()
    }

    /// What the transcript keeps once the box is gone: the reason and the
    /// question as rp leaves them on screen, with the answer after it, or
    /// nothing after it when nobody answered.
    /// The question as the mode answered it: `y (auto-yes)` after it.
    pub fn cell_by(&self, answer: Answer, mode: &str) -> Cell {
        self.cell_with(Some(answer), Some(mode))
    }

    pub fn cell(&self, answer: Option<Answer>) -> Cell {
        self.cell_with(answer, None)
    }

    fn cell_with(&self, answer: Option<Answer>, mode: Option<&str>) -> Cell {
        let mut lines: Vec<Line<'static>> = Vec::new();
        for notice in &self.question.reason {
            let style = notice_style(notice.level);
            lines.extend(
                notice
                    .message
                    .split('\n')
                    .map(|l| Line::from(Span::styled(l.to_string(), style))),
            );
        }
        let mut spans = prompt_spans(&self.question);
        if let Some(answer) = answer {
            spans.push(Span::raw(format!(" {}", answer_word(answer))));
        }
        if let Some(mode) = mode {
            spans.push(Span::styled(format!(" ({mode})"), Style::new().dim()));
        }
        lines.push(Line::from(spans));
        Cell::lines(lines)
    }
}

/// What the box is titled with, by the question's kind (rpctl
/// `modes.KINDS`).
pub fn kind_label(kind: Option<QuestionKind>) -> &'static str {
    match kind {
        Some(QuestionKind::Destructive) => "destructive command",
        Some(QuestionKind::SecretRead) => "env-file read",
        Some(QuestionKind::Consult) => "ask_claude",
        Some(QuestionKind::Network) => "network access",
        Some(QuestionKind::Fetch) => "fetch_url",
        Some(QuestionKind::ProtectedWrite) => "protected write",
        Some(QuestionKind::Unsandboxed) => "unsandboxed command",
        Some(QuestionKind::Remote) => "ssh command",
        Some(_) | None => "question",
    }
}

/// What a typed answer shows as: rp's `y`, `n`, `a`, a pick's number.
pub fn answer_word(answer: Answer) -> String {
    match answer {
        Answer::Yes => "y".into(),
        Answer::No => "n".into(),
        Answer::Always => "a".into(),
        Answer::Pick(n) => (u64::from(n) + 1).to_string(),
    }
}

/// The question as rp asks it: the prompt and its choices, in yellow.
fn prompt_spans(question: &Question) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!("{} {}", question.prompt, question.hint()),
        Style::new().yellow(),
    )]
}

/// The keys the box takes, dim until it takes them.
fn keys_spans(question: &Question, armed: bool) -> Vec<Span<'static>> {
    let mark = |default: bool| if default { " (default)" } else { "" };
    let mut keys: Vec<(&str, String)> = if question.choices == Choices::Pick {
        vec![
            ("↑↓", "choose".into()),
            ("Enter", "pick".into()),
            ("q", "cancel".into()),
        ]
    } else {
        vec![
            ("y", format!("yes{}", mark(question.default == Answer::Yes))),
            ("n", format!("no{}", mark(question.default == Answer::No))),
        ]
    };
    if question.choices == Choices::YesNoAlways {
        keys.push((
            "a",
            format!("always{}", mark(question.default == Answer::Always)),
        ));
    }
    if question.kind.is_some() {
        keys.push(("⇧Tab", "mode".into()));
    }
    let mut spans = Vec::new();
    for (i, (key, what)) in keys.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::raw(format!("[{key}]")).cyan());
        spans.push(Span::raw(format!(" {what}")));
    }
    if !armed {
        for span in &mut spans {
            span.style = Style::new().dim();
        }
    }
    spans
}

/// `spans` word-wrapped to `width`, each row with the cells it takes.
fn wrapped(spans: &[Span<'_>], width: usize) -> Vec<(Line<'static>, usize)> {
    wrap_spans(spans, width)
        .into_iter()
        .map(|row| (Line::from(row.spans), row.width))
        .collect()
}

/// `┌ title ───┐` across `width`; the title is cut to fit.
fn top_border(title: &str, width: usize, style: Style) -> Line<'static> {
    let room = width - 4;
    let title: String = if text_width(title) + 2 > room {
        title.chars().take(room.saturating_sub(2)).collect()
    } else {
        title.to_string()
    };
    let rest = width - 4 - text_width(&title);
    Line::from(Span::styled(
        format!("┌ {title} {}┐", "─".repeat(rest)),
        style,
    ))
}

#[cfg(test)]
#[path = "question_tests.rs"]
mod tests;
