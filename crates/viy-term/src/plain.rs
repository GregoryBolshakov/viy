//! The plain renderer (ui-plan 10): line output for pipes, scripts and
//! `viy ask`, with no live region, no colour and no escape sequence of its
//! own: every string goes through the sanitizer (ui-plan 7) as on the
//! screen, since stdout may still be a terminal. The answer is written as
//! it streams, everything else on lines of its own; a question has nobody
//! to ask and takes its unattended answer (no for every approval, as rp's
//! `_confirm` without a TTY).

use std::io;
use std::io::Write;

use viy_protocol::EventMsg;
use viy_protocol::Op;
use viy_protocol::ToolCallStatus;
use viy_protocol::TurnOutcome;

use crate::question::answer_word;
use crate::sanitize::sanitize_after;

pub struct Plain<W: Write> {
    out: W,
    /// The last thing written did not end its line.
    mid_line: bool,
    /// The last char written, for the sanitizer's joiners.
    last: Option<char>,
}

impl<W: Write> Plain<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            mid_line: false,
            last: None,
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
            // rp prints a command's output after it, then its result.
            EventMsg::ToolCallOutputDelta(e) => {
                self.end_line()?;
                self.text(&e.chunk)?;
            }
            EventMsg::ToolCallEnd(e) => {
                let mut said: Vec<String> = e.detail.iter().cloned().collect();
                match e.status {
                    ToolCallStatus::Completed => {}
                    ToolCallStatus::Failed => said.push("failed".into()),
                    ToolCallStatus::Declined => said.push("declined".into()),
                    _ => said.push("interrupted".into()),
                }
                if !said.is_empty() {
                    self.line(&format!("  [{}]", said.join(", ")))?;
                }
            }
            EventMsg::QuestionRequest(e) => {
                let answer = e.question.unattended;
                for notice in &e.question.reason {
                    self.line(&notice.message)?;
                }
                self.line(&format!(
                    "{} {} {} (no terminal to ask)",
                    e.question.prompt,
                    e.question.hint(),
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
        let shown = sanitize_after(self.last, text);
        self.out.write_all(shown.as_bytes())?;
        self.last = shown.chars().next_back();
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
            self.last = Some('\n');
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "plain_tests.rs"]
mod tests;
