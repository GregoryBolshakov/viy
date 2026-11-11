//! The chat's screen (ui-plan 3): the transcript, scrolled by viy, above a
//! live region of the turn's activity line, the input box and a footer. Engine events come in, [`Op`]s go out
//! (kb d036), and frames are drawn at most 30 a second, each in one
//! synchronized write.
//!
//! The answer streams as Markdown: its finished blocks become cells, the
//! open one is drawn again with each delta as the transcript's live tail
//! ([`markdown::finished`]), which scrolls with it. Reasoning is drawn live
//! (rp `/think on`) and then kept as a cell; running tools show a card with
//! the last lines of their output in the tail, a cell when the call ends.
//! Ctrl-T draws every card's output and every trace whole. The footer
//! shows the engine's statuses, queued lines and the scroll position
//! ([`crate::footer`]); the tab title spins while a turn runs ([`tabtitle`]), if
//! the chat turned it on. Questions are boxes above the input, taking only
//! keys pressed after they have been seen ([`crate::question`]).

use std::borrow::Cow;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::io;
use std::io::Write;
use std::pin::Pin;
use std::time::Duration;

use crossterm::event::Event as TermEvent;
use crossterm::event::EventStream;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use futures_core::Stream;
use ratatui::backend::Backend;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;
use tokio::sync::mpsc;
use tokio::time::Instant;
use viy_protocol::ContextCompactedEvent;
use viy_protocol::Event;
use viy_protocol::EventMsg;
use viy_protocol::HistoryItem;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Op;
use viy_protocol::StatusPhase;
use viy_protocol::ToolCallEndEvent;
use viy_protocol::ToolCallStatus;
use viy_protocol::TurnOutcome;

use crate::cells::Reasoning;
use crate::cells::Running;
use crate::cells::ToolCard;
use crate::composer::Action;
use crate::composer::Composer;
use crate::composer::POPUP_ROWS;
use crate::footer::Counts;
use crate::footer::Status;
use crate::frame::FrameLimiter;
use crate::history::History;
use crate::markdown;
use crate::question::Questions;
use crate::question::Taken;
use crate::sanitize::sanitize_line_after;
use crate::tabtitle;
use crate::terminal::Terminal;
use crate::transcript::Cell;
use crate::transcript::TranscriptView;
use crate::tui::Tui;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// One spinner frame; while a turn runs, a frame is drawn this often.
const SPINNER_FRAME: Duration = Duration::from_millis(80);
/// How long the header's art takes to dissolve once typing starts, and
/// to come back, the dissolve played in reverse, once the box is cleared.
const ART_DISSOLVE: Duration = Duration::from_millis(250);
/// Rows one wheel notch scrolls.
const WHEEL_ROWS: usize = 3;

/// Queued messages shown above the input at most.
const QUEUED_ROWS: usize = 3;

/// What reaches the app from the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Resize(u16, u16),
    /// SIGTERM: close the chat, as at the prompt.
    Terminate,
    /// SIGHUP: the terminal is gone; close the chat without drawing.
    Hangup,
}

/// Why [`App::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Ctrl-Z: suspend, then run again.
    Suspend,
    /// The engine has shut down.
    Exit,
}

/// The turn running: what the activity line and the live tail show.
struct Turn {
    started: Instant,
    phase: Option<StatusPhase>,
    tokens: u64,
    /// The round's reasoning so far, and when it began.
    reasoning: String,
    reasoning_since: Option<Instant>,
    /// The answer since the last tool call; `answer[..committed]` is in
    /// the transcript, as the cell `answer_cell`.
    answer: String,
    committed: usize,
    answer_cell: Option<usize>,
    /// Tool calls running, in the order they began.
    tools: Vec<(ToolCard, Instant)>,
}

pub struct App {
    ops: mpsc::UnboundedSender<Op>,
    cells: Vec<Cell>,
    view: TranscriptView,
    composer: Composer,
    turn: Option<Turn>,
    /// Messages sent that have not started yet, oldest first.
    queued: VecDeque<String>,
    /// The centipede header, first in the transcript (CHAT_HEADER).
    header: Option<crate::header::Header>,
    /// The start of the input box that lines not sent were put back as.
    given_back: String,
    /// The questions the engine waits on, oldest first.
    questions: Questions,
    /// Your messages `/rewind` can go back to: their cell and text, since
    /// the last compaction (kb d045).
    points: Vec<(usize, String)>,
    /// The rewind pick: which of `points` is chosen.
    picking: Option<usize>,
    /// A message or command went to the engine since the transcript was
    /// last rewritten: the header's art stays hidden until it shows again.
    sent: bool,
    /// How far the header's art has dissolved, 0 (whole) to 1 (gone),
    /// in time rather than eased.
    art_gone: f32,
    /// While the art dissolves or comes back: when it last moved.
    art_moved: Option<Instant>,
    /// Esc was pressed at an empty, idle prompt: a second opens the pick.
    esc_primed: bool,
    /// The engine's statuses, for the footer and the tab title.
    status: Status,
    /// The tab title, if the chat turned it on, and the title written last.
    tab: Option<tabtitle::Encoding>,
    tab_shown: Option<String>,
    /// Shutdown was sent; the chat closes when the engine says it has.
    closing: bool,
    /// The engine has stopped.
    stopped: bool,
    /// No more drawing: the terminal hung up or a write failed (kept to
    /// return once the engine has stopped).
    blind: Option<io::Error>,
    limiter: FrameLimiter,
    last_frame: Option<Instant>,
    /// Where the caret was drawn in the last frame, if anywhere. viy draws
    /// its own caret (a blinking reversed block) and keeps the terminal's
    /// cursor hidden at the top left: a terminal made shorter scrolls its
    /// screen up to keep its cursor in view before viy repaints (GNOME
    /// Terminal's VTE does), so with the cursor in the input box at the
    /// bottom a resize jolted the whole screen up and back. With it at the
    /// top, the screen's top stays put.
    caret: Option<Position>,
    dirty: bool,
    /// The next frame rewrites every cell (Ctrl-L).
    repaint: bool,
    /// Tool output and reasoning drawn whole (Ctrl-T).
    expanded: bool,
    /// Answers drawn as Markdown (rp `/md on`, its default).
    markdown: bool,
    /// The reasoning trace drawn (rp `/think on`, its default).
    show_think: bool,
}

