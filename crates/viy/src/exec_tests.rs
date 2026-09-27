//! `viy exec` (kb d083): rpctl test_features.py AskSaveTests turned
//! around, as Codex's `exec` saves unless `--ephemeral`; `exec resume` and
//! `exec fork`.

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_testkit::mock_llm::MockLlm;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::cli::ExecForkArgs;
use crate::cli::ExecResumeArgs;

struct Rig {
    paths: Paths,
    project: std::path::PathBuf,
    _scratch: ScratchDir,
}

fn rig() -> Rig {
    let scratch = ScratchDir::new("exec").unwrap();
    let paths = Paths::under(scratch.path().join("home"));
    let project = std::fs::canonicalize(scratch.path())
        .unwrap()
        .join("project");
    std::fs::create_dir_all(&project).unwrap();
    Rig {
        paths,
        project,
        _scratch: scratch,
    }
}

fn args(prompt: &str) -> ExecArgs {
    ExecArgs {
        command: None,
        prompt: Some(prompt.into()),
        url: None,
        system: None,
        think: false,
        ephemeral: false,
    }
}

impl Rig {
    /// Runs `args` against a server streaming `stream`: (stdout, stderr).
    fn exec(&self, mut args: ExecArgs, stream: Value) -> (String, String) {
        let mock = MockLlm::start(&json!({"responses": [{"stream": stream}]})).unwrap();
        args.url = Some(mock.uri());
        let mut cfg = dev::scripted_base();
        cfg.set("CHAT_AUTO_TITLE", "0");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        cmd_exec(&args, &self.paths, cfg, &self.project, &mut out, &mut err).unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn store(&self) -> Sessions {
        Sessions::new(self.paths.sessions_dir(), Arc::new(SystemTimeProvider))
    }

    fn contents(&self, id: &str) -> Vec<Value> {
        sessions::replay(&self.store().path_of(id))
            .unwrap()
            .messages
            .iter()
            .map(|m| m["content"].clone())
            .collect()
    }
}

#[test]
fn exec_saves_a_one_shot_chat_and_says_its_id_on_stderr() {
    let r = rig();
    let (out, err) = r.exec(args("what is 2+2"), json!([{"content": "4"}]));
    assert_eq!(out, "4\n");
    let rows = r.store().list(None);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].exec);
    assert_eq!(err, format!("session id: {}\n", rows[0].id));
    assert_eq!(r.contents(&rows[0].id), [json!("what is 2+2"), json!("4")]);
    let events = sessions::read_events(&r.store().path_of(&rows[0].id)).unwrap();
    assert_eq!(events.last().unwrap()["reason"], "exec");
}

#[test]
fn ephemeral_keeps_nothing() {
    let r = rig();
    let mut a = args("what is 2+2");
    a.ephemeral = true;
    let (out, err) = r.exec(a, json!([{"content": "4"}]));
    assert_eq!((out.as_str(), err.as_str()), ("4\n", ""));
    assert_eq!(r.store().list(None), vec![]);
}

/// Not in rp's tests: `--think` writes the trace before the answer, as
/// rp's chat does off a terminal.
#[test]
fn think_writes_the_trace_first() {
    let r = rig();
    let mut a = args("what is 2+2");
    a.think = true;
    let (out, _) = r.exec(a, json!([{"reasoning": "add them"}, {"content": "4"}]));
    assert_eq!(out, "[thinking] add them\n\n4\n");
}

#[test]
fn exec_resume_goes_on_in_the_last_chat_or_one_named() {
    let r = rig();
    r.exec(args("first"), json!([{"content": "one"}]));
    let id = r.store().list(None)[0].id.clone();
    let mut resume = args("");
    resume.prompt = None;
    resume.command = Some(ExecCommand::Resume(ExecResumeArgs {
        session_id: Some("second".into()),
        prompt: None,
        last: true,
        all: false,
    }));
    let (out, err) = r.exec(resume, json!([{"content": "two"}]));
    assert_eq!(out, "two\n");
    assert_eq!(err, format!("session id: {id}\n"));
    assert_eq!(
        r.contents(&id),
        [json!("first"), json!("one"), json!("second"), json!("two")]
    );
    assert_eq!(r.store().list(None).len(), 1);
    let mut by_id = args("");
    by_id.prompt = None;
    by_id.command = Some(ExecCommand::Resume(ExecResumeArgs {
        session_id: Some(id.clone()),
        prompt: Some("third".into()),
        last: false,
        all: false,
    }));
    r.exec(by_id, json!([{"content": "three"}]));
    assert_eq!(r.contents(&id).len(), 6);
}

#[test]
fn exec_fork_goes_on_in_a_one_shot_copy() {
    let r = rig();
    r.exec(args("first"), json!([{"content": "one"}]));
    let id = r.store().list(None)[0].id.clone();
    let mut fork = args("");
    fork.prompt = None;
    fork.command = Some(ExecCommand::Fork(ExecForkArgs {
        session_id: id.clone(),
        prompt: Some("second".into()),
    }));
    let (out, err) = r.exec(fork, json!([{"content": "two"}]));
    assert_eq!(out, "two\n");
    let rows = r.store().list(None);
    assert_eq!(rows.len(), 2);
    let twin = rows.iter().find(|row| row.id != id).unwrap();
    assert!(twin.exec);
    assert_eq!(err, format!("session id: {}\n", twin.id));
    assert_eq!(r.contents(&id).len(), 2);
    assert_eq!(r.contents(&twin.id).len(), 4);
}

#[test]
fn exec_resume_needs_a_chat() {
    let r = rig();
    let mut resume = args("x");
    resume.command = Some(ExecCommand::Resume(ExecResumeArgs {
        session_id: Some("nothing-like-it".into()),
        prompt: Some("hi".into()),
        last: false,
        all: false,
    }));
    let mock = MockLlm::start(&json!({"responses": []})).unwrap();
    resume.url = Some(mock.uri());
    let failed = cmd_exec(
        &resume,
        &r.paths,
        dev::scripted_base(),
        &r.project,
        &mut Vec::new(),
        &mut Vec::new(),
    );
    assert_eq!(
        failed.err().and_then(|f| f.message),
        Some("No session found matching 'nothing-like-it'.".into())
    );
}
