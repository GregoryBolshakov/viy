//! The engine's submission loop, after Codex's (core/src/session/handlers.rs
//! `submission_loop`; core/src/codex_thread.rs `submit` and `next_event`).
//! The UI submits [`Op`]s and reads [`Event`]s; the loop keeps reading Ops
//! while a turn runs as a task of its own, so an interrupt or an answer to
//! the turn's question gets through. Turns and slash commands queue and run
//! one at a time, in the order submitted.
//!
//! A question is a [`EventMsg::QuestionRequest`] with an id, resolved by the
//! [`Op::QuestionAnswer`] naming it, as Codex's `request_user_input` and
//! `notify_user_input_response` (kb d036). A question whose turn is
//! interrupted, or that is still open when the engine stops, gets the
//! question's unattended answer if anything still awaits it. A question
//! nothing waits for any more, because the work that asked it stopped or
//! dropped it, is closed with [`EventMsg::QuestionResolved`], so the UI
//! takes its box away (Codex's `ServerRequestResolved`).
//!
//! A line is admitted before its turn starts ([`Turns::admit`]): one that
//! nothing could answer, because no pod is ready (kb d041), is refused with
//! [`EventMsg::TurnRefused`] and never becomes a turn. Work that runs
//! beside the queue, a pod command, takes a [`TurnContext::background`] of
//! its own, whose questions stay open after the command that started it;
//! Ctrl-C with no turn running reaches it through [`Turns::interrupt`].

use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use viy_protocol::Answer;
use viy_protocol::Asking;
use viy_protocol::CommandCompleteEvent;
use viy_protocol::Event;
use viy_protocol::EventMsg;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Op;
use viy_protocol::Question;
use viy_protocol::QuestionRequestEvent;
use viy_protocol::QuestionResolvedEvent;
use viy_protocol::Submission;
use viy_protocol::TurnAbortReason;
use viy_protocol::TurnAbortedEvent;
use viy_protocol::TurnCompleteEvent;
use viy_protocol::TurnOutcome;
use viy_protocol::TurnRefusedEvent;
use viy_protocol::TurnStartedEvent;

/// A boxed future, as the engine's traits return.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What runs a turn and a slash command: the chat (WP 3.2 on), or a test's
/// stand-in.
pub trait Turns: Send + Sync + 'static {
    /// Runs the user's turn; how it ended. The engine says it started and
    /// ended ([`EventMsg::TurnStarted`], [`EventMsg::TurnComplete`]).
    fn turn(self: Arc<Self>, ctx: TurnContext, text: String) -> BoxFuture<'static, TurnOutcome>;

    /// Runs a slash command; the engine says when it is done
    /// ([`EventMsg::CommandComplete`]).
    fn command(self: Arc<Self>, ctx: TurnContext, text: String) -> BoxFuture<'static, ()>;

    /// Whether the line may start a turn now; `Err` says why not, and the
    /// engine refuses it ([`EventMsg::TurnRefused`]) before anything of it
    /// is shown as a turn.
    fn admit(self: Arc<Self>, ctx: TurnContext) -> BoxFuture<'static, Result<(), String>> {
        let _ = ctx;
        Box::pin(async { Ok(()) })
    }

    /// [`Op::OpenSession`]: go on in a saved chat, named or picked.
    fn open_session(
        self: Arc<Self>,
        ctx: TurnContext,
        open: OpenSession,
    ) -> BoxFuture<'static, ()> {
        let _ = (ctx, open);
        Box::pin(async {})
    }

    /// Ctrl-C with no turn or command running: for the work running beside
    /// the queue (a pod command).
    fn interrupt(&self) {}

    /// Once, when the engine starts, before any submission: what the
    /// frontend shows from the start (the footer's statuses), tagged with
    /// submission id `0`.
    fn opened(&self, ctx: &TurnContext) {
        let _ = ctx;
    }

    /// The chat is being closed: a turn cancelled from now on was cut short
    /// by that, not by Ctrl-C.
    fn closing(&self) {}

    /// The last thing before [`EventMsg::ShutdownComplete`], once no turn
    /// runs: the chat saves and releases what it holds.
    fn shutdown(self: Arc<Self>, ctx: TurnContext) -> BoxFuture<'static, ()> {
        let _ = ctx;
        Box::pin(async {})
    }
}

