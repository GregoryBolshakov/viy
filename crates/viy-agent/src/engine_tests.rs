//! The submission loop with a stand-in turn: events in order, queued turns,
//! questions answered by an Op, interrupt and shutdown.

use pretty_assertions::assert_eq;
use viy_protocol::DeltaEvent;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::QuestionKind;

use super::*;

/// A turn that thinks, echoes the line, asks when the line says "ask",
/// asks through a pod command's asker when it says "pod", asks and stops
/// waiting when it says "drop", and never ends when it says "hang". A
/// command becomes a notice.
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
            if text.contains("drop") {
                drop(ctx.ask(&Question::yes_no(None, "  still there?")));
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
    // Answered by the UI: nothing more to say about it.
    assert!(
        !msgs(&rest)
            .iter()
            .any(|m| matches!(m, EventMsg::QuestionResolved(_)))
    );
}

#[tokio::test]
async fn a_question_nothing_waits_for_is_closed() {
    let mut engine = engine();
    let id = engine
        .submit(Op::UserTurn {
            text: "drop it".into(),
        })
        .unwrap();
    let events = until(&mut engine, turn_done(&id)).await;
    let msgs = msgs(&events);
    let asked = msgs
        .iter()
        .position(|m| matches!(m, EventMsg::QuestionRequest(_)))
        .unwrap();
    assert_eq!(
        msgs[asked + 1],
        EventMsg::QuestionResolved(QuestionResolvedEvent {
            question_id: "q1".into()
        })
    );
    assert_eq!(msgs[asked + 2], delta("echo: drop it"));
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
    // The question goes with the turn, and the UI is told before the turn
    // ends.
    assert_eq!(
        msgs(&rest),
        [
            EventMsg::QuestionResolved(QuestionResolvedEvent {
                question_id: request.question_id.clone()
            }),
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: id.clone(),
                reason: TurnAbortReason::Interrupted
            })
        ]
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

/// Shift+Tab's op is answered at once, between turns or during one; until
/// phase 4 brings the modes, with a notice saying so.
#[tokio::test]
async fn shift_tab_is_answered_until_the_modes_come() {
    let mut engine = engine();
    engine.submit(Op::CycleMode { question: None }).unwrap();
    let events = until(&mut engine, |e| matches!(e.msg, EventMsg::Notice(_))).await;
    let Some(EventMsg::Notice(notice)) = events.last().map(|e| &e.msg) else {
        panic!("{events:?}");
    };
    assert_eq!(notice.code, "not_ported");
    assert_eq!(
        notice.message,
        "Shift+Tab: the chat modes come with phase 4"
    );
}

#[test]
fn questions_left_open_are_closed_in_the_order_asked() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let questions = Questions::default();
    let question = Question::yes_no(None, "go?");
    let asking: Vec<Asking> = (0..10)
        .map(|_| questions.ask(&tx, "1", &question))
        .collect();
    questions.forget(&tx, "1");
    let closed: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|e| match e.msg {
            EventMsg::QuestionResolved(r) => Some(r.question_id),
            _ => None,
        })
        .collect();
    let expected: Vec<String> = (1..=10).map(|n| format!("q{n}")).collect();
    assert_eq!(closed, expected);
    // Dropped after: already closed, nothing more is said.
    drop(asking);
    assert!(rx.try_recv().is_err());
}

/// A stand-in whose lines saying "offline" are refused, whose commands
/// start a background job asking a question, and which counts the
/// interrupts that reached it.
#[derive(Default)]
struct Gated {
    interrupts: std::sync::atomic::AtomicUsize,
}

