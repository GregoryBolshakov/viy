use pretty_assertions::assert_eq;
use viy_protocol::DeltaEvent;
use viy_protocol::EventMsg;
use viy_protocol::QuestionKind;

use super::*;

fn write_question() -> Question {
    Question::yes_no(
        Some(QuestionKind::ProtectedWrite),
        "  let the model write it?",
    )
}

#[test]
fn answers_from_the_script_and_records_in_order() {
    let f = ScriptedFrontend::new([("write it?", Answer::Yes)]);
    let event = Event {
        id: "1".into(),
        msg: EventMsg::AgentMessageContentDelta(DeltaEvent { delta: "ok".into() }),
    };
    f.emit(event.clone());
    assert_eq!(f.ask(&write_question()), Answer::Yes);
    assert_eq!(
        f.shown(),
        [
            Shown::Event(event.clone()),
            Shown::Question {
                question: write_question(),
                answer: Answer::Yes
            }
        ]
    );
    assert_eq!(f.events(), [event]);
    f.assert_done();
}

#[test]
#[should_panic(expected = "unscripted question")]
fn an_unscripted_question_gets_no_and_fails_the_test() {
    let f = ScriptedFrontend::new([("run it?", Answer::Yes)]);
    assert_eq!(f.ask(&write_question()), Answer::No);
    f.assert_done();
}

#[test]
#[should_panic(expected = "never asked for")]
fn a_leftover_answer_fails_the_test() {
    ScriptedFrontend::new([("run it?", Answer::Yes)]).assert_done();
}

#[test]
fn unattended_gives_each_question_its_own_answer() {
    let f = ScriptedFrontend::unattended();
    assert_eq!(f.ask(&write_question()), Answer::No);
    let backup = Question {
        default: Answer::Yes,
        unattended: Answer::Yes,
        ..Question::yes_no(None, "No GPU free here. Try the backup region?")
    };
    assert_eq!(f.ask(&backup), Answer::Yes);
    f.assert_done();
}
