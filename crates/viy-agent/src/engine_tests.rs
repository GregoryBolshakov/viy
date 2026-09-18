//! The submission loop with a stand-in turn: events in order, queued turns,
//! questions answered by an Op, interrupt and shutdown.

use pretty_assertions::assert_eq;
use viy_protocol::DeltaEvent;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::QuestionKind;

use super::*;

/// A turn that thinks, echoes the line, asks when the line says "ask",
/// asks through a pod command's asker when it says "pod", and never ends
/// when it says "hang". A command becomes a notice.
pub(crate) struct StandIn;

pub(crate) fn delta(text: impl Into<String>) -> EventMsg {
    EventMsg::AgentMessageContentDelta(DeltaEvent { delta: text.into() })
}

impl Turns for StandIn {
    fn turn(self: Arc<Self>, ctx: TurnContext, text: String) -> BoxFuture<'static, TurnOutcome> {
        Box::pin(async move {
            ctx.emit(EventMsg::ReasoningContentDelta(DeltaEvent {
                delta: "thinking".into(),
            }));
            if text.contains("ask") {
                let question = Question::yes_no(Some(QuestionKind::Destructive), "  rm -rf build?");
                let answer = ctx.ask(&question).await;
                ctx.emit(delta(format!("answered {answer:?}")));
            }
            if text.contains("pod") {
                let asker = ctx.asker();
                let ask: &(dyn Fn(&Question) -> Option<Asking> + Send + Sync) = &asker;
                let question = Question::yes_no(None, "Try another region?");
                if let Some(asking) = ask(&question) {
                    ctx.emit(delta(format!("pod answered {:?}", asking.await)));
                }
            }
            if text.contains("hang") {
                std::future::pending::<()>().await;
            }
            ctx.emit(delta(format!("echo: {text}")));
            TurnOutcome::Answered
        })
    }

    fn command(self: Arc<Self>, ctx: TurnContext, text: String) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            ctx.emit(EventMsg::Notice(NoticeEvent {
                level: NoticeLevel::Info,
                code: "command".into(),
                message: text,
            }));
        })
    }
}

fn engine() -> Engine {
    Engine::spawn(Arc::new(StandIn))
}

/// Events until one that `ends` says ends the wait, inclusive.
async fn until(engine: &mut Engine, ends: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    while let Some(event) = engine.next_event().await {
        let last = ends(&event);
        seen.push(event);
        if last {
            break;
        }
    }
    seen
}

fn turn_done(id: &str) -> impl Fn(&Event) -> bool + '_ {
    move |e| e.id == id && matches!(e.msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
}

fn msgs(events: &[Event]) -> Vec<EventMsg> {
    events.iter().map(|e| e.msg.clone()).collect()
}

#[tokio::test]
async fn a_turn_says_it_started_streams_and_ended() {
    let mut engine = engine();
    let id = engine
        .submit(Op::UserTurn {
            text: "hello".into(),
        })
        .unwrap();
    let events = until(&mut engine, turn_done(&id)).await;
    assert!(events.iter().all(|e| e.id == id));
    assert_eq!(
        msgs(&events),
        [
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: id.clone(),
                user_text: "hello".into()
            }),
            EventMsg::ReasoningContentDelta(DeltaEvent {
                delta: "thinking".into()
            }),
            delta("echo: hello"),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: id.clone(),
                outcome: TurnOutcome::Answered
            }),
        ]
    );
}

#[tokio::test]
async fn turns_and_commands_queue_and_run_in_order() {
    let mut engine = engine();
    let first = engine.submit(Op::UserTurn { text: "one".into() }).unwrap();
    let command = engine
        .submit(Op::Command {
            text: "/maxtok 2048".into(),
        })
        .unwrap();
    let second = engine.submit(Op::UserTurn { text: "two".into() }).unwrap();
    let events = until(&mut engine, turn_done(&second)).await;
    let order: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
    let mut runs = order.clone();
    runs.dedup();
    assert_eq!(runs, [&first, &command, &second]);
    assert!(events.contains(&Event {
        id: command.clone(),
        msg: EventMsg::CommandComplete(CommandCompleteEvent {
            command: "/maxtok 2048".into()
        })
    }));
}

