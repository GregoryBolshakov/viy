//! `viy archive`, `viy unarchive` and `viy delete`: the saved chats as
//! Codex's session commands handle them (kb d083; Codex 970111f
//! `tui/src/session_archive_commands.rs`). A chat is named by its id or
//! its exact name; archived chats move to `archived_sessions/` and come
//! back whole; a deleted one is gone, after a question unless `--force`
//! with an id.

use std::io::BufRead;
use std::io::Write;
use std::sync::Arc;

use viy_agent::sessions::LookupError;
use viy_agent::sessions::SessionError;
use viy_agent::sessions::Sessions;
use viy_agent::sessions::Summary;
use viy_agent::sessions::is_session_id;
use viy_config::Paths;
use viy_time::SystemTimeProvider;

use crate::Failure;
use crate::cli::DeleteArgs;
use crate::cli::SessionArg;

/// What a session command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Archive,
    Unarchive,
    Delete,
}

/// How `viy delete` asks before deleting.
pub struct Asking<'a> {
    /// Stdin and stderr are terminals.
    pub can_ask: bool,
    pub input: &'a mut dyn BufRead,
    pub err: &'a mut dyn Write,
}

/// The chat `target` names in `store` (its id, else the exact name of one
/// interactive chat), or Codex's words for none; `place` says where it
/// looked ("active ", "archived ", or nothing).
pub fn find(store: &Sessions, target: &str, place: &str) -> Result<Summary, Failure> {
    store.lookup(target).map_err(|e| match e {
        LookupError::Missing => {
            Failure::from(format!("No {place}session found matching '{target}'."))
        }
        LookupError::Ambiguous(a, b) => Failure::from(format!(
            "Multiple sessions match '{target}' (including {a} and {b}); use a session id to \
             disambiguate."
        )),
    })
}

/// Runs `viy archive` or `viy unarchive`.
pub fn cmd_archive(
    action: Action,
    target: &SessionArg,
    paths: &Paths,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let active = Sessions::new(paths.sessions_dir(), Arc::new(SystemTimeProvider));
    let archived = active.archived();
    let (from, place) = match action {
        Action::Unarchive => (&archived, "archived "),
        _ => (&active, "active "),
    };
    let row = find(from, &target.session, place)?;
    let done = match action {
        Action::Unarchive => active.unarchive(&row.id),
        _ => active.archive(&row.id),
    };
    done.map_err(|e| failure(e, &row.id))?;
    say(out, &success(action, &row, &target.session))
}

/// Runs `viy delete`.
pub fn cmd_delete(
    args: &DeleteArgs,
    paths: &Paths,
    asking: Asking<'_>,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let target = &args.target.session;
    let active = Sessions::new(paths.sessions_dir(), Arc::new(SystemTimeProvider));
    let archived = active.archived();
    if args.force && !is_session_id(target) {
        return Err("--force requires a session id; names must be confirmed interactively".into());
    }
    let (store, row) = match find(&active, target, "") {
        Ok(row) => (&active, row),
        Err(_) => (&archived, find(&archived, target, "active or archived ")?),
    };
    if !args.force {
        if !asking.can_ask {
            return Err(
                "cannot confirm session deletion without an interactive terminal; \
                        rerun with --force and a session id"
                    .into(),
            );
        }
        let question = format!(
            "Permanently delete session '{}' ({})?\nThis cannot be undone.\nContinue? [y/N]: ",
            row.title, row.id
        );
        asking
            .err
            .write_all(question.as_bytes())
            .and_then(|()| asking.err.flush())
            .map_err(|e| Failure::from(e.to_string()))?;
        let mut answer = String::new();
        let _ = asking.input.read_line(&mut answer);
        let answer = answer.trim();
        if !(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")) {
            return say(out, "Delete cancelled.");
        }
    }
    store.delete(&row.id).map_err(|e| failure(e, &row.id))?;
    say(out, &success(Action::Delete, &row, target))
}

/// Codex's `success_message`: the name too when the chat was named by it.
fn success(action: Action, row: &Summary, target: &str) -> String {
    let done = match action {
        Action::Archive => "Archived",
        Action::Unarchive => "Unarchived",
        Action::Delete => "Deleted",
    };
    if target == row.id {
        format!("{done} session {}.", row.id)
    } else {
        format!("{done} session {} ({}).", row.title, row.id)
    }
}

fn failure(e: SessionError, id: &str) -> Failure {
    match e {
        SessionError::Busy(_) => {
            Failure::from(format!("session {id} is open in a viy chat; quit it first"))
        }
        SessionError::Io(e) => Failure::from(e.to_string()),
    }
}

fn say(out: &mut dyn Write, text: &str) -> Result<(), Failure> {
    writeln!(out, "{text}").map_err(|e| Failure::from(e.to_string()))
}

#[cfg(test)]
#[path = "sessions_cli_tests.rs"]
mod tests;
