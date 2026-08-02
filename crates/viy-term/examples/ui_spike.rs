//! WP 0.8's UI spike (docs/implementation-plan.md, flags F32 and F33): the
//! alternate-screen layout of Codex's default, with the transcript viy draws
//! and scrolls itself above a live region, fed by a fake model stream, to
//! try in GNOME Terminal, tmux, over ssh and at 60 columns before phase 3
//! builds on it (kb d038). Not part of viy.
//!
//! ```text
//! cargo build -p viy-term --example ui_spike
//! target/debug/examples/ui_spike
//! ```
//!
//! Enter sends (during a stream it queues), Ctrl-J or a trailing `\` adds a
//! newline, Shift+Tab cycles the mode, Ctrl-C stops the stream or exits,
//! Ctrl-L repaints, PageUp/PageDown and the mouse wheel scroll the
//! transcript, End follows it again. With the mouse captured, Shift+drag is
//! the terminal's own selection. Commands: `/rewind` (the last exchange
//! gone, its message back in the box), `/flood N` (N transcript lines),
//! `/redraw`, `/panic` (the terminal must come back usable), `/quit`.

use std::io;
use std::io::Stdout;
use std::io::Write;
use std::time::Duration;
use std::time::Instant;

use crossterm::cursor::Show;
use crossterm::event;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableMouseCapture;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseEventKind;
use crossterm::execute;
use crossterm::queue;
use crossterm::terminal;
use crossterm::terminal::BeginSynchronizedUpdate;
use crossterm::terminal::EndSynchronizedUpdate;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use viy_term::terminal::Terminal;
use viy_term::textarea::TextArea;
use viy_term::wrap::wrap_line;

/// At most 30 frames a second (ui-plan 11).
const FRAME: Duration = Duration::from_millis(33);
/// One word of the fake answer every this often.
const WORD: Duration = Duration::from_millis(25);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MODES: [&str; 2] = ["normal", "auto-yes"];
/// Rows of the answer in progress shown in the live region.
const ANSWER_ROWS: usize = 8;
/// Rows one wheel notch scrolls.
const WHEEL_ROWS: usize = 3;

const ANSWERS: [&str; 2] = [
    "The tunnel guard runs before every /health poll. When the forward is dead it \
     restarts ssh with the same arguments, waits for the port to answer, and only \
     then lets the poll go through, so a dropped connection costs one retry instead \
     of a failed turn.\n\n\
     Wide text wraps by cells, not characters: 日本語のテキストは二列ずつ使います, \
     and emoji 🦀🦀🦀 too. A long path without spaces: \
     /workspace/rpctl-cache/vllm/torch_compile_cache/0123456789abcdef0123456789abcdef/rank_0_0/backbone\n\n\
     That is the whole mechanism; the rest is logging.",
    "Short answer: yes.\n\n\
     - one list item that is long enough to wrap at sixty columns when the window is narrow\n\
     - another item\n\n\
     Paragraphs move up into the transcript as soon as they are finished, and the \
     part still being written stays in the live region below.",
];

fn main() -> io::Result<()> {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        hook(info);
    }));

    terminal::enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let result = run();
    restore()?;
    if let Ok(lines) = &result {
        println!("ui spike: {lines} transcript lines (viy would keep them in the session log)");
    }
    result.map(|_| ())
}

/// Gives the shell back its screen, and with it its history, untouched.
fn restore() -> io::Result<()> {
    execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    )?;
    terminal::disable_raw_mode()
}