/// The engine's end of the channels: where a turn's events go and how it
/// asks.
#[derive(Clone)]
pub struct TurnContext {
    sub_id: String,
    events: mpsc::UnboundedSender<Event>,
    questions: Questions,
}

impl TurnContext {
    /// The submission this turn or command answers.
    pub fn sub_id(&self) -> &str {
        &self.sub_id
    }

    /// Sends one event, tagged with the submission's id.
    pub fn emit(&self, msg: EventMsg) {
        emit(&self.events, &self.sub_id, msg);
    }

    /// Asks the user: the question goes out as a request event, and the
    /// future resolves when its answer comes in.
    pub fn ask(&self, question: &Question) -> Asking {
        self.questions.ask(&self.events, &self.sub_id, question)
    }

    /// A context for work that goes on after this submission has ended (a
    /// pod command in the background): the same channels, a submission id
    /// of its own, so the end of this one closes none of its questions; a
    /// question it asked closes when its future goes.
    pub fn background(&self) -> Self {
        Self {
            sub_id: format!("{}b", self.sub_id),
            events: self.events.clone(),
            questions: self.questions.clone(),
        }
    }

    /// [`TurnContext::ask`] in the shape the pod commands take questions in
    /// (`viy_pod::notice::Asker`): there is always someone to ask.
    pub fn asker(&self) -> impl Fn(&Question) -> Option<Asking> + Send + Sync + '_ {
        |question| Some(self.ask(question))
    }
}

#[cfg(test)]
impl TurnContext {
    /// A context outside any engine, for a test that calls the chat
    /// directly: its events come out of the receiver; its questions are
    /// answered with [`TurnContext::answer`].
    pub(crate) fn detached() -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (events, rx) = mpsc::unbounded_channel();
        let ctx = Self {
            sub_id: "test".into(),
            events,
            questions: Questions::default(),
        };
        (ctx, rx)
    }

    pub(crate) fn answer(&self, id: &str, answer: Answer) {
        self.questions.answer(id, answer);
    }
}

fn emit(events: &mpsc::UnboundedSender<Event>, id: &str, msg: EventMsg) {
    // Nobody reading any more is the engine's end, not an error of the turn.
    let _ = events.send(Event {
        id: id.to_string(),
        msg,
    });
}

/// The open questions, by id.
#[derive(Clone, Default)]
struct Questions(Arc<Mutex<QuestionsState>>);

#[derive(Default)]
struct QuestionsState {
    asked: u64,
    /// Question id to the asking submission and where its answer goes.
    open: HashMap<String, (String, oneshot::Sender<Answer>)>,
}

impl Questions {
    fn lock(&self) -> std::sync::MutexGuard<'_, QuestionsState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ask(
        &self,
        events: &mpsc::UnboundedSender<Event>,
        sub_id: &str,
        question: &Question,
    ) -> Asking {
        let (tx, rx) = oneshot::channel();
        let question_id = {
            let mut state = self.lock();
            state.asked += 1;
            let id = format!("q{}", state.asked);
            state.open.insert(id.clone(), (sub_id.to_string(), tx));
            id
        };
        emit(
            events,
            sub_id,
            EventMsg::QuestionRequest(QuestionRequestEvent {
                question_id: question_id.clone(),
                question: question.clone(),
            }),
        );
        let unattended = question.unattended;
        // Closes the question if the future goes before its answer came.
        let waiting = Waiting {
            questions: self.clone(),
            events: events.clone(),
            sub_id: sub_id.to_string(),
            question_id,
        };
        Box::pin(async move {
            let _waiting = waiting;
            rx.await.unwrap_or(unattended)
        })
    }

    /// Resolves question `id`; an id that is not open is ignored.
    fn answer(&self, id: &str, answer: Answer) {
        if let Some((_, tx)) = self.lock().open.remove(id) {
            let _ = tx.send(answer);
        }
    }

    /// Closes question `id` unanswered, if it is still open.
    fn close(&self, events: &mpsc::UnboundedSender<Event>, sub_id: &str, id: &str) {
        if self.lock().open.remove(id).is_some() {
            resolved(events, sub_id, id);
        }
    }

    /// Closes the questions of submission `sub_id`, whose work has ended.
    fn forget(&self, events: &mpsc::UnboundedSender<Event>, sub_id: &str) {
        let mut closed: Vec<String> = Vec::new();
        self.lock().open.retain(|id, (asker, _)| {
            let keep = asker != sub_id;
            if !keep {
                closed.push(id.clone());
            }
            keep
        });
        // In the order they were asked: q9 before q10.
        closed.sort_by_key(|id| (id.len(), id.clone()));
        for id in closed {
            resolved(events, sub_id, &id);
        }
    }
}

