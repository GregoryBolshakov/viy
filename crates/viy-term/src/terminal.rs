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

//! The screen terminal: ratatui's `Terminal` cut down to one viewport that
//! covers the alternate screen, as Codex's owned-transcript layout uses it
//! (docs/ui-plan.md 2). It writes only cells: nothing here clears the
//! scrollback, so the shell's history outlives viy (kb d038).

use std::io;
use std::io::Write;

use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::widgets::Widget;

/// One frame, handed to [`Terminal::draw`]'s callback.
pub struct Frame<'a> {
    area: Rect,
    buffer: &'a mut Buffer,
    cursor: Option<Position>,
}

impl Frame<'_> {
    /// The whole screen.
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
    pub fn new(backend: B) -> io::Result<Self> {
        let size = backend.size()?;
        let area = Rect::from((Position::ORIGIN, size));
        let mut terminal = Self {
            backend,
            buffers: [Buffer::empty(area), Buffer::empty(area)],
            current: 0,
            hidden_cursor: false,
        };
        terminal.clear()?;
        Ok(terminal)
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn size(&self) -> Size {
        self.buffers[self.current].area.as_size()
    }

    /// Takes the new screen size and repaints everything on the next draw.
    pub fn resize(&mut self, size: Size) -> io::Result<()> {
        let area = Rect::from((Position::ORIGIN, size));
        for buffer in &mut self.buffers {
            buffer.resize(area);
        }
        self.clear()
    }

    /// Clears the visible screen (`CSI 2J`, never `CSI 3J`) and writes every
    /// cell on the next draw.
    pub fn clear(&mut self) -> io::Result<()> {
        self.backend.clear()?;
        let previous = &mut self.buffers[1 - self.current];
        previous.reset();
        for cell in &mut previous.content {
            cell.set_diff_option(CellDiffOption::AlwaysUpdate);
        }
        Ok(())
    }

    /// Renders one frame and writes the cells that changed since the last.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        let area = self.buffers[self.current].area;
        let mut frame = Frame {
            area,
            buffer: &mut self.buffers[self.current],
            cursor: None,
        };
        render(&mut frame);
        let cursor = frame.cursor;

        let updates = self.buffers[1 - self.current].diff(&self.buffers[self.current]);
        // An empty diff writes nothing: the backend's draw resets the
        // colours even when it has no cells.
        if !updates.is_empty() {
            if !self.hidden_cursor {
                self.backend.hide_cursor()?;
                self.hidden_cursor = true;
            }
            self.backend.draw(updates.into_iter())?;
        }

        match cursor {
            Some(position) => {
                self.backend.set_cursor_position(position)?;
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
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
