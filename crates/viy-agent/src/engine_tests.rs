//! The submission loop with a stand-in turn: events in order, queued turns,
//! questions answered by an Op, interrupt and shutdown.

use pretty_assertions::assert_eq;
use viy_protocol::DeltaEvent;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::QuestionKind;
use viy_tools::modes;

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
            // "askall" asks once per kind, always offered; "askpw" a
            // protected write; "asknone" a question without a kind; any
            // other "ask" a destructive command.
            let kinds: Vec<Option<QuestionKind>> = if text.contains("askall") {
                modes::KINDS.iter().copied().map(Some).collect()
            } else if text.contains("askpw") {
                vec![Some(QuestionKind::ProtectedWrite)]
            } else if text.contains("asknone") {
                vec![None]
            } else if text.contains("ask") {
                vec![Some(QuestionKind::Destructive)]
            } else {
                Vec::new()
            };
            for kind in kinds {
                let question = if text.contains("askall") {
                    Question::yes_no_always(kind, "  run it?")
                } else {
                    Question::yes_no(kind, "  rm -rf build?")
                };
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
            if text.contains("secret") {
                // only its length, never the secret, reaches the events
                let got = ctx.ask_secret("  PIN?").await;
                ctx.emit(delta(format!(
                    "secret of {}",
                    got.map_or(0, |s| s.expose().len())
                )));
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
            question_id: "q1".into(),
            by: None,
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
                question_id: request.question_id.clone(),
                by: None,
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

// ------------------------------------------------------- the modes
// rpctl tests/test_modes.py ConfirmTests, ported to the engine: the mode
// answers before a question reaches the UI.

fn auto() -> &'static Mode {
    modes::get("auto-yes").unwrap()
}

fn requests(events: &[Event]) -> Vec<Question> {
    events
        .iter()
        .filter_map(|e| match &e.msg {
            EventMsg::QuestionRequest(r) => Some(r.question.clone()),
            _ => None,
        })
        .collect()
}

fn notices_coded<'a>(events: &'a [Event], code: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|e| match &e.msg {
            EventMsg::Notice(n) if n.code == code => Some(n.message.as_str()),
            _ => None,
        })
        .collect()
}

/// Runs `text` as a turn, answering every question that reaches the UI
/// with `answer`.
async fn turn_answering(engine: &mut Engine, text: &str, answer: Answer) -> Vec<Event> {
    let id = engine.submit(Op::UserTurn { text: text.into() }).unwrap();
    let mut seen = Vec::new();
    while let Some(event) = engine.next_event().await {
        if let EventMsg::QuestionRequest(r) = &event.msg {
            engine
                .submit(Op::QuestionAnswer {
                    id: r.question_id.clone(),
                    answer,
                })
                .unwrap();
        }
        let done = turn_done(&id)(&event);
        seen.push(event);
        if done {
            break;
        }
    }
    seen
}

#[tokio::test]
async fn auto_yes_answers_its_kinds_without_asking() {
    let mut engine = Engine::spawn_with(Arc::new(StandIn), auto(), true);
    let events = turn_answering(&mut engine, "askall", Answer::No).await;
    // Only the kinds auto-yes leaves alone reach the UI.
    let asked: Vec<_> = requests(&events).iter().map(|q| q.kind).collect();
    assert_eq!(
        asked,
        [
            Some(QuestionKind::ProtectedWrite),
            Some(QuestionKind::Unsandboxed)
        ]
    );
    let yes = notices_coded(&events, "auto_yes");
    // One line per answered question, where the answer would be, and the
    // count after the turn; plain yes, never "always".
    assert_eq!(yes.len(), auto().auto_yes.len() + 1);
    assert!(
        yes.iter()
            .take(6)
            .all(|l| *l == "  run it? [y/N/a=always] y (auto-yes)")
    );
    assert_eq!(
        yes[6],
        "auto-yes answered 6 questions: 1 destructive, 1 secret read, 1 consult, \
         1 network, 1 fetch, 1 remote"
    );
    assert_eq!(
        msgs(&events)
            .iter()
            .filter(|m| **m == delta("answered Yes"))
            .count(),
        6
    );
    // The report comes before the turn's end and is counted afresh.
    let report = events
        .iter()
        .position(
            |e| matches!(&e.msg, EventMsg::Notice(n) if n.message.starts_with("auto-yes answered")),
        )
        .unwrap();
    assert!(matches!(events[report + 1].msg, EventMsg::TurnComplete(_)));
    let again = turn_answering(&mut engine, "quiet", Answer::No).await;
    assert!(notices_coded(&again, "auto_yes").is_empty());
}