fn resolved(events: &mpsc::UnboundedSender<Event>, sub_id: &str, id: &str) {
    emit(
        events,
        sub_id,
        EventMsg::QuestionResolved(QuestionResolvedEvent {
            question_id: id.to_string(),
        }),
    );
}

/// An asked question's future holds this: dropped before the answer came
/// (the turn was interrupted, a tool call cancelled), it closes the
/// question.
struct Waiting {
    questions: Questions,
    events: mpsc::UnboundedSender<Event>,
    sub_id: String,
    question_id: String,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.questions
            .close(&self.events, &self.sub_id, &self.question_id);
    }
}

/// A running engine. Dropping it stops the engine.
pub struct Engine {
    submissions: mpsc::UnboundedSender<Submission>,
    events: mpsc::UnboundedReceiver<Event>,
    submitted: AtomicU64,
}

/// The engine has stopped and takes no more submissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the engine has stopped")]
pub struct EngineStopped;

impl Engine {
    /// Starts the engine's loop on the current tokio runtime, with `turns`
    /// running what is submitted.
    pub fn spawn(turns: Arc<dyn Turns>) -> Self {
        let (submissions, rx_sub) = mpsc::unbounded_channel();
        let (tx_event, events) = mpsc::unbounded_channel();
        tokio::spawn(submission_loop(turns, rx_sub, tx_event));
        Self {
            submissions,
            events,
            submitted: AtomicU64::new(0),
        }
    }

    /// Submits `op`; the id its events will carry.
    pub fn submit(&self, op: Op) -> Result<String, EngineStopped> {
        let id = (self.submitted.fetch_add(1, Ordering::Relaxed) + 1).to_string();
        self.submissions
            .send(Submission { id: id.clone(), op })
            .map_err(|_| EngineStopped)?;
        Ok(id)
    }

    /// The next event; `None` once the engine has stopped and every event
    /// was read.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }
}

/// [`Op::OpenSession`]'s fields: the saved chat to go on in, or how to
/// pick one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenSession {
    pub id: Option<String>,
    pub all: bool,
    pub include_exec: bool,
    pub fork: bool,
}

/// What a running task is.
#[derive(Clone, PartialEq, Eq)]
enum Work {
    Turn,
    Command,
    /// [`Op::OpenSession`], shown as `/resume`.
    Open(OpenSession),
}

struct Running {
    sub_id: String,
    work: Work,
    /// The user's line or the command.
    text: String,
    task: JoinHandle<()>,
}

impl Running {
    /// Cancels the task, waits until it can send nothing more, and says it
    /// was stopped.
    async fn cancel(self, questions: &Questions, events: &mpsc::UnboundedSender<Event>) {
        self.task.abort();
        let _ = self.task.await;
        questions.forget(events, &self.sub_id);
        let msg = match self.work {
            Work::Turn => EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: self.sub_id.clone(),
                reason: TurnAbortReason::Interrupted,
            }),
            // A command stopped half way has still stopped running.
            Work::Command | Work::Open(_) => {
                EventMsg::CommandComplete(CommandCompleteEvent { command: self.text })
            }
        };
        emit(events, &self.sub_id, msg);
    }
}

