//! Cutting styled lines into screen rows.

use std::borrow::Cow;

use ratatui::text::Line;
use ratatui::text::Span;

use crate::sanitize::Glyph;
use crate::sanitize::TAB;
use crate::sanitize::glyphs;

/// Places glyphs in rows of a width: a glyph that does not fit starts the
/// next row (unless its row is empty), and a tab becomes spaces to the
/// next stop, never split across rows.
pub struct Rows {
    width: usize,
    /// Cells used in the current row.
    used: usize,
}

/// Where a glyph went.
pub struct Placed<'a> {
    /// It starts a new row.
    pub wrap: bool,
    /// Its first cell in its row.
    pub column: usize,
    /// What is drawn.
    pub text: Cow<'a, str>,
}

impl Rows {
    pub fn new(width: usize) -> Self {
        Self {
            width: width.max(1),
            used: 0,
        }
    }

    /// Cells used in the current row.
    pub fn used(&self) -> usize {
        self.used
    }

    pub fn place<'a>(&mut self, glyph: &'a Glyph) -> Placed<'a> {
        let tab = glyph.text == "\t";
        let width = if tab { 1 } else { glyph.width };
        let wrap = self.used > 0 && self.used + width > self.width;
        if wrap {
            self.used = 0;
        }
        let column = self.used;
        let text = if tab {
            let spaces = (TAB - column % TAB).min(self.width - column);
            self.used += spaces;
            Cow::Owned(" ".repeat(spaces))
        } else {
            self.used += glyph.width;
            Cow::Borrowed(glyph.text.as_str())
        };
        Placed { wrap, column, text }
    }
}

/// Cuts `line` into rows at most `width` cells wide, breaking between
/// graphemes, with every char that could act on the terminal made visible
/// (ui-plan 7) and tabs as spaces to the next stop. The line's style is
/// kept on every row.
pub fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    wrap_rows(line, width)
        .into_iter()
        .map(|(row, _)| row)
        .collect()
}

/// [`wrap_line`], with how many of the line's chars each row holds: a char
/// drawn as `<U+202E>` or a tab's spaces counts as one, in the row where it
/// starts.
pub fn wrap_rows(line: &Line<'_>, width: usize) -> Vec<(Line<'static>, usize)> {
    let mut rows = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut chars = 0;
    let mut prev = None;
    let mut place = Rows::new(width);
    for span in &line.spans {
        let mut text = String::new();
        for glyph in glyphs(&span.content, prev) {
            let placed = place.place(&glyph);
            if placed.wrap {
                spans.push(Span::styled(std::mem::take(&mut text), span.style));
                let row = Line::from(std::mem::take(&mut spans)).style(line.style);
                rows.push((row, std::mem::take(&mut chars)));
            }
            chars += glyph.chars;
            text.push_str(&placed.text);
            prev = placed.text.chars().next_back().or(prev);
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    rows.push((Line::from(spans).style(line.style), chars));
    rows
}

#[cfg(test)]
#[path = "wrap_tests.rs"]
mod tests;
