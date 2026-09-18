//! The chat engine: request payloads, SSE streaming, the system prompt and
//! instruction files, context accounting, sessions and file history, the tool
//! loop, elision and compaction, tracing, the explore workflow, `ask_claude`
//! and the toolbox. It emits events and asks through requests; it never
//! touches the terminal (docs/plan.md 5.2, kb d022, d023).
//!
//! So far the submission loop ([`Engine`], [`Turns`]), the chat's turn
//! without tools ([`chat`]: the payload, the streamed reply, the system
//! prompt's first parts, the title) and the scenario driver ([`drive`]).

pub mod chat;
pub mod drive;
mod engine;
pub mod payload;
pub mod prompt;
pub mod stream;

pub use engine::BoxFuture;
pub use engine::Engine;
pub use engine::EngineStopped;
pub use engine::TurnContext;
pub use engine::Turns;
