//! Cutting text into screen rows: styled lines cut between graphemes
//! ([`wrap_line`], [`wrap_rows`]: code and tool output), plain text
//! word-wrapped into rows that keep the byte ranges they show
//! ([`word_wrap`], the input box: Codex's `wrapping::wrap_ranges` over
//! viy's sanitized glyphs), and styled lines word-wrapped the same way
//! ([`wrap_spans`], prose in the transcript and the answer).

use std::borrow::Cow;
use std::ops::Range;

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

/// One unit of a text as drawn in a row: a grapheme, a char shown as
/// `<U+202E>`, or a tab as spaces to the next stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCell {
    /// Its bytes in the text.
    pub range: Range<usize>,
    /// Its first column in the row.
    pub column: usize,
    pub width: usize,
    /// What is drawn.
    pub drawn: String,
}

/// One screen row of a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRow {
    /// The bytes it shows; a line's newline belongs to no row.
    pub range: Range<usize>,
    pub cells: Vec<TextCell>,
    /// The last row of its line: its end is the line's end, not the next
    /// row's start.
    pub ends_line: bool,
}

impl TextRow {
    /// Cells used, spaces hanging past the width included.
    pub fn width(&self) -> usize {
        self.cells.last().map_or(0, |c| c.column + c.width)
    }

    pub fn drawn(&self) -> String {
        self.cells.iter().map(|c| c.drawn.as_str()).collect()
    }

    /// The column of byte `pos` in this row: that of the first cell at or
    /// after it, or the column after the last.
    pub fn column_of(&self, pos: usize) -> usize {
        self.cells
            .iter()
            .find(|c| c.range.start >= pos)
            .map_or_else(|| self.width(), |c| c.column)
    }

    /// The byte offset shown at `column`: the start of the last cell
    /// starting at or before it; past the end, the row's end, or on a row
    /// that wraps, its last cell (its end would show on the next row).
    pub fn pos_at(&self, column: usize) -> usize {
        if column >= self.width() && (self.ends_line || self.cells.is_empty()) {
            return self.range.end;
        }
        self.cells
            .iter()
            .take_while(|c| c.column <= column)
            .last()
            .map_or(self.range.start, |c| c.range.start)
    }
}

/// The row of byte `pos`: the last row starting at or before it.
pub fn row_of(rows: &[TextRow], pos: usize) -> usize {
    rows.partition_point(|r| r.range.start <= pos)
        .saturating_sub(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Space,
    Tab,
    Other,
}

/// A char of the text with the glyphs drawn for it.
struct Unit {
    range: Range<usize>,
    width: usize,
    drawn: String,
    kind: Kind,
}

/// `line`'s glyphs grouped by the char of the text they start with.
fn units(line: &str) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    for glyph in glyphs(line, None) {
        match units.last_mut() {
            Some(unit) if glyph.chars == 0 => {
                unit.width += glyph.width;
                unit.drawn.push_str(&glyph.text);
            }
            _ => {
                if let Some(unit) = units.last_mut() {
                    unit.range.end = glyph.at;
                }
                units.push(Unit {
                    range: glyph.at..line.len(),
                    width: glyph.width,
                    kind: match glyph.text.as_str() {
                        " " => Kind::Space,
                        "\t" => Kind::Tab,
                        _ => Kind::Other,
                    },
                    drawn: glyph.text,
                });
            }
        }
    }
    units
}

/// Word-wraps `text` into rows at most `width` cells wide, as drawn (every
/// char that could act on the terminal shown, ui-plan 7). A row breaks
/// after the last space or tab that leaves the next word whole, or between
/// graphemes in a word longer than the row; a char is never split. Spaces
/// at a break stay at the end of their row, past the width if need be, so
/// the next row starts with the next word. Each line gives at least one
/// row.
pub fn word_wrap(text: &str, width: usize) -> Vec<TextRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut offset = 0;
    for line in text.split('\n') {
        wrap_one(line, offset, width, &mut rows);
        offset += line.len() + 1;
    }
    rows
}

