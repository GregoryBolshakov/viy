//! `viy dev drive` against the scripted model server: every recorded
//! scenario without build tools sends what rp sent for it (WP 3.2's gate,
//! with `tools/drive_rp.py --client viy` on fakellm as its twin).

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_testkit::mock_llm::MockLlm;
use viy_testkit::scenario;
use viy_testkit::scratch::ScratchDir;

use crate::Terminal;

fn run_capture(args: &[&str]) -> (i32, Vec<Value>, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = crate::run(
        args.iter().map(std::ffi::OsString::from),
        &mut out,
        &mut err,
        Terminal::default(),
    );
    let events = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (code, events, String::from_utf8(err).unwrap())
}

/// Drives scenario `name` through viy against the scripted server, from a
/// workspace laid out as the scenario says (as drive_rp does): its events,
/// its requests normalized as drive_rp writes them, and its session log
/// normalized as drive_rp normalizes rp's.
fn drive(name: &str) -> (Vec<Value>, Vec<Value>, Option<Vec<Value>>) {
    let file = scenario::dir().join(format!("{name}.json"));
    let script = scenario::load(name).unwrap();
    let mock = MockLlm::start(&script).unwrap();
    let scratch = ScratchDir::new("drive").unwrap();
    let base = std::fs::canonicalize(scratch.path()).unwrap();
    let workspace = base.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    for (rel, text) in script["workspace"].as_object().into_iter().flatten() {
        let path = workspace.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text.as_str().unwrap()).unwrap();
    }
    let result = base.join("result.json");
    let (code, events, err) = run_capture(&[
        "dev",
        "drive",
        file.to_str().unwrap(),
        "--url",
        &mock.uri(),
        "--result",
        result.to_str().unwrap(),
        "--dir",
        workspace.to_str().unwrap(),
    ]);
    assert_eq!((code, err.as_str()), (0, ""), "{name}");
    let result: Value = serde_json::from_str(&std::fs::read_to_string(result).unwrap()).unwrap();
    let today = result["today"].as_str().unwrap();
    let subs = [
        (today, "{TODAY}"),
        (workspace.to_str().unwrap(), "{WORKSPACE}"),
        (base.to_str().unwrap(), "{BASE}"),
    ];
    let log = mock
        .requests()
        .iter()
        .map(|r| scenario::normalize(&r.log_entry(), &subs))
        .collect();
    let session = result["session"]
        .as_str()
        .map(|path| scenario::session_events(path.as_ref(), &subs).unwrap());
    (events, log, session)
}

fn tools_off(name: &str) -> bool {
    scenario::load(name).unwrap()["config"]["TOOLS_ENABLED"] == "0"
}

#[test]
fn every_scenario_without_build_tools_sends_what_rp_sent() {
    let names: Vec<String> = scenario::recorded()
        .unwrap()
        .into_iter()
        .filter(|n| tools_off(n))
        .collect();
    assert!(names.len() >= 4, "{names:?}");
    for name in names {
        let (_, viy, session) = drive(&name);
        let rp = scenario::rp_log(&name).unwrap().entries;
        assert_eq!(scenario::compare(&rp, &viy), None, "{name}");
        // And keeps the session rp kept: the same events, keys in order.
        let rp_session = scenario::rp_session(&name).unwrap();
        assert_eq!(session, rp_session, "{name}: the session log");
        assert_eq!(
            serde_json::to_string(&session).unwrap(),
            serde_json::to_string(&rp_session).unwrap(),
            "{name}: the session log's key order"
        );
    }
}

#[test]
fn drive_shows_scenario_one_as_events() {
    let (events, _, _) = drive("plain-two-turns");
    let types: Vec<&str> = events
        .iter()
        .map(|e| e["msg"]["type"].as_str().unwrap())
        .filter(|t| !t.ends_with("_delta"))
        .collect();
    assert_eq!(
        types,
        [
            "turn_started",
            "round_started",
            "token_count",
            "background",
            "turn_complete",
            "title_changed",
            "background",
            "turn_started",
            "round_started",
            "token_count",
            "turn_complete",
            "shutdown_complete"
        ]
    );
    let answer: String = events
        .iter()
        .filter(|e| e["id"] == "2" && e["msg"]["type"] == "agent_message_content_delta")
        .map(|e| e["msg"]["delta"].as_str().unwrap())
        .collect();
    assert_eq!(
        answer,
        "Rain on the tin roof\na thousand small drums at once\nthen the gutter sighs"
    );
    let title = events
        .iter()
        .find(|e| e["msg"]["type"] == "title_changed")
        .unwrap();
    assert_eq!(title["msg"]["title"], "Haiku definition");
}

