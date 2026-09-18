//! The driver on synthetic scenarios through the stand-in turn (WP 3.1's
//! gate), and on the scenario files the request-parity harness uses.

use std::path::Path;
use std::sync::Arc;

use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::engine::tests::StandIn;
use crate::engine::tests::delta;

fn scenario(value: Value) -> Scenario {
    Scenario::from_json(&value).unwrap()
}

#[tokio::test]
async fn steps_and_scripted_answers_go_through_ops_and_events() {
    let mut engine = Engine::spawn(Arc::new(StandIn));
    let run = drive(
        &mut engine,
        &scenario(json!({
            "steps": [
                {"user": "hello"},
                {"command": "/maxtok 2048"},
                {"user": "ask before deleting"},
            ],
            "approvals": [{"question": "rm -rf build", "answer": "y"}],
        })),
    )
    .await;
    assert_eq!(run.problems, Vec::<String>::new());
    let msgs: Vec<EventMsg> = run.events.iter().map(|e| e.msg.clone()).collect();
    assert!(msgs.contains(&delta("echo: hello")));
    assert!(msgs.contains(&EventMsg::CommandComplete(
        viy_protocol::CommandCompleteEvent {
            command: "/maxtok 2048".into()
        }
    )));
    assert!(msgs.contains(&delta("answered Yes")));
    assert_eq!(msgs.last(), Some(&EventMsg::ShutdownComplete));
    let turns_done = msgs
        .iter()
        .filter(|m| matches!(m, EventMsg::TurnComplete(_)))
        .count();
    assert_eq!(turns_done, 2);
}

#[tokio::test]
async fn unscripted_questions_and_unasked_answers_are_problems() {
    let mut engine = Engine::spawn(Arc::new(StandIn));
    let run = drive(
        &mut engine,
        &scenario(json!({
            "steps": [{"user": "ask"}],
            "approvals": [
                {"question": "something else", "answer": "y"},
                {"question": "never asked", "answer": "n"},
            ],
        })),
    )
    .await;
    assert_eq!(
        run.problems,
        [
            "unscripted question \"  rm -rf build?\" (script expected Some(\"something else\"))",
            "scripted answer never asked for: \"never asked\"",
        ]
    );
    // The unscripted question got its unattended answer.
    assert!(run.events.iter().any(|e| e.msg == delta("answered No")));
}

#[test]
fn the_parity_scenarios_parse() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/scenarios");
    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
    };
    let plain = scenario(read("plain-two-turns.json"));
    assert_eq!(
        plain.steps,
        [
            Step::User("What is a haiku? One sentence, please.".into()),
            Step::User("Now write one about rain on a tin roof.".into()),
        ]
    );
    assert!(plain.approvals.is_empty());
    let write = scenario(read("read-then-protected-write.json"));
    assert_eq!(write.approvals.len(), 1);
}

#[test]
fn a_malformed_scenario_says_where() {
    assert_eq!(
        Scenario::from_json(&json!({"steps": [{"user": "a", "command": "/b"}]})),
        Err("step 0: needs one of \"user\" or \"command\"".into())
    );
    assert_eq!(
        Scenario::from_json(&json!({"approvals": [{"question": "q", "answer": "yes"}]})),
        Err("approval 0: answer Some(\"yes\") is not \"y\", \"n\" or \"a\"".into())
    );
}