#[tokio::test]
async fn a_question_is_a_request_answered_by_an_op() {
    let mut engine = engine();
    let id = engine
        .submit(Op::UserTurn {
            text: "ask first".into(),
        })
        .unwrap();
    let events = until(&mut engine, |e| {
        matches!(e.msg, EventMsg::QuestionRequest(_))
    })
    .await;
    let EventMsg::QuestionRequest(request) = &events.last().unwrap().msg else {
        unreachable!()
    };
    assert_eq!(request.question.prompt, "  rm -rf build?");
    assert_eq!(events.last().unwrap().id, id);
    engine
        .submit(Op::QuestionAnswer {
            id: request.question_id.clone(),
            answer: Answer::Yes,
        })
        .unwrap();
    let rest = until(&mut engine, turn_done(&id)).await;
    assert!(msgs(&rest).contains(&delta("answered Yes")));
}

#[tokio::test]
async fn the_asker_carries_a_pod_commands_question() {
    let mut engine = engine();
    let id = engine
        .submit(Op::UserTurn {
            text: "pod up".into(),
        })
        .unwrap();
    let events = until(&mut engine, |e| {
        matches!(e.msg, EventMsg::QuestionRequest(_))
    })
    .await;
    let EventMsg::QuestionRequest(request) = &events.last().unwrap().msg else {
        unreachable!()
    };
    engine
        .submit(Op::QuestionAnswer {
            id: request.question_id.clone(),
            answer: Answer::No,
        })
        .unwrap();
    let rest = until(&mut engine, turn_done(&id)).await;
    assert!(msgs(&rest).contains(&delta("pod answered No")));
}

#[tokio::test]
async fn interrupt_ends_the_turn_and_forgets_its_question() {
    let mut engine = engine();
    let id = engine
        .submit(Op::UserTurn {
            text: "ask then hang".into(),
        })
        .unwrap();
    let events = until(&mut engine, |e| {
        matches!(e.msg, EventMsg::QuestionRequest(_))
    })
    .await;
    let EventMsg::QuestionRequest(request) = &events.last().unwrap().msg else {
        unreachable!()
    };
    engine.submit(Op::Interrupt).unwrap();
    let rest = until(&mut engine, turn_done(&id)).await;
    assert_eq!(
        msgs(&rest),
        [EventMsg::TurnAborted(TurnAbortedEvent {
            turn_id: id.clone(),
            reason: TurnAbortReason::Interrupted
        })]
    );
    // A late answer goes nowhere, and the next turn runs as usual.
    engine
        .submit(Op::QuestionAnswer {
            id: request.question_id.clone(),
            answer: Answer::Yes,
        })
        .unwrap();
    let next = engine
        .submit(Op::UserTurn {
            text: "again".into(),
        })
        .unwrap();
    let events = until(&mut engine, turn_done(&next)).await;
    assert!(events.iter().all(|e| e.id == next));
    assert!(msgs(&events).contains(&delta("echo: again")));
}

#[tokio::test]
async fn shutdown_stops_the_turn_and_ends_with_shutdown_complete() {
    let mut engine = engine();
    let turn = engine
        .submit(Op::UserTurn {
            text: "hang".into(),
        })
        .unwrap();
    until(&mut engine, |e| {
        matches!(e.msg, EventMsg::ReasoningContentDelta(_))
    })
    .await;
    let shutdown = engine.submit(Op::Shutdown).unwrap();
    let mut rest = Vec::new();
    while let Some(event) = engine.next_event().await {
        rest.push(event);
    }
    assert_eq!(
        rest,
        [
            Event {
                id: turn.clone(),
                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: turn.clone(),
                    reason: TurnAbortReason::Interrupted
                })
            },
            Event {
                id: shutdown,
                msg: EventMsg::ShutdownComplete
            },
        ]
    );
    assert_eq!(engine.submit(Op::Interrupt), Err(EngineStopped));
}

#[tokio::test]
async fn an_interrupt_with_nothing_running_changes_nothing() {
    let mut engine = engine();
    engine.submit(Op::Interrupt).unwrap();
    let id = engine.submit(Op::UserTurn { text: "hi".into() }).unwrap();
    let events = until(&mut engine, turn_done(&id)).await;
    assert!(matches!(events[0].msg, EventMsg::TurnStarted(_)));
}
