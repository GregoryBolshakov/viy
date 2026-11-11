use serde::Deserialize;
use serde::Serialize;

use crate::Answer;
use crate::Secret;

/// One request from the UI, with the id its events will carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    pub id: String,
    pub op: Op,
}

/// What the UI asks the engine to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Op {
    /// A line entered at the prompt: starts a turn, or waits for the one
    /// running to end.
    UserTurn { text: String },
    /// A slash command, `/` included; the engine answers with
    /// [`crate::EventMsg::CommandComplete`] once it has run.
    Command { text: String },
    /// Stop the running turn (Ctrl-C); the engine answers with
    /// [`crate::EventMsg::TurnAborted`].
    Interrupt,
    /// The user's answer to the [`crate::EventMsg::QuestionRequest`] with
    /// this `id`.
    QuestionAnswer { id: String, answer: Answer },
    /// The user's answer to the [`crate::EventMsg::SecretRequest`] with
    /// this `id`; `None`: they gave none.
    SecretAnswer { id: String, secret: Option<Secret> },
    /// Shift+Tab: the next chat mode (rpctl `modes.after`); the engine
    /// answers with [`crate::EventMsg::ModeChanged`]. `question` is the
    /// question Shift+Tab was pressed at, which the new mode answers if it
    /// answers its kind (rp's `_shift_tab`); a question the user was not
    /// looking at is never answered this way.
    CycleMode { question: Option<String> },
    /// `viy resume` and `viy fork` (kb d083): go on in saved chat `id`, or
    /// without one in the chat picked from the saved chats, as Codex's
    /// resume picker; in a copy of it with `fork`. `all`: every project's
    /// chats in the pick; `include_exec`: the one-shot chats of `viy exec`
    /// too. The engine answers with [`crate::EventMsg::CommandComplete`]
    /// for `/resume`.
    OpenSession {
        id: Option<String>,
        all: bool,
        include_exec: bool,
        fork: bool,
    },
    /// Leave the chat; the engine answers with
    /// [`crate::EventMsg::ShutdownComplete`].
    Shutdown,
}