impl App {
    /// The chat's screen; what is sent goes into `history`.
    pub fn new(ops: mpsc::UnboundedSender<Op>, history: History) -> Self {
        Self {
            ops,
            cells: Vec::new(),
            view: TranscriptView::default(),
            composer: Composer::new(history),
            turn: None,
            queued: VecDeque::new(),
            header: None,
            given_back: String::new(),
            questions: Questions::default(),
            points: Vec::new(),
            sent: false,
            art_gone: 0.0,
            art_moved: None,
            picking: None,
            esc_primed: false,
            status: Status::default(),
            tab: None,
            tab_shown: None,
            closing: false,
            stopped: false,
            blind: None,
            limiter: FrameLimiter::default(),
            last_frame: None,
            caret: None,
            dirty: true,
            repaint: false,
            expanded: false,
            markdown: true,
            show_think: true,
        }
    }

    /// Sets the tab title from now on, in `encoding` (rp's
    /// `CHAT_TAB_TITLE`): the terminal's own title must already be saved
    /// ([`Tui::save_title`]).
    pub fn tab_title(&mut self, encoding: tabtitle::Encoding) {
        self.tab = Some(encoding);
        self.tab_shown = None;
        self.dirty = true;
    }

    /// The terminal was given back and taken again (Ctrl-Z): it shows the
    /// shell's title, and the next frame writes the chat's.
    pub fn retitle(&mut self) {
        self.tab_shown = None;
        self.dirty = true;
    }

    /// Adds a cell to the transcript.
    pub fn push(&mut self, cell: Cell) {
        self.cells.push(cell);
        self.dirty = true;
    }

