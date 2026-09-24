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
use viy_testkit::screen::Feed;
use viy_testkit::screen::Recorder;

pub struct VT100Backend {
    inner: CrosstermBackend<Screen>,
}

/// The emulator, every byte it was sent, and how often it was flushed.
pub struct Screen {
    pub parser: vt100::Parser<Recorder>,
    feed: Feed,
    pub written: Vec<u8>,
    pub flushes: usize,
}

impl Write for Screen {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written.extend_from_slice(buf);
        self.feed.process(&mut self.parser, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

impl VT100Backend {
    pub fn new(width: u16, height: u16) -> Self {
        crossterm::style::force_color_output(true);
        Self {
            inner: CrosstermBackend::new(Screen {
                parser: viy_testkit::screen::parser(height, width),
                feed: Feed::default(),
                written: Vec::new(),
                flushes: 0,
            }),
        }
    }

    pub fn vt100(&self) -> &vt100::Parser<Recorder> {
        &self.inner.writer().parser
    }

    /// How many bytes were written so far.
    pub fn bytes_written(&self) -> usize {
        self.inner.writer().written.len()
    }

    /// How many times the output was flushed: a real terminal gets a write
    /// for each.
    pub fn flushes(&self) -> usize {
        self.inner.writer().flushes
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
        viy_testkit::screen::rows(self.vt100().screen())
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
