//! rpctl's MonitorTests (test_workflows.py), and the tracer's files.

use std::sync::Arc;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::stream::Round;

fn quiet() -> Tracer {
    Tracer::new(
        PathBuf::from("/nonexistent/viy-state"),
        false,
        Arc::new(|| Some("s1".into())),
    )
}

fn monitor() -> TurnMonitor {
    TurnMonitor::new(quiet(), "main", None, Arc::new(|_, _| {}))
}

fn out(reasoning: &str, finish: &str) -> Round {
    Round {
        reasoning: reasoning.into(),
        finish: Some(finish.into()),
        ..Round::default()
    }
}

#[test]
fn repeat_note_then_alarm() {
    let mut m = monitor();
    assert_eq!(m.on_tool("read_file", r#"{"path": "a"}"#, "{}", 0.0), None);
    let note = m
        .on_tool("read_file", r#"{ "path":"a" }"#, "{}", 0.0)
        .unwrap();
    assert!(note.contains("already made"), "{note}");
    m.on_tool("read_file", r#"{"path": "a"}"#, "{}", 0.0);
    assert!(m.alarms.contains(&"repeat_call".to_string()));
}

#[test]
fn same_call_with_a_new_result_is_progress() {
    let mut m = monitor();
    for i in 0..4 {
        let payload = format!(r#"{{"failed": {}}}"#, 3 - i);
        let note = m.on_tool("run_shell", r#"{"command": "pytest"}"#, &payload, 0.0);
        assert_eq!(note, None);
    }
    assert!(m.alarms.is_empty());
}

#[test]
fn going_in_circles() {
    let mut m = monitor();
    for p in ["a", "b", "a", "b", "a", "b"] {
        m.on_tool("read_file", &json!({"path": p}).to_string(), "{}", 0.0);
    }
    assert!(m.alarms.contains(&"going_in_circles".to_string()));
}

#[test]
fn identical_output_for_different_arguments() {
    let mut m = monitor();
    let big = json!({"content": "x".repeat(300)}).to_string();
    assert_eq!(
        m.on_tool("read_file", r#"{"path": "a", "offset": 1}"#, &big, 0.0),
        None
    );
    let note = m
        .on_tool("read_file", r#"{"path": "a", "offset": 50}"#, &big, 0.0)
        .unwrap();
    assert!(note.contains("same output"), "{note}");
    assert!(m.alarms.is_empty());
    m.on_tool("read_file", r#"{"path": "a", "offset": 90}"#, &big, 0.0);
    assert!(m.alarms.contains(&"same_result".to_string()));
    // Small results like {"ok": true} never count.
    let mut m2 = monitor();
    for p in ["a", "b", "c"] {
        m2.on_tool(
            "write_file",
            &json!({"path": p}).to_string(),
            r#"{"ok": true}"#,
            0.0,
        );
    }
    assert!(m2.alarms.is_empty());
}

#[test]
fn consecutive_errors() {
    let mut m = monitor();
    for i in 0..3 {
        m.on_tool(
            "run_shell",
            &json!({"command": format!("x{i}")}).to_string(),
            r#"{"error": "boom"}"#,
            0.0,
        );
    }
    assert_eq!(m.alarms, ["tool_errors"]);
    let mut m2 = monitor();
    for (i, p) in [r#"{"error": "x"}"#, r#"{"ok": 1}"#, r#"{"error": "x"}"#]
        .iter()
        .enumerate()
    {
        m2.on_tool(
            "run_shell",
            &json!({"command": i.to_string()}).to_string(),
            p,
            0.0,
        );
    }
    assert!(m2.alarms.is_empty());
}

#[test]
fn repetitive_reasoning_and_token_limit() {
    let mut m = monitor();
    let looping = "Wait, let me re-check the value of the counter here. ".repeat(5);
    m.on_round(
        0,
        &out(&format!("Fine start. {looping}"), "length"),
        1.0,
        "auto",
    );
    let mut codes = m.alarms.clone();
    codes.sort();
    assert_eq!(codes, ["repetitive_reasoning", "token_limit"]);
    let mut m2 = monitor();
    m2.on_round(
        0,
        &out(
            "One idea here that is long enough. Another different idea that is long enough.",
            "stop",
        ),
        1.0,
        "auto",
    );
    assert!(m2.alarms.is_empty());
}

// ------------------------------------------------------------ the pieces

#[test]
fn signatures_ignore_key_order_and_command_spacing() {
    assert_eq!(
        call_signature("run_shell", r#"{"timeout": 5, "command": "ls   -la\n"}"#),
        call_signature("run_shell", r#"{"command": "ls -la", "timeout": 5}"#)
    );
    assert_eq!(
        call_signature("read_file", r#"{"path": {"b": 1, "a": 2}}"#),
        call_signature("read_file", r#"{"path": {"a": 2, "b": 1}}"#)
    );
    assert_eq!(call_signature("list_dir", "  "), "list_dir:{}");
    assert_eq!(call_signature("x", "not  json\n"), "x:not json");
}

#[test]
fn tool_errors_as_rp_reads_them() {
    assert_eq!(tool_error(r#"{"error": "boom"}"#).as_deref(), Some("boom"));
    assert_eq!(tool_error(r#"{"error": ""}"#), None);
    assert_eq!(
        tool_error(r#"{"declined": true}"#).as_deref(),
        Some("declined")
    );
    assert_eq!(
        tool_error(r#"{"denied": true, "note": "no"}"#).as_deref(),
        Some("no")
    );
    assert_eq!(tool_error("[1]"), None);
    assert_eq!(tool_error("plain text"), None);
    assert_eq!(
        tool_error(&json!({"error": "é".repeat(300)}).to_string())
            .unwrap()
            .chars()
            .count(),
        200
    );
}

#[test]
fn notes_go_into_an_object_or_on_a_line_of_their_own() {
    assert_eq!(with_note(r#"{"a":1}"#, "n"), r#"{"a":1,"rpctl_note":"n"}"#);
    assert_eq!(with_note("[1]", "n"), "[1]\nn");
    assert_eq!(with_note("text", "n"), "text\nn");
}

#[test]
fn sentences_split_after_a_stop_and_at_line_breaks() {
    assert_eq!(
        sentences("One. Two!  Three?\nFour\n\nfive. "),
        ["One.", "Two!", "Three?", "Four", "five.", ""]
    );
    assert_eq!(sentences("a.b c"), ["a.b c"]);
    // The most repeated wins; the first seen wins a tie.
    let a = "this sentence is long enough to count. ".repeat(4);
    let b = "another sentence that is long enough too. ".repeat(4);
    assert_eq!(
        repeated_sentence(&format!("{a}{b}")),
        Some(("this sentence is long enough to count.".into(), 4))
    );
    assert_eq!(repeated_sentence(&"short. ".repeat(9)), None);
}

#[test]
fn files_and_rows() {
    let scratch = ScratchDir::new("trace").unwrap();
    let t = Tracer::new(scratch.path().to_path_buf(), true, Arc::new(|| None));
    assert_eq!(t.sid(), format!("nosession-{}", std::process::id()));
    let said = Arc::new(std::sync::Mutex::new(Vec::new()));
    let echo: Echo = {
        let said = Arc::clone(&said);
        Arc::new(move |sev, line| said.lock().unwrap().push((sev, line)))
    };
    let mut m = TurnMonitor::new(t.clone(), "main", Some(3), echo);
    m.on_round(0, &out("", "length"), 400.0, "auto");
    m.mark("no_answer", "the turn ended without an answer");
    m.mark("no_answer", "the turn ended without an answer");
    let mut extra = Map::new();
    extra.insert("interrupted".into(), json!(true));
    let row = m.finish(extra);
    assert_eq!(
        row["alarms"],
        json!(["token_limit", "slow_round", "no_answer"])
    );
    assert_eq!(
        *said.lock().unwrap(),
        [
            (
                Severity::Warn,
                "  [alarm: token_limit] main: a reply hit the token limit (round 0)".to_string()
            ),
            (
                Severity::Error,
                "  [alarm: no_answer] the turn ended without an answer".to_string()
            ),
        ]
    );
    let read = |p: PathBuf| -> Vec<Value> {
        std::fs::read_to_string(p)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let turns = read(t.turns_path());
    assert_eq!(turns.len(), 1);
    let keys: Vec<&str> = turns[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "ts",
            "sid",
            "label",
            "turn",
            "secs",
            "rounds",
            "tool_calls",
            "errors",
            "repeats",
            "same_results",
            "completion_tokens",
            "reasoning_chars",
            "helpers",
            "helper_failures",
            "alarms",
            "interrupted"
        ]
    );
    let alarms = read(t.alarms_path());
    assert_eq!(alarms.len(), 3);
    assert_eq!(alarms[1]["severity"], "info");
    assert_eq!(alarms[2]["turn"], 3);
    let kinds: Vec<Value> = read(t.trace_path())
        .iter()
        .map(|r| r["kind"].clone())
        .collect();
    assert_eq!(
        kinds,
        [
            json!("round"),
            json!("alarm"),
            json!("alarm"),
            json!("alarm"),
            json!("turn_end")
        ]
    );
}

// ------------------------------------------------------------------- oracle

/// Generated turns through rpctl's TurnMonitor and viy's: the notes the
/// model reads, the alarms and what they said. Spacing is ASCII and the
/// usual Unicode: Python's `str.split` also splits at U+001C-U+001F, which
/// Rust's does not (a command differing only there is a repeat in rp).
#[cfg(feature = "oracle")]
#[test]
fn monitor_matches_rpctl() {
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    const NAMES: &[&str] = &["read_file", "run_shell", "list_dir", "write_file"];
    const ARGS: &[&str] = &[
        r#"{"path": "a"}"#,
        r#"{ "path":"a" }"#,
        r#"{"path": "b"}"#,
        r#"{"path": "a", "offset": 1}"#,
        r#"{"offset": 1, "path": "a"}"#,
        r#"{"path": "a", "offset": 1.0}"#,
        r#"{"command": "ls  -la"}"#,
        r#"{"command": "ls -la\n"}"#,
        "{\"command\": \"ls\u{a0}-la\"}",
        r#"{"command": "pytest", "timeout": 5}"#,
        r#"{"timeout": 5, "command": "pytest"}"#,
        r#"{"command": ["ls"]}"#,
        r#"{"path": {"b": 1, "a": [2, {"d": 1, "c": 0}]}}"#,
        r#"{"path": {"a": [2, {"c": 0, "d": 1}], "b": 1}}"#,
        r#"{"path": "é"}"#,
        "",
        "  ",
        "{}",
        "not json",
        "not  json",
        r#"{"path": "a""#,
        "[1, 2]",
        "7",
    ];
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let big = |c: &str| json!({"content": c.repeat(250)}).to_string();
    let payloads: Vec<String> = vec![
        "{}".into(),
        r#"{"ok": true}"#.into(),
        r#"{"error": "boom"}"#.into(),
        r#"{"error": ""}"#.into(),
        r#"{"error": 0}"#.into(),
        r#"{"declined": true}"#.into(),
        r#"{"denied": 1, "note": "the user said no"}"#.into(),
        r#"{"declined": false}"#.into(),
        big("x"),
        big("y"),
        "x".repeat(199),
        "x".repeat(200),
        "plain text".into(),
        "[1]".into(),
    ];
    const WORDS: &[&str] = &[
        "Wait, let me re-check the value of the counter here.",
        "Another different idea that is long enough.",
        "short.",
        " ",
        "  ",
        "\n",
        "\n\n",
        "! ",
        "? ",
        ". ",
        "WAIT, LET ME RE-CHECK THE VALUE OF THE COUNTER HERE.",
        "é",
        "\t",
    ];
    for case in 0..cases(3000) {
        let calls: Vec<(String, String, String)> = (0..g.below(14))
            .map(|_| {
                (
                    g.pick(NAMES).to_string(),
                    g.pick(ARGS).to_string(),
                    g.pick(&payloads).clone(),
                )
            })
            .collect();
        let rounds: Vec<(String, String, f64)> = (0..g.below(3))
            .map(|_| {
                (
                    g.string(WORDS, 12),
                    g.pick(&["stop", "length", "tool_calls"]).to_string(),
                    *g.pick(&[1.0, 299.5, 300.0, 301.0]),
                )
            })
            .collect();
        let said = Arc::new(std::sync::Mutex::new(Vec::new()));
        let echo: Echo = {
            let said = Arc::clone(&said);
            Arc::new(move |_, line| said.lock().unwrap().push(line))
        };
        let mut m = TurnMonitor::new(quiet(), "main", None, echo);
        for (i, (reasoning, finish, secs)) in rounds.iter().enumerate() {
            m.on_round(i as u64, &out(reasoning, finish), *secs, "auto");
        }
        let notes: Vec<Option<String>> = calls
            .iter()
            .map(|(n, a, p)| m.on_tool(n, a, p, 0.0))
            .collect();
        let mine = json!({"notes": notes, "alarms": m.alarms, "said": *said.lock().unwrap()});
        let theirs = oracle
            .call(
                "trace.monitor",
                json!([
                    calls
                        .iter()
                        .map(|(n, a, p)| json!([n, a, p]))
                        .collect::<Vec<_>>(),
                    rounds
                        .iter()
                        .map(|(r, f, s)| json!([r, f, s]))
                        .collect::<Vec<_>>()
                ]),
                json!({}),
            )
            .unwrap()
            .outcome;
        assert_eq!(
            theirs,
            Outcome::Returned(mine),
            "case {case}, seed {:#x}: {calls:?} {rounds:?}",
            seed()
        );
    }
}
