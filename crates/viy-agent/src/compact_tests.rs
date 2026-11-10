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
        toolbox: crate::toolbox::Dirs::default(),
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

/// The `chat/compaction` fixture's cases of `kind`.
fn compaction_cases(kind: &str) -> Vec<Value> {
    let fixture = viy_testkit::fixtures::load("chat/compaction").unwrap();
    fixture
        .cases
        .into_iter()
        .filter(|c| c["kind"] == kind)
        .collect()
}

fn compaction_chat(dir: &Path, case: &Value) -> Chat {
    let mut chat = chat_in(dir, &case["config"]);
    chat.messages = case["history"].as_array().unwrap().clone();
    if let Some(system) = case["system"].as_str() {
        chat.system = Some(system.to_string());
    }
    chat
}

#[test]
fn summary_plans_equal_rpctl() {
    let cases = compaction_cases("plan");
    assert_eq!(cases.len(), 16);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let scratch = ScratchDir::new("compaction").unwrap();
        let chat = compaction_chat(scratch.path(), &case);
        let plan = chat.plan_summary(case["focus"].as_str(), case["merge"] == true);
        let want = &case["plan"];
        let Some(plan) = plan else {
            assert!(want.is_null(), "{name}: viy has no plan, rpctl has one");
            continue;
        };
        assert!(!want.is_null(), "{name}: viy has a plan, rpctl none");
        let list = |k: &str| want[k].as_array().unwrap().clone();
        assert_eq!(
            parsed(&plan.pinned),
            parsed(&list("pinned")),
            "{name}: pinned"
        );
        assert_eq!(parsed(&plan.chunk), parsed(&list("chunk")), "{name}: chunk");
        assert_eq!(
            (
                plan.end as u64,
                plan.dropped as u64,
                plan.mode.as_str(),
                plan.think
            ),
            (
                want["end"].as_u64().unwrap(),
                want["dropped"].as_u64().unwrap(),
                want["mode"].as_str().unwrap(),
                want["think"].as_bool().unwrap()
            ),
            "{name}"
        );
        assert_eq!(parsed(&plan.msgs), parsed(&list("msgs")), "{name}: msgs");
        assert_eq!(
            parsed(&plan.plain_msgs),
            parsed(&list("plain_msgs")),
            "{name}: plain_msgs"
        );
    }
}

#[test]
fn trim_plans_equal_rpctl() {
    let cases = compaction_cases("trim");
    assert_eq!(cases.len(), 4);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let scratch = ScratchDir::new("compaction").unwrap();
        let chat = compaction_chat(scratch.path(), &case);
        let extra = case["extra"].as_array().cloned().unwrap_or_default();
        match (chat.trim_plan(&extra), &case["trim"]) {
            (None, Value::Null) => {}
            (Some((el, frac)), want) if !want.is_null() => {
                assert_eq!(el.n as u64, want["n"].as_u64().unwrap(), "{name}");
                assert!(
                    (frac - want["frac"].as_f64().unwrap()).abs() < 1e-12,
                    "{name}"
                );
            }
            (ours, want) => panic!(
                "{name}: viy {:?}, rpctl {want}",
                ours.map(|(e, f)| (e.n, f))
            ),
        }
    }
}

