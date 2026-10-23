//! The centipede header at the top of the chat's transcript: a flat,
//! stylized centipede (`tools/header-art/centipede.py`, through
//! `tools/header_art.py`), 11 rows high and built in at twenty widths,
//! every 12 columns from 80 to 308, its body half a wave longer at each, as sextant cells: 2x3 solid blocks, each with a
//! foreground and a background colour. The largest that fits the terminal
//! is drawn, its leftmost block in the column where the title starts, and
//! under it, as Codex's session header, `>_ viy (vX)` and the chat's
//! directory. It is the first entry of the transcript
//! and scrolls away with it. Where the art is empty the terminal's own
//! background shows. Without truecolor (`COLORTERM`) the colours are the
//! nearest of the 256 xterm colours.
//!
//! Regenerate after changing the art: `python3 tools/header_art.py`.
//!
//! The files are parsed here into cells (a character and its colours),
//! never written to the terminal as they are: the screen only ever carries
//! viy's own sequences (kb d076).

use std::sync::OnceLock;

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

type Rgb = (u8, u8, u8);

/// One cell of the art: its character, foreground and background.
type ArtCell = (char, Option<Rgb>, Option<Rgb>);

macro_rules! arts {
    ($($w:literal)*) => {
        /// The widths the art is built in at, and each as
        /// `tools/header_art.py` writes it.
        const ARTS: &[(usize, &str)] = &[
            $(($w, include_str!(concat!("header/w", stringify!($w), ".ansi"))),)*
        ];
    };
}

// Keep in step with WIDTHS in tools/header_art.py: every 12 columns from
// the default 80-column window to a large monitor's full screen.
arts!(80 92 104 116 128 140 152 164 176 188 200 212 224 236 248 260 272 284 296 308);

/// The widths the art is built in at, narrowest first.
pub fn widths() -> impl Iterator<Item = usize> {
    ARTS.iter().map(|(w, _)| *w)
}

/// Narrower than this, the header is left out (the smallest art).
pub const MIN_WIDTH: usize = 80;

/// Where the title starts, and the art's leftmost block with it. Every art
/// is drawn narrower than its width by at least this (the tests hold it).
const INDENT: usize = 2;

/// How the terminal takes colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colors {
    /// 24-bit colour.
    True,
    /// The 256 xterm colours.
    Indexed,
}

impl Colors {
    /// This terminal's, from `COLORTERM` (`truecolor` or `24bit`).
    pub fn detect() -> Self {
        match std::env::var("COLORTERM").as_deref() {
            Ok("truecolor" | "24bit") => Self::True,
            _ => Self::Indexed,
        }
    }
}

/// The header as the transcript draws it: the art, and under it, as
/// Codex's session header, `>_ viy (vX)` and the chat's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub colors: Colors,
    /// viy's version, as `viy --version` gives it.
    pub version: String,
    /// The chat's directory, `~` standing for the home directory.
    pub directory: String,
}

impl Header {
    /// Its rows at `width`: the art (none below [`MIN_WIDTH`]), a blank
    /// row, the title and the directory, cut in the middle to fit, both
    /// as Codex's `SessionHeaderHistoryCell` draws them.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = self.art(width);
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(INDENT)),
            Span::styled(">_ ", Style::new().fg(Color::Cyan)),
            Span::styled("viy", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" (v{})", self.version),
                Style::new().add_modifier(Modifier::DIM),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("     "),
            Span::styled(
                fit_path(&self.directory, width.saturating_sub(5)),
                Style::new().add_modifier(Modifier::DIM),
            ),
        ]));
        lines
    }

    /// The largest art that fits `width`, [`INDENT`] columns in, under
    /// the title's start; none below [`MIN_WIDTH`].
    fn art(&self, width: usize) -> Vec<Line<'static>> {
        let Some((_, art)) = pick(width) else {
            return Vec::new();
        };
        art.iter()
            .map(|row| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                if !row.is_empty() {
                    spans.push(Span::raw(" ".repeat(INDENT)));
                }
                let mut text = String::new();
                let mut style = Style::new();
                for &(ch, fg, bg) in row {
                    let mut next = Style::new();
                    if let Some(rgb) = fg.filter(|_| ch != ' ') {
                        next = next.fg(self.color(rgb));
                    }
                    if let Some(rgb) = bg {
                        next = next.bg(self.color(rgb));
                    }
                    if next != style && !text.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut text), style));
                    }
                    style = next;
                    text.push(ch);
                }
                if !text.is_empty() {
                    spans.push(Span::styled(text, style));
                }
                Line::from(spans)
            })
            .collect()
    }

    fn color(&self, (r, g, b): Rgb) -> Color {
        match self.colors {
            Colors::True => Color::Rgb(r, g, b),
            Colors::Indexed => Color::Indexed(xterm256(r, g, b)),
        }
    }
}

