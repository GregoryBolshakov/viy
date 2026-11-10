//! The elision against rpctl's: the `chat/elision` fixture (rpctl's
//! `_elide_before` and `_maybe_elide_tool_results` on the same histories).
//!
//! A write_file call whose contents are elided gets its arguments written
//! again: rpctl with `json.dumps` (", " and ": "), viy compactly (kb d030),
//! so the arguments are compared as JSON and the characters saved differ
//! by exactly the two spellings' lengths, which the notice's "~N tokens"
//! shows.

use std::path::Path;
use std::sync::Arc;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_config::config::Config;
use viy_http::ReqwestHttp;
use viy_protocol::EventMsg;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::chat::Places;
use crate::stream::Endpoint;

fn chat_in(dir: &Path, over: &Value) -> Chat {
    let mut cfg = Config::defaults();
    for (key, value) in [
        ("VLLM_API_KEY", "k"),
        ("TRACE_ENABLED", "0"),
        ("INSTRUCTIONS_ENABLED", "0"),
        ("TOOLS_ENABLED", "0"),
        ("SEARCH_ENABLED", "0"),
    ] {
        cfg.set(key, value);
    }
    for (key, value) in over.as_object().into_iter().flatten() {
        cfg.set(key, value.as_str().unwrap());
    }
    let ws = dir.join("project");
    std::fs::create_dir_all(&ws).unwrap();
    let nowhere = dir.join("nowhere");
    let places = Places {
        workspace: ws.clone(),
        config_dir: nowhere.join("config"),
        home: nowhere.join("home"),
        project_dir: ws,
        locks_dir: nowhere.join("locks"),
        quarantine_dir: nowhere.join("quarantine"),
        sandbox_tmp_dir: nowhere.join("sandbox-tmp"),
        state_dir: nowhere.join("state"),
        doctrine_sources: Vec::new(),
        guarded: Vec::new(),
    };
    let endpoint = Endpoint {
        base: "http://127.0.0.1:9".into(),
        key: "k".into(),
        backend: "runpod".into(),
        is_vllm: true,
        http: Arc::new(ReqwestHttp::new().unwrap()),
    };
    Chat::new(
        &cfg,
        places,
        endpoint,
        "m".into(),
        "2026-07-03 (Friday)".into(),
        Arc::new(|| {}),
    )
    .unwrap()
}

/// `messages` with every tool call's arguments parsed, when they are JSON.
fn parsed(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            for tc in m["tool_calls"].as_array_mut().into_iter().flatten() {
                if let Some(Ok(v)) = tc["function"]["arguments"]
                    .as_str()
                    .map(serde_json::from_str::<Value>)
                {
                    tc["function"]["arguments"] = json!({"$json": v});
                }
            }
            m
        })
        .collect()
}

/// How many more characters rpctl's spelling of the rewritten write_file
/// arguments has than viy's.
fn spelling_delta(rp: &[Value], viy: &[Value]) -> isize {
    let args = |ms: &[Value]| -> Vec<String> {
        ms.iter()
            .flat_map(|m| m["tool_calls"].as_array().cloned().unwrap_or_default())
            .filter_map(|tc| tc["function"]["arguments"].as_str().map(str::to_string))
            .collect()
    };
    args(rp)
        .iter()
        .zip(args(viy))
        .filter(|(a, _)| a.contains("[elided: "))
        .map(|(a, b)| a.chars().count() as isize - b.chars().count() as isize)
        .sum()
}

fn paths(set: &HashSet<PathBuf>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().map(|p| p.display().to_string()).collect();
    v.sort();
    v
}

fn with_dir(v: &Value, dir: &Path) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|p| {
            p.as_str()
                .unwrap()
                .replace("$T", &dir.display().to_string())
        })
        .collect()
}

#[test]
fn elide_before_equals_rpctl() {
    let fixture = viy_testkit::fixtures::load("chat/elision").unwrap();
    let cases: Vec<&Value> = fixture
        .cases
        .iter()
        .filter(|c| c["kind"] == "elide")
        .collect();
    assert_eq!(cases.len(), 5);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let scratch = ScratchDir::new("elision").unwrap();
        let chat = chat_in(scratch.path(), &case["config"]);
        let history = case["history"].as_array().unwrap();
        let cut = case["cut"].as_u64().unwrap() as usize;
        let el = elide_before(history, cut, &|tc| chat.files.call_path(tc));
        let rp = case["messages"].as_array().unwrap();
        assert_eq!(parsed(&el.messages), parsed(rp), "{name}");
        assert_eq!(el.n, case["n"].as_u64().unwrap() as usize, "{name}");
        assert_eq!(
            el.saved - spelling_delta(rp, &el.messages),
            case["saved"].as_i64().unwrap() as isize,
            "{name}"
        );
        assert_eq!(
            paths(&el.forget),
            with_dir(&case["forget"], scratch.path()),
            "{name}"
        );
        assert_eq!(
            paths(&el.kept),
            with_dir(&case["kept"], scratch.path()),
            "{name}"
        );
    }
}

