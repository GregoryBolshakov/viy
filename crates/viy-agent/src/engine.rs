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
//! The chat mode (`viy_tools::modes`, rpctl's `modes.py`) answers some
//! questions before they reach the UI: a question whose kind the mode says
//! yes to is answered yes with a notice in its place, only while someone is
//! there to ask (`attended`; with nobody to ask the UI answers each
//! question's unattended answer, rp's "no TTY means no"). Shift+Tab
//! ([`Op::CycleMode`]) steps to the next mode and answers the question it
//! was pressed at if the new mode says yes to its kind. After a turn, a
//! notice counts the questions the mode answered.
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
use viy_protocol::ModeChangedEvent;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Op;
use viy_protocol::Question;
use viy_protocol::QuestionKind;
use viy_protocol::QuestionRequestEvent;
use viy_protocol::QuestionResolvedEvent;
use viy_protocol::Submission;
use viy_protocol::TurnAbortReason;
use viy_protocol::TurnAbortedEvent;
use viy_protocol::TurnCompleteEvent;
use viy_protocol::TurnOutcome;
use viy_protocol::TurnRefusedEvent;
use viy_protocol::TurnStartedEvent;
use viy_tools::modes;
use viy_tools::modes::Mode;

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

    /// The chat mode in force.
    pub fn mode(&self) -> &'static Mode {
        self.questions.lock().mode
    }

    /// Switches the chat mode (`/mode <name>`).
    pub fn set_mode(&self, mode: &'static Mode) {
        self.questions.lock().mode = mode;
        self.emit(mode_changed(mode));
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

/// The open questions, by id, and the chat mode that may answer them.
#[derive(Clone)]
struct Questions(Arc<Mutex<QuestionsState>>);

impl Default for Questions {
    fn default() -> Self {
        Self::new(modes::DEFAULT, false)
    }
}

struct QuestionsState {
    asked: u64,
    /// Question id to the asking submission, where its answer goes and the
    /// question (Shift+Tab may answer it).
    open: HashMap<String, Open>,
    mode: &'static Mode,
    /// Someone is there to ask: only then does a mode answer yes.
    attended: bool,
    /// The kinds the mode said yes to in this turn, in order.
    auto_yessed: Vec<QuestionKind>,
}

struct Open {
    asker: String,
    answer: oneshot::Sender<Answer>,
    question: Question,
}

/// A question the mode answered before it went out: its asker, the
/// question and the mode's name.
struct AutoYes {
    asker: String,
    question: Question,
    mode: &'static str,
}

impl Questions {
    fn new(mode: &'static Mode, attended: bool) -> Self {
        Self(Arc::new(Mutex::new(QuestionsState {
            asked: 0,
            open: HashMap::new(),
            mode,
            attended,
            auto_yessed: Vec::new(),
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QuestionsState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The mode's yes, when it says yes to `question`'s kind and someone is
    /// there: counted for the turn's report.
    fn auto_yes(state: &mut QuestionsState, asker: &str, question: &Question) -> Option<AutoYes> {
        let kind = question
            .kind
            .filter(|_| state.attended && state.mode.answers(question.kind))?;
        state.auto_yessed.push(kind);
        Some(AutoYes {
            asker: asker.to_string(),
            question: question.clone(),
            mode: state.mode.name,
        })
    }

    /// Shift+Tab: the next mode, and the yes to question `id` if the new
    /// mode says yes to its kind. The question was shown, so its box is
    /// closed with the mode's answer ([`QuestionResolvedEvent::by`]) rather
    /// than a notice that would repeat it.
    fn cycle(&self, id: Option<&str>, events: &mpsc::UnboundedSender<Event>, sub_id: &str) {
        let (mode, answered) = {
            let mut state = self.lock();
            state.mode = modes::after(state.mode);
            let answers = |open: &Open| state.attended && state.mode.answers(open.question.kind);
            let pending = id
                .filter(|id| state.open.get(*id).is_some_and(answers))
                .and_then(|id| Some((id.to_string(), state.open.remove(id)?)));
            if let Some(kind) = pending.as_ref().and_then(|(_, open)| open.question.kind) {
                state.auto_yessed.push(kind);
            }
            (state.mode, pending)
        };
        emit(events, sub_id, mode_changed(mode));
        if let Some((id, open)) = answered {
            let _ = open.answer.send(Answer::Yes);
            emit(
                events,
                &open.asker,
                EventMsg::QuestionResolved(QuestionResolvedEvent {
                    question_id: id,
                    by: Some(mode.name.to_string()),
                }),
            );
        }
    }

    /// The turn's report of the questions the mode answered, if any, and a
    /// fresh count.
    fn report(&self) -> Option<String> {
        let mut state = self.lock();
        let kinds = std::mem::take(&mut state.auto_yessed);
        if kinds.is_empty() {
            return None;
        }
        let mut counts: Vec<(QuestionKind, usize)> = Vec::new();
        for kind in &kinds {
            match counts.iter_mut().find(|(k, _)| k == kind) {
                Some((_, n)) => *n += 1,
                None => counts.push((*kind, 1)),
            }
        }
        let n = kinds.len();
        let what: Vec<String> = counts
            .iter()
            .map(|(k, c)| format!("{c} {}", modes::kind_words(*k)))
            .collect();
        Some(format!(
            "{} answered {n} question{}: {}",
            state.mode.name,
            if n > 1 { "s" } else { "" },
            what.join(", ")
        ))
    }

    fn clear_report(&self) {
        self.lock().auto_yessed.clear();
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
            if let Some(yes) = Self::auto_yes(&mut state, sub_id, question) {
                drop(state);
                said_yes(events, &yes);
                return Box::pin(async { Answer::Yes });
            }
            state.asked += 1;
            let id = format!("q{}", state.asked);
            state.open.insert(
                id.clone(),
                Open {
                    asker: sub_id.to_string(),
                    answer: tx,
                    question: question.clone(),
                },
            );
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
        if let Some(open) = self.lock().open.remove(id) {
            let _ = open.answer.send(answer);
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
        self.lock().open.retain(|id, open| {
            let keep = open.asker != sub_id;
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

/// What rp prints for the mode's yes: the question's reasons, then the
/// question with `y` and the mode where the user's answer would be.
fn said_yes(events: &mpsc::UnboundedSender<Event>, yes: &AutoYes) {
    for reason in &yes.question.reason {
        emit(events, &yes.asker, EventMsg::Notice(reason.clone()));
    }
    emit(
        events,
        &yes.asker,
        EventMsg::Notice(NoticeEvent {
            level: NoticeLevel::Info,
            code: "auto_yes".into(),
            message: format!(
                "{} {} y ({})",
                yes.question.prompt,
                yes.question.hint(),
                yes.mode
            ),
        }),
    );
}

fn mode_changed(mode: &Mode) -> EventMsg {
    EventMsg::ModeChanged(ModeChangedEvent {
        mode: mode.name.to_string(),
        label: mode.label.to_string(),
    })
}

fn resolved(events: &mpsc::UnboundedSender<Event>, sub_id: &str, id: &str) {
    emit(
        events,
        sub_id,
        EventMsg::QuestionResolved(QuestionResolvedEvent {
            question_id: id.to_string(),
            by: None,
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
    ///
    /// Nobody is there to ask, so no mode answers: `viy exec`, the plain
    /// renderer, tests.
    pub fn spawn(turns: Arc<dyn Turns>) -> Self {
        Self::spawn_with(turns, modes::DEFAULT, false)
    }

    /// [`Engine::spawn`] in chat `mode` (CHAT_MODE), `attended` when a
    /// person answers the questions (the terminal UI, a scripted driver).
    pub fn spawn_with(turns: Arc<dyn Turns>, mode: &'static Mode, attended: bool) -> Self {
        let (submissions, rx_sub) = mpsc::unbounded_channel();
        let (tx_event, events) = mpsc::unbounded_channel();
        let questions = Questions::new(mode, attended);
        tokio::spawn(submission_loop(turns, rx_sub, tx_event, questions));
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
            Work::Turn => {
                // Before the turn's last event, as a finished turn's.
                if let Some(text) = questions.report() {
                    emit(events, &self.sub_id, auto_yes_report(text));
                }
                EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: self.sub_id.clone(),
                    reason: TurnAbortReason::Interrupted,
                })
            }
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
    questions: Questions,
) {
    // For the footer; with nobody to ask there is no footer, and no mode
    // answers anything.
    let (start_mode, attended) = {
        let state = questions.lock();
        (state.mode, state.attended)
    };
    if attended && start_mode != modes::DEFAULT {
        emit(&events, "0", mode_changed(start_mode));
    }
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
                    Op::CycleMode { question } => {
                        questions.cycle(question.as_deref(), &events, &sub.id);
                    }
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
                ctx.questions.clear_report();
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
                if let Some(report) = ctx.questions.report() {
                    ctx.emit(auto_yes_report(report));
                }
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

/// rp's dim line after a turn: how many questions the mode answered.
fn auto_yes_report(text: String) -> EventMsg {
    EventMsg::Notice(NoticeEvent {
        level: NoticeLevel::Info,
        code: "auto_yes".into(),
        message: text,
    })
}

#[cfg(test)]
#[path = "engine_tests.rs"]
pub(crate) mod tests;
