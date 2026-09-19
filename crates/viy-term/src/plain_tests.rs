use pretty_assertions::assert_eq;
use viy_protocol::Answer;
use viy_protocol::DeltaEvent;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::QuestionKind;
use viy_protocol::QuestionRequestEvent;
use viy_protocol::TurnAbortReason;
use viy_protocol::TurnAbortedEvent;
use viy_protocol::TurnCompleteEvent;
use viy_protocol::TurnStartedEvent;

use super::*;

fn delta(text: &str) -> EventMsg {
    EventMsg::AgentMessageContentDelta(DeltaEvent { delta: text.into() })
}

fn notice(text: &str) -> EventMsg {
    EventMsg::Notice(NoticeEvent {
        level: NoticeLevel::Warn,
        code: "test".into(),
        message: text.into(),
    })
}

fn complete(outcome: TurnOutcome) -> EventMsg {
    EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "1".into(),
        outcome,
    })
}

fn render(events: &[EventMsg]) -> (String, Vec<Op>) {
    let mut plain = Plain::new(Vec::new());
    let ops = events
        .iter()
        .filter_map(|e| plain.event(e).unwrap())
        .collect();
    (String::from_utf8(plain.into_inner()).unwrap(), ops)
}

#[test]
fn the_answer_streams_and_notices_take_lines_of_their_own() {
    let (text, ops) = render(&[
        EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "1".into(),
            user_text: "hi".into(),
        }),
        delta("Hel"),
        delta("lo"),
        notice("[context: 12% of 262k]"),
        delta("there."),
        complete(TurnOutcome::Answered),
        notice("after"),
    ]);
    assert_eq!(text, "Hello\n[context: 12% of 262k]\nthere.\nafter\n");
    assert_eq!(ops, []);
}

#[test]
fn failed_and_interrupted_turns_say_so() {
    let (text, _) = render(&[
        delta("partial"),
        complete(TurnOutcome::Failed {
            message: "the request failed".into(),
        }),
        delta("more\n"),
        EventMsg::TurnAborted(TurnAbortedEvent {
            turn_id: "2".into(),
            reason: TurnAbortReason::Interrupted,
        }),
    ]);
    assert_eq!(text, "partial\n✗ the request failed\nmore\n■ interrupted\n");
}

#[test]
fn a_question_takes_its_unattended_answer() {
    let approval =
        Question::yes_no_always(Some(QuestionKind::Network), "run_shell wants the network?");
    let mut boot = Question::yes_no(None, "try another region?");
    boot.unattended = Answer::Yes;
    let ask = |id: &str, question: Question| {
        EventMsg::QuestionRequest(QuestionRequestEvent {
            question_id: id.into(),
            question,
        })
    };
    let (text, ops) = render(&[ask("q1", approval), ask("q2", boot)]);
    assert_eq!(
        text,
        "run_shell wants the network? [y/N/a=always] n (no terminal to ask)\n\
         try another region? [y/N] y (no terminal to ask)\n"
    );
    assert_eq!(
        ops,
        [
            Op::QuestionAnswer {
                id: "q1".into(),
                answer: Answer::No
            },
            Op::QuestionAnswer {
                id: "q2".into(),
                answer: Answer::Yes
            },
        ]
    );
}

#[test]
fn escape_sequences_in_the_text_are_written_visible() {
    let (text, _) = render(&[delta("a\x1b]52;c;aGk=\x07b\tc\r\n")]);
    assert_eq!(text, "a␛]52;c;aGk=␇b\tc␍\n");
}
