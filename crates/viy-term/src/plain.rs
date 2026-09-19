//! The plain renderer (ui-plan 10): line output for pipes, scripts and
//! `viy ask`, with no live region, no colour and no escape sequence of its
//! own. The answer is written as it streams, everything else on lines of
//! its own; a question has nobody to ask and takes its unattended answer
//! (no for every approval, as rp's `_confirm` without a TTY).

use std::io;
use std::io::Write;

use viy_protocol::EventMsg;
use viy_protocol::Op;
use viy_protocol::TurnOutcome;

use crate::app::answer_word;
use crate::app::choices;

pub struct Plain<W: Write> {
    out: W,
    /// The last thing written did not end its line.
    mid_line: bool,
}

impl<W: Write> Plain<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            mid_line: false,
        }
    }

    pub fn into_inner(self) -> W {
        self.out
    }

    /// Writes what `msg` shows; the answer to send back for a question.
    pub fn event(&mut self, msg: &EventMsg) -> io::Result<Option<Op>> {
        match msg {
            EventMsg::AgentMessageContentDelta(e) => self.text(&e.delta)?,
            EventMsg::TurnComplete(e) => {
                self.end_line()?;
                if let TurnOutcome::Failed { message } = &e.outcome {
                    self.line(&format!("✗ {message}"))?;
                }
            }
            EventMsg::TurnAborted(_) => {
                self.end_line()?;
                self.line("■ interrupted")?;
            }
            EventMsg::Notice(notice) => self.line(&notice.message)?,
            EventMsg::ToolCallBegin(e) => self.line(&format!("▸ {}  {}", e.tool, e.summary))?,
            EventMsg::QuestionRequest(e) => {
                let answer = e.question.unattended;
                self.line(&format!(
                    "{} {} {} (no terminal to ask)",
                    e.question.prompt,
                    choices(e.question.choices),
                    answer_word(answer)
                ))?;
                self.out.flush()?;
                return Ok(Some(Op::QuestionAnswer {
                    id: e.question_id.clone(),
                    answer,
                }));
            }
            _ => return Ok(None),
        }
        self.out.flush()?;
        Ok(None)
    }

    fn text(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.out.write_all(visible(text).as_bytes())?;
        self.mid_line = !text.ends_with('\n');
        Ok(())
    }

    /// `text` on a line of its own.
    fn line(&mut self, text: &str) -> io::Result<()> {
        self.end_line()?;
        self.text(text)?;
        self.end_line()
    }

    fn end_line(&mut self) -> io::Result<()> {
        if std::mem::take(&mut self.mid_line) {
            self.out.write_all(b"\n")?;
        }
        Ok(())
    }
}

/// Controls other than newline and tab as visible symbols: only viy writes
/// escape sequences, and the plain renderer writes none (ui-plan 7).
fn visible(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            '\0'..='\x1f' => char::from_u32(0x2400 + u32::from(c)).unwrap_or('\u{fffd}'),
            '\x7f' => '\u{2421}',
            '\u{80}'..='\u{9f}' => '\u{fffd}',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
#[path = "plain_tests.rs"]
mod tests;
