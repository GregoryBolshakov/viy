//! viy's own terminal UI (docs/ui-plan.md): the alternate screen, with the
//! transcript viy draws and scrolls itself above a live region, as in
//! Codex's default layout (kb d038); the input composer, streaming Markdown,
//! questions, the footer, the terminal-escape sanitizer and the tab title;
//! plus the plain renderer for pipes and `viy ask` (ROADMAP phase 3).

pub mod app;
pub mod frame;
pub mod plain;
pub mod sanitize;
pub mod terminal;
pub mod textarea;
pub mod tmux;
pub mod transcript;
pub mod tui;
pub mod wrap;

#[cfg(test)]
mod test_backend;
