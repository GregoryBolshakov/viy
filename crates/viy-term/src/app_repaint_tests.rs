//! Repaints (WP 3.12, ui-plan 4): `/rewind`'s pick in the transcript
//! (kb d045), `/new`, `/resume` drawing the whole chat (kb d052), `/load`,
//! the compaction marker that keeps the transcript (kb d053), and the
//! screen's own commands.

use pretty_assertions::assert_eq;
use viy_protocol::ContextCompactedEvent;
use viy_protocol::InputPrefillEvent;

use super::*;

fn rewritten(reason: RewriteReason, items: Vec<HistoryItem>) -> EventMsg {
    EventMsg::HistoryRewritten(HistoryRewrittenEvent { reason, items })
}

fn user(text: &str) -> HistoryItem {
    HistoryItem::User { text: text.into() }
}

fn agent(text: &str) -> HistoryItem {
    HistoryItem::Agent { text: text.into() }
}

/// Three finished turns: `one`, `two`, `three`, each answered `ok`.
fn three_turns(f: &mut Fixture) {
    for text in ["one", "two", "three"] {
        f.app.event(started(text));
        f.app.event(delta("ok"));
        f.app.event(complete());
    }
}

/// Whether the row starting with `you › {text}` is drawn reversed.
fn reversed(f: &mut Fixture, text: &str) -> bool {
    let rows = f.screen();
    let want = format!("you › {text}");
    let y = rows
        .iter()
        .position(|r| r == &want)
        .unwrap_or_else(|| panic!("{rows:?}"));
    let screen = f.term.backend().vt100().screen();
    screen.cell(u16::try_from(y).unwrap(), 6).unwrap().inverse()
}

fn send(f: &mut Fixture, line: &str) {
    f.type_text(line);
    f.key(KeyCode::Enter);
}

#[test]
fn rewind_picks_your_message_in_the_transcript() {
    let mut f = fixture(40, 14);
    three_turns(&mut f);
    send(&mut f, "/rewind");
    assert_eq!(f.ops(), []);
    assert!(reversed(&mut f, "three"));
    assert!(!reversed(&mut f, "two"));
    let screen = f.screen();
    assert!(
        screen.iter().any(|r| r.starts_with("rewind: ↑↓ choose")),
        "{screen:?}"
    );
    f.key(KeyCode::Up);
    assert!(reversed(&mut f, "two"));
    assert!(!reversed(&mut f, "three"));
    f.key(KeyCode::Home);
    assert!(reversed(&mut f, "one"));
    f.key(KeyCode::Down);
    f.key(KeyCode::Enter);
    assert_eq!(
        f.ops(),
        [Op::Command {
            text: "/rewind 2".into()
        }]
    );
    assert!(!reversed(&mut f, "two"));
    // The chat answers: the history from the cut, the message to edit.
    f.app.event(rewritten(
        RewriteReason::Rewind,
        vec![user("one"), agent("ok")],
    ));
    f.app.event(EventMsg::InputPrefill(InputPrefillEvent {
        text: "two".into(),
    }));
    let screen = f.screen();
    assert!(!screen.iter().any(|r| r.contains("three")), "{screen:?}");
    assert_eq!(screen[9..12], ["you › one", "", "ok"], "{screen:?}");
    assert!(screen.iter().any(|r| r == "› two▮"), "{screen:?}");
}

#[test]
fn esc_twice_at_an_empty_idle_prompt_opens_the_pick_and_esc_leaves_it() {
    let mut f = fixture(40, 14);
    three_turns(&mut f);
    f.key(KeyCode::Esc);
    f.key(KeyCode::Char('x'));
    f.key(KeyCode::Backspace);
    f.key(KeyCode::Esc);
    assert!(!reversed(&mut f, "three"), "a key between the two Escs");
    f.key(KeyCode::Esc);
    assert!(reversed(&mut f, "three"));
    f.key(KeyCode::Up);
    f.key(KeyCode::Esc);
    assert!(!reversed(&mut f, "two"));
    assert!(!reversed(&mut f, "three"));
    assert_eq!(f.ops(), []);
    // Not while a turn runs.
    f.app.event(started("four"));
    f.key(KeyCode::Esc);
    f.key(KeyCode::Esc);
    assert!(!reversed(&mut f, "three"));
}

#[test]
fn rewind_with_nothing_to_pick_asks_the_chat() {
    let mut f = fixture(40, 8);
    send(&mut f, "/rewind");
    assert_eq!(
        f.ops(),
        [Op::Command {
            text: "/rewind".into()
        }]
    );
    // With a number, it goes straight to the chat.
    three_turns(&mut f);
    send(&mut f, "/rewind 1");
    assert_eq!(
        f.ops(),
        [Op::Command {
            text: "/rewind 1".into()
        }]
    );
}

#[test]
fn new_draws_an_empty_transcript() {
    let mut f = fixture(30, 8);
    three_turns(&mut f);
    f.app.event(rewritten(RewriteReason::New, vec![]));
    f.app.event(notice("new chat"));
    assert_eq!(f.screen(), ["", "", "", "", "", "new chat", "› ▮", ""]);
}

