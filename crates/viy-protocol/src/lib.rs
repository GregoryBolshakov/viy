//! What passes between the engine and whatever shows it (docs/ui-plan.md 8,
//! kb d023), shaped after Codex's `codex-protocol` crate: the UI submits
//! [`Op`]s, the engine sends back [`Event`]s tagged with the id of the
//! submission they belong to, and the engine's questions go to a
//! [`Frontend`]. Both enums are `#[non_exhaustive]` and grow with their
//! consumers (the TUI, the plain renderer, `viy dev drive`, tests); names
//! follow Codex's where one fits.
//!
//! Its own crate so the pod commands (failover questions, WP 2.8) and the chat
//! engine share one frontend without depending on each other.

mod event;
mod frontend;
mod op;
mod question;

pub use event::*;
pub use frontend::Frontend;
pub use op::Op;
pub use op::Submission;
pub use question::Answer;
pub use question::Choices;
pub use question::Question;
pub use question::QuestionKind;

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
