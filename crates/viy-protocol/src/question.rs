use serde::Deserialize;
use serde::Serialize;

/// A question the engine asks the user (rp's `_confirm`, the failover and
/// resume questions). The chat mode's auto-yes is decided by the engine
/// before a question reaches the frontend (rpctl `modes.py`); the frontend
/// only shows and collects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Lets a chat mode answer for the user; `None` always waits
    /// (`/mount`'s "read-write?").
    pub kind: Option<QuestionKind>,
    pub prompt: String,
    pub choices: Choices,
    /// What Enter alone answers.
    pub default: Answer,
    /// What a frontend with nobody to ask answers: no for every approval
    /// (rp's `_confirm` without a TTY), the default for `viy up`'s questions
    /// (rp's `pod._ask`).
    pub unattended: Answer,
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
        }
    }

    /// `[y/N/a=always]`: as [`Question::yes_no`], with "always" offered.
    pub fn yes_no_always(kind: Option<QuestionKind>, prompt: impl Into<String>) -> Self {
        Self {
            choices: Choices::YesNoAlways,
            ..Self::yes_no(kind, prompt)
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Choices {
    YesNo,
    YesNoAlways,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Yes,
    No,
    /// Yes, and remember it (only offered with [`Choices::YesNoAlways`]).
    Always,
}