#[tokio::test]
async fn auto_yes_still_asks_the_rest() {
    for text in ["askpw", "asknone"] {
        let mut engine = Engine::spawn_with(Arc::new(StandIn), auto(), true);
        let events = turn_answering(&mut engine, text, Answer::No).await;
        assert_eq!(requests(&events).len(), 1, "{text}");
        assert!(msgs(&events).contains(&delta("answered No")));
        assert!(notices_coded(&events, "auto_yes").is_empty());
    }
}

#[tokio::test]
async fn normal_asks_everything() {
    let mut engine = Engine::spawn_with(Arc::new(StandIn), modes::DEFAULT, true);
    let events = turn_answering(&mut engine, "askall", Answer::Yes).await;
    assert_eq!(requests(&events).len(), modes::KINDS.len());
    assert!(notices_coded(&events, "auto_yes").is_empty());
}

/// rp's `_confirm` is no without a terminal, whatever the mode: viy's
/// engine says yes for no one then, and the question goes to the UI, whose
/// plain renderer answers its unattended answer, no for every approval.
#[tokio::test]
async fn no_terminal_means_no() {
    let mut unattended = Engine::spawn_with(Arc::new(StandIn), auto(), false);
    let events = turn_answering(&mut unattended, "ask", Answer::No).await;
    let asked = requests(&events);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].unattended, Answer::No);
    assert!(notices_coded(&events, "auto_yes").is_empty());
    // The plain engine is the unattended one.
    let mut plain = engine();
    let events = turn_answering(&mut plain, "askall", Answer::No).await;
    assert_eq!(requests(&events).len(), modes::KINDS.len());
}

#[tokio::test]
async fn shift_tab_at_a_question_switches_and_answers() {
    let mut engine = Engine::spawn_with(Arc::new(StandIn), modes::DEFAULT, true);
    let id = engine.submit(Op::UserTurn { text: "ask".into() }).unwrap();
    let events = until(&mut engine, |e| {
        matches!(e.msg, EventMsg::QuestionRequest(_))
    })
    .await;
    let EventMsg::QuestionRequest(request) = &events.last().unwrap().msg else {
        unreachable!()
    };
    engine
        .submit(Op::CycleMode {
            question: Some(request.question_id.clone()),
        })
        .unwrap();
    let rest = until(&mut engine, turn_done(&id)).await;
    let m = msgs(&rest);
    assert!(m.contains(&EventMsg::ModeChanged(ModeChangedEvent {
        mode: "auto-yes".into(),
        label: "⏵⏵ auto-yes".into(),
    })));
    assert!(
        m.contains(&EventMsg::QuestionResolved(QuestionResolvedEvent {
            question_id: request.question_id.clone(),
            by: Some("auto-yes".into()),
        }))
    );
    assert!(m.contains(&delta("answered Yes")));
    // The box shows the answer (the UI's cell), so no notice repeats the
    // question; the turn's count still has it.
    assert_eq!(
        notices_coded(&rest, "auto_yes"),
        ["auto-yes answered 1 question: 1 destructive"]
    );
}

