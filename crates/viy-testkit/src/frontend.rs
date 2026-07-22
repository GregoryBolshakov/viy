//! A [`Frontend`] for tests: it records every event and question, and
//! answers questions from a script, the way a scenario's `approvals` answer
//! rp's questions in `tools/drive_rp.py` (tests/scenarios/README.md).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::PoisonError;

use viy_protocol::Answer;
use viy_protocol::Event;
use viy_protocol::Frontend;
use viy_protocol::Question;

/// What the frontend was shown, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Shown {
    Event(Event),
    Question { question: Question, answer: Answer },
}

#[derive(Debug, Default)]
pub struct ScriptedFrontend {
    /// `(part of the prompt, answer)`, in the order the questions come.
    script: Mutex<VecDeque<(String, Answer)>>,
    /// No script: every question gets its `unattended` answer.
    unattended: bool,
    shown: Mutex<Vec<Shown>>,
    problems: Mutex<Vec<String>>,
}

impl ScriptedFrontend {
    pub fn new<'a>(script: impl IntoIterator<Item = (&'a str, Answer)>) -> Self {
        Self {
            script: Mutex::new(
                script
                    .into_iter()
                    .map(|(p, a)| (p.to_string(), a))
                    .collect(),
            ),
            ..Self::default()
        }
    }

    /// Nobody at the terminal (rp without a TTY).
    pub fn unattended() -> Self {
        Self {
            unattended: true,
            ..Self::default()
        }
    }

    pub fn shown(&self) -> Vec<Shown> {
        lock(&self.shown).clone()
    }

    pub fn events(&self) -> Vec<Event> {
        lock(&self.shown)
            .iter()
            .filter_map(|s| match s {
                Shown::Event(e) => Some(e.clone()),
                Shown::Question { .. } => None,
            })
            .collect()
    }

    /// Panics if a question was not the scripted one, or a scripted answer
    /// was never asked for. Called at the end of a test, so a question asked
    /// on another thread cannot slip by.
    pub fn assert_done(&self) {
        let mut problems = lock(&self.problems).clone();
        problems.extend(
            lock(&self.script)
                .iter()
                .map(|(p, _)| format!("scripted answer never asked for: {p:?}")),
        );
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}

impl Frontend for ScriptedFrontend {
    fn emit(&self, event: Event) {
        lock(&self.shown).push(Shown::Event(event));
    }

    fn ask(&self, question: &Question) -> Answer {
        let answer = if self.unattended {
            question.unattended
        } else {
            match lock(&self.script).pop_front() {
                Some((part, answer)) if question.prompt.contains(&part) => answer,
                expected => {
                    lock(&self.problems).push(format!(
                        "unscripted question {:?} (script expected {:?})",
                        question.prompt,
                        expected.map(|(p, _)| p)
                    ));
                    question.unattended
                }
            }
        };
        lock(&self.shown).push(Shown::Question {
            question: question.clone(),
            answer,
        });
        answer
    }
}

/// A test that panicked while holding a lock should not hide what was
/// recorded.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[path = "frontend_tests.rs"]
mod tests;
