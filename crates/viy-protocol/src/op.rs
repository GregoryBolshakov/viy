use serde::Deserialize;
use serde::Serialize;

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
    /// A line entered at the prompt: starts a turn.
    UserTurn { text: String },
    /// Stop the running turn (Ctrl-C); the engine answers with
    /// [`crate::EventMsg::TurnAborted`].
    Interrupt,
    /// Leave the chat.
    Shutdown,
}
