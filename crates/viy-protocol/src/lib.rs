//! What passes between the engine and whatever shows it (docs/ui-plan.md 8,
//! kb d023), shaped after Codex's `codex-protocol` crate: the UI submits
//! [`Op`]s, the engine sends back [`Event`]s tagged with the id of the
//! submission they belong to. A question is an event too
//! ([`EventMsg::QuestionRequest`]), answered by [`Op::QuestionAnswer`] naming
//! its id, as Codex's `RequestUserInput` and `UserInputAnswer` (kb d036).
//! Both enums are `#[non_exhaustive]` and grow with their consumers (the
//! TUI, the plain renderer, `viy dev drive`, tests); names follow Codex's
//! where one fits.
//!
//! Its own crate so the pod commands (failover questions, WP 2.8) and the chat
//! engine share these types without depending on each other.

mod event;
mod op;
mod question;

pub use event::*;
pub use op::Op;
pub use op::Submission;
pub use question::Answer;
pub use question::Asking;
pub use question::AskingSecret;
pub use question::Choices;
pub use question::Question;
pub use question::QuestionKind;
pub use question::Secret;

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