/// Runs the chat and returns how many transcript lines it ended with.
fn run() -> io::Result<usize> {
    let mut app = App::new(Terminal::new(CrosstermBackend::new(io::stdout()))?);
    app.commit(vec![
        Line::from(
            "viy ui spike · Enter sends · Ctrl-J newline · Shift+Tab mode · Ctrl-C stops/exits · Ctrl-L repaints"
                .dim(),
        ),
        Line::from(
            "PageUp/PageDown or the wheel scroll, End follows · Shift+drag selects".dim(),
        ),
        Line::from("commands: /rewind  /flood N  /redraw  /panic  /quit".dim()),
    ]);
    while !app.quit {
        let timeout = FRAME.saturating_sub(app.last_frame.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => app.key(key)?,
                Event::Paste(text) => app.input.insert_str(&text),
                Event::Resize(width, height) => app.term.resize(Size::new(width, height))?,
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => app.scroll_up(WHEEL_ROWS),
                    MouseEventKind::ScrollDown => app.scroll_down(WHEEL_ROWS),
                    _ => continue,
                },
                _ => {}
            }
            app.dirty = true;
        }
        app.step();
        if app.last_frame.elapsed() >= FRAME {
            app.draw()?;
        }
    }
    app.finish_stream(true);
    Ok(app.transcript.len())
}

struct Stream {
    answer: &'static str,
    /// Bytes of `answer` released so far.
    sent: usize,
    /// The released text not yet committed to the scrollback.
    live: String,
    started: Instant,
    next_word: Instant,
}

struct App {
    term: Terminal<CrosstermBackend<Stdout>>,
    /// Everything committed; viy draws it, the terminal keeps no copy.
    transcript: Vec<Line<'static>>,
    /// Rows scrolled back from the end of the transcript; 0 follows it.
    scroll_back: usize,
    /// Transcript rows at the last frame, to hold a scrolled-back view still
    /// while rows arrive.
    last_rows: usize,
    /// Transcript rows the last frame showed: a page.
    page: usize,
    /// Where each user turn starts in `transcript`, and its text.
    turns: Vec<(usize, String)>,
    input: TextArea,
    queued: Vec<String>,
    mode: usize,
    stream: Option<Stream>,
    answers_given: usize,
    started: Instant,
    last_frame: Instant,
    dirty: bool,
    quit: bool,
}

impl App {
    fn new(term: Terminal<CrosstermBackend<Stdout>>) -> Self {
        let now = Instant::now();
        Self {
            term,
            transcript: Vec::new(),
            scroll_back: 0,
            last_rows: 0,
            page: 1,
            turns: Vec::new(),
            input: TextArea::default(),
            queued: Vec::new(),
            mode: 0,
            stream: None,
            answers_given: 0,
            started: now,
            last_frame: now - FRAME,
            dirty: true,
            quit: false,
        }
    }

    fn commit(&mut self, lines: Vec<Line<'static>>) {
        self.transcript.extend(lines);
        self.dirty = true;
    }

    fn key(&mut self, key: KeyEvent) -> io::Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.stream.is_some() {
                    self.finish_stream(true);
                } else if !self.input.is_empty() {
                    self.input.take();
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.quit = true,
            KeyCode::Char('l') if ctrl => self.repaint()?,
            KeyCode::Char('j') if ctrl => self.input.insert_char('\n'),
            KeyCode::Enter if self.input.text().ends_with('\\') => {
                self.input.backspace();
                self.input.insert_char('\n');
            }
            KeyCode::Enter => self.submit()?,
            KeyCode::BackTab => self.mode = (self.mode + 1) % MODES.len(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End if self.scroll_back > 0 => self.scroll_back = 0,
            KeyCode::End => self.input.move_end(),
            KeyCode::PageUp => self.scroll_up(self.page.saturating_sub(1).max(1)),
            KeyCode::PageDown => self.scroll_down(self.page.saturating_sub(1).max(1)),
            KeyCode::Char(c) if !ctrl => self.input.insert_char(c),
            _ => {}
        }
        Ok(())
    }