#[tokio::test]
async fn shift_tab_asks_again_when_the_new_mode_does_not_answer() {
    let mut engine = Engine::spawn_with(Arc::new(StandIn), modes::DEFAULT, true);
    let id = engine
        .submit(Op::UserTurn {
            text: "askpw".into(),
        })
        .unwrap();
    let events = until(&mut engine, |e| {
        matches!(e.msg, EventMsg::QuestionRequest(_))
    })
    .await;
    let EventMsg::QuestionRequest(request) = &events.last().unwrap().msg else {
        unreachable!()
    };
    let qid = request.question_id.clone();
    engine
        .submit(Op::CycleMode {
            question: Some(qid.clone()),
        })
        .unwrap();
    let changed = until(&mut engine, |e| matches!(e.msg, EventMsg::ModeChanged(_))).await;
    assert!(
        !msgs(&changed)
            .iter()
            .any(|m| matches!(m, EventMsg::QuestionResolved(_)))
    );
    engine
        .submit(Op::QuestionAnswer {
            id: qid,
            answer: Answer::No,
        })
        .unwrap();
    let rest = until(&mut engine, turn_done(&id)).await;
    assert!(msgs(&rest).contains(&delta("answered No")));
}

/// Shift+Tab typed ahead names no question: the mode changes and nothing is
/// answered (kb d080).
#[tokio::test]
async fn shift_tab_without_a_question_only_changes_the_mode() {
    let mut engine = Engine::spawn_with(Arc::new(StandIn), modes::DEFAULT, true);
    let id = engine.submit(Op::UserTurn { text: "ask".into() }).unwrap();
    let events = until(&mut engine, |e| {
        matches!(e.msg, EventMsg::QuestionRequest(_))
    })
    .await;
    let EventMsg::QuestionRequest(request) = &events.last().unwrap().msg else {
        unreachable!()
    };
    let qid = request.question_id.clone();
    engine.submit(Op::CycleMode { question: None }).unwrap();
    engine.submit(Op::CycleMode { question: None }).unwrap();
    let changed = until(
        &mut engine,
        |e| matches!(&e.msg, EventMsg::ModeChanged(m) if m.mode == "normal"),
    )
    .await;
    assert!(
        !msgs(&changed)
            .iter()
            .any(|m| matches!(m, EventMsg::QuestionResolved(_)))
    );
    engine
        .submit(Op::QuestionAnswer {
            id: qid,
            answer: Answer::No,
        })
        .unwrap();
    let rest = until(&mut engine, turn_done(&id)).await;
    assert!(msgs(&rest).contains(&delta("answered No")));
}