#[test]
fn maybe_elide_equals_rpctl() {
    let fixture = viy_testkit::fixtures::load("chat/elision").unwrap();
    let cases: Vec<&Value> = fixture
        .cases
        .iter()
        .filter(|c| c["kind"] == "maybe")
        .collect();
    assert_eq!(cases.len(), 10);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let scratch = ScratchDir::new("elision").unwrap();
        let mut chat = chat_in(scratch.path(), &case["config"]);
        assert_eq!(
            (chat.elide_threshold, chat.elide_keep_turns),
            (
                case["threshold"].as_f64().unwrap(),
                case["keep_turns"].as_u64().unwrap() as usize
            ),
            "{name}"
        );
        let history = case["history"].as_array().unwrap().clone();
        chat.messages = history.clone();
        for rel in case["seen"].as_array().into_iter().flatten() {
            let path = chat
                .files
                .resolve_in_workspace(rel.as_str().unwrap())
                .unwrap();
            chat.files.remember_read(path, "sha".into());
        }
        let (ctx, mut rx) = TurnContext::detached();
        chat.maybe_elide_tool_results(&ctx, &json!(case["user"]));
        let rp = case["messages"].as_array().unwrap();
        assert_eq!(parsed(&chat.messages), parsed(rp), "{name}");
        let mut said = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let EventMsg::Notice(n) = e.msg {
                said.push(n.message);
            }
        }
        let printed = case["printed"].as_str().unwrap();
        if printed.is_empty() {
            assert_eq!(said, Vec::<String>::new(), "{name}");
        } else {
            // The token count differs by the write_file spellings only.
            let delta = spelling_delta(rp, &chat.messages) as f64;
            let ours = said.join("\n");
            let number = |s: &str| -> i64 {
                let at = s.find('~').unwrap() + 1;
                s[at..].split(' ').next().unwrap().parse().unwrap()
            };
            let cpt = chat.estimator.chars_per_token;
            let rp_saved_chars = number(printed) as f64 * cpt;
            assert!(
                ((number(&ours) as f64 * cpt - delta) - rp_saved_chars).abs() < 2.0 * cpt,
                "{name}: {ours} vs {printed}"
            );
            let words = |s: &str| s.split('~').next().unwrap().to_string();
            assert_eq!(words(&ours), words(printed.trim_end()), "{name}");
        }
        let mut seen: Vec<String> = chat
            .files
            .seen_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            with_dir(&case["seen_after"], scratch.path()),
            "{name}"
        );
    }
}

/// rpctl test_features.py ContextTests' `_turn`: a write_file of `big`
/// whose result is `big` too.
fn write_turn(i: usize, big: &str) -> Vec<Value> {
    let args = json!({"path": format!("f{i}.txt"), "content": big}).to_string();
    vec![
        json!({"role": "user", "content": format!("turn {i}")}),
        json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": format!("c{i}"), "type": "function",
             "function": {"name": "write_file", "arguments": args}}]}),
        json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": big}),
        json!({"role": "assistant", "content": format!("done {i}")}),
    ]
}

/// rpctl test_workflows.py's `_turn`: a plain exchange.
fn plain_turn(i: usize) -> Vec<Value> {
    vec![
        json!({"role": "user", "content": format!("q{i}")}),
        json!({"role": "assistant", "content": format!("a{i}")}),
    ]
}

/// A chat with build tools on in a workspace of its own.
fn tool_chat() -> (crate::chat::tests::Direct, ScratchDir) {
    crate::chat::tests::shell_chat(json!([]), &[])
}

fn elide_now(d: &mut crate::chat::tests::Direct) {
    d.chat.elide_threshold = 1e-9;
    let ctx = d.ctx.clone();
    d.chat.maybe_elide_tool_results(&ctx, &json!("next"));
}