    fn submit(&mut self) -> io::Result<()> {
        let text = self.input.text().to_string();
        match text.split_whitespace().collect::<Vec<_>>().as_slice() {
            [] => {}
            ["/quit"] => {
                self.input.take();
                self.quit = true;
            }
            ["/panic"] => panic!("/panic: testing the terminal restore"),
            ["/redraw"] => {
                self.input.take();
                self.repaint()?;
            }
            ["/rewind"] if self.stream.is_none() => {
                self.input.take();
                if let Some((start, text)) = self.turns.pop() {
                    self.transcript.truncate(start);
                    self.input.set_text(&text);
                    self.repaint()?;
                }
            }
            ["/flood", n] => {
                self.input.take();
                let n: usize = n.parse().unwrap_or(100);
                self.commit(
                    (1..=n)
                        .map(|i| Line::from(format!("flood line {i} of {n}")))
                        .collect(),
                );
            }
            _ if self.stream.is_some() => self.queued.push(self.input.take()),
            _ => {
                let text = self.input.take();
                self.start_turn(text);
            }
        }
        Ok(())
    }

    fn start_turn(&mut self, text: String) {
        self.turns.push((self.transcript.len(), text.clone()));
        let mut lines = vec![Line::from("")];
        for (i, row) in text.split('\n').enumerate() {
            let prefix = if i == 0 { "you › " } else { "      " };
            lines.push(Line::from(vec![
                prefix.cyan().bold(),
                row.to_string().into(),
            ]));
        }
        self.commit(lines);
        let now = Instant::now();
        self.stream = Some(Stream {
            answer: ANSWERS[self.answers_given % ANSWERS.len()],
            sent: 0,
            live: String::new(),
            started: now,
            next_word: now,
        });
        self.answers_given += 1;
    }

    /// Releases the words that are due and commits finished paragraphs.
    fn step(&mut self) {
        let Some(stream) = &mut self.stream else {
            return;
        };
        let mut finished = Vec::new();
        while stream.next_word <= Instant::now() && stream.sent < stream.answer.len() {
            let rest = &stream.answer[stream.sent..];
            let word_end = rest
                .char_indices()
                .skip(1)
                .find(|&(_, c)| c == ' ' || c == '\n')
                .map_or(rest.len(), |(i, _)| i);
            stream.live.push_str(&rest[..word_end]);
            stream.sent += word_end;
            stream.next_word += WORD;
            while let Some(end) = stream.live.find("\n\n") {
                finished.push(stream.live[..end].to_string());
                stream.live.drain(..end + 2);
            }
            self.dirty = true;
        }
        let done = stream.sent >= stream.answer.len();
        for paragraph in finished {
            self.commit_paragraph(&paragraph);
        }
        if done {
            self.finish_stream(false);
        }
    }

    fn commit_paragraph(&mut self, text: &str) {
        let mut lines = vec![Line::from("")];
        lines.extend(text.split('\n').map(|row| Line::from(row.to_string())));
        self.commit(lines);
    }

    fn finish_stream(&mut self, interrupted: bool) {
        let Some(stream) = self.stream.take() else {
            return;
        };
        if !stream.live.is_empty() {
            self.commit_paragraph(&stream.live);
        }
        if interrupted {
            self.commit(vec![Line::from("■ interrupted".yellow())]);
        }
        if !self.queued.is_empty() {
            let text = self.queued.remove(0);
            self.start_turn(text);
        }
    }

    /// Repaints the screen from state (ui-plan 4); the terminal's
    /// scrollback is never touched.
    fn repaint(&mut self) -> io::Result<()> {
        self.scroll_back = 0;
        self.dirty = true;
        self.term.clear()
    }

    fn scroll_up(&mut self, rows: usize) {
        self.scroll_back = (self.scroll_back + rows).min(self.last_rows.saturating_sub(1));
    }

    fn scroll_down(&mut self, rows: usize) {
        self.scroll_back = self.scroll_back.saturating_sub(rows);
    }

