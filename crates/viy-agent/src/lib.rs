//! The chat engine: request payloads, SSE streaming, the system prompt and
//! instruction files, context accounting, sessions and file history, the tool
//! loop, elision and compaction, tracing, the explore workflow, `ask_claude`
//! and the toolbox. It emits events and asks through requests; it never
//! touches the terminal (docs/plan.md 5.2, kb d022, d023).
//!
//! So far the submission loop ([`Engine`], [`Turns`]), the chat's turn
//! without tools ([`chat`]: the payload, the streamed reply, the system
//! prompt with the instruction files, the context accounting, the title),
//! the session logs ([`sessions`]), the model's server as the chat sees
//! it ([`server`]) and the scenario driver ([`drive`]).

pub mod chat;
pub mod context;
pub mod drive;
mod engine;
pub mod filehistory;
pub mod files;
pub mod instructions;
pub mod kb;
pub mod payload;
pub mod prompt;
pub mod server;
pub mod sessions;
pub mod stream;
pub mod toolbox;
pub mod trace;

pub use engine::BoxFuture;
pub use engine::Engine;
pub use engine::EngineStopped;
pub use engine::OpenSession;
pub use engine::TurnContext;
pub use engine::Turns;