#[test]
fn applied_summaries_equal_rpctl() {
    let cases = compaction_cases("apply");
    assert_eq!(cases.len(), 4);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let scratch = ScratchDir::new("compaction").unwrap();
        let mut chat = compaction_chat(scratch.path(), &case);
        for rel in case["seen"].as_array().into_iter().flatten() {
            let path = chat
                .files
                .resolve_in_workspace(rel.as_str().unwrap())
                .unwrap();
            chat.files.remember_read(path, "sha".into());
        }
        let plan = chat.plan_summary(None, case["merge"] == true).unwrap();
        chat.apply_summary(&plan, case["summary"].as_str().unwrap());
        assert_eq!(
            parsed(&chat.messages),
            parsed(case["messages"].as_array().unwrap()),
            "{name}"
        );
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

#[test]
fn incremental_instructions_and_restates_equal_rpctl() {
    let cases = compaction_cases("instruction");
    assert_eq!(cases.len(), 5);
    for case in cases {
        let chunk = case["chunk"].as_array().unwrap();
        assert_eq!(
            incremental_instruction(chunk, case["tokens"].as_u64().unwrap()),
            case["text"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
    let cases = compaction_cases("restates");
    assert_eq!(cases.len(), 4);
    for case in cases {
        assert_eq!(
            restates(
                case["earlier"].as_str().unwrap(),
                case["new"].as_str().unwrap()
            ),
            case["restates"].as_bool().unwrap(),
            "{}",
            case["name"]
        );
    }
}

// ---- rpctl test_workflows.py CompactionTests, BackgroundCompactionTests ----

/// rpctl test_workflows.py's `_turn(i)`: a question and its answer.
fn qa(i: usize) -> Vec<Value> {
    vec![
        json!({"role": "user", "content": format!("q{i} {}", "u".repeat(400))}),
        json!({"role": "assistant", "content": format!("a{i}")}),
    ]
}

fn streamed(text: &str) -> Value {
    json!({"stream": [{"content": text}]})
}

/// A chat with build tools on whose server answers `responses`.
fn compacting(responses: Value, over: &[(&str, &str)]) -> (crate::chat::tests::Direct, ScratchDir) {
    crate::chat::tests::shell_chat(responses, over)
}

async fn compact_now(d: &mut crate::chat::tests::Direct, arg: &str) -> bool {
    let ctx = d.ctx.clone();
    d.chat.compact(&ctx, arg, false, &[]).await
}

fn sent(d: &crate::chat::tests::Direct) -> Vec<Value> {
    d.mock.chat_bodies()
}

fn starts_summary(m: &Value) -> bool {
    m["content"]
        .as_str()
        .is_some_and(|c| c.starts_with(SUMMARY_HEAD))
}

// rpctl CompactionTests.test_request_shares_the_normal_turn_prefix.
#[tokio::test]
async fn request_shares_the_normal_turn_prefix() {
    let (mut d, _s) = compacting(json!([streamed("the summary")]), &[]);
    d.chat.messages = qa(1);
    assert!(compact_now(&mut d, "").await);
    let body = &sent(&d)[0];
    let system = d.chat.system_prompt().unwrap();
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": system})
    );
    assert_eq!(body["messages"].as_array().unwrap()[1..3], qa(1)[..]);
    assert_eq!(body["tool_choice"], "none"); // schemas stay in the prompt
    assert!(body["tools"].as_array().is_some_and(|t| !t.is_empty()));
    assert!(starts_summary(&d.chat.messages[0]));
}

// rpctl CompactionTests.test_thinking_is_off_unless_configured.
#[tokio::test]
async fn thinking_is_off_unless_configured() {
    let off = json!({"enable_thinking": false, "preserve_thinking": false});
    let (mut d, _s) = compacting(json!([streamed("the summary")]), &[]);
    d.chat.messages = qa(1);
    compact_now(&mut d, "").await;
    assert_eq!(sent(&d)[0]["chat_template_kwargs"], off);
    let (mut d, _s) = compacting(
        json!([streamed("the summary")]),
        &[("COMPACT_THINKING", "1")],
    );
    d.chat.messages = qa(1);
    compact_now(&mut d, "").await;
    assert_ne!(sent(&d)[0]["chat_template_kwargs"], off);
}

// rpctl CompactionTests.test_a_connection_lost_mid_stream_fails_cleanly,
// the connection breaking in the scripted HTTP client after one piece.
#[tokio::test]
async fn a_connection_lost_mid_stream_fails_cleanly() {
    use viy_testkit::http::FakeHttp;
    use viy_testkit::http::Piece;
    use viy_testkit::http::Reply;
    let (mut d, _s) = compacting(json!([]), &[]);
    let first = b"data: {\"choices\": [{\"delta\": {\"content\": \"par\"}}]}\n\n".to_vec();
    d.chat.endpoint.http = Arc::new(FakeHttp::new([Reply::Answer {
        status: 200,
        body: vec![
            Piece::Data(first),
            Piece::Fail(viy_http::HttpError::Network("gone".into())),
        ],
    }]));
    d.chat.messages = qa(1);
    assert!(!compact_now(&mut d, "full").await);
    assert!(
        d.said()
            .iter()
            .any(|s| s.contains("connection lost mid-reply"))
    );
    assert_eq!(d.chat.messages, qa(1));
}

// rpctl CompactionTests.test_empty_reply_is_retried_without_tools.
#[tokio::test]
async fn empty_reply_is_retried_without_tools() {
    let (mut d, _s) = compacting(json!([{"stream": []}, streamed("the summary")]), &[]);
    d.chat.messages = qa(1);
    assert!(compact_now(&mut d, "").await);
    let modes: Vec<Value> = sent(&d).iter().map(|b| b["tool_choice"].clone()).collect();
    assert_eq!(modes, [json!("none"), Value::Null]); // the retry omits the tools
    assert!(sent(&d)[1].get("tools").is_none());
    assert!(
        d.chat.messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("the summary")
    );
}

// rpctl CompactionTests.test_reasoning_that_fills_the_budget_reports_the_limit.
#[tokio::test]
async fn reasoning_that_fills_the_budget_reports_the_limit() {
    let (mut d, _s) = compacting(json!([{"stream": [{"finish": "length"}]}]), &[]);
    d.chat.messages = qa(1);
    assert!(!compact_now(&mut d, "").await);
    assert_eq!(sent(&d).len(), 1); // no retry: more budget is the fix
    assert!(
        d.said()
            .iter()
            .any(|s| s.contains("reasoning, before any summary"))
    );
    assert_eq!(d.chat.messages, qa(1));
}

// rpctl CompactionTests.test_oversize_history_keeps_the_earlier_summary_and_whole_turns.
#[tokio::test]
async fn oversize_history_keeps_the_earlier_summary_and_whole_turns() {
    let (mut d, _s) = compacting(json!([streamed("new summary")]), &[]);
    let pinned = vec![
        json!({"role": "user", "content": format!("{SUMMARY_HEAD}\n\nold facts")}),
        json!({"role": "assistant", "content": "Understood."}),
    ];
    let call = json!({"id": "r1", "type": "function",
                      "function": {"name": "list_dir", "arguments": "{}"}});
    let tool_turn = vec![
        json!({"role": "user", "content": format!("q2 {}", "u".repeat(400))}),
        json!({"role": "assistant", "content": null, "tool_calls": [call]}),
        json!({"role": "tool", "tool_call_id": "r1", "content": "t".repeat(400)}),
        json!({"role": "assistant", "content": "a2"}),
    ];
    d.chat.messages = [pinned.clone(), qa(1), tool_turn, qa(3), qa(4)].concat();
    let base = d.chat.with_system(&[]);
    // the pass is fitted with the longest form of the instruction
    let instr = json!({"role": "user", "content": incremental_instruction(
        &[json!({"role": "user", "content": "x".repeat(80)})], 12000)});
    let fits = d
        .chat
        .estimate_tokens(&[base.clone(), pinned.clone(), qa(3), vec![instr]].concat());
    d.chat.max_model_len = fits + d.chat.settings.max_tokens as u64;
    assert_eq!(d.chat.prompt_budget(), fits);
    assert!(compact_now(&mut d, "").await);
    let msgs = sent(&d)[0]["messages"].as_array().unwrap().clone();
    // the newest turn is not summarized: it stays verbatim after the summary
    assert_eq!(
        msgs[..msgs.len() - 1],
        [base, pinned.clone(), qa(3)].concat()[..]
    );
    assert!(
        msgs.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("starting with the user's message that begins \"q3 uuu")
    );
    assert!(
        d.said()
            .iter()
            .any(|s| s.contains("6 oldest messages after the earlier summary"))
    );
    assert_eq!(d.chat.messages[2..], qa(4)[..]);
    assert_eq!(
        d.chat.messages[0]["content"],
        format!(
            "{}{SUMMARY_NEXT}new summary",
            pinned[0]["content"].as_str().unwrap()
        )
    );
}

fn shell_turn(output: &str) -> Vec<Value> {
    let call = json!({"id": "r1", "type": "function",
                      "function": {"name": "run_shell", "arguments": "{\"command\": \"ls\"}"}});
    vec![
        json!({"role": "user", "content": "q1"}),
        json!({"role": "assistant", "content": null, "tool_calls": [call]}),
        json!({"role": "tool", "tool_call_id": "r1", "content": output}),
        json!({"role": "assistant", "content": "a1"}),
    ]
}

// rpctl CompactionTests.test_cheap_trim_first_and_model_only_when_needed.
#[tokio::test]
async fn cheap_trim_first_and_model_only_when_needed() {
    let (mut d, _s) = compacting(json!([streamed("the summary")]), &[]);
    let big = shell_turn(&format!("EXIT 0 {}", "x".repeat(20000)));
    d.chat.messages = [big.clone(), qa(2)].concat();
    assert!(compact_now(&mut d, "").await);
    assert!(sent(&d).is_empty()); // no model call
    assert!(d.said().iter().any(|s| s.contains("without the model")));
    let note = d.chat.messages[2]["content"].as_str().unwrap();
    assert!(note.starts_with("[elided") && note.contains("EXIT 0")); // the head survives
    assert_eq!(d.chat.messages.len(), 6);
    // when trimming cannot reach the target, the model summarizes instead
    d.chat.messages = [big.clone(), qa(2)].concat();
    d.chat.fractions.trim_target = 0.0;
    assert!(compact_now(&mut d, "").await);
    let msgs = sent(&d)[0]["messages"].as_array().unwrap().clone();
    assert_eq!(msgs[1..msgs.len() - 1], big[..]); // newest turn not sent
    assert_eq!(d.chat.messages[2..], qa(2)[..]);
}

// rpctl CompactionTests.test_full_or_a_focus_always_summarizes.
#[tokio::test]
async fn full_or_a_focus_always_summarizes() {
    let (mut d, _s) = compacting(
        json!([streamed("the summary"), streamed("the summary")]),
        &[],
    );
    let hist = [shell_turn(&"x".repeat(20000)), qa(2)].concat();
    for (i, arg) in ["full", "the deploy"].iter().enumerate() {
        d.chat.messages = hist.clone();
        assert!(compact_now(&mut d, arg).await);
        assert_eq!(sent(&d).len(), i + 1);
    }
    let last = sent(&d)[1]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert!(
        last["content"]
            .as_str()
            .unwrap()
            .contains("Pay particular attention to: the deploy.")
    );
}

// rpctl CompactionTests.test_an_incremental_pass_that_rewrites_everything_replaces.
#[tokio::test]
async fn an_incremental_pass_that_rewrites_everything_replaces() {
    let earlier: String = (0..10)
        .map(|i| format!("- fact number {i} that is long enough to count as a line"))
        .collect::<Vec<_>>()
        .join("\n");
    let (mut d, _s) = compacting(
        json!([
            streamed(&format!("# Full summary\n{earlier}\n- and turn 1")),
            streamed("- turn 1 asked for q1")
        ]),
        &[],
    );
    let pinned = vec![
        json!({"role": "user", "content": format!("{SUMMARY_HEAD}\n\n{earlier}")}),
        json!({"role": "assistant", "content": "Understood."}),
    ];
    d.chat.messages = [pinned.clone(), qa(1), qa(2)].concat();
    assert!(compact_now(&mut d, "").await);
    let head = d.chat.messages[0]["content"].as_str().unwrap().to_string();
    assert!(!head.contains(SUMMARY_NEXT));
    assert_eq!(head.matches("fact number 3 that").count(), 1);
    // a genuine increment is appended
    d.chat.messages = [pinned.clone(), qa(1), qa(2)].concat();
    assert!(compact_now(&mut d, "").await);
    assert_eq!(
        d.chat.messages[0]["content"],
        format!(
            "{}{SUMMARY_NEXT}- turn 1 asked for q1",
            pinned[0]["content"].as_str().unwrap()
        )
    );
}

// rpctl CompactionTests.test_large_summaries_are_consolidated.
#[test]
fn large_summaries_are_consolidated() {
    let (mut d, _s) = compacting(json!([]), &[]);
    let pinned = vec![
        json!({"role": "user", "content": format!("{SUMMARY_HEAD}\n\n{}", "s".repeat(4000))}),
        json!({"role": "assistant", "content": "Understood."}),
    ];
    d.chat.messages = [pinned, qa(1), qa(2)].concat();
    d.chat.fractions.consolidate_at = 0.0001;
    // a foreground pass never merges: it appends
    assert_eq!(
        d.chat.plan_summary(None, false).unwrap().mode,
        Mode::Incremental
    );
    // the background pass merges
    let plan = d.chat.plan_summary(None, true).unwrap();
    assert_eq!(plan.mode, Mode::Consolidate);
    let instr = plan.msgs.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(instr.starts_with(&compact_instruction()));
    assert!(instr.contains("one summary of at most about"));
    d.chat.apply_summary(&plan, "merged");
    assert_eq!(
        d.chat.messages[0]["content"],
        format!("{SUMMARY_HEAD}\n\nmerged")
    );
    assert_eq!(d.chat.messages[2..], qa(2)[..]);
}

// rpctl CompactionTests.test_files_read_in_the_kept_turn_stay_writable.
#[tokio::test]
async fn files_read_in_the_kept_turn_stay_writable() {
    let (mut d, s) = compacting(json!([streamed("the summary")]), &[]);
    let ws = s.path().join("ws");
    std::fs::write(ws.join("a.txt"), "one").unwrap();
    std::fs::write(ws.join("b.txt"), "two").unwrap();
    let mut messages = d.read_turn(1, "a.txt").await;
    messages.extend(d.read_turn(2, "b.txt").await);
    d.chat.messages = messages;
    assert!(compact_now(&mut d, "").await);
    assert!(not_read(&d.write_new("a.txt").await));
    assert_eq!(d.write_new("b.txt").await["ok"], true);
}

// rpctl CompactionTests.test_a_bare_summary_is_not_compacted_again.
#[tokio::test]
async fn a_bare_summary_is_not_compacted_again() {
    let (mut d, _s) = compacting(json!([streamed("bigger summary")]), &[]);
    d.chat.messages = vec![
        json!({"role": "user", "content": format!("{SUMMARY_HEAD}\n\nfacts")}),
        json!({"role": "assistant", "content": "Understood."}),
    ];
    assert!(!compact_now(&mut d, "").await);
    assert!(sent(&d).is_empty());
    assert!(d.said().iter().any(|s| s.contains("nothing new")));
}

// rpctl CompactionTests.test_each_pass_is_traced.
#[tokio::test]
async fn each_pass_is_traced() {
    let (mut d, s) = compacting(
        json!([{"stream": [{"reasoning": "hmm"}, {"content": "the summary"},
                           {"usage": {"prompt_tokens": 900, "completion_tokens": 40,
                                      "total_tokens": 940}}]}]),
        &[],
    );
    d.chat.messages = qa(1);
    compact_now(&mut d, "").await;
    let path = d.chat.tracer().trace_path();
    let rows: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|r: &Value| r["kind"] == "compact")
        .collect();
    assert_eq!(rows.len(), 1, "{}", s.path().display());
    let r = &rows[0];
    assert_eq!(
        (
            r["status"].clone(),
            r["prompt_tokens"].clone(),
            r["completion_tokens"].clone(),
            r["summary_chars"].clone(),
            r["reasoning_chars"].clone()
        ),
        (json!("ok"), json!(900), json!(40), json!(11), json!(3))
    );
    assert!(r.get("ttft_secs").is_some());
}

/// rpctl BackgroundCompactionTests' chat: the background pass starts at
/// once and trimming never suffices.
fn background_chat(responses: Value) -> (crate::chat::tests::Direct, ScratchDir) {
    let (mut d, s) = compacting(responses, &[]);
    d.chat.fractions.background_at = 0.0001;
    d.chat.fractions.trim_target = 0.0;
    (d, s)
}

fn bg_reply(text: &str) -> Value {
    json!({"message": text, "usage": {"prompt_tokens": 1, "completion_tokens": 7,
                                      "total_tokens": 8}})
}

async fn background_done(d: &crate::chat::tests::Direct) {
    for _ in 0..500 {
        if !d.chat.background_running() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the background summary did not finish");
}

// rpctl BackgroundCompactionTests.test_summary_written_after_a_turn_is_used_at_compaction.
#[tokio::test]
async fn summary_written_after_a_turn_is_used_at_compaction() {
    let (mut d, _s) = background_chat(json!([bg_reply("bg summary")]));
    d.chat.messages = [qa(1), qa(2)].concat();
    d.chat.maybe_start_background();
    background_done(&d).await;
    assert_eq!(sent(&d)[0]["stream"], false);
    d.chat.messages.extend(qa(3)); // a turn finished after the snapshot
    assert!(compact_now(&mut d, "").await);
    assert_eq!(sent(&d).len(), 1); // no foreground pass
    assert!(
        d.said()
            .iter()
            .any(|s| s.contains("written in the background"))
    );
    assert_eq!(
        d.chat.messages[0]["content"],
        format!("{SUMMARY_HEAD}\n\nbg summary")
    );
    assert_eq!(d.chat.messages[2..], [qa(2), qa(3)].concat()[..]);
    assert!(d.chat.background.is_none());
}

// rpctl BackgroundCompactionTests.test_a_changed_history_discards_it.
#[tokio::test]
async fn a_changed_history_discards_it() {
    let (mut d, _s) = background_chat(json!([
        bg_reply("bg summary"),
        bg_reply("bg summary"),
        streamed("fresh")
    ]));
    d.chat.messages = [qa(1), qa(2)].concat();
    d.chat.maybe_start_background();
    background_done(&d).await;
    let ctx = d.ctx.clone();
    d.chat.new_session(&ctx, None).await;
    assert!(d.chat.background.is_none());
    d.chat.messages = [qa(5), qa(6)].concat();
    d.chat.maybe_start_background();
    background_done(&d).await;
    d.chat.messages[0] = json!({"role": "user", "content": "edited"});
    assert!(compact_now(&mut d, "").await);
    assert_eq!(sent(&d).len(), 3); // the foreground pass ran
    assert!(
        d.chat.messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("fresh")
    );
}

// rpctl BackgroundCompactionTests.test_not_started_below_the_threshold_or_twice.
#[tokio::test]
async fn not_started_below_the_threshold_or_twice() {
    let (mut d, _s) = background_chat(json!([
        {"message": "slow", "delay": 0.5, "usage": {"completion_tokens": 1}}
    ]));
    d.chat.fractions.background_at = 0.99;
    d.chat.messages = [qa(1), qa(2)].concat();
    d.chat.maybe_start_background();
    assert!(d.chat.background.is_none());
    d.chat.fractions.background_at = 0.0001;
    d.chat.maybe_start_background();
    assert!(d.chat.background_running());
    d.chat.maybe_start_background();
    background_done(&d).await;
    assert_eq!(sent(&d).len(), 1); // started once
}

// rpctl WriteGuardForgetsTests.test_compaction_new_chat_and_resume_forget_reads.
#[tokio::test]
async fn compaction_new_chat_and_resume_forget_reads() {
    let (mut d, s) = compacting(json!([streamed("summary")]), &[]);
    std::fs::write(s.path().join("ws").join("a.txt"), "one").unwrap();
    d.read_text("a.txt").await;
    d.chat.messages = qa(1);
    assert!(compact_now(&mut d, "").await);
    assert!(not_read(&d.write_new("a.txt").await));
    d.read_text("a.txt").await;
    let ctx = d.ctx.clone();
    d.chat.checkpoint(&ctx);
    let first = d.chat.session_id().unwrap();
    d.chat.new_session(&ctx, None).await;
    assert!(not_read(&d.write_new("a.txt").await));
    d.read_text("a.txt").await;
    assert!(d.chat.attach_session(&ctx, &first).await);
    assert!(not_read(&d.write_new("a.txt").await));
}

// rpctl ChatAutosaveTests.test_compaction_is_logged_without_a_transcript_backup:
// the session log is the backup, and search still finds what was summarised.
#[tokio::test]
async fn compaction_is_logged_and_the_old_text_still_found() {
    let mut d = crate::chat::tests::direct(json!([streamed("the summary")]), &[]);
    d.chat.messages = vec![
        json!({"role": "user", "content": "old detail"}),
        json!({"role": "assistant", "content": "noted"}),
    ];
    let ctx = d.ctx.clone();
    d.chat.checkpoint(&ctx);
    assert!(d.chat.compact(&ctx, "", false, &[]).await);
    let id = d.chat.session_id().unwrap();
    assert_eq!(d.replayed(&id).messages, d.chat.messages);
    assert!(
        d.chat.messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("the summary")
    );
    let found = d.store().search("old detail", None);
    assert_eq!(found[0].id, id);
    assert!(
        d.said()
            .iter()
            .any(|s| s.ends_with("the session log keeps the full history."))
    );
}

// rpctl ResumeShowsWholeChatTests.test_a_real_compaction_shows_every_turn_once_on_resume.
#[tokio::test]
async fn a_real_compaction_shows_every_turn_once_on_resume() {
    let mut d = crate::chat::tests::direct(
        json!([
            streamed("answer 1"),
            streamed("answer 2"),
            streamed("answer 3"),
            streamed("the summary"),
            streamed("answer 4")
        ]),
        &[],
    );
    for n in 1..4 {
        d.send(&format!("question {n}")).await;
    }
    let ctx = d.ctx.clone();
    assert!(d.chat.compact(&ctx, "", false, &[]).await);
    d.send("question 4").await;
    let id = d.chat.session_id().unwrap();
    d.chat.end_session(&ctx, "exit").await;
    let st = d.replayed(&id);
    assert_eq!(st.messages, d.chat.messages);
    let items = crate::chat::history_items(&st.messages, &st.earlier);
    let text = format!("{items:?}");
    for n in 1..5 {
        assert_eq!(text.matches(&format!("question {n}")).count(), 1, "{text}");
        assert_eq!(text.matches(&format!("answer {n}")).count(), 1, "{text}");
    }
    assert!(!text.contains("the summary"));
}
