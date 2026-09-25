//! The chat's screen (ui-plan 3): the transcript, scrolled by viy, above a
//! live region of the turn's activity line, the answer being written, the
//! input box and a footer. Engine events come in, [`Op`]s go out
//! (kb d036), and frames are drawn at most 30 a second, each in one
//! synchronized write.
//!
//! The answer streams as Markdown: its finished blocks move up into the
//! transcript, the open one is drawn again with each delta under the
//! activity line ([`markdown::finished`]). Reasoning is drawn live (rp
//! `/think on`) and then kept as a cell; running tools show a card with
//! the last lines of their output, which moves up when the call ends.
//! Ctrl-T draws every card's output and every trace whole. The footer
//! shows the title, queued lines and the scroll position (the statuses
//! are WP 3.10), and a question is answered as with nobody to ask, its
//! unattended answer, until the question boxes of WP 3.11.

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
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;
use tokio::sync::mpsc;
use tokio::time::Instant;
use viy_protocol::Answer;
use viy_protocol::Choices;
use viy_protocol::Event;
use viy_protocol::EventMsg;
use viy_protocol::HistoryItem;
use viy_protocol::Op;
use viy_protocol::Question;
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
use crate::frame::FrameLimiter;
use crate::history::History;
use crate::markdown;
use crate::sanitize::sanitize_line_after;
use crate::terminal::Terminal;
use crate::transcript::Cell;
use crate::transcript::TranscriptView;
use crate::tui::Tui;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// One spinner frame; while a turn runs, a frame is drawn this often.
const SPINNER_FRAME: Duration = Duration::from_millis(80);
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

