//! The transcript above the live region: the cells of [`crate::cells`],
//! which viy lays out, scrolls and draws itself (ui-plan 2 and 3; Codex's
//! `transcript_view.rs`). It follows the bottom until scrolled back, and
//! then holds still while rows arrive. The reading position is an anchor in
//! a cell (a row's key), not a screen row, so a resize lays every cell out
//! again at the new width and the same text stays at the top (Codex's
//! anchors).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

pub use crate::cells::Cell;

/// Where the view is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Position {
    /// The last rows, and the rows that arrive.
    #[default]
    Latest,
    /// Scrolled back: this text at the top.
    Reading(Anchor),
}

/// A place in the text: a cell and the key of a row in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Anchor {
    cell: usize,
    key: (usize, usize),
}

/// A cell laid out at one width.
#[derive(Debug, Default)]
struct Layout {
    rows: Vec<Line<'static>>,
    /// Each row's key, growing down the cell.
    starts: Vec<(usize, usize)>,
}

impl Layout {
    fn new(cell: &Cell, width: usize, expanded: bool) -> Self {
        let (rows, starts) = cell
            .rows(width, expanded)
            .into_iter()
            .map(|row| (row.line, row.key))
            .unzip();
        Self { rows, starts }
    }

    /// The row holding `key`.
    fn row_of(&self, key: (usize, usize)) -> usize {
        self.starts
            .partition_point(|&start| start <= key)
            .saturating_sub(1)
    }
}

#[derive(Debug, Default)]
pub struct TranscriptView {
    position: Position,
    /// The width the layouts are for.
    width: u16,
    /// One per cell, in order.
    layouts: Vec<Layout>,
    /// Rows the last render showed: a page.
    page: usize,
    /// Tool output and reasoning drawn whole (Ctrl-T).
    expanded: bool,
    /// A cell to bring into view on the next render.
    reveal: Option<usize>,
}

impl TranscriptView {
    /// Cell `index` and the ones after it changed: they are laid out again
    /// on the next render.
    pub fn changed(&mut self, index: usize) {
        self.layouts.truncate(index);
    }

    /// Draws tool output and reasoning whole, or folded again.
    pub fn set_expanded(&mut self, expanded: bool) {
        if expanded != self.expanded {
            self.expanded = expanded;
            self.layouts.clear();
        }
    }

    /// Forgets the layouts, for a transcript whose cells were replaced
    /// (rewind, `/new`, resume), and follows its end.
    pub fn reset(&mut self) {
        self.layouts.clear();
        self.position = Position::Latest;
    }

    pub fn is_following(&self) -> bool {
        self.position == Position::Latest
    }

    /// Rows the last render showed.
    pub fn page(&self) -> usize {
        self.page
    }

    /// Rows below the view, for the footer's "↓ N rows (End)".
    pub fn rows_below(&self) -> usize {
        self.total().saturating_sub(self.top() + self.page)
    }

    /// Scrolls back `rows`; nothing when all of it fits.
    pub fn scroll_up(&mut self, rows: usize) {
        if self.total() > self.page {
            self.position = Position::Reading(self.anchor_at(self.top().saturating_sub(rows)));
        }
    }

    /// Scrolls forward `rows`; reaching the end follows it again.
    pub fn scroll_down(&mut self, rows: usize) {
        let top = self.top() + rows;
        self.position = if top + self.page >= self.total() {
            Position::Latest
        } else {
            Position::Reading(self.anchor_at(top))
        };
    }

    /// The transcript's start.
    pub fn home(&mut self) {
        if self.total() > self.page {
            self.position = Position::Reading(self.anchor_at(0));
        }
    }

    /// Brings cell `index` into view on the next render, if it is not.
    pub fn reveal(&mut self, index: usize) {
        self.reveal = Some(index);
    }

    /// Follows the end again.
    pub fn end(&mut self) {
        self.position = Position::Latest;
    }

    /// Draws `cells` into `area`: the page at the reading position, or the
    /// last rows; a transcript shorter than the area sits at its bottom,
    /// right above the live region, as a shell's output does, but one that
    /// opens with the centipede header sits at the top, the header being
    /// the top of the chat, until it fills the area and scrolls.
    pub fn render(&mut self, cells: &[Cell], area: Rect, buf: &mut Buffer) {
        self.prepare(cells, area.width, area.height);
        let top = self.top();
        let end = (top + self.page).min(self.total());
        let headed = matches!(cells.first(), Some(Cell::Header(_)));
        let gap = if headed { 0 } else { self.page - (end - top) };
        let y = (area.y..area.bottom()).skip(gap);
        for (y, row) in y.zip(self.rows(top, end)) {
            buf.set_line(area.x, y, row, area.width);
        }
    }

    /// Lays `cells` out for an area of `width` x `height`, so
    /// [`Self::rows_below`] counts what the next render shows; rendering
    /// does it too.
    pub fn prepare(&mut self, cells: &[Cell], width: u16, height: u16) {
        self.lay_out(cells, width);
        self.page = usize::from(height);
        if let Some(cell) = self.reveal.take()
            && cell < self.layouts.len()
        {
            let start: usize = self.layouts[..cell].iter().map(|l| l.rows.len()).sum();
            let end = start + self.layouts[cell].rows.len();
            let top = self.top();
            if start < top || end > top + self.page {
                // A third of a page above it, for what led to it.
                let row = start.saturating_sub(self.page / 3);
                self.position = Position::Reading(self.anchor_at(row));
            }
        }
        if let Position::Reading(_) = self.position
            && self.top() + self.page >= self.total()
        {
            self.position = Position::Latest;
        }
    }

    /// Rows `start..end` of the whole transcript.
    fn rows(&self, start: usize, end: usize) -> impl Iterator<Item = &Line<'static>> {
        self.layouts
            .iter()
            .flat_map(|layout| &layout.rows)
            .skip(start)
            .take(end - start)
    }

    fn lay_out(&mut self, cells: &[Cell], width: u16) {
        if width != self.width {
            self.width = width;
            self.layouts.clear();
        }
        self.layouts.truncate(cells.len());
        let width = usize::from(width.max(1));
        for cell in &cells[self.layouts.len()..] {
            self.layouts.push(Layout::new(cell, width, self.expanded));
        }
    }

    fn total(&self) -> usize {
        self.layouts.iter().map(|l| l.rows.len()).sum()
    }

    /// The first row shown.
    fn top(&self) -> usize {
        match self.position {
            Position::Latest => self.total().saturating_sub(self.page),
            Position::Reading(anchor) => self.row_of(anchor),
        }
    }

    fn row_of(&self, anchor: Anchor) -> usize {
        let before: usize = self.layouts[..anchor.cell.min(self.layouts.len())]
            .iter()
            .map(|l| l.rows.len())
            .sum();
        before
            + self
                .layouts
                .get(anchor.cell)
                .map_or(0, |l| l.row_of(anchor.key))
    }

    fn anchor_at(&self, mut row: usize) -> Anchor {
        for (cell, layout) in self.layouts.iter().enumerate() {
            if let Some(&key) = layout.starts.get(row) {
                return Anchor { cell, key };
            }
            row -= layout.rows.len();
        }
        Anchor {
            cell: self.layouts.len(),
            key: (0, 0),
        }
    }
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