    fn live_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(stream) = &self.stream {
            let frame = (stream.started.elapsed().as_millis() / 80) as usize % SPINNER.len();
            let words = stream.answer[..stream.sent].split_whitespace().count();
            lines.push(Line::from(vec![
                SPINNER[frame].magenta(),
                format!(
                    " writing · {} tok · {:.1} s",
                    words * 4 / 3,
                    stream.started.elapsed().as_secs_f32()
                )
                .dim(),
            ]));
            let rows: Vec<Line<'static>> = stream
                .live
                .split('\n')
                .flat_map(|row| wrap_line(&Line::from(row.to_string()), usize::from(width)))
                .collect();
            let hidden = rows.len().saturating_sub(ANSWER_ROWS);
            lines.extend(rows.into_iter().skip(hidden));
        }
        lines
    }

    fn footer(&self) -> Line<'static> {
        let mode = match self.mode {
            0 => Span::from(MODES[0]),
            _ => format!("⏵⏵ {}", MODES[self.mode]).yellow(),
        };
        let idle = 3600u64.saturating_sub(self.started.elapsed().as_secs());
        let mut spans = vec![
            mode,
            format!(
                " │ medium │ ctx 16% 41k/262k │ A100 1.79$/h │ idle {}m │ tunnel ok",
                idle / 60
            )
            .dim(),
        ];
        if !self.queued.is_empty() {
            spans.push(format!(" │ {} queued", self.queued.len()).cyan());
        }
        if self.scroll_back > 0 {
            spans.push(format!(" │ ↓ {} rows (End)", self.scroll_back).cyan());
        }
        Line::from(spans)
    }

    fn draw(&mut self) -> io::Result<()> {
        if !self.dirty && self.stream.is_none() {
            return Ok(());
        }
        let size = self.term.size();
        let width = size.width.max(4);
        let mut live = self.live_lines(width);
        let (input_rows, input_cursor) = self.input.layout(width - 2);
        // The live region may take all but two rows; the transcript gets
        // the rest.
        let max_live = usize::from(size.height.saturating_sub(2)).max(2);
        let fixed = input_rows.len() + 1;
        let live_room = max_live.saturating_sub(fixed);
        if live.len() > live_room {
            live.drain(..live.len() - live_room);
        }
        let input_top = live.len();
        let mut bottom = live;
        for (i, row) in input_rows.into_iter().enumerate() {
            let prefix = if i == 0 { "› ".cyan() } else { "  ".into() };
            bottom.push(Line::from(vec![prefix, row.into()]));
        }
        bottom.push(self.footer());
        let bottom_height = u16::try_from(bottom.len())
            .unwrap_or(u16::MAX)
            .min(size.height);

        // The transcript, wrapped at the current width every frame: a resize
        // reflows it with no trace in the terminal.
        let rows: Vec<Line<'static>> = self
            .transcript
            .iter()
            .flat_map(|line| wrap_line(line, usize::from(width)))
            .collect();
        if self.scroll_back > 0 {
            self.scroll_back += rows.len().saturating_sub(self.last_rows);
        }
        self.last_rows = rows.len();
        self.scroll_back = self.scroll_back.min(rows.len().saturating_sub(1));
        let top_height = size.height - bottom_height;
        self.page = usize::from(top_height).max(1);
        let end = rows.len() - self.scroll_back;
        let shown: Vec<Line<'static>> = rows[end.saturating_sub(self.page)..end].to_vec();
        // Short transcripts sit right above the live region, as in a shell.
        let gap = top_height.saturating_sub(u16::try_from(shown.len()).unwrap_or(u16::MAX));

        let top = Rect::new(0, gap, size.width, top_height - gap);
        let live_area = Rect::new(0, top_height, size.width, bottom_height);
        let cursor = Position::new(
            2 + input_cursor.x,
            top_height + u16::try_from(input_top).unwrap_or(0) + input_cursor.y,
        );
        queue!(self.term.backend_mut(), BeginSynchronizedUpdate)?;
        self.term.draw(|frame| {
            frame.render_widget(Paragraph::new(shown), top);
            frame.render_widget(Paragraph::new(bottom), live_area);
            frame.set_cursor_position(cursor);
        })?;
        queue!(self.term.backend_mut(), EndSynchronizedUpdate)?;
        self.term.backend_mut().flush()?;
        self.last_frame = Instant::now();
        self.dirty = false;
        Ok(())
    }
}
