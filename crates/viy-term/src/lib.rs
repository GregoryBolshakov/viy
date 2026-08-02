//! viy's own terminal UI (docs/ui-plan.md): the inline viewport with the
//! committed transcript in scrollback and a live region below it, the input
//! composer, streaming Markdown, questions, the footer, the terminal-escape
//! sanitizer and the tab title; plus the plain renderer for pipes and
//! `viy ask` (ROADMAP phase 3).

pub mod history;
pub mod terminal;
pub mod textarea;

#[cfg(test)]
mod test_backend;
