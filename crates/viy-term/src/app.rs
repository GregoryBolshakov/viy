//! The chat's screen (ui-plan 3): the transcript, scrolled by viy, above a
//! live region of the turn's activity line, the answer being written, the
//! input box and a footer. Engine events come in, [`Op`]s go out
//! (kb d036), and frames are drawn at most 30 a second, each in one
//! synchronized write.
//!
//! The shell of WP 3.6: its widgets are the first cut. The answer is plain
//! text whose finished paragraphs move up into the transcript (Markdown is
//! WP 3.9), the input is the spike's text area (the composer is WP 3.8),
//! the footer shows the title, queued lines and the scroll position (the
//! statuses are WP 3.10), and a question is answered as with nobody to ask,
//! its unattended answer, until the question boxes of WP 3.11.

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
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Op;
use viy_protocol::Question;
use viy_protocol::StatusPhase;
use viy_protocol::TurnOutcome;

use crate::composer::Action;
use crate::composer::Composer;
use crate::composer::POPUP_ROWS;
use crate::frame::FrameLimiter;
use crate::history::History;
use crate::sanitize::sanitize_line_after;
use crate::terminal::Terminal;
use crate::transcript::Cell;
use crate::transcript::TranscriptView;
use crate::tui::Tui;
use crate::wrap::wrap_line;

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

/// The turn running: what the activity line and the live answer show.
struct Turn {
    started: Instant,
    phase: Option<StatusPhase>,
    tokens: u64,
    /// The answer not yet moved up into the transcript; it never starts
    /// inside a code fence.
    live: String,
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
                self.push(user_cell(&e.user_text));
                self.turn = Some(Turn {
                    started: Instant::now(),
                    phase: None,
                    tokens: 0,
                    live: String::new(),
                });
            }
            EventMsg::AgentMessageContentDelta(e) => {
                if let Some(turn) = &mut self.turn {
                    turn.live.push_str(&e.delta);
                    turn.phase = Some(StatusPhase::Answering);
                }
                self.commit_finished();
            }
            EventMsg::ReasoningContentDelta(_) => {
                if let Some(turn) = &mut self.turn {
                    turn.phase = Some(StatusPhase::Reasoning);
                }
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
                    self.push(vec![Line::from(format!("✗ {message}").red())]);
                }
            }
            EventMsg::TurnAborted(_) => {
                self.end_turn();
                self.push(vec![Line::from("■ interrupted".yellow())]);
            }
            EventMsg::Notice(notice) => self.push(notice_cell(&notice)),
            EventMsg::ToolCallBegin(e) => {
                self.commit_live();
                self.push(vec![Line::from(
                    format!("▸ {}  {}", e.tool, e.summary).dim(),
                )]);
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
                self.cells = e.items.iter().map(history_cell).collect();
                self.view.reset();
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

    /// Moves the answer's finished paragraphs into the transcript: up to
    /// the last blank line outside a code fence.
    fn commit_finished(&mut self) {
        let Some(turn) = &mut self.turn else {
            return;
        };
        let mut fenced = false;
        let mut cut = None;
        let mut start = 0;
        for line in turn.live.split_inclusive('\n') {
            if !line.ends_with('\n') {
                break;
            }
            start += line.len();
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
            } else if !fenced && line.trim().is_empty() {
                cut = Some(start);
            }
        }
        if let Some(end) = cut {
            let done: String = turn.live.drain(..end).collect();
            let text = done.trim_end_matches('\n');
            if !text.trim().is_empty() {
                self.push(answer_cell(text));
            }
        }
    }

    /// Moves the whole answer so far into the transcript.
    fn commit_live(&mut self) {
        let Some(turn) = &mut self.turn else {
            return;
        };
        let done = std::mem::take(&mut turn.live);
        let text = done.trim_end_matches('\n');
        if !text.trim().is_empty() {
            self.push(answer_cell(text));
        }
    }

    fn end_turn(&mut self) {
        self.commit_live();
        self.turn = None;
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
        for row in turn.live.split('\n') {
            lines.extend(wrap_line(&Line::from(row.to_string()), width));
        }
        if turn.live.ends_with('\n') || turn.live.is_empty() {
            lines.pop();
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
/// not applied: the live region's last check (ui-plan 7). The transcript's
/// rows come from [`wrap_line`], which does the same.
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

fn user_cell(text: &str) -> Cell {
    let mut cell = vec![Line::from("")];
    for (i, row) in text.split('\n').enumerate() {
        let prefix = if i == 0 { "you › " } else { "      " };
        cell.push(Line::from(vec![
            prefix.cyan().bold(),
            row.to_string().into(),
        ]));
    }
    cell
}

fn answer_cell(text: &str) -> Cell {
    let mut cell = vec![Line::from("")];
    cell.extend(text.split('\n').map(|row| Line::from(row.to_string())));
    cell
}

fn notice_cell(notice: &NoticeEvent) -> Cell {
    notice
        .message
        .split('\n')
        .map(|row| {
            let row = row.to_string();
            Line::from(match notice.level {
                NoticeLevel::Info => row.dim(),
                NoticeLevel::Warn => row.yellow(),
                NoticeLevel::Error => row.red(),
            })
        })
        .collect()
}

fn question_cell(question: &Question, answer: Answer) -> Cell {
    vec![Line::from(
        format!(
            "{} {} {} (question boxes come with WP 3.11)",
            question.prompt,
            choices(question.choices),
            answer_word(answer)
        )
        .yellow(),
    )]
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

fn history_cell(item: &HistoryItem) -> Cell {
    match item {
        HistoryItem::User { text } => user_cell(text),
        HistoryItem::Agent { text } => answer_cell(text),
        HistoryItem::ToolCall { tool, summary, .. } => {
            vec![Line::from(format!("▸ {tool}  {summary}").dim())]
        }
        HistoryItem::Notice(notice) => notice_cell(notice),
        _ => Vec::new(),
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
