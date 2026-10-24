//! Question boxes in the chat's screen (WP 3.11, ui-plan 6, flag F37):
//! typed-ahead keys never answer, Enter alone takes the default, Ctrl-C at
//! a box answers no, questions wait their turn.

use pretty_assertions::assert_eq;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;

use super::*;
use crate::question::GUARD;

fn ask(f: &mut Fixture, id: &str, question: Question) {
    f.app.event(EventMsg::QuestionRequest(QuestionRequestEvent {
        question_id: id.into(),
        question,
    }));
}

fn network() -> Question {
    Question {
        reason: vec![NoticeEvent {
            level: NoticeLevel::Warn,
            code: "approve_auto".into(),
            message: "  wants the network: pip install httpx".into(),
        }],
        ..Question::yes_no_always(Some(QuestionKind::Network), "  run it with network access?")
    }
}

fn answers(ops: &[Op]) -> Vec<(String, Answer)> {
    ops.iter()
        .filter_map(|op| match op {
            Op::QuestionAnswer { id, answer } => Some((id.clone(), *answer)),
            _ => None,
        })
        .collect()
}

impl Fixture {
    /// `code` pressed `gap` after the last key.
    fn key_after(&mut self, gap: Duration, code: KeyCode) {
        self.clock += gap;
        self.app.dirty = true;
        self.app
            .key(KeyEvent::new(code, KeyModifiers::NONE), self.clock);
    }

    /// Lets the clock run past the guard.
    fn wait(&mut self) {
        self.clock += GUARD + Duration::from_millis(1);
    }
}

#[test]
fn a_box_is_drawn_above_the_input_and_answered_once_seen() {
    let mut f = fixture(50, 12);
    f.app.event(started("install httpx"));
    ask(&mut f, "q1", network());
    let screen = f.screen();
    assert_eq!(
        screen[3..],
        [
            "⠋ waiting · 0 s · Ctrl-C stops",
            "┌ network access ────────────────────────────────┐",
            "│   wants the network: pip install httpx         │",
            "│   run it with network access? [y/N/a=always]   │",
            "│ [y] yes   [n] no (default)   [a] always        │",
            "│ [⇧Tab] mode                                    │",
            "└────────────────────────────────────────────────┘",
            "› ▮",
            "",
        ],
        "{screen:?}"
    );
    f.wait();
    f.key(KeyCode::Char('a'));
    assert_eq!(
        f.ops(),
        [Op::QuestionAnswer {
            id: "q1".into(),
            answer: Answer::Always
        }]
    );
    let screen = f.screen();
    assert_eq!(
        screen[6..10],
        [
            "you › install httpx",
            "  wants the network: pip install httpx",
            "  run it with network access? [y/N/a=always] a",
            "⠋ waiting · 0 s · Ctrl-C stops",
        ],
        "{screen:?}"
    );
}

/// Scripted type-ahead: keys at a typist's pace, from before the box came
/// up to after it, never answer it; they go to the input box. Only a key
/// after a pause of the guard does.
#[test]
fn typed_ahead_keys_never_answer() {
    let mut seed: u64 = 0x5eed_0b0c;
    let mut next = move |n: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let keys = ['y', 'Y', 'n', 'a', 'A', 'e', 's', ' ', '\r'];
    for round in 0..300 {
        let mut f = fixture(60, 12);
        f.app.event(started("work"));
        for _ in 0..next(6) {
            let c = keys[next(keys.len() as u64) as usize];
            let code = if c == '\r' {
                KeyCode::Enter
            } else {
                KeyCode::Char(c)
            };
            f.key_after(Duration::from_millis(10 + next(280)), code);
        }
        let question = match next(3) {
            0 => network(),
            1 => Question::yes_no(Some(QuestionKind::Destructive), "  run it?"),
            _ => Question {
                default: Answer::Yes,
                unattended: Answer::Yes,
                ..Question::yes_no(None, "Try the backup region?")
            },
        };
        ask(&mut f, "q1", question);
        // Drawn at some point within the typing.
        f.clock += Duration::from_millis(next(250));
        f.screen();
        for _ in 0..1 + next(12) {
            let c = keys[next(keys.len() as u64) as usize];
            let code = if c == '\r' {
                KeyCode::Enter
            } else {
                KeyCode::Char(c)
            };
            f.key_after(Duration::from_millis(1 + next(299)), code);
            if next(4) == 0 {
                f.screen();
            }
        }
        assert_eq!(answers(&f.ops()), [], "round {round}");
        assert!(f.app.questions.front().is_some(), "round {round}");
        f.wait();
        f.key(KeyCode::Char('n'));
        assert_eq!(
            answers(&f.ops()),
            [("q1".to_string(), Answer::No)],
            "round {round}"
        );
    }
}