async fn submission_loop(
    turns: Arc<dyn Turns>,
    mut submissions: mpsc::UnboundedReceiver<Submission>,
    events: mpsc::UnboundedSender<Event>,
) {
    let questions = Questions::default();
    turns.opened(&TurnContext {
        sub_id: "0".into(),
        events: events.clone(),
        questions: questions.clone(),
    });
    let (done_tx, mut done) = mpsc::unbounded_channel::<String>();
    let mut queued: VecDeque<Submission> = VecDeque::new();
    let mut running: Option<Running> = None;
    loop {
        if running.is_none()
            && let Some(sub) = queued.pop_front()
        {
            running = start(&turns, sub, &events, &questions, &done_tx);
        }
        tokio::select! {
            biased;
            sub = submissions.recv() => {
                let Some(sub) = sub else {
                    // Every handle is gone: nobody is left to show anything.
                    if let Some(task) = running.take() {
                        task.cancel(&questions, &events).await;
                    }
                    return;
                };
                match sub.op {
                    Op::UserTurn { .. } | Op::Command { .. } | Op::OpenSession { .. } => {
                        queued.push_back(sub);
                    }
                    Op::QuestionAnswer { id, answer } => questions.answer(&id, answer),
                    // The modes are phase 4's (modes.py, kb d024); until then
                    // Shift+Tab says so.
                    Op::CycleMode { .. } => emit(
                        &events,
                        &sub.id,
                        EventMsg::Notice(NoticeEvent {
                            level: NoticeLevel::Info,
                            code: "not_ported".into(),
                            message: "Shift+Tab: the chat modes come with phase 4".into(),
                        }),
                    ),
                    Op::Interrupt => match running.take() {
                        Some(task) => task.cancel(&questions, &events).await,
                        None => turns.interrupt(),
                    },
                    Op::Shutdown => {
                        turns.closing();
                        if let Some(task) = running.take() {
                            task.cancel(&questions, &events).await;
                        }
                        let ctx = TurnContext {
                            sub_id: sub.id.clone(),
                            events: events.clone(),
                            questions: questions.clone(),
                        };
                        Arc::clone(&turns).shutdown(ctx).await;
                        emit(&events, &sub.id, EventMsg::ShutdownComplete);
                        return;
                    }
                    _ => {}
                }
            }
            Some(sub_id) = done.recv() => {
                if running.as_ref().is_some_and(|r| r.sub_id == sub_id) {
                    running = None;
                    questions.forget(&events, &sub_id);
                }
            }
        }
    }
}

fn start(
    turns: &Arc<dyn Turns>,
    sub: Submission,
    events: &mpsc::UnboundedSender<Event>,
    questions: &Questions,
    done: &mpsc::UnboundedSender<String>,
) -> Option<Running> {
    let ctx = TurnContext {
        sub_id: sub.id.clone(),
        events: events.clone(),
        questions: questions.clone(),
    };
    let (work, text) = match sub.op {
        Op::UserTurn { text } => (Work::Turn, text),
        Op::Command { text } => (Work::Command, text),
        Op::OpenSession {
            id,
            all,
            include_exec,
            fork,
        } => (
            Work::Open(OpenSession {
                id,
                all,
                include_exec,
                fork,
            }),
            "/resume".to_string(),
        ),
        _ => return None,
    };
    let turns = Arc::clone(turns);
    let done = done.clone();
    let sub_id = sub.id.clone();
    let shown = text.clone();
    // What cancelling it says: a turn aborted, or a command ended.
    let kind = match work {
        Work::Turn => Work::Turn,
        _ => Work::Command,
    };
    let task = tokio::spawn(async move {
        match work {
            Work::Turn => {
                if let Err(message) = Arc::clone(&turns).admit(ctx.clone()).await {
                    ctx.emit(EventMsg::TurnRefused(TurnRefusedEvent { text, message }));
                    let _ = done.send(ctx.sub_id);
                    return;
                }
                ctx.emit(EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: ctx.sub_id.clone(),
                    user_text: text.clone(),
                }));
                let outcome = turns.turn(ctx.clone(), text).await;
                ctx.emit(EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: ctx.sub_id.clone(),
                    outcome,
                }));
            }
            Work::Command => {
                turns.command(ctx.clone(), text.clone()).await;
                ctx.emit(EventMsg::CommandComplete(CommandCompleteEvent {
                    command: text,
                }));
            }
            Work::Open(open) => {
                turns.open_session(ctx.clone(), open).await;
                ctx.emit(EventMsg::CommandComplete(CommandCompleteEvent {
                    command: text,
                }));
            }
        }
        let _ = done.send(ctx.sub_id);
    });
    Some(Running {
        sub_id,
        work: kind,
        text: shown,
        task,
    })
}

#[cfg(test)]
#[path = "engine_tests.rs"]
pub(crate) mod tests;