#[test]
fn a_resumed_chat_is_drawn_whole() {
    let mut f = fixture(90, 16);
    f.app.event(rewritten(
        RewriteReason::Resume,
        vec![
            user("question 1"),
            agent("answer 1"),
            HistoryItem::Compacted,
            user("question 2"),
            HistoryItem::Thought,
            HistoryItem::ToolCall {
                call_id: "c1".into(),
                tool: "run".into(),
                summary: "ls".into(),
            },
            agent("answer 2"),
        ],
    ));
    let screen = f.screen();
    let text = screen.join("\n");
    for want in [
        "you › question 1",
        "answer 1",
        "[earlier turns compacted: the model continues from a summary of what is above]",
        "you › question 2",
        "thought",
        "answer 2",
    ] {
        assert!(screen.iter().any(|r| r.starts_with(want)), "{want}\n{text}");
    }
    assert!(text.contains("run") && text.contains("ls"), "{text}");
    // Only what follows the marker can be rewound to.
    send(&mut f, "/rewind");
    f.key(KeyCode::Up);
    assert!(reversed(&mut f, "question 2"));
}

#[test]
fn a_compaction_keeps_the_transcript_and_adds_a_marker() {
    let mut f = fixture(70, 16);
    three_turns(&mut f);
    f.app
        .event(EventMsg::ContextCompacted(ContextCompactedEvent {
            messages_before: 6,
            messages_after: 2,
            tokens_before: 84_000,
            tokens_after: 9_000,
        }));
    let screen = f.screen();
    for want in ["you › one", "you › two", "you › three"] {
        assert!(screen.iter().any(|r| r == want), "{screen:?}");
    }
    assert!(
        screen
            .iter()
            .any(|r| r.starts_with("compacted 6 messages -> 2 (84000 → 9000 tokens).")),
        "{screen:?}"
    );
    // The summarised turns are no longer rewind points: the chat says so.
    send(&mut f, "/rewind");
    assert_eq!(
        f.ops(),
        [Op::Command {
            text: "/rewind".into()
        }]
    );
}

#[test]
fn the_screens_own_commands_never_reach_the_chat() {
    let mut f = fixture(40, 8);
    send(&mut f, "/think off");
    send(&mut f, "/md off");
    send(&mut f, "/md maybe");
    send(&mut f, "/redraw");
    assert_eq!(f.ops(), []);
    assert!(!f.app.show_think);
    assert!(!f.app.markdown);
    let screen = f.screen();
    for want in [
        "reasoning trace hidden",
        "Markdown rendering off",
        "usage: /md on|off",
    ] {
        assert!(screen.iter().any(|r| r == want), "{screen:?}");
    }
    send(&mut f, "/help");
    assert_eq!(
        f.ops(),
        [Op::Command {
            text: "/help".into()
        }]
    );
    send(&mut f, "/exit");
    assert_eq!(f.ops(), [Op::Shutdown]);
}

#[test]
fn a_pick_question_is_answered_from_its_list() {
    let mut f = fixture(60, 16);
    let options = vec![
        "two  just now · 2 msgs · b".to_string(),
        "one  5m ago · 2 msgs · a".into(),
    ];
    f.app.event(EventMsg::QuestionRequest(QuestionRequestEvent {
        question_id: "q1".into(),
        question: Question::pick("resume which chat?", options, 0),
    }));
    let screen = f.screen();
    assert!(
        screen
            .iter()
            .any(|r| r.contains("resume which chat? [1-2, Enter = 1, q = cancel]")),
        "{screen:?}"
    );
    // The chosen one is marked and reversed, inside the frame too.
    assert!(chosen_row(&mut f, "›  1. two  just now"), "{screen:?}");
    // The arrows choose at once, in the guard, and quickly one after
    // another: choosing answers nothing.
    f.key(KeyCode::Down);
    assert!(chosen_row(&mut f, "›  2. one  5m ago"), "{:?}", f.screen());
    f.key(KeyCode::Up);
    f.key(KeyCode::Down);
    assert!(chosen_row(&mut f, "›  2. one  5m ago"), "{:?}", f.screen());
    // End in the guard is the transcript's, not the pick's.
    f.key(KeyCode::End);
    assert!(chosen_row(&mut f, "›  2. one  5m ago"), "{:?}", f.screen());
    // An arrow in the guard starts it again: an Enter right behind it
    // (each key here comes 50 ms after the last) answers nothing.
    f.key(KeyCode::Down);
    f.key(KeyCode::Enter);
    assert_eq!(f.ops(), []);
    f.clock += crate::question::GUARD + Duration::from_millis(1);
    f.key(KeyCode::Enter);
    assert_eq!(
        f.ops(),
        [Op::QuestionAnswer {
            id: "q1".into(),
            answer: Answer::Pick(1)
        }]
    );
    let screen = f.screen();
    assert!(
        screen
            .iter()
            .any(|r| r == "resume which chat? [1-2, Enter = 1, q = cancel] 2"),
        "{screen:?}"
    );
}

/// Whether the row holding `text` is on screen and drawn reversed.
fn chosen_row(f: &mut Fixture, text: &str) -> bool {
    let rows = f.screen();
    let Some(y) = rows.iter().position(|r| r.contains(text)) else {
        return false;
    };
    let x = rows[y].find(text).unwrap();
    let x = rows[y][..x].chars().count();
    let screen = f.term.backend().vt100().screen();
    screen
        .cell(u16::try_from(y).unwrap(), u16::try_from(x).unwrap())
        .unwrap()
        .inverse()
}

#[test]
fn with_a_draft_the_arrows_stay_the_input_boxs() {
    let mut f = fixture(60, 16);
    f.type_text("line one");
    f.app.event(EventMsg::QuestionRequest(QuestionRequestEvent {
        question_id: "q1".into(),
        question: Question::pick("resume which chat?", vec!["a".into(), "b".into()], 0),
    }));
    // In the guard, typing into the draft: Down is the draft's.
    f.screen();
    f.key(KeyCode::Down);
    assert!(chosen_row(&mut f, "›  1. a"), "{:?}", f.screen());
}