#[test]
fn a_key_before_the_guard_goes_to_the_input_box() {
    let mut f = fixture(50, 10);
    ask(&mut f, "q1", Question::yes_no(None, "go?"));
    f.screen();
    f.key(KeyCode::Char('y'));
    assert_eq!(f.ops(), []);
    assert_eq!(f.screen()[8], "› y▮");
    // Still typing: each key starts the guard again.
    f.key_after(Duration::from_millis(290), KeyCode::Char('e'));
    f.key_after(Duration::from_millis(290), KeyCode::Char('s'));
    f.key_after(Duration::from_millis(290), KeyCode::Char('y'));
    assert_eq!(f.ops(), []);
    f.wait();
    f.key(KeyCode::Char('y'));
    assert_eq!(answers(&f.ops()), [("q1".to_string(), Answer::Yes)]);
    assert_eq!(f.screen().last().map(String::as_str), Some(""));
    assert!(f.screen().iter().any(|r| r == "› yesy▮"));
}

#[test]
fn a_paste_starts_the_guard_again() {
    let mut f = fixture(50, 10);
    ask(&mut f, "q1", Question::yes_no(None, "go?"));
    f.screen();
    f.wait();
    f.app.questions.typed(f.clock);
    f.key(KeyCode::Char('y'));
    assert_eq!(answers(&f.ops()), []);
}

#[test]
fn enter_alone_takes_the_default() {
    let mut f = fixture(50, 10);
    ask(
        &mut f,
        "q1",
        Question::yes_no(Some(QuestionKind::Destructive), "  run it?"),
    );
    let resume = Question {
        default: Answer::Yes,
        unattended: Answer::No,
        ..Question::yes_no(None, "Continue a copy of it instead?")
    };
    ask(&mut f, "q2", resume);
    f.screen();
    f.wait();
    f.key(KeyCode::Enter);
    f.screen();
    f.wait();
    f.key(KeyCode::Enter);
    assert_eq!(
        answers(&f.ops()),
        [
            ("q1".to_string(), Answer::No),
            ("q2".to_string(), Answer::Yes)
        ]
    );
    let screen = f.screen();
    assert!(
        screen.iter().any(|r| r == "  run it? [y/N] n"),
        "{screen:?}"
    );
    assert!(
        screen
            .iter()
            .any(|r| r == "Continue a copy of it instead? [Y/n] y"),
        "{screen:?}"
    );
    assert_eq!(f.app.composer.is_empty(), true);
}

#[test]
fn ctrl_c_at_a_box_answers_no_and_the_turn_goes_on() {
    let mut f = fixture(50, 10);
    f.app.event(started("work"));
    ask(
        &mut f,
        "q1",
        Question::yes_no(Some(QuestionKind::Destructive), "  run it?"),
    );
    f.screen();
    f.wait();
    f.ctrl('c');
    assert_eq!(
        f.ops(),
        [Op::QuestionAnswer {
            id: "q1".into(),
            answer: Answer::No
        }]
    );
    f.ctrl('c');
    assert_eq!(f.ops(), [Op::Interrupt]);
}

#[test]
fn ctrl_c_at_a_box_not_yet_taking_keys_stops_the_work_that_asked() {
    // A command's question (`/resume`'s copy): no turn runs.
    let mut f = fixture(50, 10);
    f.type_text("draft");
    ask(
        &mut f,
        "q1",
        Question::yes_no(None, "Continue a copy of it instead?"),
    );
    f.screen();
    f.ctrl('c');
    assert_eq!(f.ops(), [Op::Interrupt]);
    assert!(f.screen().iter().any(|r| r == "› draft▮"));
}

#[test]
fn enter_with_a_draft_sends_it_and_leaves_the_question() {
    let mut f = fixture(50, 12);
    f.app.event(started("work"));
    f.type_text("next");
    let failover = Question {
        default: Answer::Yes,
        unattended: Answer::Yes,
        ..Question::yes_no(None, "Try the backup region?")
    };
    ask(&mut f, "q1", failover);
    f.screen();
    f.wait();
    f.key(KeyCode::Enter);
    assert_eq!(
        f.ops(),
        [Op::UserTurn {
            text: "next".into()
        }]
    );
    assert!(f.app.questions.front().is_some());
}

