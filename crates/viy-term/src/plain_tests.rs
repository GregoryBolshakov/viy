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

/// The hostile-string corpus through every event the plain renderer writes
/// (ui-plan 7): it writes no escape and no control but newline and tab,
/// whatever the text, so a terminal on stdout shows it and does nothing.
#[test]
fn hostile_strings_write_no_escape_and_reach_no_terminal_feature() {
    use viy_protocol::Choices;
    use viy_protocol::ToolCallBeginEvent;
    use viy_testkit::hostile::CORPUS;

    let mut events = Vec::new();
    for text in CORPUS {
        events.extend([
            delta(text),
            complete(TurnOutcome::Answered),
            notice(text),
            complete(TurnOutcome::Failed {
                message: (*text).into(),
            }),
            EventMsg::ToolCallBegin(ToolCallBeginEvent {
                call_id: "c1".into(),
                tool: (*text).into(),
                summary: (*text).into(),
            }),
            EventMsg::ToolCallOutputDelta(viy_protocol::ToolCallOutputDeltaEvent {
                call_id: "c1".into(),
                chunk: (*text).into(),
            }),
            EventMsg::ToolCallEnd(viy_protocol::ToolCallEndEvent {
                call_id: "c1".into(),
                status: viy_protocol::ToolCallStatus::Failed,
                detail: Some((*text).into()),
                duration_ms: 1,
                sandboxed: false,
                network: false,
            }),
            EventMsg::QuestionRequest(QuestionRequestEvent {
                question_id: "q1".into(),
                question: Question {
                    kind: Some(QuestionKind::Destructive),
                    prompt: (*text).into(),
                    choices: Choices::YesNo,
                    default: Answer::No,
                    unattended: Answer::No,
                },
            }),
        ]);
    }
    // The whole corpus as one stream too: an escape cut between two pieces.
    events.extend(CORPUS.iter().map(|t| delta(t)));
    let (text, _) = render(&events);
    let stray: Vec<char> = text
        .chars()
        .filter(|&c| c != '\n' && c != '\t' && crate::sanitize::unsafe_in_cell(&c.to_string()))
        .collect();
    assert_eq!(stray, Vec::<char>::new());
    let mut parser = viy_testkit::screen::parser(50, 80);
    viy_testkit::screen::feed(&mut parser, text.replace('\n', "\r\n").as_bytes());
    assert_eq!(parser.callbacks().foreign(), Vec::<String>::new());
    assert!(text.contains("␛]52;c;cm0gLXJmIH4K␇"), "{text}");
    assert!(text.contains("invoice_<U+202E>fdp.exe"), "{text}");
}

/// rp prints a command, its output, then `[exit 1]`.
#[test]
fn a_tool_call_shows_its_output_and_result() {
    use viy_protocol::ToolCallBeginEvent;
    use viy_protocol::ToolCallEndEvent;
    use viy_protocol::ToolCallOutputDeltaEvent;
    use viy_protocol::ToolCallStatus;

    let end = |status, detail: Option<&str>| {
        EventMsg::ToolCallEnd(ToolCallEndEvent {
            call_id: "c1".into(),
            status,
            detail: detail.map(str::to_string),
            duration_ms: 1,
            sandboxed: true,
            network: false,
        })
    };
    let (text, _) = render(&[
        delta("Checking."),
        EventMsg::ToolCallBegin(ToolCallBeginEvent {
            call_id: "c1".into(),
            tool: "run_shell".into(),
            summary: "false; echo hi".into(),
        }),
        EventMsg::ToolCallOutputDelta(ToolCallOutputDeltaEvent {
            call_id: "c1".into(),
            chunk: "hi\nthere".into(),
        }),
        end(ToolCallStatus::Completed, Some("exit 1")),
        end(ToolCallStatus::Completed, None),
        end(ToolCallStatus::Declined, None),
    ]);
    assert_eq!(
        text,
        "Checking.\n▸ run_shell  false; echo hi\nhi\nthere\n  [exit 1]\n  [declined]\n"
    );
}
