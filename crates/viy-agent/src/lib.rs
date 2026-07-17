//! The chat engine: request payloads, SSE streaming, the system prompt and
//! instruction files, context accounting, sessions and file history, the tool
//! loop, elision and compaction, tracing, the explore workflow, `ask_claude`
//! and the toolbox. It emits events and asks through requests; it never
//! touches the terminal (docs/plan.md 5.2, kb d022, d023).