// rpctl ContextTests.test_old_tool_output_is_elided_past_the_threshold.
#[test]
fn old_tool_output_is_elided_past_the_threshold() {
    let (mut d, _s) = tool_chat();
    let big = "x".repeat(5000);
    d.chat.messages = (0..4).flat_map(|i| write_turn(i, &big)).collect();
    elide_now(&mut d);
    let tools: Vec<&str> = d
        .chat
        .messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(tools[0].starts_with("[elided: this write_file result (5000 chars)"));
    assert!(tools[1].starts_with("[elided"));
    assert_eq!(tools[2..], [big.as_str(), big.as_str()]); // the last 2 turns are kept
    let args: Value = serde_json::from_str(
        d.chat.messages[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(args["path"], "f0.txt");
    assert!(
        args["content"]
            .as_str()
            .unwrap()
            .starts_with("[elided: 5000 chars written")
    );
    let n = d.chat.messages.len();
    assert!(
        d.chat.messages[n - 3]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains(&big)
    );
    assert!(d.said()[0].contains("elided 4 old tool outputs"));
}

// rpctl ContextTests.test_nothing_is_elided_below_the_threshold.
#[test]
fn nothing_is_elided_below_the_threshold() {
    let (mut d, _s) = tool_chat();
    d.chat.messages = (0..4)
        .flat_map(|i| write_turn(i, &"y".repeat(5000)))
        .collect();
    let before = d.chat.messages.clone();
    d.chat.elide_threshold = 1e9;
    let ctx = d.ctx.clone();
    d.chat.maybe_elide_tool_results(&ctx, &json!("next"));
    assert_eq!(d.chat.messages, before);
    assert!(d.said().is_empty());
}

impl crate::chat::tests::Direct {
    async fn read_text(&mut self, path: &str) -> String {
        let args = json!({"path": path});
        self.chat
            .files
            .read_file(args.as_object().unwrap(), &self.ctx)
            .await
            .result
            .to_string()
    }

    async fn write_new(&mut self, path: &str) -> Value {
        let args = json!({"path": path, "content": "new"});
        let ctx = self.ctx.clone();
        self.chat
            .write_file(&ctx, "w1", args.as_object().unwrap(), "20260727-000000")
            .await
            .result
    }

    async fn read_turn(&mut self, i: usize, path: &str) -> Vec<Value> {
        let call = json!({"id": format!("r{i}"), "type": "function",
                          "function": {"name": "read_file",
                                       "arguments": json!({"path": path}).to_string()}});
        let content = self.read_text(path).await;
        vec![
            json!({"role": "user", "content": format!("q{i}")}),
            json!({"role": "assistant", "content": null, "tool_calls": [call]}),
            json!({"role": "tool", "tool_call_id": format!("r{i}"), "content": content}),
            json!({"role": "assistant", "content": format!("a{i}")}),
        ]
    }
}

fn not_read(out: &Value) -> bool {
    out["error"]
        .as_str()
        .is_some_and(|e| e.contains("have not read it"))
}

// rpctl WriteGuardForgetsTests.test_elided_read_must_be_read_again.
#[tokio::test]
async fn elided_read_must_be_read_again() {
    let (mut d, s) = tool_chat();
    let ws = s.path().join("ws");
    std::fs::write(ws.join("a.txt"), "a".repeat(2000)).unwrap();
    std::fs::write(ws.join("b.txt"), "b".repeat(2000)).unwrap();
    let mut messages = d.read_turn(1, "a.txt").await;
    messages.extend(d.read_turn(2, "b.txt").await);
    messages.extend(plain_turn(3));
    messages.extend(plain_turn(4));
    d.chat.messages = messages;
    elide_now(&mut d);
    assert!(
        d.chat.messages[2]["content"]
            .as_str()
            .unwrap()
            .starts_with("[elided")
    );
    assert!(not_read(&d.write_new("a.txt").await));
    assert!(not_read(&d.write_new("b.txt").await));
}

// rpctl WriteGuardForgetsTests.test_a_recent_read_keeps_the_file_writable.
#[tokio::test]
async fn a_recent_read_keeps_the_file_writable() {
    let (mut d, s) = tool_chat();
    std::fs::write(s.path().join("ws").join("a.txt"), "a".repeat(2000)).unwrap();
    let mut messages = d.read_turn(1, "a.txt").await;
    messages.extend(plain_turn(2));
    messages.extend(d.read_turn(3, "a.txt").await);
    messages.extend(plain_turn(4));
    d.chat.messages = messages;
    elide_now(&mut d);
    assert!(
        d.chat.messages[2]["content"]
            .as_str()
            .unwrap()
            .starts_with("[elided")
    );
    assert_eq!(d.write_new("a.txt").await["ok"], true);
}

// rpctl WriteGuardForgetsTests.test_explore_reports_are_not_elided.
#[test]
fn explore_reports_are_not_elided() {
    let (mut d, _s) = tool_chat();
    let call = json!({"id": "x1", "type": "function",
                      "function": {"name": "explore", "arguments": "{}"}});
    let report = json!({"results": [{"report": "r".repeat(2000)}]}).to_string();
    let mut messages = vec![
        json!({"role": "user", "content": "q1"}),
        json!({"role": "assistant", "content": null, "tool_calls": [call]}),
        json!({"role": "tool", "tool_call_id": "x1", "content": report}),
        json!({"role": "assistant", "content": "a1"}),
    ];
    messages.extend(plain_turn(2));
    messages.extend(plain_turn(3));
    d.chat.messages = messages;
    elide_now(&mut d);
    assert_eq!(d.chat.messages[2]["content"], report);
}

// The session log keeps the originals: the elided history is written as a
// `replace` at the next checkpoint, and replaying the log gives it.
#[tokio::test]
async fn the_session_log_takes_the_elided_history_as_a_replace() {
    let (mut d, _s) = tool_chat();
    let big = "x".repeat(5000);
    d.chat.messages = (0..4).flat_map(|i| write_turn(i, &big)).collect();
    let ctx = d.ctx.clone();
    d.chat.checkpoint(&ctx);
    elide_now(&mut d);
    d.chat.checkpoint(&ctx);
    let id = d.chat.session_id().unwrap();
    let log = std::fs::read_to_string(d.store_path(&id)).unwrap();
    let events: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events.last().unwrap()["t"], "replace");
    assert_eq!(events.iter().filter(|e| e["t"] == "msg").count(), 16);
    assert_eq!(d.replayed(&id).messages, d.chat.messages);
}
