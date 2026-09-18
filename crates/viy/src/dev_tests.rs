use pretty_assertions::assert_eq;
use serde_json::Value;

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

#[test]
fn drive_takes_scenario_one_through_the_engine() {
    let scenario = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/scenarios/plain-two-turns.json"
    );
    let (code, events, err) = run_capture(&["dev", "drive", scenario]);
    assert_eq!((code, err.as_str()), (0, ""));
    let types: Vec<&str> = events
        .iter()
        .map(|e| e["msg"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "turn_started",
            "turn_complete",
            "turn_started",
            "turn_complete",
            "shutdown_complete"
        ]
    );
    assert_eq!(
        events[0]["msg"]["user_text"],
        "What is a haiku? One sentence, please."
    );
    assert_eq!(
        events[1]["msg"]["outcome"]["message"],
        "the chat's model turn comes with WP 3.2"
    );
}

#[test]
fn drive_says_what_is_wrong_with_a_scenario() {
    let (code, _, err) = run_capture(&["dev", "drive", "/nonexistent/scenario.json"]);
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