/// A chat started in a mode other than normal says so first, so the
/// footer shows it (rp's prompt label).
#[tokio::test]
async fn a_chat_started_in_auto_yes_says_so() {
    let mut engine = Engine::spawn_with(Arc::new(StandIn), auto(), true);
    let first = engine.next_event().await.unwrap();
    assert_eq!(
        first.msg,
        EventMsg::ModeChanged(ModeChangedEvent {
            mode: "auto-yes".into(),
            label: "⏵⏵ auto-yes".into(),
        })
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

/// An interrupted turn's count comes before its TurnAborted, as a finished
/// turn's comes before TurnComplete (found by the WP 4.3 review).
#[tokio::test]
async fn an_interrupted_turn_reports_before_it_ends() {
    let mut engine = Engine::spawn_with(Arc::new(StandIn), auto(), true);
    let id = engine
        .submit(Op::UserTurn {
            text: "ask hang".into(),
        })
        .unwrap();
    until(
        &mut engine,
        |e| matches!(&e.msg, EventMsg::Notice(n) if n.code == "auto_yes"),
    )
    .await;
    engine.submit(Op::Interrupt).unwrap();
    let rest = until(&mut engine, turn_done(&id)).await;
    let n = rest.len();
    assert!(
        matches!(rest[n - 1].msg, EventMsg::TurnAborted(_)),
        "{rest:?}"
    );
    assert!(
        matches!(&rest[n - 2].msg, EventMsg::Notice(x) if x.message == "auto-yes answered 1 question: 1 destructive"),
        "{rest:?}"
    );
}

/// An unattended engine says nothing about a mode that answers nothing.
#[tokio::test]
async fn an_unattended_chat_does_not_announce_its_mode() {
    let mut engine = Engine::spawn_with(Arc::new(StandIn), auto(), false);
    let id = engine
        .submit(Op::UserTurn {
            text: "quiet".into(),
        })
        .unwrap();
    let events = until(&mut engine, turn_done(&id)).await;
    assert!(
        !msgs(&events)
            .iter()
            .any(|m| matches!(m, EventMsg::ModeChanged(_)))
    );
}

// ---- secrets (WP 8.1) ---------------------------------------------------------------

async fn secret_turn(engine: &mut Engine, answer: impl Fn(&str) -> Op) -> Vec<Event> {
    let id = engine
        .submit(Op::UserTurn {
            text: "secret".into(),
        })
        .unwrap();
    let mut seen = Vec::new();
    while let Some(event) = engine.next_event().await {
        if let EventMsg::SecretRequest(r) = &event.msg {
            engine.submit(answer(&r.question_id)).unwrap();
        }
        let done = turn_done(&id)(&event);
        seen.push(event);
        if done {
            break;
        }
    }
    seen
}

#[tokio::test]
async fn a_secret_is_a_request_answered_by_its_op() {
    let mut engine = Engine::spawn_with(Arc::new(StandIn), modes::DEFAULT, true);
    let events = secret_turn(&mut engine, |id| Op::SecretAnswer {
        id: id.into(),
        secret: Some(viy_protocol::Secret::new(" 2468 ")),
    })
    .await;
    let m = msgs(&events);
    // trimmed, as rp's getpass().strip()
    assert!(
        m.iter()
            .any(|m| matches!(m, EventMsg::SecretRequest(r) if r.prompt == "  PIN?"))
    );
    assert!(m.contains(&delta("secret of 4")));
    assert!(!format!("{m:?}").contains("2468"));
}

#[tokio::test]
async fn nobody_to_ask_means_no_secret_and_no_request() {
    let mut engine = engine(); // unattended
    let events = secret_turn(&mut engine, |id| Op::SecretAnswer {
        id: id.into(),
        secret: Some(viy_protocol::Secret::new("x")),
    })
    .await;
    let m = msgs(&events);
    assert!(!m.iter().any(|m| matches!(m, EventMsg::SecretRequest(_))));
    assert!(m.contains(&delta("secret of 0")));
}

#[tokio::test]
async fn a_yes_or_an_empty_secret_gives_none() {
    for op in [
        |id: &str| Op::QuestionAnswer {
            id: id.into(),
            answer: Answer::Yes,
        },
        |id: &str| Op::SecretAnswer {
            id: id.into(),
            secret: Some(viy_protocol::Secret::new("")),
        },
        |id: &str| Op::SecretAnswer {
            id: id.into(),
            secret: Some(viy_protocol::Secret::new("  \t ")),
        },
    ] {
        let mut engine = Engine::spawn_with(Arc::new(StandIn), modes::DEFAULT, true);
        let events = secret_turn(&mut engine, op).await;
        assert!(msgs(&events).contains(&delta("secret of 0")));
    }
}

#[tokio::test]
async fn no_mode_answers_a_secret() {
    let mut engine = Engine::spawn_with(Arc::new(StandIn), auto(), true);
    let id = engine
        .submit(Op::UserTurn {
            text: "secret".into(),
        })
        .unwrap();
    let events = until(&mut engine, |e| matches!(e.msg, EventMsg::SecretRequest(_))).await;
    let EventMsg::SecretRequest(r) = &events.last().unwrap().msg else {
        unreachable!()
    };
    // Shift+Tab at it changes the mode and answers nothing
    engine
        .submit(Op::CycleMode {
            question: Some(r.question_id.clone()),
        })
        .unwrap();
    engine.submit(Op::Interrupt).unwrap();
    let rest = until(&mut engine, turn_done(&id)).await;
    assert!(!msgs(&rest).contains(&delta("secret of 0")));
    assert!(msgs(&rest).iter().any(|m| matches!(
        m,
        EventMsg::QuestionResolved(QuestionResolvedEvent { by: None, .. })
    )));
}
