//! Approval policy for the model's shell commands (rpctl's `approve.py`:
//! the parsers, the policy and the network reviewer; quarantine comes with
//! the sandbox).
//!
//! Every command runs in the bubblewrap sandbox without asking. A question
//! comes only when the command looks destructive to the workspace (`rm -r`,
//! `git clean`...), or wants the network and touches something sensitive
//! (rp, viy, tok, ssh, pipe-to-shell), or internet text is in the chat, or
//! the reviewer rates it high or fails. Whether a question waits for the
//! user or is answered yes is the chat mode's call, never this module's.
//!
//! The sandbox is the security boundary. These checks are a speed bump for
//! honest mistakes, not a defence against a model trying to get past them.
//!
//! Each rule is checked in two views of the command (kb d033, d090): the
//! argv view (`argv`), Codex's structure, which sees the program and options
//! the shell will run past quotes and wrappers; and the literal view
//! (`literal`), the text cut into tokens as rpctl's patterns read it, which
//! sees commands handed to another program as data. A rule matches when
//! either view matches, so viy asks wherever rpctl asks, and in a few places
//! more.

mod argv;
mod chars;
mod decide;
mod literal;
mod pipe;
mod plain;
mod review;
mod tmp_rm;

use std::path::Path;

pub use chars::is_py_space;
pub use decide::Decision;
pub use decide::Facts;
pub use decide::Hearing;
pub use decide::Pending;
pub use decide::decide;
use literal::Literal;
pub use pipe::pipes_program_into_interpreter;
pub use pipe::split_shell;
pub use plain::NO_PREFIX;
pub use plain::Op;
pub use plain::Pipeline;
pub use plain::add_rule;
pub use plain::load_rules;
pub use plain::parse_rules;
pub use plain::plain_pipeline;
pub use plain::plain_segments;
pub use plain::prefix_for;
pub use plain::rules_allow_net;
pub use review::REVIEW_PRIVATE_GIT;
pub use review::REVIEW_REMOTE_SYSTEM;
pub use review::REVIEW_SYSTEM;
pub use review::Review;
pub use review::Risk;
pub use review::Verdict;
pub use review::files_for_review;
pub use review::parse_verdict;
pub use review::review_schema;
pub use tmp_rm::sandbox_tmp_rm;

/// After this many refusals in a row the turn is stopped, so a model that
/// keeps rephrasing a denied action cannot grind the reviewer (or the user)
/// down.
pub const MAX_CONSECUTIVE_DENIALS: usize = 3;

/// The file name of the rules file in viy's config directory.
pub const RULES_FILE: &str = "approval.rules";

/// Why `cmd` needs a question because it is destructive to the workspace,
/// or `None`. Still sandboxed; these only add a question because the
/// workspace is the user's real project and has no undo. Pass `sandbox_ws`
/// (the workspace) only when `cmd` will run in the sandbox: deletes
/// confined to its throwaway `/tmp` then go through without asking.
pub fn destructive_reason(cmd: &str, sandbox_ws: Option<&Path>) -> Option<&'static str> {
    let lit = Literal::new(cmd);
    let cmds = argv::commands(cmd);
    let any = |f: fn(&[String]) -> bool| cmds.iter().any(|a| f(a));
    if (lit.rm() || any(argv::rm)) && !sandbox_ws.is_some_and(|ws| sandbox_tmp_rm(cmd, ws)) {
        return Some("recursive/forced delete");
    }
    // The git ones overwrite or delete working-tree files; with the model's
    // own writable .git they succeed.
    if lit.git() || any(argv::git) {
        return Some("discards uncommitted work");
    }
    if lit.find_delete() || any(argv::find_delete) {
        return Some("find -delete");
    }
    if lit.overwrite() || any(argv::overwrite) {
        return Some("overwrites data");
    }
    if lit.bulk_permissions() || any(argv::bulk_permissions) {
        return Some("bulk permission change");
    }
    None
}

/// Programs that control pods (rp, viy) or the token vault.
const POD_AND_VAULT: [&str; 3] = ["rp", "tok", "viy"];
const REMOTE: [&str; 4] = ["ssh", "scp", "sftp", "rsync"];
const RAW_SOCKET: [&str; 4] = ["nc", "ncat", "netcat", "socat"];

/// Why `cmd`, given the network, must go to the user and never to the
/// reviewer: it spends money (rp, viy), touches credentials (tok, ssh keys)
/// or runs unseen code. `None` otherwise.
pub fn net_ask_reason(cmd: &str) -> Option<&'static str> {
    net_ask(cmd, &POD_AND_VAULT)
}

/// [`net_ask_reason`] with rpctl's own names only (rp and tok, not viy):
/// where only viy's name asks, the decision still consults the reviewer
/// first, so a critical verdict refuses as in rpctl.
pub(crate) fn rp_net_ask_reason(cmd: &str) -> Option<&'static str> {
    net_ask(cmd, &POD_AND_VAULT[..2])
}

fn net_ask(cmd: &str, pods: &[&str]) -> Option<&'static str> {
    let lit = Literal::new(cmd);
    let cmds = argv::commands(cmd);
    let any = |names: &[&str]| cmds.iter().any(|a| argv::program(a, names));
    if lit.program(pods) || any(pods) {
        return Some("controls pods or the token vault");
    }
    if lit.program(&REMOTE) || any(&REMOTE) {
        return Some("remote login / copy");
    }
    if lit.word(&RAW_SOCKET) || any(&RAW_SOCKET) {
        return Some("raw socket tool");
    }
    if pipes_program_into_interpreter(cmd) {
        return Some("pipes downloaded content into an interpreter");
    }
    None
}

#[cfg(test)]
#[path = "approve_tests.rs"]
mod tests;
