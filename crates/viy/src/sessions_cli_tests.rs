//! `viy archive`, `viy unarchive`, `viy delete` (kb d083), with Codex's
//! words.

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_testkit::scratch::ScratchDir;

use super::*;

struct Rig {
    paths: Paths,
    store: Sessions,
    _scratch: ScratchDir,
}

fn rig() -> Rig {
    let scratch = ScratchDir::new("sessions-cli").unwrap();
    let paths = Paths::under(scratch.path().join("home"));
    let store = Sessions::new(paths.sessions_dir(), Arc::new(SystemTimeProvider));
    Rig {
        paths,
        store,
        _scratch: scratch,
    }
}

impl Rig {
    /// A saved chat named `title`.
    fn chat(&self, title: &str) -> String {
        let mut log = self.store.create(Some("/p"), Some("m"));
        log.sync(None, &[json!({"role": "user", "content": "hi"})], false)
            .unwrap();
        log.rename(title).unwrap();
        log.close("exit").unwrap();
        log.id().to_string()
    }

    fn archive(&self, action: Action, target: &str) -> Result<String, Failure> {
        let mut out = Vec::new();
        let target = SessionArg {
            session: target.into(),
        };
        cmd_archive(action, &target, &self.paths, &mut out)
            .map(|()| String::from_utf8(out).unwrap())
    }

    /// `viy delete`, answering `answer` when asked: (stdout, stderr).
    fn delete(
        &self,
        target: &str,
        force: bool,
        can_ask: bool,
        answer: &str,
    ) -> Result<(String, String), Failure> {
        let args = DeleteArgs {
            target: SessionArg {
                session: target.into(),
            },
            force,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let asking = Asking {
            can_ask,
            input: &mut answer.as_bytes(),
            err: &mut err,
        };
        cmd_delete(&args, &self.paths, asking, &mut out).map(|()| {
            (
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        })
    }
}

fn message(failed: Result<impl std::fmt::Debug, Failure>) -> Option<String> {
    failed.err().and_then(|f| f.message)
}

#[test]
fn archive_and_unarchive_by_name_or_id() {
    let r = rig();
    let id = r.chat("Plan");
    assert_eq!(
        r.archive(Action::Archive, "Plan").unwrap(),
        format!("Archived session Plan ({id}).\n")
    );
    assert_eq!(r.store.list(None), vec![]);
    assert_eq!(
        message(r.archive(Action::Archive, "Plan")),
        Some("No active session found matching 'Plan'.".into())
    );
    assert_eq!(
        r.archive(Action::Unarchive, &id).unwrap(),
        format!("Unarchived session {id}.\n")
    );
    assert_eq!(r.store.list(None)[0].id, id);
    assert_eq!(
        message(r.archive(Action::Unarchive, &id)),
        Some(format!("No archived session found matching '{id}'."))
    );
}

#[test]
fn a_name_two_chats_share_needs_an_id() {
    let r = rig();
    r.chat("Plan");
    r.chat("Plan");
    let said = message(r.archive(Action::Archive, "Plan")).unwrap();
    assert!(
        said.starts_with("Multiple sessions match 'Plan' (including "),
        "{said}"
    );
    assert!(
        said.ends_with("); use a session id to disambiguate."),
        "{said}"
    );
}

#[test]
fn delete_asks_first_and_removes_for_good() {
    let r = rig();
    let id = r.chat("Plan");
    let (out, err) = r.delete("Plan", false, true, "n\n").unwrap();
    assert_eq!(out, "Delete cancelled.\n");
    assert_eq!(
        err,
        format!(
            "Permanently delete session 'Plan' ({id})?\nThis cannot be undone.\nContinue? [y/N]: "
        )
    );
    assert_eq!(r.store.list(None).len(), 1);
    let (out, _) = r.delete("Plan", false, true, "y\n").unwrap();
    assert_eq!(out, format!("Deleted session Plan ({id}).\n"));
    assert_eq!(r.store.list(None), vec![]);
    assert!(!r.store.path_of(&id).exists());
}

#[test]
fn delete_without_a_terminal_needs_force_and_an_id() {
    let r = rig();
    let id = r.chat("Plan");
    assert_eq!(
        message(r.delete("Plan", false, false, "")),
        Some(
            "cannot confirm session deletion without an interactive terminal; rerun with \
             --force and a session id"
                .into()
        )
    );
    assert_eq!(
        message(r.delete("Plan", true, false, "")),
        Some("--force requires a session id; names must be confirmed interactively".into())
    );
    let (out, err) = r.delete(&id, true, false, "").unwrap();
    assert_eq!(
        (out, err),
        (format!("Deleted session {id}.\n"), String::new())
    );
}

#[test]
fn delete_reaches_archived_chats_too() {
    let r = rig();
    let id = r.chat("Plan");
    r.archive(Action::Archive, &id).unwrap();
    r.delete(&id, true, false, "").unwrap();
    assert_eq!(r.store.archived().list(None), vec![]);
    assert_eq!(
        message(r.delete(&id, true, false, "")),
        Some(format!(
            "No active or archived session found matching '{id}'."
        ))
    );
}

#[test]
fn a_chat_open_elsewhere_is_left_alone() {
    let r = rig();
    let id = r.chat("Plan");
    let (held, _) = r.store.open(&id).unwrap();
    assert_eq!(
        message(r.archive(Action::Archive, "Plan")),
        Some(format!("session {id} is open in a viy chat; quit it first"))
    );
    assert_eq!(
        message(r.delete(&id, true, false, "")),
        Some(format!("session {id} is open in a viy chat; quit it first"))
    );
    drop(held);
}
