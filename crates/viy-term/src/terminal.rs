// Derived from `ratatui::Terminal` through Codex's
// `codex-rs/tui/src/custom_terminal.rs`; ratatui is licensed as follows:
//
// The MIT License (MIT)
// Copyright (c) 2016-2022 Florian Dehau
// Copyright (c) 2023-2025 The Ratatui Developers
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! The inline terminal: ratatui's `Terminal` cut down to an inline viewport
//! whose area the caller moves, as Codex does instead of using stock
//! `Viewport::Inline` (docs/ui-plan.md 2). Everything above the viewport is
//! the terminal's own scrollback, written by [`crate::history`].

use std::io;
use std::io::Write;

use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::widgets::Widget;

/// One frame of the viewport, handed to [`Terminal::draw`]'s callback.
pub struct Frame<'a> {
    area: Rect,
    buffer: &'a mut Buffer,
    cursor: Option<Position>,
}

impl Frame<'_> {
    /// The viewport, in screen coordinates.
    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buffer);
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }

    /// Shows the cursor at `position` after the frame; without a call the
    /// cursor is hidden.
    pub fn set_cursor_position(&mut self, position: impl Into<Position>) {
        self.cursor = Some(position.into());
    }
}

pub struct Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    backend: B,
    /// The current and the previous frame; a draw writes only their
    /// difference.
    buffers: [Buffer; 2],
    current: usize,
    hidden_cursor: bool,
    viewport_area: Rect,
    screen_size: Size,
    /// Where the cursor was left: the anchor for moving the viewport when
    /// the terminal is resized.
    cursor_pos: Position,
}

impl<B> Drop for Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    fn drop(&mut self) {
        if self.hidden_cursor {
            let _ = self.backend.show_cursor();
            let _ = Backend::flush(&mut self.backend);
        }
    }
}

impl<B> Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    /// A terminal whose empty viewport starts on the cursor's row. A
    /// terminal that does not answer the cursor query starts at the top.
    pub fn new(mut backend: B) -> io::Result<Self> {
        let screen_size = backend.size()?;
        let cursor_pos = backend.get_cursor_position().unwrap_or_default();
        Ok(Self::with_size_and_cursor(backend, screen_size, cursor_pos))
    }

    pub fn with_size_and_cursor(backend: B, screen_size: Size, cursor_pos: Position) -> Self {
        Self {
            backend,
            buffers: [Buffer::empty(Rect::ZERO), Buffer::empty(Rect::ZERO)],
            current: 0,
            hidden_cursor: false,
            viewport_area: Rect::new(0, cursor_pos.y, 0, 0),
            screen_size,
            cursor_pos,
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn viewport_area(&self) -> Rect {
        self.viewport_area
    }

    pub fn screen_size(&self) -> Size {
        self.screen_size
    }

    pub fn set_screen_size(&mut self, size: Size) {
        self.screen_size = size;
    }

    /// Where the cursor was last put.
    pub fn cursor_position(&self) -> Position {
        self.cursor_pos
    }

    /// Asks the terminal where the cursor is (`CSI 6n`).
    pub fn query_cursor_position(&mut self) -> io::Result<Position> {
        self.backend.get_cursor_position()
    }

    /// Moves or resizes the viewport. The screen is not touched: the caller
    /// clears what the old area leaves behind.
    pub fn set_viewport_area(&mut self, area: Rect) {
        for buffer in &mut self.buffers {
            buffer.resize(area);
        }
        self.viewport_area = area;
    }

    /// Renders one frame and writes the cells that changed since the last.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        let mut frame = Frame {
            area: self.viewport_area,
            buffer: &mut self.buffers[self.current],
            cursor: None,
        };
        render(&mut frame);
        let cursor = frame.cursor;

        let updates = self.buffers[1 - self.current].diff(&self.buffers[self.current]);
        // An empty diff writes nothing: the backend's draw resets the
        // colours even when it has no cells.
        if let Some(&(x, y, _)) = updates.last() {
            if !self.hidden_cursor {
                self.backend.hide_cursor()?;
                self.hidden_cursor = true;
            }
            self.cursor_pos = Position { x, y };
            self.backend.draw(updates.into_iter())?;
        }

        match cursor {
            Some(position) => {
                self.backend.set_cursor_position(position)?;
                self.cursor_pos = position;
                self.backend.show_cursor()?;
                self.hidden_cursor = false;
            }
            None if !self.hidden_cursor => {
                self.backend.hide_cursor()?;
                self.hidden_cursor = true;
            }
            None => {}
        }

        self.current = 1 - self.current;
        self.buffers[self.current].reset();
        Backend::flush(&mut self.backend)
    }

    /// Clears from `position` to the end of the screen and repaints the
    /// whole viewport on the next draw.
    pub fn clear_from(&mut self, position: Position) -> io::Result<()> {
        self.backend.set_cursor_position(position)?;
        self.backend.clear_region(ClearType::AfterCursor)?;
        self.cursor_pos = position;
        self.invalidate_viewport();
        Ok(())
    }

    /// Makes the next draw write every cell, for when something other than
    /// [`Terminal::draw`] changed the screen under the viewport.
    pub fn invalidate_viewport(&mut self) {
        let previous = &mut self.buffers[1 - self.current];
        previous.reset();
        for cell in &mut previous.content {
            cell.set_diff_option(CellDiffOption::AlwaysUpdate);
        }
    }

    /// Clears the screen and the scrollback (`CSI 2J`, `CSI 3J`) and puts
    /// the cursor home, in one write as `clear && printf '\e[3J'` does
    /// (Codex's `clear_scrollback_and_visible_screen_ansi`).
    pub fn clear_scrollback_and_screen(&mut self) -> io::Result<()> {
        write!(self.backend, "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H")?;
        Write::flush(&mut self.backend)?;
        self.cursor_pos = Position::ORIGIN;
        self.invalidate_viewport();
        Ok(())
    }
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