impl Turns for Gated {
    fn turn(self: Arc<Self>, ctx: TurnContext, text: String) -> BoxFuture<'static, TurnOutcome> {
        Arc::new(StandIn).turn(ctx, text)
    }

    fn admit(self: Arc<Self>, _ctx: TurnContext) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Err("no pod is running".to_string()) })
    }

    fn command(self: Arc<Self>, ctx: TurnContext, _text: String) -> BoxFuture<'static, ()> {
        let job = ctx.background();
        tokio::spawn(async move {
            let answer = job.ask(&Question::yes_no(None, "Try the backup?")).await;
            job.emit(delta(format!("job answered {answer:?}")));
        });
        Box::pin(async {})
    }

    fn open_session(
        self: Arc<Self>,
        ctx: TurnContext,
        open: OpenSession,
    ) -> BoxFuture<'static, ()> {
        Box::pin(async move { ctx.emit(delta(format!("open {open:?}"))) })
    }

    fn interrupt(&self) {
        self.interrupts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_line_not_admitted_is_refused_before_it_is_a_turn() {
    let mut engine = Engine::spawn(Arc::new(Gated::default()));
    let id = engine.submit(Op::UserTurn { text: "hi".into() }).unwrap();
    let events = until(&mut engine, |e| matches!(e.msg, EventMsg::TurnRefused(_))).await;
    assert_eq!(
        msgs(&events),
        [EventMsg::TurnRefused(TurnRefusedEvent {
            text: "hi".into(),
            message: "no pod is running".into()
        })]
    );
    assert_eq!(events[0].id, id);
    // The queue goes on.
    let next = engine.submit(Op::Command { text: "/x".into() }).unwrap();
    until(&mut engine, |e| {
        e.id == next && matches!(e.msg, EventMsg::CommandComplete(_))
    })
    .await;
}

#[tokio::test]
async fn background_work_keeps_its_question_after_its_command_ends() {
    let mut engine = Engine::spawn(Arc::new(Gated::default()));
    let id = engine.submit(Op::Command { text: "/up".into() }).unwrap();
    let events = until(&mut engine, |e| {
        matches!(e.msg, EventMsg::QuestionRequest(_))
    })
    .await;
    let Some(EventMsg::QuestionRequest(asked)) = events.last().map(|e| e.msg.clone()) else {
        panic!("{events:?}");
    };
    assert_eq!(events.last().unwrap().id, format!("{id}b"));
    // Its command has ended, and the question is still open.
    assert!(
        events
            .iter()
            .any(|e| matches!(e.msg, EventMsg::CommandComplete(_)))
    );
    engine
        .submit(Op::QuestionAnswer {
            id: asked.question_id,
            answer: Answer::Yes,
        })
        .unwrap();
    let rest = until(&mut engine, |e| {
        matches!(e.msg, EventMsg::AgentMessageContentDelta(_))
    })
    .await;
    assert_eq!(rest.last().unwrap().msg, delta("job answered Yes"));
    assert!(
        !rest
            .iter()
            .any(|e| matches!(e.msg, EventMsg::QuestionResolved(_)))
    );
}

#[tokio::test]
async fn an_interrupt_with_no_turn_reaches_the_background_work() {
    let gated = Arc::new(Gated::default());
    let mut engine = Engine::spawn(gated.clone());
    engine.submit(Op::Interrupt).unwrap();
    let id = engine.submit(Op::Command { text: "/x".into() }).unwrap();
    until(&mut engine, |e| {
        e.id == id && matches!(e.msg, EventMsg::CommandComplete(_))
    })
    .await;
    assert_eq!(
        gated.interrupts.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn opening_a_saved_chat_runs_in_the_queue_as_resume() {
    let mut engine = Engine::spawn(Arc::new(Gated::default()));
    let id = engine
        .submit(Op::OpenSession {
            id: None,
            all: true,
            include_exec: false,
            fork: true,
        })
        .unwrap();
    let events = until(&mut engine, |e| {
        matches!(e.msg, EventMsg::CommandComplete(_))
    })
    .await;
    assert_eq!(
        msgs(&events),
        [
            delta("open OpenSession { id: None, all: true, include_exec: false, fork: true }"),
            EventMsg::CommandComplete(CommandCompleteEvent {
                command: "/resume".into()
            })
        ]
    );
    assert!(events.iter().all(|e| e.id == id));
}
