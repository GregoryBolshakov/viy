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

use crossterm::cursor::Hide;
use crossterm::cursor::MoveTo;
use crossterm::cursor::Show;
use crossterm::queue;
use crossterm::terminal::BeginSynchronizedUpdate;
use crossterm::terminal::EndSynchronizedUpdate;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::widgets::Widget;

use crate::sanitize::unsafe_in_cell;

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
    /// Where the cursor is shown, `None` when hidden. It starts shown at a
    /// position no frame asks for, so the first frame places or hides it.
    cursor_shown: Option<Position>,
}

impl<B> Drop for Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    fn drop(&mut self) {
        if self.cursor_shown.is_none() {
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
            cursor_shown: Some(Position::new(u16::MAX, u16::MAX)),
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

    /// The frame drawn last, and where it put the cursor.
    #[cfg(test)]
    pub(crate) fn last_frame(&self) -> (&Buffer, Option<Position>) {
        (&self.buffers[1 - self.current], self.cursor_shown)
    }

    pub fn size(&self) -> Size {
        self.buffers[self.current].area.as_size()
    }

    /// Takes the new screen size; the next draw rewrites every cell. Nothing
    /// is written until then, so no blank screen shows between the resize
    /// and that frame: clearing first made a resize flicker in GNOME
    /// Terminal, a blank screen for each resize event.
    pub fn resize(&mut self, size: Size) {
        let area = Rect::from((Position::ORIGIN, size));
        for buffer in &mut self.buffers {
            buffer.resize(area);
        }
        self.invalidate();
    }

    /// Makes the next draw rewrite every cell, spaces included, so whatever
    /// the terminal shows now (a reflow, a stray write) is overwritten in one
    /// frame without clearing first (Codex's `invalidate_viewport`).
    pub fn invalidate(&mut self) {
        let previous = &mut self.buffers[1 - self.current];
        previous.reset();
        for cell in &mut previous.content {
            cell.set_diff_option(CellDiffOption::AlwaysUpdate);
        }
    }

    /// Clears the visible screen (`CSI 2J`, never `CSI 3J`) and writes every
    /// cell on the next draw. The screen is blank until then; a repaint
    /// while running uses [`Terminal::invalidate`].
    pub fn clear(&mut self) -> io::Result<()> {
        self.backend.clear()?;
        self.invalidate();
        Ok(())
    }

    /// Renders one frame and writes the cells that changed since the last,
    /// inside a synchronized update (`CSI ?2026h` ... `CSI ?2026l`). The
    /// frame is queued whole and flushed once: the backend's own cursor
    /// calls flush each, which cut a frame into several writes that a
    /// terminal without synchronized output (GNOME Terminal's VTE) may show
    /// half drawn. Writes nothing when neither a cell nor the cursor
    /// changed.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        let area = self.buffers[self.current].area;
        let mut frame = Frame {
            area,
            buffer: &mut self.buffers[self.current],
            cursor: None,
        };
        render(&mut frame);
        let cursor = frame.cursor;
        scrub(&mut self.buffers[self.current]);

        let updates = self.buffers[1 - self.current].diff(&self.buffers[self.current]);
        // An empty diff writes nothing: the backend's draw resets the
        // colours even when it has no cells.
        // Writing cells moves the terminal's cursor: hidden while they are
        // written, placed again after.
        let cells = !updates.is_empty();
        let hide = self.cursor_shown.is_some() && (cells || cursor.is_none());
        let place = cursor.filter(|&p| cells || hide || Some(p) != self.cursor_shown);
        if cells || hide || place.is_some() {
            queue!(self.backend, BeginSynchronizedUpdate)?;
            if hide {
                queue!(self.backend, Hide)?;
                self.cursor_shown = None;
            }
            if cells {
                self.backend.draw(updates.into_iter())?;
            }
            if let Some(position) = place {
                queue!(self.backend, MoveTo(position.x, position.y), Show)?;
                self.cursor_shown = Some(position);
            } else if cursor.is_none() {
                // A hidden cursor rests at the top left, not where the last
                // cell was written: a terminal made shorter keeps its cursor
                // in view by scrolling the screen up, and at the top it
                // has nothing to scroll for.
                queue!(self.backend, MoveTo(0, 0))?;
            }
            queue!(self.backend, EndSynchronizedUpdate)?;
        }

        self.current = 1 - self.current;
        self.buffers[self.current].reset();
        Backend::flush(&mut self.backend)
    }
}

/// The last check before cells reach the terminal (ui-plan 7): a cell whose
/// symbol holds a control or a bidi control is written as U+FFFD. Widgets
/// sanitize their text, so this only catches one that did not. It runs on
/// the whole frame before the diff, which measures each symbol's width to
/// skip the cells a wide one covers: scrubbing only the cells the diff
/// writes would leave its neighbours unwritten.
fn scrub(buffer: &mut Buffer) {
    for cell in &mut buffer.content {
        if unsafe_in_cell(cell.symbol()) {
            cell.set_symbol("\u{fffd}");
        }
    }
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