/// The turn running: what the activity line and the live region show.
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
    title: Option<String>,
    /// The chat mode, once the engine has named one.
    mode: Option<String>,
    /// Shutdown was sent; the chat closes when the engine says it has.
    closing: bool,
    /// The engine has stopped.
    stopped: bool,
    /// No more drawing: the terminal hung up or a write failed (kept to
    /// return once the engine has stopped).
    blind: Option<io::Error>,
    limiter: FrameLimiter,
    last_frame: Option<Instant>,
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
            title: None,
            mode: None,
            closing: false,
            stopped: false,
            blind: None,
            limiter: FrameLimiter::default(),
            last_frame: None,
            dirty: true,
            repaint: false,
            expanded: false,
            markdown: true,
            show_think: true,
        }
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
        if self.dirty {
            return Some(self.limiter.deadline(now));
        }
        let animating = self.turn.is_some() || self.closing;
        animating.then(|| self.last_frame.map_or(now, |t| t + SPINNER_FRAME))
    }

    /// Takes in one engine event.
    pub fn event(&mut self, msg: EventMsg) {
        self.dirty = true;
        match msg {
            EventMsg::TurnStarted(e) => {
                self.queued.pop_front();
                self.push(Cell::user(&e.user_text));
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
            EventMsg::TurnAborted(_) => {
                self.end_turn();
                self.push(Cell::lines(vec![Line::from("■ interrupted".yellow())]));
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
            EventMsg::TitleChanged(e) => self.title = Some(e.title),
            EventMsg::ModeChanged(e) => self.mode = Some(e.mode),
            EventMsg::QuestionRequest(e) => {
                let answer = e.question.unattended;
                let _ = self.ops.send(Op::QuestionAnswer {
                    id: e.question_id,
                    answer,
                });
                self.push(question_cell(&e.question, answer));
            }
            EventMsg::HistoryRewritten(e) => {
                self.cells = e
                    .items
                    .iter()
                    .filter_map(|item| history_cell(item, self.markdown))
                    .collect();
                self.view.reset();
                // The answer's cell, if one is being written, is gone.
                if let Some(turn) = &mut self.turn {
                    turn.answer_cell = None;
                }
            }
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
            Input::Paste(text) => self.composer.paste(&text),
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
        let page = self.view.page().saturating_sub(1).max(1);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.closing {
                    // A second Ctrl-C while closing: leave now.
                    return Some(Flow::Exit);
                } else if self.turn.is_some() {
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
            _ => match self.composer.key(key, now) {
                Action::Submit(text) => self.submit(text),
                Action::CycleMode => {
                    let _ = self.ops.send(Op::CycleMode);
                }
                Action::None => {}
            },
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
        let op = if text.starts_with('/') {
            Op::Command { text }
        } else {
            self.queued.push_back(text.clone());
            Op::UserTurn { text }
        };
        let _ = self.ops.send(op);
        self.view.end();
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
        if std::mem::take(&mut self.repaint) {
            term.invalidate();
        }
        let now = Instant::now();
        if let Err(e) = self.render(term) {
            self.blind = Some(e);
            self.close();
        }
        self.limiter.drawn(now);
        self.last_frame = Some(now);
        self.dirty = false;
    }

    fn render<B>(&mut self, term: &mut Terminal<B>) -> io::Result<()>
    where
        B: Backend<Error = io::Error> + Write,
    {
        let size = term.size();
        let width = size.width.max(4);
        let (input_rows, input_cursor) = self.composer.layout(width - 2);
        let mut live = self.live_lines(usize::from(width));
        live.extend(self.queued_lines(usize::from(width)));
        live.extend(self.popup_lines(usize::from(width)));
        // The live region may take all but two rows; the transcript the
        // rest. The answer's oldest rows give way first, then the
        // activity line above them.
        let room = usize::from(size.height.saturating_sub(2)).max(2);
        let fixed = input_rows.len() + 1;
        let live_room = room.saturating_sub(fixed);
        if live.len() > live_room {
            let first = usize::from(live_room > 0);
            live.drain(first..first + live.len() - live_room);
        }
        let input_top = live.len();
        let mut bottom = live;
        for (i, row) in input_rows.into_iter().enumerate() {
            let prefix = if i == 0 { "› ".cyan() } else { "  ".into() };
            bottom.push(Line::from(vec![prefix, row.into()]));
        }
        let bottom_height = u16::try_from(bottom.len() + 1)
            .unwrap_or(u16::MAX)
            .min(size.height);
        let top_height = size.height - bottom_height;
        self.view.prepare(&self.cells, size.width, top_height);
        bottom.push(self.footer());
        let bottom: Vec<Line<'static>> = bottom.into_iter().map(clean_row).collect();
        let top = Rect::new(0, 0, size.width, top_height);
        let live_area = Rect::new(0, top_height, size.width, bottom_height);
        let cursor = Position::new(
            2 + input_cursor.x,
            top_height + u16::try_from(input_top).unwrap_or(0) + input_cursor.y,
        );
        let (cells, view) = (&self.cells, &mut self.view);
        term.draw(|frame| {
            view.render(cells, top, frame.buffer_mut());
            frame.render_widget(Paragraph::new(bottom), live_area);
            if cursor.y < size.height {
                frame.set_cursor_position(cursor);
            }
        })
    }

    /// The activity line and the answer being written.
    fn live_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
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
        if self.show_think && !turn.reasoning.trim().is_empty() {
            let live = Reasoning {
                text: turn.reasoning.clone(),
                took: None,
                shown: true,
            };
            // Without the blank row a cell starts with.
            lines.extend(live.rows(width, false).into_iter().skip(1).map(|r| r.line));
        }
        let tail = &turn.answer[turn.committed..];
        if !tail.trim().is_empty() {
            let rows = Cell::answer(tail, self.markdown).rows(width, false);
            lines.extend(rows.into_iter().skip(1).map(|r| r.line));
        }
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

    fn footer(&self) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = vec![
            self.title
                .clone()
                .unwrap_or_else(|| "viy".to_string())
                .dim(),
        ];
        if let Some(mode) = self.mode.as_ref().filter(|m| *m != "normal") {
            spans.push(format!(" │ {mode}").yellow());
        }
        if !self.queued.is_empty() && self.turn.is_some() {
            spans.push(format!(" │ {} queued", self.queued.len()).cyan());
        }
        let below = self.view.rows_below();
        if below > 0 {
            spans.push(format!(" │ ↓ {below} rows (End)").cyan());
        }
        Line::from(spans)
    }
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
            Flow::Suspend => tui.suspend()?,
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

fn question_cell(question: &Question, answer: Answer) -> Cell {
    Cell::lines(vec![Line::from(
        format!(
            "{} {} {} (question boxes come with WP 3.11)",
            question.prompt,
            choices(question.choices),
            answer_word(answer)
        )
        .yellow(),
    )])
}

/// rp's `[y/N]` and `[y/N/a=always]`.
pub fn choices(choices: Choices) -> &'static str {
    match choices {
        Choices::YesNoAlways => "[y/N/a=always]",
        _ => "[y/N]",
    }
}

pub fn answer_word(answer: Answer) -> &'static str {
    match answer {
        Answer::Yes => "y",
        Answer::No => "n",
        Answer::Always => "a",
    }
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
        _ => return None,
    })
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