/// `path` in at most `max` columns: whole if it fits; else its first
/// part, `…` and as many of its last parts as fit (Codex's
/// `center_truncate_path`); else its end after `…`.
fn fit_path(path: &str, max: usize) -> String {
    if path.width() <= max {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('/').collect();
    for keep in (1..parts.len().saturating_sub(1)).rev() {
        let fitted = format!("{}/…/{}", parts[0], parts[parts.len() - keep..].join("/"));
        if fitted.width() <= max {
            return fitted;
        }
    }
    if max == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 1;
    for ch in path.chars().rev() {
        let w = ch.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        tail.push(ch);
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

/// The largest art no wider than `width`, and its width.
fn pick(width: usize) -> Option<(usize, &'static [Vec<ArtCell>])> {
    static PARSED: OnceLock<Vec<Vec<Vec<ArtCell>>>> = OnceLock::new();
    let parsed = PARSED.get_or_init(|| ARTS.iter().map(|(_, text)| parse(text)).collect());
    ARTS.iter()
        .zip(parsed)
        .rev()
        .find(|((w, _), _)| *w <= width)
        .map(|((w, _), cells)| (*w, cells.as_slice()))
}

/// `tools/header_art.py`'s output: per row, characters after the SGR
/// sequences that colour them (`38;2;R;G;B`, `48;2;R;G;B`, `39`, `49`, `0`).
/// Anything else in a sequence is not the generator's and is skipped; the
/// trailing blanks without a background are dropped.
fn parse(text: &str) -> Vec<Vec<ArtCell>> {
    text.lines()
        .map(|line| {
            let mut cells: Vec<ArtCell> = Vec::new();
            let (mut fg, mut bg) = (None, None);
            let mut chars = line.chars();
            while let Some(ch) = chars.next() {
                if ch != '\x1b' {
                    cells.push((ch, fg, bg));
                    continue;
                }
                let sequence: String = chars.by_ref().take_while(|&c| c != 'm').collect();
                let fields: Vec<u8> = sequence
                    .trim_start_matches('[')
                    .split(';')
                    .filter_map(|f| f.parse().ok())
                    .collect();
                let mut rest = fields.as_slice();
                while let Some((&first, tail)) = rest.split_first() {
                    rest = match (first, tail) {
                        (38, [2, r, g, b, more @ ..]) => {
                            fg = Some((*r, *g, *b));
                            more
                        }
                        (48, [2, r, g, b, more @ ..]) => {
                            bg = Some((*r, *g, *b));
                            more
                        }
                        (39, more) => {
                            fg = None;
                            more
                        }
                        (49, more) => {
                            bg = None;
                            more
                        }
                        (0, more) => {
                            (fg, bg) = (None, None);
                            more
                        }
                        (_, more) => more,
                    };
                }
            }
            while cells
                .last()
                .is_some_and(|&(ch, _, bg)| ch == ' ' && bg.is_none())
            {
                cells.pop();
            }
            cells
        })
        .collect()
}

/// The nearest of the 256 xterm colours: the 6x6x6 cube or the grey ramp,
/// whichever is closer.
fn xterm256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| (i32::from(**l) - i32::from(v)).abs())
            .map_or(0, |(i, _)| i)
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let mean = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let grey_index = (mean.saturating_sub(8) / 10).min(23) as u8;
    let grey = 8 + 10 * grey_index;
    let distance = |(cr, cg, cb): (u8, u8, u8)| {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(cr, r) + d(cg, g) + d(cb, b)
    };
    if distance((grey, grey, grey)) < distance(cube) {
        232 + grey_index
    } else {
        16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8
    }
}

#[cfg(test)]
#[path = "header_tests.rs"]
mod tests;