#[test]
fn a_second_ctrl_c_while_closing_leaves_even_with_a_box() {
    let mut f = fixture(50, 10);
    ask(&mut f, "q1", Question::yes_no(None, "go?"));
    f.screen();
    f.app.close();
    f.ops();
    f.wait();
    assert_eq!(f.ctrl('c'), Some(Flow::Exit));
}

#[test]
fn ctrl_c_before_the_box_is_drawn_stops_the_turn() {
    let mut f = fixture(50, 10);
    f.app.event(started("work"));
    ask(
        &mut f,
        "q1",
        Question::yes_no(Some(QuestionKind::Destructive), "  run it?"),
    );
    f.ctrl('c');
    assert_eq!(f.ops(), [Op::Interrupt]);
    // The engine drops the question with the turn; its line stays.
    f.app
        .event(EventMsg::QuestionResolved(QuestionResolvedEvent {
            question_id: "q1".into(),
            by: None,
        }));
    f.app.event(EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: "1".into(),
        reason: TurnAbortReason::Interrupted,
    }));
    let screen = f.screen();
    assert_eq!(
        screen[6..8],
        ["  run it? [y/N]", "■ interrupted"],
        "{screen:?}"
    );
    assert!(f.app.questions.front().is_none());
}

#[test]
fn questions_wait_their_turn_and_the_footer_counts_them() {
    let mut f = fixture(50, 12);
    f.app.event(started("work"));
    ask(
        &mut f,
        "q1",
        Question::yes_no(Some(QuestionKind::Fetch), "  let the model read a.example?"),
    );
    ask(
        &mut f,
        "q2",
        Question::yes_no(Some(QuestionKind::Network), "  run it?"),
    );
    let screen = f.screen();
    assert!(
        screen.iter().any(|r| r.starts_with("┌ fetch_url ")),
        "{screen:?}"
    );
    assert_eq!(screen[11], "1 question waiting");
    f.wait();
    f.key(KeyCode::Char('y'));
    let screen = f.screen();
    assert!(
        screen.iter().any(|r| r.starts_with("┌ network access ")),
        "{screen:?}"
    );
    assert_eq!(screen[11], "");
    // The second box was only just drawn: an immediate key cannot answer.
    f.key(KeyCode::Char('y'));
    assert_eq!(answers(&f.ops()), [("q1".to_string(), Answer::Yes)]);
}

#[test]
fn shift_tab_at_a_box_names_it_and_typed_ahead_it_does_not() {
    let mut f = fixture(50, 10);
    ask(
        &mut f,
        "q1",
        Question::yes_no(Some(QuestionKind::Network), "  run it?"),
    );
    f.screen();
    f.key(KeyCode::BackTab);
    assert_eq!(f.ops(), [Op::CycleMode { question: None }]);
    f.wait();
    f.key(KeyCode::BackTab);
    assert_eq!(
        f.ops(),
        [Op::CycleMode {
            question: Some("q1".into())
        }]
    );
    assert!(f.app.questions.front().is_some());
}

#[test]
fn a_box_the_screen_cannot_hold_takes_no_key_and_says_why() {
    let mut f = fixture(50, 6);
    ask(&mut f, "q1", network());
    let screen = f.screen();
    assert!(
        screen.iter().any(|r| r.starts_with("too little room")),
        "{screen:?}"
    );
    f.wait();
    f.key(KeyCode::Char('y'));
    assert_eq!(answers(&f.ops()), []);
    // Taller: the box is drawn whole, then answered after the guard.
    f.input(Input::Resize(50, 12));
    f.screen();
    f.key(KeyCode::Backspace);
    f.wait();
    f.key(KeyCode::Char('y'));
    assert_eq!(answers(&f.ops()), [("q1".to_string(), Answer::Yes)]);
    // Shrunk while it waits: it stops taking keys.
    ask(&mut f, "q2", network());
    f.screen();
    f.wait();
    f.input(Input::Resize(50, 6));
    f.screen();
    f.key(KeyCode::Char('y'));
    assert_eq!(answers(&f.ops()), []);
}

#[test]
fn the_keys_light_up_when_the_guard_ends() {
    let mut f = fixture(50, 10);
    ask(&mut f, "q1", Question::yes_no(None, "go?"));
    f.screen();
    f.app.dirty = false;
    assert_eq!(f.app.next_frame(), f.app.questions.armed_at());
}
