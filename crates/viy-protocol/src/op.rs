use serde::Deserialize;
use serde::Serialize;

use crate::Answer;

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
    /// Shift+Tab: the next chat mode (rpctl `modes.after`); the engine
    /// answers with [`crate::EventMsg::ModeChanged`].
    CycleMode,
    /// Leave the chat; the engine answers with
    /// [`crate::EventMsg::ShutdownComplete`].
    Shutdown,
}