#[test]
fn the_chat_takes_the_key_and_model_from_the_backend() {
    let scratch = ScratchDir::new("drive").unwrap();
    let file = scratch.path().join("hosted.json");
    let script = json!({
        "name": "hosted",
        "config": {"TOOLS_ENABLED": "0", "BACKEND": "openai", "ENDPOINT_API_KEY": "ek",
                   "ENDPOINT_MODEL": "m1", "CHAT_AUTO_TITLE": "0"},
        "steps": [{"user": "hi"}],
        "responses": [{"stream": [{"content": "hello"}]}],
    });
    std::fs::write(&file, script.to_string()).unwrap();
    let mock = MockLlm::start(&script).unwrap();
    let (code, _, err) = run_capture(&[
        "dev",
        "drive",
        file.to_str().unwrap(),
        "--url",
        &mock.uri(),
        "--dir",
        scratch.path().to_str().unwrap(),
    ]);
    assert_eq!((code, err.as_str()), (0, ""));
    let post = &mock.requests()[1];
    assert_eq!(post.header("authorization"), Some("Bearer ek"));
    assert_eq!(post.body.as_ref().unwrap()["model"], "m1");
}

#[test]
fn drive_refuses_build_tools_and_says_what_is_wrong() {
    let tools = scenario::dir().join("read-then-protected-write.json");
    let (code, _, err) = run_capture(&[
        "dev",
        "drive",
        tools.to_str().unwrap(),
        "--url",
        "http://127.0.0.1:9",
    ]);
    assert_eq!(code, 1);
    assert!(err.contains("build tools come with phase 4"), "{err}");
    let (code, _, err) = run_capture(&[
        "dev",
        "drive",
        "/nonexistent/scenario.json",
        "--url",
        "http://127.0.0.1:9",
    ]);
    assert_eq!(code, 1);
    assert!(err.starts_with("/nonexistent/scenario.json: "), "{err}");
}

#[test]
fn dev_is_hidden_from_help() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    crate::run(
        ["--help"].map(std::ffi::OsString::from),
        &mut out,
        &mut err,
        Terminal::default(),
    );
    assert!(!String::from_utf8(out).unwrap().contains("dev"));
}

#[test]
fn live_runs_take_the_given_config_under_the_scenario() {
    let mut base = viy_config::Config::defaults();
    base.set("VLLM_API_KEY", "real");
    base.set("TOOLS_ENABLED", "1");
    let cfg = super::config(base, &json!({"config": {"TOOLS_ENABLED": "0"}})).unwrap();
    assert_eq!(
        (cfg.get("VLLM_API_KEY"), cfg.get("TOOLS_ENABLED")),
        (Some("real"), Some("0"))
    );
    assert_eq!(super::scripted_base().get("VLLM_API_KEY"), Some("fake-key"));
    let cli = crate::cli::parse(
        ["dev", "drive", "s.json", "--url", "u", "--live"].map(std::ffi::OsString::from),
    )
    .unwrap();
    let crate::cli::Command::Dev(args) = cli.command else {
        panic!()
    };
    assert!(args.live());
}

#[test]
fn a_scripted_run_refuses_instruction_files_from_outside_the_workspace() {
    let scratch = ScratchDir::new("drive").unwrap();
    let outer = std::fs::canonicalize(scratch.path()).unwrap();
    std::fs::write(outer.join("AGENTS.md"), "the folder's own rules").unwrap();
    let inner = outer.join("ws");
    std::fs::create_dir(&inner).unwrap();
    let file = scenario::dir().join("plain-two-turns.json");
    let (code, events, err) = run_capture(&[
        "dev",
        "drive",
        file.to_str().unwrap(),
        "--url",
        "http://127.0.0.1:9",
        "--dir",
        inner.to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    assert!(events.is_empty());
    assert!(
        err.starts_with(&format!(
            "an instruction file from outside the scenario would be sent: {}",
            outer.join("AGENTS.md").display()
        )),
        "{err}"
    );
}

#[test]
fn a_scripted_run_in_a_project_refuses_the_projects_own_files() {
    // The repository sits in a folder whose instruction files a scenario
    // must never carry.
    let file = scenario::dir().join("plain-two-turns.json");
    let crate_dir = env!("CARGO_MANIFEST_DIR");
    let (code, _, err) = run_capture(&[
        "dev",
        "drive",
        file.to_str().unwrap(),
        "--url",
        "http://127.0.0.1:9",
        "--dir",
        crate_dir,
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.starts_with("an instruction file from outside the scenario would be sent: "),
        "{err}"
    );
}
