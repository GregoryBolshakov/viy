//! A ratatui backend that writes into a `vt100` terminal emulator, so tests
//! read the screen the bytes produce (Codex's `tui/src/test_backend.rs`). It
//! never asks the real terminal for its size or the cursor.

use std::io;
use std::io::Write;

use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::backend::CrosstermBackend;
use ratatui::backend::WindowSize;
use ratatui::buffer::Cell;
use ratatui::layout::Position;
use ratatui::layout::Size;

pub struct VT100Backend {
    inner: CrosstermBackend<Screen>,
}

/// The emulator, and every byte it was sent.
pub struct Screen {
    pub parser: vt100::Parser,
    pub written: Vec<u8>,
}

impl Write for Screen {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written.extend_from_slice(buf);
        self.parser.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.parser.flush()
    }
}

impl VT100Backend {
    pub fn new(width: u16, height: u16) -> Self {
        crossterm::style::force_color_output(true);
        Self {
            inner: CrosstermBackend::new(Screen {
                parser: vt100::Parser::new(height, width, 0),
                written: Vec::new(),
            }),
        }
    }

    pub fn vt100(&self) -> &vt100::Parser {
        &self.inner.writer().parser
    }

    /// How many bytes were written so far.
    pub fn bytes_written(&self) -> usize {
        self.inner.writer().written.len()
    }

    /// Every byte written so far.
    pub fn written(&self) -> &[u8] {
        &self.inner.writer().written
    }

    /// Resizes the emulated screen, as a window resize does.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.inner
            .writer_mut()
            .parser
            .screen_mut()
            .set_size(height, width);
    }

    /// The screen's rows without trailing blanks, `▮` at the cursor when it
    /// is shown.
    pub fn screen_rows(&self) -> Vec<String> {
        let screen = self.vt100().screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        (0..rows)
            .map(|row| {
                let mut text = String::new();
                for col in 0..cols {
                    if row == cursor_row && col == cursor_col && !screen.hide_cursor() {
                        text.push('▮');
                        continue;
                    }
                    match screen.cell(row, col) {
                        Some(cell) if cell.is_wide_continuation() => {}
                        Some(cell) if cell.has_contents() => text.push_str(cell.contents()),
                        _ => text.push(' '),
                    }
                }
                text.trim_end_matches(' ').to_string()
            })
            .collect()
    }
}

impl Write for VT100Backend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.writer_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.writer_mut().flush()
    }
}

impl Backend for VT100Backend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        let (row, col) = self.vt100().screen().cursor_position();
        Ok(Position::new(col, row))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        let (rows, cols) = self.vt100().screen().size();
        Ok(Size::new(cols, rows))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size()?,
            pixels: Size::new(640, 480),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.writer_mut().flush()
    }
}
