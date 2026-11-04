use std::collections::HashSet;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_testkit::fixtures;

use super::*;

const KINDS: [QuestionKind; 8] = [
    QuestionKind::Destructive,
    QuestionKind::SecretRead,
    QuestionKind::Consult,
    QuestionKind::Network,
    QuestionKind::Fetch,
    QuestionKind::ProtectedWrite,
    QuestionKind::Unsandboxed,
    QuestionKind::Remote,
];

fn cases(name: &str) -> Vec<Value> {
    fixtures::load("ui/questions")
        .unwrap()
        .cases
        .into_iter()
        .filter(|c| c["name"] == name)
        .collect()
}

fn kind_of(v: &Value) -> Option<QuestionKind> {
    serde_json::from_value(v.clone()).unwrap()
}

/// rp's result as an answer: `True`, `False`, `"always"`.
fn rp_answer(v: &Value) -> Answer {
    match v {
        Value::Bool(true) => Answer::Yes,
        Value::Bool(false) => Answer::No,
        Value::String(s) if s == "always" => Answer::Always,
        other => panic!("rp answered {other}"),
    }
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

/// A queue holding `question`, drawn at `t0`.
fn shown(question: Question, t0: Instant) -> Questions {
    let mut q = Questions::default();
    q.push("q1".into(), question);
    q.drawn(t0, true);
    q
}

const AFTER: Duration = Duration::from_millis(301);

#[test]
fn every_kind_rp_asks_has_a_box_title() {
    let case = &cases("kinds")[0];
    let rp: HashSet<String> = case["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_string())
        .collect();
    let viy: HashSet<String> = KINDS
        .iter()
        .map(|k| {
            serde_json::to_value(k)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(viy, rp);
    for name in case["auto_yes"].as_array().unwrap() {
        assert!(kind_of(name).is_some(), "{name}");
    }
    let labels: HashSet<&str> = KINDS.iter().map(|&k| kind_label(Some(k))).collect();
    assert_eq!(labels.len(), KINDS.len());
    assert!(!labels.contains(kind_label(None)));
}

#[test]
fn the_question_line_is_rps() {
    for case in cases("confirm") {
        let kind = kind_of(&case["kind"]);
        let question = if case["always"] == true {
            Question::yes_no_always(kind, "  run it?")
        } else {
            Question::yes_no(kind, "  run it?")
        };
        let line = format!("{} {} ", question.prompt, question.hint());
        assert_eq!(json!(line), case["line"], "{case}");
    }
    for case in cases("pod_ask") {
        let default = if case["default"] == true {
            Answer::Yes
        } else {
            Answer::No
        };
        let question = Question {
            default,
            unattended: default,
            ..Question::yes_no(None, "Try the backup region?")
        };
        let line = format!("{} {} ", question.prompt, question.hint());
        assert_eq!(json!(line), case["line"], "{case}");
    }
}

/// A key the box takes answers as rp takes the same line; a key it leaves
/// to the input box is one rp would have taken as no.
#[test]
fn a_key_means_what_the_typed_line_means_to_rp() {
    let t0 = Instant::now();
    let mut checked = 0;
    let mut questions: Vec<(Question, Value)> = Vec::new();
    for case in cases("confirm") {
        let kind = kind_of(&case["kind"]);
        let question = if case["always"] == true {
            Question::yes_no_always(kind, "  run it?")
        } else {
            Question::yes_no(kind, "  run it?")
        };
        questions.push((question, case));
    }
    for case in cases("pod_ask") {
        let default = if case["default"] == true {
            Answer::Yes
        } else {
            Answer::No
        };
        let question = Question {
            default,
            unattended: default,
            ..Question::yes_no(None, "Try the backup region?")
        };
        questions.push((question, case));
    }
    for (question, case) in questions {
        let typed = case["typed"].as_str().unwrap();
        let mut chars = typed.chars();
        let code = match (chars.next(), chars.next()) {
            (None, _) => KeyCode::Enter,
            (Some(c), None) => KeyCode::Char(c),
            _ => continue,
        };
        let rp = rp_answer(&case["result"]);
        let mut q = shown(question, t0);
        match q.key(press(code), t0 + AFTER, false) {
            Some(Taken::Answered { answer, .. }) => assert_eq!(answer, rp, "{case}"),
            None => assert_eq!(rp, Answer::No, "{case}"),
            other => panic!("{other:?}"),
        }
        checked += 1;
    }
    assert!(checked >= 30, "{checked}");
}

#[test]
fn with_no_terminal_every_approval_is_no() {
    for case in cases("no_terminal") {
        if case.get("pod_default").is_some() {
            assert_eq!(case["result"], case["pod_default"]);
            continue;
        }
        assert_eq!(case["result"], false, "{case}");
        let kind = kind_of(&case["kind"]);
        for question in [
            Question::yes_no(kind, "  run it?"),
            Question::yes_no_always(kind, "  run it?"),
        ] {
            assert_eq!(question.unattended, Answer::No);
        }
    }
}

#[test]
fn no_key_answers_before_the_box_is_drawn() {
    let t0 = Instant::now();
    let mut q = Questions::default();
    q.push("q1".into(), Question::yes_no(None, "go?"));
    for key in [press(KeyCode::Char('y')), press(KeyCode::Enter), ctrl_c()] {
        assert_eq!(q.key(key, t0 + Duration::from_secs(5), false), None);
    }
    assert_eq!(q.armed_at(), None);
}

#[test]
fn the_guard_runs_from_the_box_drawn_or_the_last_key_typed() {
    let t0 = Instant::now();
    let mut q = shown(Question::yes_no(None, "go?"), t0);
    assert_eq!(q.armed_at(), Some(t0 + GUARD));
    assert_eq!(
        q.key(
            press(KeyCode::Char('y')),
            t0 + GUARD - Duration::from_millis(1),
            false
        ),
        None
    );
    // Typing goes on: the guard starts again from each key.
    q.typed(t0 + Duration::from_millis(250));
    assert_eq!(q.armed_at(), Some(t0 + Duration::from_millis(550)));
    assert!(!q.armed(t0 + Duration::from_millis(400)));
    assert!(q.armed(t0 + Duration::from_millis(550)));
    let Some(Taken::Answered { answer, question }) = q.key(
        press(KeyCode::Char('y')),
        t0 + Duration::from_millis(550),
        false,
    ) else {
        panic!()
    };
    assert_eq!((answer, question.id.as_str()), (Answer::Yes, "q1"));
    assert!(q.front().is_none());
}

#[test]
fn a_key_typed_before_the_box_counts_too() {
    let t0 = Instant::now();
    let mut q = Questions::default();
    q.typed(t0);
    q.push("q1".into(), Question::yes_no(None, "go?"));
    q.drawn(t0 - Duration::from_millis(100), true);
    assert_eq!(q.armed_at(), Some(t0 + GUARD));
}

#[test]
fn ctrl_c_answers_no_once_the_box_takes_keys() {
    let t0 = Instant::now();
    let mut q = shown(Question::yes_no(Some(QuestionKind::Network), "go?"), t0);
    // Typed ahead, it is the app's: it stops the work that asked.
    assert_eq!(q.key(ctrl_c(), t0, false), None);
    let Some(Taken::Answered { answer, .. }) = q.key(ctrl_c(), t0 + AFTER, true) else {
        panic!()
    };
    assert_eq!(answer, Answer::No);
}

#[test]
fn enter_with_a_draft_sends_the_draft_not_the_default() {
    let t0 = Instant::now();
    let question = Question {
        default: Answer::Yes,
        unattended: Answer::Yes,
        ..Question::yes_no(None, "Try the backup region?")
    };
    let mut q = shown(question, t0);
    assert_eq!(q.key(press(KeyCode::Enter), t0 + AFTER, true), None);
    let Some(Taken::Answered { answer, .. }) = q.key(press(KeyCode::Enter), t0 + AFTER, false)
    else {
        panic!()
    };
    assert_eq!(answer, Answer::Yes);
}

#[test]
fn a_box_cut_short_stops_taking_keys_until_whole_again() {
    let t0 = Instant::now();
    let mut q = shown(Question::yes_no(None, "go?"), t0);
    assert!(q.armed(t0 + AFTER));
    q.drawn(t0 + AFTER, false);
    assert_eq!(q.key(press(KeyCode::Char('y')), t0 + AFTER, false), None);
    assert_eq!(q.key(ctrl_c(), t0 + AFTER, false), None);
    // Whole again: the guard starts from there.
    let t1 = t0 + Duration::from_secs(2);
    q.drawn(t1, true);
    assert_eq!(q.armed_at(), Some(t1 + GUARD));
}

#[test]
fn shift_tab_at_the_box_names_it_and_leaves_it_open() {
    let t0 = Instant::now();
    let mut q = shown(Question::yes_no(Some(QuestionKind::Network), "go?"), t0);
    let back_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(q.key(back_tab, t0, false), None);
    assert_eq!(
        q.key(back_tab, t0 + AFTER, false),
        Some(Taken::CycleMode {
            question_id: "q1".into()
        })
    );
    assert_eq!(q.front().map(|b| b.id.as_str()), Some("q1"));
}

#[test]
fn other_keys_and_modified_ones_are_left_alone() {
    let t0 = Instant::now();
    let mut q = shown(Question::yes_no(None, "go?"), t0);
    for key in [
        press(KeyCode::Char('x')),
        press(KeyCode::Char('a')),
        press(KeyCode::Esc),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::ALT),
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
    ] {
        assert_eq!(q.key(key, t0 + AFTER, false), None, "{key:?}");
    }
}

#[test]
fn questions_wait_their_turn_and_each_has_its_own_guard() {
    let t0 = Instant::now();
    let mut q = Questions::default();
    q.push("q1".into(), Question::yes_no(None, "first?"));
    q.push("q2".into(), Question::yes_no(None, "second?"));
    q.push("q3".into(), Question::yes_no(None, "third?"));
    assert_eq!(q.waiting(), 2);
    q.drawn(t0, true);
    assert!(
        q.key(press(KeyCode::Char('n')), t0 + AFTER, false)
            .is_some()
    );
    assert_eq!(q.front().unwrap().id, "q2");
    // Not drawn yet: the key that answered the first cannot answer it.
    assert_eq!(q.key(press(KeyCode::Char('n')), t0 + AFTER, false), None);
    assert_eq!(q.armed_at(), None);
    // The engine drops one that waits.
    assert_eq!(q.resolve("q3").unwrap().id, "q3");
    assert_eq!(q.waiting(), 0);
    assert_eq!(q.resolve("q3"), None);
}

fn text(rows: &[Line<'_>]) -> Vec<String> {
    rows.iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn reasoned() -> Question {
    Question {
        reason: vec![NoticeEvent {
            level: NoticeLevel::Warn,
            code: "approve_auto".into(),
            message: "  run_shell wants the network and pip install httpx".into(),
        }],
        ..Question::yes_no_always(Some(QuestionKind::Network), "  run it with network access?")
    }
}

#[test]
fn a_box_shows_its_kind_reason_question_and_keys() {
    let t0 = Instant::now();
    let q = shown(reasoned(), t0);
    let (rows, whole) = q.rows(60, 20, t0);
    assert!(whole);
    assert_eq!(
        text(&rows),
        [
            "┌ network access ──────────────────────────────────────────┐",
            "│   run_shell wants the network and pip install httpx      │",
            "│   run it with network access? [y/N/a=always]             │",
            "│ [y] yes   [n] no (default)   [a] always   [⇧Tab] mode    │",
            "└──────────────────────────────────────────────────────────┘",
        ]
    );
    // Dim until it takes keys, then not.
    let keys = |row: &Line<'_>| -> Vec<Style> {
        row.spans[1..row.spans.len() - 2]
            .iter()
            .map(|s| s.style)
            .collect()
    };
    assert!(keys(&rows[3]).iter().all(|&s| s == Style::new().dim()));
    let (rows, _) = q.rows(60, 20, t0 + AFTER);
    assert!(keys(&rows[3]).iter().all(|&s| s != Style::new().dim()));
}

#[test]
fn a_box_without_room_for_its_frame_goes_without_then_says_it_does_not_fit() {
    let t0 = Instant::now();
    let q = shown(reasoned(), t0);
    let (rows, whole) = q.rows(60, 5, t0);
    assert!(whole);
    assert_eq!(rows.len(), 5);
    let (rows, whole) = q.rows(60, 4, t0);
    assert!(whole);
    assert_eq!(
        text(&rows),
        [
            "  run_shell wants the network and pip install httpx",
            "  run it with network access? [y/N/a=always]",
            "[y] yes   [n] no (default)   [a] always   [⇧Tab] mode",
        ]
    );
    // The reason never gives way on its own: the box is cut and says so.
    let (rows, whole) = q.rows(60, 2, t0);
    assert!(!whole);
    assert_eq!(
        text(&rows),
        [
            "[y] yes   [n] no (default)   [a] always   [⇧Tab] mode",
            "too little room for this question: make the terminal taller",
        ]
    );
    for max in 0..2 {
        let (rows, whole) = q.rows(60, max, t0);
        assert!(!whole);
        assert!(rows.len() <= max, "{max}: {rows:?}");
    }
}

#[test]
fn a_default_yes_question_says_so() {
    let t0 = Instant::now();
    let question = Question {
        default: Answer::Yes,
        unattended: Answer::Yes,
        ..Question::yes_no(None, "Try the backup region?")
    };
    let q = shown(question, t0);
    let (rows, _) = q.rows(40, 20, t0);
    assert_eq!(
        text(&rows),
        [
            "┌ question ────────────────────────────┐",
            "│ Try the backup region? [Y/n]         │",
            "│ [y] yes (default)   [n] no           │",
            "└──────────────────────────────────────┘",
        ]
    );
}

#[test]
fn the_transcript_keeps_the_reason_question_and_answer() {
    let b = QuestionBox {
        id: "q1".into(),
        question: reasoned(),
        shown: None,
        chosen: 0,
    };
    let Cell::Lines { lines, .. } = b.cell(Some(Answer::Always)) else {
        panic!()
    };
    assert_eq!(
        text(&lines),
        [
            "  run_shell wants the network and pip install httpx",
            "  run it with network access? [y/N/a=always] a",
        ]
    );
    let Cell::Lines { lines, .. } = b.cell(None) else {
        panic!()
    };
    assert_eq!(
        text(&lines)[1],
        "  run it with network access? [y/N/a=always]"
    );
}

#[test]
fn a_question_that_can_be_called_off_takes_q_esc_and_ctrl_c_as_cancel() {
    let t0 = Instant::now();
    let question = Question::yes_no_cancel("put it back as it was?");
    let (rows, _) = shown(question.clone(), t0).rows(50, 20, t0);
    assert_eq!(
        text(&rows),
        [
            "┌ question ──────────────────────────────────────┐",
            "│ put it back as it was? [Y/n, q = cancel]       │",
            "│ [y] yes (default)   [n] no   [q] cancel        │",
            "└────────────────────────────────────────────────┘",
        ]
    );
    for (key, want) in [
        (press(KeyCode::Enter), Answer::Yes),
        (press(KeyCode::Char('y')), Answer::Yes),
        (press(KeyCode::Char('n')), Answer::No),
        (press(KeyCode::Char('q')), Answer::Cancel),
        (press(KeyCode::Esc), Answer::Cancel),
        (ctrl_c(), Answer::Cancel),
    ] {
        let mut q = shown(question.clone(), t0);
        let Some(Taken::Answered { answer, .. }) = q.key(key, t0 + AFTER, false) else {
            panic!("{key:?}")
        };
        assert_eq!(answer, want, "{key:?}");
    }
    assert_eq!(answer_word(Answer::Cancel), "q");
    // Not one of the others: q there is nobody's.
    let mut q = shown(Question::yes_no(None, "go?"), t0);
    assert_eq!(q.key(press(KeyCode::Char('q')), t0 + AFTER, false), None);
}
