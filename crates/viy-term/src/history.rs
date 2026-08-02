//! Writes finished transcript lines into the terminal's scrollback above the
//! inline viewport, after Codex's `codex-rs/tui/src/insert_history.rs`: a
//! scroll region (DECSTBM) limited to the rows above the viewport, so new
//! lines push old ones up and off the screen into the scrollback while the
//! viewport stays put. This is an escape-sequence operation, not a ratatui
//! render.

use std::fmt;
use std::io;
use std::io::Write;

use crossterm::Command;
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Print;
use crossterm::style::PrintStyledContent;
use crossterm::style::StyledContent;
use crossterm::terminal::Clear;
use crossterm::terminal::ClearType;
use ratatui::backend::Backend;
use ratatui::backend::IntoCrossterm;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use crate::terminal::Terminal;

/// Inserts `lines` above the viewport, each cut into rows of the screen's
/// width. If the viewport is not at the bottom of the screen it moves down
/// first, as far as the lines need and the screen allows, and the first rows
/// go into the space it left. Rows that do not fit there scroll the region
/// above the viewport, which needs at least two rows above it.
pub fn insert_history_lines<B>(terminal: &mut Terminal<B>, lines: &[Line<'_>]) -> io::Result<()>
where
    B: Backend<Error = io::Error> + Write,
{
    let screen = terminal.screen_size();
    let mut area = terminal.viewport_area();
    let rows: Vec<Line<'static>> = lines
        .iter()
        .flat_map(|line| wrap_line(line, usize::from(screen.width.max(1))))
        .collect();
    let row_count = u16::try_from(rows.len()).unwrap_or(u16::MAX);
    let cursor = terminal.cursor_position();
    let writer = terminal.backend_mut();

    // Room below the viewport: scroll the region from the viewport down
    // with reverse index (`ESC M`) at its top.
    let old_top = area.top();
    let scroll_by = if area.bottom() < screen.height {
        row_count.min(screen.height - area.bottom())
    } else {
        0
    };
    if scroll_by > 0 {
        queue!(
            writer,
            SetScrollRegion(old_top + 1..screen.height),
            MoveTo(0, old_top)
        )?;
        for _ in 0..scroll_by {
            queue!(writer, Print("\x1bM"))?;
        }
        queue!(writer, ResetScrollRegion)?;
        area.y += scroll_by;
    }
    let (into_gap, scrolled) = rows.split_at(usize::from(scroll_by));
    for (y, row) in (old_top..).zip(into_gap) {
        queue!(writer, MoveTo(0, y))?;
        write_row(writer, row)?;
    }

    // Each further row starts with a newline at the bottom of the region
    // above the viewport, which scrolls that region and nothing else.
    if !scrolled.is_empty() {
        queue!(
            writer,
            SetScrollRegion(1..area.top()),
            MoveTo(0, area.top().saturating_sub(1))
        )?;
        for row in scrolled {
            queue!(writer, Print("\r\n"))?;
            write_row(writer, row)?;
        }
        queue!(writer, ResetScrollRegion)?;
    }
    queue!(writer, MoveTo(cursor.x, cursor.y))?;
    Write::flush(writer)?;

    if area != terminal.viewport_area() {
        terminal.set_viewport_area(area);
        terminal.invalidate_viewport();
    }
    Ok(())
}

fn write_row(writer: &mut impl Write, row: &Line<'_>) -> io::Result<()> {
    queue!(writer, Clear(ClearType::UntilNewLine))?;
    for span in &row.spans {
        let style = span.style.patch(row.style).into_crossterm();
        queue!(
            writer,
            PrintStyledContent(StyledContent::new(style, span.content.as_ref()))
        )?;
    }
    Ok(())
}

/// Cuts `line` into rows at most `width` cells wide, breaking between
/// characters, with controls made visible (ui-plan 7: only the renderer
/// writes escape sequences). The line's style is kept on every row.
pub fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in &line.spans {
        let mut text = String::new();
        for c in span.content.chars().map(visible) {
            let w = c.width().unwrap_or(0);
            if used + w > width && used > 0 {
                spans.push(Span::styled(std::mem::take(&mut text), span.style));
                rows.push(Line::from(std::mem::take(&mut spans)).style(line.style));
                used = 0;
            }
            text.push(c);
            used += w;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    rows.push(Line::from(spans).style(line.style));
    rows
}

/// C0 controls and DEL as their Control Pictures (`␛`), C1 controls as
/// U+FFFD.
fn visible(c: char) -> char {
    match c {
        '\0'..='\x1f' => char::from_u32(0x2400 + u32::from(c)).unwrap_or('\u{fffd}'),
        '\x7f' => '\u{2421}',
        '\u{80}'..='\u{9f}' => '\u{fffd}',
        _ => c,
    }
}

/// `CSI top ; bottom r`, 1-based rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetScrollRegion(pub std::ops::Range<u16>);

impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }
}

/// `CSI r`: the scroll region back to the whole screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetScrollRegion;

impl Command for ResetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