    /// Runs until the engine has shut down or Ctrl-Z, drawing on `term`.
    /// The engine's events come from `events`, the terminal's from `input`.
    /// A failed write stops the drawing and closes the chat; its error is
    /// returned once the engine has stopped.
    pub async fn run<B>(
        &mut self,
        term: &mut Terminal<B>,
        input: &mut mpsc::UnboundedReceiver<Input>,
        events: &mut mpsc::UnboundedReceiver<Event>,
    ) -> io::Result<Flow>
    where
        B: Backend<Error = io::Error> + Write,
    {
        let mut input_open = true;
        loop {
            if self.stopped {
                return match self.blind.take() {
                    Some(e) => Err(e),
                    None => Ok(Flow::Exit),
                };
            }
            let frame = self.next_frame();
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.event(event.msg),
                    None => self.stopped = true,
                },
                got = input.recv(), if input_open => match got {
                    Some(got) => {
                        if let Some(flow) = self.on_input(got, term) {
                            return Ok(flow);
                        }
                    }
                    None => input_open = false,
                },
                () = sleep_until(frame), if frame.is_some() => self.draw(term),
            }
        }
    }

    /// When the next frame is due: soon after a change, and every spinner
    /// frame while something runs.
    fn next_frame(&self) -> Option<Instant> {
        if self.blind.is_some() {
            return None;
        }
        let now = Instant::now();
        if self.dirty || self.art_moved.is_some() {
            return Some(self.limiter.deadline(now));
        }
        let animating = self.turn.is_some() || self.closing;
        let next = if animating {
            Some(self.last_frame.map_or(now, |t| t + SPINNER_FRAME))
        } else {
            self.status.next_change(now)
        };
        // The box's keys light up when it starts taking them.
        let armed = self.questions.armed_at().filter(|&at| at > now);
        match (next, armed) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Draws `header` first in the transcript, now and after every
    /// repaint (CHAT_HEADER).
    pub fn header(&mut self, header: crate::header::Header) {
        self.cells.insert(0, Cell::Header(header.clone()));
        self.header = Some(header);
        for (cell, _) in &mut self.points {
            *cell += 1;
        }
        self.view.reset();
        self.dirty = true;
    }

    /// Takes in one engine event.
    pub fn event(&mut self, msg: EventMsg) {
        self.dirty = true;
        if self.status.event(&msg, Instant::now()) {
            return;
        }
        match msg {
            EventMsg::TurnStarted(e) => {
                self.queued.pop_front();
                self.push(Cell::user(&e.user_text));
                self.points.push((self.cells.len() - 1, e.user_text));
                self.turn = Some(Turn {
                    started: Instant::now(),
                    phase: None,
                    tokens: 0,
                    reasoning: String::new(),
                    reasoning_since: None,
                    answer: String::new(),
                    committed: 0,
                    answer_cell: None,
                    tools: Vec::new(),
                });
            }
            EventMsg::ReasoningContentDelta(e) => {
                self.commit_answer();
                if let Some(turn) = &mut self.turn {
                    turn.reasoning.push_str(&e.delta);
                    turn.reasoning_since.get_or_insert_with(Instant::now);
                    turn.phase = Some(StatusPhase::Reasoning);
                }
            }
            EventMsg::AgentMessageContentDelta(e) => {
                self.commit_reasoning();
                if let Some(turn) = &mut self.turn {
                    turn.answer.push_str(&e.delta);
                    if e.delta.contains('\r') || turn.answer.ends_with('\r') {
                        turn.answer = markdown::visible_cr(&turn.answer, false).into_owned();
                    }
                    turn.phase = Some(StatusPhase::Answering);
                }
                self.commit_finished();
            }
            EventMsg::Status(e) => {
                if let Some(turn) = &mut self.turn {
                    turn.phase = Some(e.phase);
                    turn.tokens = e.tokens;
                }
            }
            EventMsg::TurnComplete(e) => {
                self.end_turn();
                if let TurnOutcome::Failed { message } = e.outcome {
                    self.push(Cell::lines(vec![Line::from(format!("✗ {message}").red())]));
                }
            }
            // Stopped before it started (Ctrl-C while it was admitted):
            // the line was never sent, so it goes back to the input box.
            EventMsg::TurnAborted(_) if self.turn.is_none() => {
                if let Some(text) = self.queued.pop_front() {
                    self.give_back(&text);
                }
            }
            EventMsg::TurnAborted(_) => {
                self.end_turn();
                self.push(Cell::lines(vec![Line::from("■ interrupted".yellow())]));
            }
            // Nothing answers yet (kb d041): the line goes back into the
            // input box and the chat says why; nothing of it is in the
            // transcript.
            EventMsg::TurnRefused(e) => {
                self.queued.pop_front();
                self.give_back(&e.text);
                self.push(Cell::lines(vec![Line::from(e.message.yellow())]));
            }
            EventMsg::Notice(notice) => self.push(Cell::notice(&notice)),
            EventMsg::ToolCallBegin(e) => {
                self.commit_reasoning();
                self.commit_answer();
                if let Some(turn) = &mut self.turn {
                    let card = ToolCard::new(&e.call_id, &e.tool, &e.summary);
                    turn.tools.push((card, Instant::now()));
                    turn.phase = Some(StatusPhase::RunningTools);
                }
            }
            EventMsg::ToolCallOutputDelta(e) => {
                if let Some(turn) = &mut self.turn
                    && let Some((card, _)) =
                        turn.tools.iter_mut().find(|(c, _)| c.call_id == e.call_id)
                {
                    card.output.push_str(&e.chunk);
                }
            }
            EventMsg::ToolCallEnd(e) => {
                let card = self.turn.as_mut().and_then(|turn| {
                    let i = turn
                        .tools
                        .iter()
                        .position(|(c, _)| c.call_id == e.call_id)?;
                    Some(turn.tools.remove(i).0)
                });
                if let Some(mut card) = card {
                    card.end = Some(e);
                    self.push(Cell::Tool(card));
                }
            }
            EventMsg::QuestionRequest(e) => self.questions.push(e.question_id, e.question),
            EventMsg::SecretRequest(e) => self.questions.push_secret(e.question_id, e.prompt),
            EventMsg::QuestionResolved(e) => {
                if let Some(question) = self.questions.resolve(&e.question_id) {
                    // Answered by the mode (Shift+Tab at the box): rp's
                    // "y (auto-yes)" where the user's answer would be.
                    let cell = match &e.by {
                        _ if question.is_secret() => question.secret_cell(false),
                        Some(mode) => question.cell_by(viy_protocol::Answer::Yes, mode),
                        None => question.cell(None),
                    };
                    self.push(cell);
                }
            }
            EventMsg::HistoryRewritten(e) => {
                self.picking = None;
                self.sent = false;
                self.cells.clear();
                self.points.clear();
                if let Some(header) = &self.header {
                    self.cells.push(Cell::Header(header.clone()));
                }
                for item in &e.items {
                    match item {
                        HistoryItem::User { text } => {
                            self.points.push((self.cells.len(), text.clone()));
                        }
                        HistoryItem::Compacted => self.points.clear(),
                        _ => {}
                    }
                    self.cells.extend(history_cell(item, self.markdown));
                }
                self.view.reset();
                // The answer's cell, if one is being written, is gone.
                if let Some(turn) = &mut self.turn {
                    turn.answer_cell = None;
                }
            }
            EventMsg::ContextCompacted(e) => {
                // What was summarised is no longer something to go back to.
                self.points.clear();
                self.push(compacted_cell(&e));
            }
            EventMsg::InputPrefill(e) => self.composer.set_text(&e.text),
            EventMsg::ShutdownComplete => self.stopped = true,
            _ => {}
        }
    }

    /// Takes in one terminal input; `Some` to leave the loop.
    fn on_input<B>(&mut self, input: Input, term: &mut Terminal<B>) -> Option<Flow>
    where
        B: Backend<Error = io::Error> + Write,
    {
        self.dirty = true;
        match input {
            Input::Key(key) if key.kind != KeyEventKind::Release => {
                return self.key(key, Instant::now());
            }
            Input::Key(_) => {}
            Input::Paste(text) => {
                // A secret takes the paste; no character of it reaches the input box.
                if !self.questions.paste(&text, Instant::now()) {
                    self.questions.typed(Instant::now());
                    self.composer.paste(&text);
                }
            }
            Input::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.view.scroll_up(WHEEL_ROWS),
                MouseEventKind::ScrollDown => self.view.scroll_down(WHEEL_ROWS),
                _ => {}
            },
            Input::Resize(width, height) => term.resize(Size::new(width, height)),
            Input::Terminate => self.close(),
            Input::Hangup => {
                self.blind = Some(io::Error::other("the terminal hung up"));
                self.close();
            }
        }
        None
    }

    /// Takes a key pressed at `now` (the input box tells a keystroke paste
    /// from typing by the time between keys).
    fn key(&mut self, key: KeyEvent, now: Instant) -> Option<Flow> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.picking.is_some() {
            self.pick_key(key);
            return None;
        }
        // A secret in front takes every key, Esc and Ctrl-C included.
        let secret_front = self
            .questions
            .front()
            .is_some_and(crate::question::QuestionBox::is_secret);
        let primed = std::mem::take(&mut self.esc_primed);
        if !secret_front && key.code == KeyCode::Esc && self.idle() && self.composer.is_empty() {
            if primed {
                self.open_pick();
            } else {
                self.esc_primed = true;
            }
            return None;
        }
        if !secret_front && self.closing && key.code == KeyCode::Char('c') && ctrl {
            // A second Ctrl-C while closing: leave now.
            return Some(Flow::Exit);
        }
        match self.questions.key(key, now, !self.composer.is_empty()) {
            Some(Taken::Answered { question, answer }) => {
                let _ = self.ops.send(Op::QuestionAnswer {
                    id: question.id.clone(),
                    answer,
                });
                self.push(question.cell(Some(answer)));
                return None;
            }
            Some(Taken::CycleMode { question_id }) => {
                let _ = self.ops.send(Op::CycleMode {
                    question: Some(question_id),
                });
                return None;
            }
            Some(Taken::Chose | Taken::Typed) => return None,
            Some(Taken::Secret { question, secret }) => {
                let given = secret.is_some();
                let _ = self.ops.send(Op::SecretAnswer {
                    id: question.id.clone(),
                    secret,
                });
                self.push(question.secret_cell(given));
                return None;
            }
            None => {}
        }
        let page = self.view.page().saturating_sub(1).max(1);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                // A question not yet taking keys: stop the work that asked
                // it, a turn or a command; the engine closes the question.
                // With no turn, a pod command running beside the chat (kb
                // d041) is stopped first, as Ctrl-C stops it in a terminal.
                // A line sent and not yet started is being admitted: it is
                // stopped, and comes back.
                if self.turn.is_some()
                    || self.questions.front().is_some()
                    || !self.queued.is_empty()
                    || (self.composer.is_empty() && self.status.pod_work())
                {
                    let _ = self.ops.send(Op::Interrupt);
                } else if !self.composer.is_empty() {
                    self.composer.clear();
                } else {
                    self.close();
                }
            }
            KeyCode::Char('d') if ctrl && self.composer.is_empty() && self.turn.is_none() => {
                self.close();
            }
            KeyCode::Char('z') if ctrl => return Some(Flow::Suspend),
            KeyCode::Char('l') if ctrl => self.repaint = true,
            KeyCode::Char('t') if ctrl => {
                self.expanded = !self.expanded;
                self.view.set_expanded(self.expanded);
            }
            KeyCode::Home if ctrl => self.view.home(),
            KeyCode::End if !self.view.is_following() => self.view.end(),
            KeyCode::PageUp => self.view.scroll_up(page),
            KeyCode::PageDown => self.view.scroll_down(page),
            _ => {
                // Typing: no question takes a key until it stops.
                self.questions.typed(now);
                match self.composer.key(key, now) {
                    Action::Submit(text) => self.submit(text),
                    Action::CycleMode => {
                        let _ = self.ops.send(Op::CycleMode { question: None });
                    }
                    Action::None => {}
                }
            }
        }
        None
    }

    /// Sends what the input box gave: a slash command, or a message, which
    /// the engine starts when the turn running ends.
    fn submit(&mut self, text: String) {
        if self.closing {
            self.composer.set_text(&text);
            return;
        }
        let op = if let Some(rest) = text.strip_prefix('/') {
            if self.screen_command(rest) {
                return;
            }
            Op::Command { text }
        } else {
            self.queued.push_back(text.clone());
            Op::UserTurn { text }
        };
        let _ = self.ops.send(op);
        self.sent = true;
        self.view.end();
    }

    /// Puts a line that was not sent back into the input box: after the
    /// lines given back before it, so several come back in the order they
    /// were sent, and ahead of anything typed since.
    fn give_back(&mut self, text: &str) {
        let draft = self.composer.text().to_string();
        let earlier = if !self.given_back.is_empty() && draft.starts_with(&self.given_back) {
            std::mem::take(&mut self.given_back)
        } else {
            String::new()
        };
        let typed = draft[earlier.len()..].trim_start_matches('\n');
        let back = if earlier.is_empty() {
            text.to_string()
        } else {
            format!("{earlier}\n{text}")
        };
        let whole = if typed.is_empty() {
            back.clone()
        } else {
            format!("{back}\n{typed}")
        };
        self.composer.set_text(&whole);
        self.given_back = back;
    }

    /// Nothing runs or waits: no turn, no queued line, no question.
    fn idle(&self) -> bool {
        self.turn.is_none()
            && self.queued.is_empty()
            && self.questions.front().is_none()
            && !self.closing
    }

    /// `/rewind`'s pick in the transcript (kb d045): your last message
    /// chosen. With none to choose, the chat says why.
    fn open_pick(&mut self) {
        let Some(last) = self.points.len().checked_sub(1) else {
            let _ = self.ops.send(Op::Command {
                text: "/rewind".into(),
            });
            return;
        };
        self.choose(last);
    }

    /// Chooses point `k`, drawn reversed, the one chosen before as it was.
    fn choose(&mut self, k: usize) {
        if let Some(old) = self.picking.replace(k) {
            self.unchoose(old);
        }
        let (cell, text) = &self.points[k];
        let cell = *cell;
        self.cells[cell] = Cell::user_chosen(text);
        self.view.changed(cell);
        self.view.reveal(cell);
    }

    fn unchoose(&mut self, k: usize) {
        if let Some((cell, text)) = self.points.get(k)
            && *cell < self.cells.len()
        {
            self.cells[*cell] = Cell::user(text);
            self.view.changed(*cell);
        }
    }

    /// Ends the pick, the transcript as it was.
    fn close_pick(&mut self) {
        if let Some(k) = self.picking.take() {
            self.unchoose(k);
        }
    }

    /// A key during the pick: Up and Down move between your messages,
    /// Home and End to the first and last, Enter goes back to before the
    /// chosen one, Esc and Ctrl-C leave it as it was.
    fn pick_key(&mut self, key: KeyEvent) {
        let Some(k) = self.picking else {
            return;
        };
        let last = self.points.len().saturating_sub(1);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up => self.choose(k.saturating_sub(1)),
            KeyCode::Down => self.choose((k + 1).min(last)),
            KeyCode::Home => self.choose(0),
            KeyCode::End => self.choose(last),
            KeyCode::Enter => {
                self.close_pick();
                let _ = self.ops.send(Op::Command {
                    text: format!("/rewind {}", k + 1),
                });
                self.view.end();
            }
            KeyCode::Esc => self.close_pick(),
            KeyCode::Char('c') if ctrl => self.close_pick(),
            _ => {}
        }
    }

    /// The slash commands that are the screen's own: `/exit`, `/redraw`
    /// (Ctrl-L), and rp's `/think` and `/md`, which only change what is
    /// shown. True when `rest` (after the `/`) was one.
    fn screen_command(&mut self, rest: &str) -> bool {
        let (cmd, arg) = rest.split_once(' ').unwrap_or((rest, ""));
        let arg = arg.trim();
        let say = |text: String| {
            Cell::notice(&NoticeEvent {
                level: NoticeLevel::Info,
                code: cmd.to_string(),
                message: text,
            })
        };
        match cmd {
            "exit" | "quit" | "q" => self.close(),
            "rewind" if arg.is_empty() && self.idle() => self.open_pick(),
            "redraw" => self.repaint = true,
            "think" => {
                self.show_think = arg != "off";
                let how = if self.show_think { "shown" } else { "hidden" };
                self.push(say(format!("reasoning trace {how}")));
            }
            "md" => {
                if matches!(arg, "on" | "off") {
                    self.markdown = arg == "on";
                } else if !arg.is_empty() {
                    self.push(Cell::notice(&NoticeEvent {
                        level: NoticeLevel::Warn,
                        code: "md".into(),
                        message: "usage: /md on|off".into(),
                    }));
                    return true;
                }
                let how = if self.markdown { "on" } else { "off" };
                self.push(say(format!("Markdown rendering {how}")));
            }
            _ => return false,
        }
        true
    }

    /// Asks the engine to close the chat: it closes the turn running into
    /// the history and saves the session first, as rp does.
    fn close(&mut self) {
        if !self.closing {
            self.closing = true;
            let _ = self.ops.send(Op::Shutdown);
        }
    }

    /// Moves the answer's finished blocks into the transcript: all but
    /// the last top-level block, which can still change.
    fn commit_finished(&mut self) {
        let Some(turn) = &self.turn else {
            return;
        };
        let cut = markdown::finished(&turn.answer[turn.committed..]);
        if cut > 0 {
            let end = turn.committed + cut;
            self.show_answer(end);
        }
    }

    /// Moves the whole answer so far into the transcript; what comes
    /// next starts a new one.
    fn commit_answer(&mut self) {
        let Some(turn) = &self.turn else {
            return;
        };
        let end = turn.answer.len();
        self.show_answer(end);
        if let Some(turn) = &mut self.turn {
            turn.answer.clear();
            turn.committed = 0;
            turn.answer_cell = None;
        }
    }

    /// Shows `answer[..end]` in the answer's transcript cell.
    fn show_answer(&mut self, end: usize) {
        let markdown = self.markdown;
        let Some(turn) = &mut self.turn else {
            return;
        };
        turn.committed = end;
        let text = &turn.answer[..end];
        if text.trim().is_empty() {
            return;
        }
        let cell = Cell::answer(text, markdown);
        match turn.answer_cell {
            Some(i) if i < self.cells.len() => {
                self.cells[i] = cell;
                self.view.changed(i);
            }
            _ => {
                turn.answer_cell = Some(self.cells.len());
                self.cells.push(cell);
            }
        }
        self.dirty = true;
    }

    /// Moves the round's reasoning into the transcript.
    fn commit_reasoning(&mut self) {
        let show_think = self.show_think;
        let Some(turn) = &mut self.turn else {
            return;
        };
        let text = std::mem::take(&mut turn.reasoning);
        let since = turn.reasoning_since.take();
        if !text.trim().is_empty() {
            self.push(Cell::Reasoning(Reasoning {
                text,
                took: since.map(|t| t.elapsed()),
                shown: show_think,
            }));
        }
    }

    fn end_turn(&mut self) {
        self.commit_reasoning();
        self.commit_answer();
        // A call the engine never ended stopped with the turn.
        let left = self.turn.take().map(|t| t.tools).unwrap_or_default();
        for (mut card, started) in left {
            card.end = Some(ToolCallEndEvent {
                call_id: card.call_id.clone(),
                status: ToolCallStatus::Interrupted,
                detail: None,
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                sandboxed: false,
                network: false,
            });
            self.push(Cell::Tool(card));
        }
    }

    /// Draws a frame; a failed write stops the drawing and closes the chat.
    fn draw<B>(&mut self, term: &mut Terminal<B>)
    where
        B: Backend<Error = io::Error> + Write,
    {
        self.draw_at(term, Instant::now());
    }

    /// [`App::draw`], with the frame drawn at `now`.
    fn draw_at<B>(&mut self, term: &mut Terminal<B>, now: Instant)
    where
        B: Backend<Error = io::Error> + Write,
    {
        if std::mem::take(&mut self.repaint) {
            term.invalidate();
        }
        if let Err(e) = self.render(term, now) {
            self.blind = Some(e);
            self.close();
        }
        self.limiter.drawn(now);
        self.last_frame = Some(now);
        self.dirty = false;
    }

    /// The header's art shows only while nothing has been sent in this
    /// chat and the input box is empty: typing dissolves it over
    /// [`ART_DISSOLVE`], clearing the box brings it back the same way in
    /// reverse (from wherever the dissolve had got to), the first message
    /// sent (or a resumed chat's) ends it straight away, as Codex's
    /// welcome logo. The title and directory stay.
    fn header_art(&mut self, now: Instant) {
        // Shown only on an empty chat: nothing typed in the input box,
        // nothing sent, and nothing in the transcript but the header (no
        // message, command output or notice).
        let empty = self.cells.len() == 1 && !self.sent && self.turn.is_none();
        let Some(Cell::Header(header)) = self.cells.first_mut() else {
            self.art_moved = None;
            return;
        };
        if empty {
            let target = if self.composer.is_empty() { 0.0 } else { 1.0 };
            let step = self.art_moved.map_or(0.0, |at| {
                now.saturating_duration_since(at).as_secs_f32() / ART_DISSOLVE.as_secs_f32()
            });
            self.art_gone = if target > self.art_gone {
                (self.art_gone + step).min(target)
            } else {
                (self.art_gone - step).max(target)
            };
            self.art_moved = (self.art_gone != target).then_some(now);
        } else {
            self.art_gone = 1.0;
            self.art_moved = None;
        }
        let art = self.art_gone < 1.0;
        let dissolve = (crate::header::smoothstep(self.art_gone) * 1000.0).round() as u16;
        if (header.art, header.dissolve) != (art, dissolve) {
            (header.art, header.dissolve) = (art, dissolve);
            self.view.changed(0);
        }
    }

    fn render<B>(&mut self, term: &mut Terminal<B>, now: Instant) -> io::Result<()>
    where
        B: Backend<Error = io::Error> + Write,
    {
        let size = term.size();
        let width = size.width.max(4);
        let (mut input_rows, mut input_cursor) = self.composer.layout(width - 2);
        // The live region may take all but two rows; the transcript the
        // rest. The question box keeps its rows longest, then the queued
        // messages and the popup, then the activity line.
        let room = usize::from(size.height.saturating_sub(2)).max(2);
        // A draft taller than that shows the rows around the cursor, as
        // Codex's composer scrolls, so the footer stays on screen (a pasted
        // 268 KB message pushed it off, seen live 2026-07-18).
        let cap = room.saturating_sub(1).max(1);
        if input_rows.len() > cap {
            let at = usize::from(input_cursor.y);
            let start = (at + 1).saturating_sub(cap).min(input_rows.len() - cap);
            input_rows.drain(..start);
            input_rows.truncate(cap);
            input_cursor.y = u16::try_from(at - start).unwrap_or(0);
        }
        let fixed = input_rows.len() + 1;
        let (question, whole) =
            self.questions
                .rows(usize::from(size.width), room.saturating_sub(fixed), now);
        let mut tail = self.queued_lines(usize::from(width));
        tail.extend(self.popup_lines(usize::from(width)));
        // The rows furthest from the input give way first.
        let excess = tail
            .len()
            .saturating_sub(room.saturating_sub(fixed + question.len()));
        tail.drain(..excess);
        let mut live = self.activity_lines(usize::from(width));
        let live_room = room.saturating_sub(fixed + question.len() + tail.len());
        if live.len() > live_room {
            let first = usize::from(live_room > 0);
            live.drain(first..first + live.len() - live_room);
        }
        live.extend(question);
        live.extend(tail);
        let input_top = live.len();
        let mut bottom = live;
        for (i, row) in input_rows.into_iter().enumerate() {
            let prefix = if i == 0 { "› ".cyan() } else { "  ".into() };
            bottom.push(Line::from(vec![prefix, row.into()]));
        }
        let bottom_rows = bottom.len() + 1;
        let bottom_height = u16::try_from(bottom_rows)
            .unwrap_or(u16::MAX)
            .min(size.height);
        let top_height = size.height - bottom_height;
        self.view
            .set_live(self.tail_lines(usize::from(size.width.max(1))));
        self.header_art(now);
        self.view.prepare(&self.cells, size.width, top_height);
        bottom.push(self.footer(usize::from(size.width)));
        let bottom: Vec<Line<'static>> = bottom.into_iter().map(clean_row).collect();
        let top = Rect::new(0, 0, size.width, top_height);
        let live_area = Rect::new(0, top_height, size.width, bottom_height);
        let cursor = Position::new(
            2 + input_cursor.x,
            top_height + u16::try_from(input_top).unwrap_or(0) + input_cursor.y,
        );
        self.write_title(term.backend_mut())?;
        let caret = (cursor.y < size.height && cursor.x < size.width).then_some(cursor);
        let (cells, view) = (&self.cells, &mut self.view);
        term.draw(|frame| {
            view.render(cells, top, frame.buffer_mut());
            frame.render_widget(Paragraph::new(bottom), live_area);
            if let Some(at) = caret {
                frame.buffer_mut()[at].set_style(
                    Style::new().add_modifier(Modifier::REVERSED | Modifier::SLOW_BLINK),
                );
            }
        })?;
        self.caret = caret;
        self.questions
            .drawn(now, whole && usize::from(bottom_height) == bottom_rows);
        Ok(())
    }

    /// The live region's rows above the input: the rewind hint, and the
    /// activity line of the turn running or of closing.
    fn activity_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if self.picking.is_some() {
            let hint = "rewind: ↑↓ choose your message · Enter goes back to before it, to edit \
                        and send again · Esc leaves it";
            lines.push(Line::from(
                hint.chars().take(width).collect::<String>().cyan(),
            ));
        }
        if self.closing {
            lines.push(Line::from("closing the chat…".dim()));
            return lines;
        }
        let Some(turn) = &self.turn else {
            return lines;
        };
        let elapsed = turn.started.elapsed();
        let frame = (elapsed.as_millis() / SPINNER_FRAME.as_millis()) as usize % SPINNER.len();
        let phase = match turn.phase {
            None | Some(StatusPhase::Waiting) => "waiting",
            Some(StatusPhase::Reasoning) => "thinking",
            Some(StatusPhase::Answering) => "writing",
            Some(StatusPhase::RunningTools) => "running tools",
            Some(_) => "working",
        };
        let mut activity = format!("{phase} · ");
        if turn.tokens > 0 {
            activity.push_str(&format!("{} tok · ", turn.tokens));
        }
        activity.push_str(&format!("{:.0} s · Ctrl-C stops", elapsed.as_secs_f32()));
        lines.push(Line::from(vec![
            SPINNER[frame].magenta(),
            " ".into(),
            activity.dim(),
        ]));
        lines
    }

    /// The transcript's live tail: what the turn is writing, drawn as the
    /// cells it becomes will draw it, so moving up into them moves no row.
    fn tail_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let Some(turn) = &self.turn else {
            return lines;
        };
        if self.show_think && !turn.reasoning.trim().is_empty() {
            let live = Reasoning {
                text: turn.reasoning.clone(),
                took: None,
                shown: true,
            };
            lines.extend(live.rows(width, false).into_iter().map(|r| r.line));
        }
        let tail = &turn.answer[turn.committed..];
        if !tail.trim().is_empty() {
            let rows = Cell::answer(tail, self.markdown).rows(width, false);
            // Going on from the answer's cell, the blank row its own cell
            // starts with is there only where the text has a blank line.
            let skip = turn.answer_cell.is_some() && !ends_blank(&turn.answer[..turn.committed]);
            lines.extend(rows.into_iter().skip(usize::from(skip)).map(|r| r.line));
        }
        let elapsed = turn.started.elapsed();
        let frame = (elapsed.as_millis() / SPINNER_FRAME.as_millis()) as usize % SPINNER.len();
        for (card, started) in &turn.tools {
            let running = Running {
                spinner: SPINNER[frame],
                elapsed: started.elapsed(),
            };
            lines.extend(
                card.rows(width, false, Some(running))
                    .into_iter()
                    .map(|r| r.line),
            );
        }
        lines
    }

    /// Messages waiting for the turn running to end, as Codex's
    /// `pending_input_preview.rs` shows them: the first line of each.
    fn queued_lines(&self, width: usize) -> Vec<Line<'static>> {
        if self.turn.is_none() {
            return Vec::new();
        }
        let mut lines: Vec<Line<'static>> = self
            .queued
            .iter()
            .take(QUEUED_ROWS)
            .map(|text| {
                let first = text.lines().next().unwrap_or_default();
                let more = if text.contains('\n') { " …" } else { "" };
                let row = format!("↳ {first}{more}");
                Line::from(row.chars().take(width).collect::<String>().dim())
            })
            .collect();
        if self.queued.len() > QUEUED_ROWS {
            let rest = self.queued.len() - QUEUED_ROWS;
            lines.push(Line::from(format!("  … {rest} more").dim()));
        }
        lines
    }

    /// The slash popup: the offers for the input, the choice reversed.
    fn popup_lines(&self, width: usize) -> Vec<Line<'static>> {
        let offers = self.composer.popup();
        let selected = self.composer.selected().min(offers.len().saturating_sub(1));
        let first = selected.saturating_sub(POPUP_ROWS - 1);
        let label_width = offers
            .iter()
            .map(|o| o.label.chars().count())
            .max()
            .unwrap_or(0);
        offers
            .iter()
            .enumerate()
            .skip(first)
            .take(POPUP_ROWS)
            .map(|(i, offer)| {
                let row = format!("  {:label_width$}  {}", offer.label, offer.about);
                let row: String = row.chars().take(width).collect();
                Line::from(if i == selected {
                    row.cyan().reversed()
                } else {
                    row.dim()
                })
            })
            .collect()
    }

    fn footer(&self, width: usize) -> Line<'static> {
        let counts = Counts {
            questions: self.questions.waiting(),
            queued: if self.turn.is_some() {
                self.queued.len()
            } else {
                0
            },
            rows_below: self.view.rows_below(),
        };
        self.status.line(counts, width, Instant::now())
    }

    /// Queues the tab title into `out` when it changed: the spinner while a
    /// turn runs, else the idle glyph (rp's `TabTitle`). Written with the
    /// frame, in its one write.
    fn write_title(&mut self, out: &mut impl Write) -> io::Result<()> {
        let Some(encoding) = self.tab else {
            return Ok(());
        };
        let busy = self
            .turn
            .as_ref()
            .map(|turn| (turn.started.elapsed().as_millis() / SPINNER_FRAME.as_millis()) as usize);
        let title = tabtitle::title(self.status.title.as_deref(), busy, encoding);
        if self.tab_shown.as_deref() == Some(title.as_str()) {
            return Ok(());
        }
        out.write_all(tabtitle::osc(&title).as_bytes())?;
        self.tab_shown = Some(title);
        Ok(())
    }
}

