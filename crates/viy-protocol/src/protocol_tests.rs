use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use super::*;

fn round_trip<T>(value: &T, expected: Value)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_value(value).unwrap();
    assert_eq!(json, expected);
    assert_eq!(&serde_json::from_value::<T>(json).unwrap(), value);
}

#[test]
fn events_are_tagged_like_codex() {
    round_trip(
        &Event {
            id: "1".into(),
            msg: EventMsg::AgentMessageContentDelta(DeltaEvent {
                delta: "Hai".into(),
            }),
        },
        json!({"id": "1", "msg": {"type": "agent_message_content_delta", "delta": "Hai"}}),
    );
    round_trip(
        &EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Failed {
                message: "HTTP 503".into(),
            },
        }),
        json!({"type": "turn_complete", "turn_id": "t1",
               "outcome": {"kind": "failed", "message": "HTTP 503"}}),
    );
    round_trip(
        &EventMsg::Background(BackgroundEvent {
            task: BackgroundTask::Helper { n: 2 },
            state: BackgroundState::Running,
            progress: None,
        }),
        json!({"type": "background", "task": {"kind": "helper", "n": 2},
               "state": "running", "progress": null}),
    );
    round_trip(
        &EventMsg::HistoryRewritten(HistoryRewrittenEvent {
            reason: RewriteReason::Rewind,
            items: vec![
                HistoryItem::User { text: "hi".into() },
                HistoryItem::Notice(NoticeEvent {
                    level: NoticeLevel::Warn,
                    code: "length_cut".into(),
                    message: "cut at max_tokens".into(),
                }),
            ],
        }),
        json!({"type": "history_rewritten", "reason": "rewind", "items": [
            {"type": "user", "text": "hi"},
            {"type": "notice", "level": "warn", "code": "length_cut",
             "message": "cut at max_tokens"}]}),
    );
}

#[test]
fn ops_are_tagged_like_events() {
    round_trip(
        &Submission {
            id: "2".into(),
            op: Op::UserTurn {
                text: "What is a haiku?".into(),
            },
        },
        json!({"id": "2", "op": {"type": "user_turn", "text": "What is a haiku?"}}),
    );
    round_trip(&Op::Interrupt, json!({"type": "interrupt"}));
    round_trip(
        &Op::QuestionAnswer {
            id: "q1".into(),
            answer: Answer::Always,
        },
        json!({"type": "question_answer", "id": "q1", "answer": "always"}),
    );
}

#[test]
fn a_question_is_a_request_event() {
    round_trip(
        &EventMsg::QuestionRequest(QuestionRequestEvent {
            question_id: "q1".into(),
            question: Question::yes_no(Some(QuestionKind::Destructive), "  run it?"),
        }),
        json!({"type": "question_request", "question_id": "q1", "question": {
            "kind": "destructive", "prompt": "  run it?", "choices": "yes_no",
            "default": "no", "unattended": "no"}}),
    );
    round_trip(
        &EventMsg::ShutdownComplete,
        json!({"type": "shutdown_complete"}),
    );
}

#[test]
fn question_kinds_serialize_as_rpctl_names_them() {
    // rpctl modes.KINDS at the pin, in order.
    let kinds = [
        QuestionKind::Destructive,
        QuestionKind::SecretRead,
        QuestionKind::Consult,
        QuestionKind::Network,
        QuestionKind::Fetch,
        QuestionKind::ProtectedWrite,
        QuestionKind::Unsandboxed,
    ];
    let names: Vec<Value> = kinds
        .iter()
        .map(|k| serde_json::to_value(k).unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "destructive",
            "secret_read",
            "consult",
            "network",
            "fetch",
            "protected_write",
            "unsandboxed"
        ]
    );
}

#[test]
fn an_approval_answers_no_by_default_and_unattended() {
    let q = Question::yes_no_always(Some(QuestionKind::Network), "  run it with network access?");
    assert_eq!(q.choices, Choices::YesNoAlways);
    assert_eq!((q.default, q.unattended), (Answer::No, Answer::No));
    let q = Question::yes_no(None, "  mount it read-write?");
    assert_eq!((q.kind, q.choices), (None, Choices::YesNo));
    assert_eq!((q.default, q.unattended), (Answer::No, Answer::No));
}
