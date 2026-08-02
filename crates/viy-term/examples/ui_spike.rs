//! WP 0.8's UI spike (docs/implementation-plan.md, flags F32 and F33): the
//! inline viewport with a live region over the terminal's own scrollback,
//! fed by a fake model stream, to try in GNOME Terminal, tmux, over ssh and
//! at 60 columns before phase 3 builds on it. Not part of viy.
//!
//! ```text
//! cargo build -p viy-term --example ui_spike
//! target/debug/examples/ui_spike
//! ```
//!
//! Enter sends (during a stream it queues), Ctrl-J or a trailing `\` adds a
//! newline, Shift+Tab cycles the mode, Ctrl-C stops the stream or exits,
//! Ctrl-L repaints. Commands: `/rewind` (repaint without the last exchange,
//! its message back in the box), `/flood N` (N lines into the scrollback),
//! `/redraw`, `/panic` (the terminal must come back usable), `/quit`.

use std::io;
use std::io::Stdout;
use std::io::Write;
use std::time::Duration;
use std::time::Instant;

use crossterm::cursor::MoveTo;
use crossterm::cursor::Show;
use crossterm::event;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::execute;
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal;
use crossterm::terminal::BeginSynchronizedUpdate;
use crossterm::terminal::EndSynchronizedUpdate;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use viy_term::history::ResetScrollRegion;
use viy_term::history::SetScrollRegion;
use viy_term::history::insert_history_lines;
use viy_term::history::wrap_line;
use viy_term::terminal::Terminal;
use viy_term::textarea::TextArea;

/// At most 30 frames a second (ui-plan 11).
const FRAME: Duration = Duration::from_millis(33);
/// One word of the fake answer every this often.
const WORD: Duration = Duration::from_millis(25);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MODES: [&str; 2] = ["normal", "auto-yes"];
/// Rows of the answer in progress shown in the live region.
const ANSWER_ROWS: usize = 8;

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
     Paragraphs move up into the scrollback as soon as they are finished, and the \
     part still being written stays in the live region below.",
];

fn main() -> io::Result<()> {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A panic can come mid-frame, inside a scroll region. `CSI r` homes
        // the cursor, so the message goes below everything instead.
        let height = terminal::size().map_or(1, |(_, height)| height);
        let _ = execute!(
            io::stdout(),
            ResetScrollRegion,
            MoveTo(0, height.saturating_sub(1)),
            Print("\r\n")
        );
        let _ = restore();
        hook(info);
    }));

    terminal::enable_raw_mode()?;
    execute!(io::stdout(), EnableBracketedPaste)?;
    let result = run();
    restore()?;
    result
}

fn restore() -> io::Result<()> {
    execute!(io::stdout(), DisableBracketedPaste, Show)?;
    terminal::disable_raw_mode()
}