/// Whether `text` ends with a blank line: where the answer's next block
/// gets a blank row above it.
fn ends_blank(text: &str) -> bool {
    text.strip_suffix('\n')
        .and_then(|rest| rest.rsplit('\n').next())
        .is_some_and(|line| line.trim().is_empty())
}

/// `line` as one row with every char that could act on the terminal shown,
/// not applied: the live region's last check (ui-plan 7). The cells' rows
/// come from the wrapping functions, which do the same.
fn clean_row(mut line: Line<'static>) -> Line<'static> {
    let mut prev = None;
    for span in &mut line.spans {
        if let Cow::Owned(text) = sanitize_line_after(prev, &span.content) {
            span.content = text.into();
        }
        prev = span.content.chars().next_back().or(prev);
    }
    line
}

/// Sleeps until `at`; never, without one.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Runs the chat on the real terminal until the engine has shut down:
/// Ctrl-Z suspends and comes back to the same screen.
pub async fn run(
    tui: &mut Tui,
    app: &mut App,
    events: &mut mpsc::UnboundedReceiver<Event>,
) -> io::Result<()> {
    let mut input = spawn_input()?;
    loop {
        match app.run(tui.terminal(), &mut input, events).await? {
            Flow::Suspend => {
                tui.suspend()?;
                app.retitle();
            }
            Flow::Exit => return Ok(()),
        }
    }
}

/// The terminal's events, SIGTERM and SIGHUP, read by a task.
fn spawn_input() -> io::Result<mpsc::UnboundedReceiver<Input>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        loop {
            let next = poll_fn(|cx| Pin::new(&mut stream).poll_next(cx));
            let input = tokio::select! {
                event = next => match event {
                    Some(Ok(TermEvent::Key(key))) => Input::Key(key),
                    Some(Ok(TermEvent::Paste(text))) => Input::Paste(text),
                    Some(Ok(TermEvent::Mouse(mouse))) => Input::Mouse(mouse),
                    Some(Ok(TermEvent::Resize(width, height))) => Input::Resize(width, height),
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => Input::Hangup,
                },
                _ = terminate.recv() => Input::Terminate,
                _ = hangup.recv() => Input::Hangup,
            };
            let last = input == Input::Hangup;
            if tx.send(input).is_err() || last {
                return;
            }
        }
    });
    Ok(rx)
}

