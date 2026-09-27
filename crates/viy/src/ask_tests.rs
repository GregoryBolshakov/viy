//! `viy ask` (rpctl test_features.py AskSaveTests).

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_agent::sessions;
use viy_testkit::mock_llm::MockLlm;
use viy_testkit::scratch::ScratchDir;

use super::*;

struct Run {
    out: String,
    paths: Paths,
    _scratch: ScratchDir,
}

fn ask(stream: serde_json::Value, save: bool, think: bool) -> Run {
    let mock = MockLlm::start(&json!({"responses": [{"stream": stream}]})).unwrap();
    let scratch = ScratchDir::new("ask").unwrap();
    let paths = Paths::under(scratch.path().join("home"));
    let project = scratch.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut cfg = dev::scripted_base();
    cfg.set("CHAT_AUTO_TITLE", "0");
    let args = AskArgs {
        prompt: Some("what is 2+2".into()),
        url: Some(mock.uri()),
        system: None,
        think,
        save,
    };
    let mut out = Vec::new();
    cmd_ask(&args, &paths, cfg, &project, &mut out).unwrap();
    Run {
        out: String::from_utf8(out).unwrap(),
        paths,
        _scratch: scratch,
    }
}

fn store(run: &Run) -> sessions::Sessions {
    sessions::Sessions::new(run.paths.sessions_dir(), Arc::new(SystemTimeProvider))
}

#[test]
fn ask_is_not_recorded_by_default() {
    let run = ask(json!([{"content": "4"}]), false, false);
    assert_eq!(run.out, "4\n");
    assert_eq!(store(&run).list(None), vec![]);
}

#[test]
fn ask_save_records_a_chat() {
    let run = ask(json!([{"content": "4"}]), true, false);
    assert_eq!(run.out, "4\n");
    let rows = store(&run).list(None);
    assert_eq!(rows.len(), 1);
    let st = sessions::replay(&store(&run).path_of(&rows[0].id)).unwrap();
    let contents: Vec<&serde_json::Value> = st.messages.iter().map(|m| &m["content"]).collect();
    assert_eq!(contents, [&json!("what is 2+2"), &json!("4")]);
    let events = sessions::read_events(&store(&run).path_of(&rows[0].id)).unwrap();
    assert_eq!(events.last().unwrap()["reason"], "ask");
}

/// Not in rp's tests: `--think` writes the trace before the answer, as
/// rp's chat does off a terminal.
#[test]
fn ask_think_writes_the_trace_first() {
    let run = ask(
        json!([{"reasoning": "add them"}, {"content": "4"}]),
        false,
        true,
    );
    assert_eq!(run.out, "[thinking] add them\n\n4\n");
    let run = ask(
        json!([{"reasoning": "add them"}, {"content": "4"}]),
        false,
        false,
    );
    assert_eq!(run.out, "4\n");
}