fn run() -> io::Result<()> {
    let mut app = App::new(Terminal::new(CrosstermBackend::new(io::stdout()))?);
    app.commit(vec![
        Line::from("viy ui spike · Enter sends · Ctrl-J newline · Shift+Tab mode · Ctrl-C stops/exits · Ctrl-L repaints".dim()),
        Line::from("commands: /rewind  /flood N  /redraw  /quit".dim()),
    ]);
    while !app.quit {
        let timeout = FRAME.saturating_sub(app.last_frame.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => app.key(key)?,
                Event::Paste(text) => app.input.insert_str(&text),
                Event::Resize(width, height) => app.resize(Size::new(width, height))?,
                _ => {}
            }
            app.dirty = true;
        }
        app.step();
        if app.last_frame.elapsed() >= FRAME {
            app.draw()?;
        }
    }
    app.leave()
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
    /// Everything committed, for repaints.
    transcript: Vec<Line<'static>>,
    /// Committed but not yet written above the viewport.
    pending: Vec<Line<'static>>,
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
            pending: Vec::new(),
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
        self.transcript.extend(lines.iter().cloned());
        self.pending.extend(lines);
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
            KeyCode::End => self.input.move_end(),
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

    /// Clears the screen and the scrollback and reprints the transcript
    /// from state (ui-plan 4).
    fn repaint(&mut self) -> io::Result<()> {
        self.term.clear_scrollback_and_screen()?;
        let width = self.term.screen_size().width;
        self.term.set_viewport_area(Rect::new(0, 0, width, 0));
        self.pending = self.transcript.clone();
        self.dirty = true;
        Ok(())
    }

    /// Keeps the viewport where the cursor went, as Codex does: terminals
    /// that re-wrap on resize move the cursor's row.
    fn resize(&mut self, size: Size) -> io::Result<()> {
        let mut area = self.term.viewport_area();
        if let Ok(cursor) = self.term.query_cursor_position() {
            let moved = i32::from(cursor.y) - i32::from(self.term.cursor_position().y);
            area.y = u16::try_from((i32::from(area.y) + moved).max(0)).unwrap_or(0);
        }
        area.width = size.width;
        area.y = area.y.min(size.height.saturating_sub(area.height));
        self.term.set_screen_size(size);
        self.term.set_viewport_area(area);
        self.term.clear_from(Position::new(0, area.y))
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
        Line::from(spans)
    }

    fn draw(&mut self) -> io::Result<()> {
        if !self.dirty && self.stream.is_none() {
            return Ok(());
        }
        let size = self.term.screen_size();
        let width = size.width.max(4);
        let mut live = self.live_lines(width);
        let (input_rows, input_cursor) = self.input.layout(width - 2);
        // Keep two rows above the viewport for the scrollback region.
        let max_height = usize::from(size.height.saturating_sub(2)).max(2);
        let fixed = input_rows.len() + 1;
        let live_room = max_height.saturating_sub(fixed);
        if live.len() > live_room {
            live.drain(..live.len() - live_room);
        }
        let mut lines = live;
        let input_top = lines.len();
        for (i, row) in input_rows.into_iter().enumerate() {
            let prefix = if i == 0 { "› ".cyan() } else { "  ".into() };
            lines.push(Line::from(vec![prefix, row.into()]));
        }
        lines.push(self.footer());
        let height = u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .min(size.height);

        queue!(self.term.backend_mut(), BeginSynchronizedUpdate)?;
        self.place_viewport(height)?;
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            insert_history_lines(&mut self.term, &pending)?;
        }
        let area = self.term.viewport_area();
        let cursor = Position::new(
            2 + input_cursor.x,
            area.y + u16::try_from(input_top).unwrap_or(0) + input_cursor.y,
        );
        self.term.draw(|frame| {
            let area = frame.area();
            frame.render_widget(Paragraph::new(lines), area);
            frame.set_cursor_position(cursor);
        })?;
        queue!(self.term.backend_mut(), EndSynchronizedUpdate)?;
        self.term.backend_mut().flush()?;
        self.last_frame = Instant::now();
        self.dirty = false;
        Ok(())
    }

    /// Gives the viewport `height` rows. Growing past the bottom of the
    /// screen pushes the rows above it into the scrollback, as Codex's
    /// `grow_viewport` does: newlines at the bottom of a region over them.
    fn place_viewport(&mut self, height: u16) -> io::Result<()> {
        let size = self.term.screen_size();
        let old = self.term.viewport_area();
        let mut area = Rect::new(0, old.y, size.width, height);
        if area.bottom() > size.height {
            let scroll_by = area.bottom() - size.height;
            if old.y <= 1 {
                // No region above to scroll: the whole screen scrolls, so
                // the old viewport must not go into the scrollback with it.
                self.term.clear_from(Position::new(0, old.y))?;
            }
            let cursor = self.term.cursor_position();
            let writer = self.term.backend_mut();
            if old.y > 1 {
                queue!(writer, SetScrollRegion(1..old.y), MoveTo(0, old.y - 1))?;
                for _ in 0..scroll_by {
                    queue!(writer, Print("\r\n"))?;
                }
                queue!(writer, ResetScrollRegion)?;
            } else {
                queue!(writer, MoveTo(0, size.height - 1))?;
                for _ in 0..scroll_by {
                    queue!(writer, Print("\r\n"))?;
                }
            }
            queue!(writer, MoveTo(cursor.x, cursor.y))?;
            area.y = size.height - height;
        }
        if area != old {
            self.term.set_viewport_area(area);
            self.term.clear_from(Position::new(0, area.y))?;
        }
        Ok(())
    }

    /// Leaves the cursor below the live region for the shell.
    fn leave(&mut self) -> io::Result<()> {
        self.finish_stream(true);
        self.dirty = true;
        self.draw()?;
        let area = self.term.viewport_area();
        let writer = self.term.backend_mut();
        queue!(
            writer,
            MoveTo(0, area.bottom().saturating_sub(1)),
            Print("\r\n")
        )?;
        writer.flush()
    }
}
