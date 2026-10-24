//! The chat modes (rpctl's `modes.py`): who answers the questions a turn
//! asks.
//!
//! Shift+Tab steps to the next mode in [`MODES`] and wraps around; `/mode
//! <name>` picks one. A chat starts in CHAT_MODE (default normal), and a
//! project's config cannot start it anywhere else (viy-config's layering).
//!
//! The modes never change what the approver decides: the sandbox, the
//! egress proxy, the reviewer's refusals and the quarantine work the same in
//! all of them. They only decide which of the approver's questions get
//! answered without the user: every question names its kind, and a mode
//! answers yes to the kinds in its `auto_yes` and leaves the rest. It
//! answers plain yes, never "always", so it saves no rule. A question
//! without a kind always waits, and with nobody to ask nothing is answered
//! yes (the engine's `attended`).

use viy_protocol::QuestionKind;

/// rpctl's `modes.KINDS`, in its order.
pub const KINDS: [QuestionKind; 8] = [
    QuestionKind::Destructive,
    QuestionKind::SecretRead,
    QuestionKind::Consult,
    QuestionKind::Network,
    QuestionKind::Fetch,
    QuestionKind::ProtectedWrite,
    QuestionKind::Unsandboxed,
    QuestionKind::Remote,
];

#[derive(Debug, PartialEq, Eq)]
pub struct Mode {
    pub name: &'static str,
    /// Shown by the prompt (viy's footer); empty shows nothing.
    pub label: &'static str,
    /// One line for Shift+Tab and `/mode`.
    pub blurb: &'static str,
    pub auto_yes: &'static [QuestionKind],
}

/// The modes, in carousel order. To add one, append it.
pub static MODES: [Mode; 2] = [
    Mode {
        name: "normal",
        label: "",
        blurb: "every question waits for your answer",
        auto_yes: &[],
    },
    // Writes to code that runs outside the sandbox and unsandboxed commands
    // still ask (Gregory, 2026-07-11); internet-facing questions joined the
    // yes list on 2026-07-13, commands on /ssh servers on 2026-07-16.
    Mode {
        name: "auto-yes",
        label: "⏵⏵ auto-yes",
        blurb: "says yes to destructive commands, env-file reads, ask_claude, network \
                commands, fetch_url and commands on /ssh servers; the client's own files \
                still ask",
        auto_yes: &[
            QuestionKind::Destructive,
            QuestionKind::SecretRead,
            QuestionKind::Consult,
            QuestionKind::Network,
            QuestionKind::Fetch,
            QuestionKind::Remote,
        ],
    },
];

/// The mode a chat starts in when CHAT_MODE names none.
pub static DEFAULT: &Mode = &MODES[0];

impl Mode {
    /// Whether this mode says yes to a question of `kind`.
    pub fn answers(&self, kind: Option<QuestionKind>) -> bool {
        kind.is_some_and(|k| self.auto_yes.contains(&k))
    }

    /// rp chat's mode line: the label (or name), the blurb, the key.
    pub fn line(&self) -> String {
        let name = if self.label.is_empty() {
            self.name
        } else {
            self.label
        };
        format!("{name} -- {} (shift+tab to cycle)", self.blurb)
    }
}

/// The mode called `name`, case and surrounding spaces ignored.
pub fn get(name: &str) -> Option<&'static Mode> {
    let name = name.trim().to_lowercase();
    MODES.iter().find(|m| m.name == name)
}

/// The next mode in the carousel.
pub fn after(mode: &Mode) -> &'static Mode {
    let i = MODES.iter().position(|m| m == mode).unwrap_or(0);
    &MODES[(i + 1) % MODES.len()]
}

/// A kind as rp's report writes it (`secret_read` → `secret read`).
pub fn kind_words(kind: QuestionKind) -> &'static str {
    match kind {
        QuestionKind::Destructive => "destructive",
        QuestionKind::SecretRead => "secret read",
        QuestionKind::Consult => "consult",
        QuestionKind::Network => "network",
        QuestionKind::Fetch => "fetch",
        QuestionKind::ProtectedWrite => "protected write",
        QuestionKind::Unsandboxed => "unsandboxed",
        QuestionKind::Remote => "remote",
        _ => "question",
    }
}

#[cfg(test)]
#[path = "modes_tests.rs"]
mod tests;