/// How a resumed or rewritten history draws `item` (kb d052): the trace of
/// a message that had one is not kept, so it is one line.
fn history_cell(item: &HistoryItem, markdown: bool) -> Option<Cell> {
    Some(match item {
        HistoryItem::User { text } => Cell::user(text),
        HistoryItem::Agent { text } => Cell::answer(text, markdown),
        HistoryItem::Reasoning { text } => Cell::Reasoning(Reasoning {
            text: text.clone(),
            took: None,
            shown: false,
        }),
        HistoryItem::ToolCall {
            call_id,
            tool,
            summary,
        } => Cell::Tool(ToolCard::new(call_id, tool, summary)),
        HistoryItem::Notice(notice) => Cell::notice(notice),
        HistoryItem::Thought => Cell::Reasoning(Reasoning {
            text: String::new(),
            took: None,
            shown: false,
        }),
        HistoryItem::Compacted => Cell::lines(vec![Line::from(
            "[earlier turns compacted: the model continues from a summary of what is above]".dim(),
        )]),
        _ => return None,
    })
}

/// The marker a compaction leaves under the transcript (kb d053).
fn compacted_cell(e: &ContextCompactedEvent) -> Cell {
    Cell::lines(vec![Line::from(
        format!(
            "compacted {} messages -> {} ({} → {} tokens). the session log keeps the full \
             history.",
            e.messages_before, e.messages_after, e.tokens_before, e.tokens_after
        )
        .dim(),
    )])
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
