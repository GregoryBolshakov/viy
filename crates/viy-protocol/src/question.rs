use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;
use serde::Serialize;

use crate::NoticeEvent;

/// A question on its way to the user: resolves to the answer. The chat's
/// engine resolves it when [`crate::Op::QuestionAnswer`] arrives, a
/// terminal when a line is read.
pub type Asking = Pin<Box<dyn Future<Output = Answer> + Send>>;

/// A question the engine asks the user (rp's `_confirm`, the failover and
/// resume questions). The chat mode's auto-yes is decided by the engine
/// before a question reaches the UI (rpctl `modes.py`); the UI only shows
/// and collects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Lets a chat mode answer for the user; `None` always waits
    /// (`/mount`'s "read-write?").
    pub kind: Option<QuestionKind>,
    pub prompt: String,
    pub choices: Choices,
    /// What Enter alone answers.
    pub default: Answer,
    /// What is answered with nobody to ask: no for every approval
    /// (rp's `_confirm` without a TTY), the default for `viy up`'s questions
    /// (rp's `pod._ask`).
    pub unattended: Answer,
    /// What rp prints on the lines before the question (why it asks, the
    /// reviewer's verdict): shown with it, and kept with its answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason: Vec<NoticeEvent>,
    /// What [`Choices::Pick`] picks from, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

impl Question {
    /// `[y/N]`: Enter and no terminal both answer no.
    pub fn yes_no(kind: Option<QuestionKind>, prompt: impl Into<String>) -> Self {
        Self {
            kind,
            prompt: prompt.into(),
            choices: Choices::YesNo,
            default: Answer::No,
            unattended: Answer::No,
            reason: Vec::new(),
            options: Vec::new(),
        }
    }

    /// `[y/N/a=always]`: as [`Question::yes_no`], with "always" offered.
    pub fn yes_no_always(kind: Option<QuestionKind>, prompt: impl Into<String>) -> Self {
        Self {
            choices: Choices::YesNoAlways,
            ..Self::yes_no(kind, prompt)
        }
    }

    /// One of `options` (rp's numbered lists: `/rewind`, `/resume`):
    /// Enter takes `default`, nobody to ask and `q` take none
    /// ([`Answer::No`]).
    pub fn pick(prompt: impl Into<String>, options: Vec<String>, default: u32) -> Self {
        Self {
            choices: Choices::Pick,
            default: Answer::Pick(default),
            options,
            ..Self::yes_no(None, prompt)
        }
    }

    /// The choices as rp writes them after the prompt: `[y/N]`, `[Y/n]`
    /// (rp's `pod._ask` when the default is yes), `[y/N/a=always]`, and
    /// for a pick rp's `/rewind` `[1-3, Enter = 3, q = cancel]`.
    pub fn hint(&self) -> String {
        match (self.choices, self.default) {
            (Choices::Pick, default) => {
                let enter = match default {
                    Answer::Pick(n) => format!(", Enter = {}", n + 1),
                    _ => String::new(),
                };
                format!("[1-{}{enter}, q = cancel]", self.options.len())
            }
            (Choices::YesNo, Answer::No) => "[y/N]".into(),
            (Choices::YesNo, _) => "[Y/n]".into(),
            (_, Answer::No) => "[y/N/a=always]".into(),
            _ => "[Y/n/a=always]".into(),
        }
    }
}

/// The kinds of question a chat mode may answer (rpctl `modes.KINDS`; the
/// serialized names are rp's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum QuestionKind {
    /// A destructive command in the real workspace.
    Destructive,
    /// Read an env file that may hold API keys.
    SecretRead,
    /// Hand a task to Claude Code (`ask_claude`).
    Consult,
    /// A sandboxed command wants the internet.
    Network,
    /// `fetch_url` a page nothing vouched for.
    Fetch,
    /// Write the client's code, `.git`, `.venv`, env files...
    ProtectedWrite,
    /// The sandbox is broken: run on the host?
    Unsandboxed,
    /// A command on a server the user lent with `/ssh`.
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Choices {
    YesNo,
    YesNoAlways,
    /// One of [`Question::options`].
    Pick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Yes,
    No,
    /// Yes, and remember it (only offered with [`Choices::YesNoAlways`]).
    Always,
    /// The option at this index (only with [`Choices::Pick`]).
    Pick(u32),
}