/// One row of a styled line cut by [`wrap_spans`].
#[derive(Debug, Clone, PartialEq)]
pub struct SpanRow {
    pub spans: Vec<Span<'static>>,
    /// Where the row starts in the line's text, in bytes.
    pub start: usize,
    /// Cells it takes.
    pub width: usize,
}

/// Word-wraps the styled `spans` of one line into rows at most `width`
/// cells wide, as [`word_wrap`] cuts plain text (every char that could act
/// on the terminal shown), each piece keeping its span's style. The spaces
/// where a row breaks are dropped, as are spaces past the width. A line
/// gives at least one row.
pub fn wrap_spans(spans: &[Span<'_>], width: usize) -> Vec<SpanRow> {
    let width = width.max(1);
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let mut ends = Vec::with_capacity(spans.len());
    let mut at = 0;
    for span in spans {
        at += span.content.len();
        ends.push(at);
    }
    word_wrap(&text, width)
        .into_iter()
        .map(|row| {
            let mut cells = row.cells;
            cells.retain(|c| c.column + c.width <= width);
            if !row.ends_line {
                while cells
                    .last()
                    .is_some_and(|c| c.drawn.chars().all(|ch| ch == ' '))
                {
                    cells.pop();
                }
            }
            let width = cells.last().map_or(0, |c| c.column + c.width);
            let mut out: Vec<Span<'static>> = Vec::new();
            let mut current = usize::MAX;
            for cell in &cells {
                let index = ends.partition_point(|&end| end <= cell.range.start);
                match out.last_mut() {
                    Some(span) if index == current => span.content.to_mut().push_str(&cell.drawn),
                    _ => {
                        let style = spans.get(index).map(|s| s.style).unwrap_or_default();
                        out.push(Span::styled(cell.drawn.clone(), style));
                        current = index;
                    }
                }
            }
            SpanRow {
                spans: out,
                start: row.range.start,
                width,
            }
        })
        .collect()
}

/// Cells `text` takes on one row, as drawn.
pub fn text_width(text: &str) -> usize {
    word_wrap(text, usize::MAX)
        .iter()
        .map(TextRow::width)
        .max()
        .unwrap_or(0)
}

fn wrap_one(line: &str, offset: usize, width: usize, rows: &mut Vec<TextRow>) {
    let units = units(line);
    let start_of = |i: usize| offset + units.get(i).map_or(line.len(), |u| u.range.start);
    let mut first = 0;
    let mut cells: Vec<TextCell> = Vec::new();
    let mut column = 0;
    // Where a new row may start: after a run of spaces or a tab.
    let mut gap: Option<usize> = None;
    let mut i = 0;
    while let Some(unit) = units.get(i) {
        let w = match unit.kind {
            Kind::Tab => TAB - column % TAB,
            _ => unit.width,
        };
        if column > 0 && column + w > width && unit.kind != Kind::Space {
            let cut = gap.filter(|&g| g > first).unwrap_or(i);
            cells.truncate(cut - first);
            rows.push(TextRow {
                range: start_of(first)..start_of(cut),
                cells: std::mem::take(&mut cells),
                ends_line: false,
            });
            first = cut;
            column = 0;
            gap = None;
            i = cut;
            continue;
        }
        cells.push(TextCell {
            range: offset + unit.range.start..offset + unit.range.end,
            column,
            width: w,
            drawn: match unit.kind {
                Kind::Tab => " ".repeat(w),
                _ => unit.drawn.clone(),
            },
        });
        column += w;
        i += 1;
        if unit.kind != Kind::Other && units.get(i).is_some_and(|u| u.kind != Kind::Space) {
            gap = Some(i);
        }
    }
    rows.push(TextRow {
        range: start_of(first)..offset + line.len(),
        cells,
        ends_line: true,
    });
}

#[cfg(test)]
#[path = "wrap_tests.rs"]
mod tests;
