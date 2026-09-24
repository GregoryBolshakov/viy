//! Text from outside never controls the terminal (ui-plan 7, flags F36).
//!
//! Everything viy draws comes from the model, tools, files or the web. Before
//! a string becomes a cell, every char that could act on the terminal or
//! hide what the text says is replaced by something visible:
//!
//! - C0 controls and DEL by their Control Pictures (`␛`, `␇`, `␡`), so no
//!   escape sequence (CSI, OSC 0/8/52, DCS) can start;
//! - C1 controls (U+0080-U+009F, which some terminals obey as `ESC [`,
//!   `ESC ]` and the rest), bidi controls (Trojan Source reordering), line
//!   and paragraph separators, interlinear annotation marks, and every
//!   Default_Ignorable_Code_Point (zero-width spaces, word joiners, soft
//!   hyphens, Hangul fillers, the tag chars of ASCII smuggling), by their
//!   code point as `<U+202E>`: shown, not applied. Codex strips a shorter
//!   list from titles only (`tui/src/terminal_title.rs`).
//!
//! The joiners and variation selectors that emoji and Indic or Persian
//! script need (ZWJ, ZWNJ, U+FE00-U+FE0F, U+E0100-U+E01EF) pass when they
//! follow a visible non-ASCII char (a variation selector also after a keycap
//! digit, `#` or `*`), one at a time; anywhere else, as after ASCII letters
//! where they would hide text, they are shown too.
//!
//! The result is idempotent, so a line can pass twice. [`glyphs`] is the one
//! place that turns text into what is drawn, measured by grapheme as
//! ratatui measures it. Only the renderer writes escape sequences, from its
//! own constants; [`unsafe_in_cell`] is the terminal's last check on every
//! cell it writes.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Tab stops, in cells.
pub const TAB: usize = 4;

/// `text`, continuing after `prev`, with every char that could act on the
/// terminal made visible; newlines and tabs stay, for the caller to lay
/// out (the plain renderer).
pub fn sanitize_after(prev: Option<char>, text: &str) -> Cow<'_, str> {
    clean(prev, text, true)
}

/// `text` as one row, continuing after `prev`: newlines and tabs are made
/// visible too.
pub fn sanitize_line_after(prev: Option<char>, text: &str) -> Cow<'_, str> {
    clean(prev, text, false)
}

fn clean(mut prev: Option<char>, text: &str, keep_layout: bool) -> Cow<'_, str> {
    let layout = |c: char| keep_layout && matches!(c, '\n' | '\t');
    let mut p = prev;
    let all_kept = text.chars().all(|c| {
        let kept = layout(c) || shown(c, p) == Shown::Itself;
        p = Some(c);
        kept
    });
    if all_kept {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if layout(c) {
            out.push(c);
        } else {
            push_visible(&mut out, c, prev);
        }
        prev = out.chars().next_back();
    }
    Cow::Owned(out)
}

/// Appends what `c` is drawn as after `prev`, the last char drawn before
/// it (`None` at the start of a line). Deciding by what is drawn, not by
/// the text, makes the result idempotent.
pub fn push_visible(out: &mut String, c: char, prev: Option<char>) {
    match shown(c, prev) {
        Shown::Itself => out.push(c),
        Shown::Picture(p) => out.push(p),
        Shown::Code => out.push_str(&format!("<U+{:04X}>", u32::from(c))),
    }
}

/// One grapheme as drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glyph {
    /// What is drawn; `"\t"` for a tab, which the caller lays out.
    pub text: String,
    /// Cells, as ratatui counts them (the grapheme's width).
    pub width: usize,
    /// The text's chars that start in this glyph: 0 for the later glyphs
    /// of a `<U+202E>`, 2 for a kept `❤` and U+FE0F.
    pub chars: usize,
    /// The byte offset in the text of the first char starting here, or of
    /// the char this glyph is part of.
    pub at: usize,
}

/// `text` as the glyphs drawn for it, after `prev`, the last char drawn
/// before it; newlines are shown as `␊`, tabs kept for the caller.
pub fn glyphs(text: &str, mut prev: Option<char>) -> Vec<Glyph> {
    let mut drawn = String::with_capacity(text.len());
    // Where each char of the text starts in `drawn`, and in the text.
    let mut starts = Vec::with_capacity(text.len());
    for (at, c) in text.char_indices() {
        starts.push((drawn.len(), at));
        if c == '\t' {
            drawn.push(c);
        } else {
            push_visible(&mut drawn, c, prev);
        }
        prev = drawn.chars().next_back();
    }
    let mut next = 0;
    let mut at = 0;
    drawn
        .grapheme_indices(true)
        .map(|(offset, g)| {
            let end = offset + g.len();
            let first = next;
            while next < starts.len() && starts[next].0 < end {
                next += 1;
            }
            if let Some(&(_, start)) = starts.get(first).filter(|_| next > first) {
                at = start;
            }
            Glyph {
                text: g.to_string(),
                width: if g == "\t" { 0 } else { g.width() },
                chars: next - first,
                at,
            }
        })
        .collect()
}

/// Whether a cell's symbol holds a char that acts on the terminal: a
/// control or a bidi control. The terminal writes U+FFFD instead; the
/// sanitizer has already made every such char visible, so this never fires
/// for text that went through it.
pub fn unsafe_in_cell(symbol: &str) -> bool {
    symbol.chars().any(|c| c.is_control() || is_bidi(c))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    Itself,
    /// A Control Picture.
    Picture(char),
    /// `<U+XXXX>`.
    Code,
}

fn shown(c: char, prev: Option<char>) -> Shown {
    match c {
        '\0'..='\x1f' => {
            Shown::Picture(char::from_u32(0x2400 + u32::from(c)).unwrap_or('\u{fffd}'))
        }
        '\x7f' => Shown::Picture('\u{2421}'),
        '\u{80}'..='\u{9f}' => Shown::Code,
        // Drawn chars are never controls or hidden ones, except the joiners
        // and selectors kept here.
        _ if is_joiner(c) && prev.is_some_and(|p| !p.is_ascii() && !is_joiner(p)) => Shown::Itself,
        _ if is_selector(c)
            && prev.is_some_and(|p| {
                (!p.is_ascii() || matches!(p, '0'..='9' | '#' | '*'))
                    && !is_joiner(p)
                    && !is_selector(p)
            }) =>
        {
            Shown::Itself
        }
        _ if is_hidden(c) => Shown::Code,
        _ => Shown::Itself,
    }
}

/// ZWNJ and ZWJ.
fn is_joiner(c: char) -> bool {
    matches!(c, '\u{200c}' | '\u{200d}')
}

/// Variation selectors.
fn is_selector(c: char) -> bool {
    matches!(c, '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
}

/// Bidi controls and the line and paragraph separators.
fn is_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Chars that reorder text or draw nothing: the bidi controls and
/// separators, the interlinear annotation marks, and Unicode's
/// Default_Ignorable_Code_Point (DerivedCoreProperties.txt, all ranges),
/// shown as their code point unless [`shown`] keeps a joiner or selector.
fn is_hidden(c: char) -> bool {
    is_bidi(c)
        || matches!(
            c,
            '\u{00ad}'
                | '\u{034f}'
                | '\u{115f}'
                | '\u{1160}'
                | '\u{17b4}'
                | '\u{17b5}'
                | '\u{180b}'..='\u{180f}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{3164}'
                | '\u{fe00}'..='\u{fe0f}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff0}'..='\u{fffb}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0000}'..='\u{e0fff}'
        )
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
