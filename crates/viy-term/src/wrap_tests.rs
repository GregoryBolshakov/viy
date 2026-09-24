use pretty_assertions::assert_eq;
use ratatui::style::Stylize;
use ratatui::text::Line;

use super::*;

fn texts(rows: &[Line<'_>]) -> Vec<String> {
    rows.iter().map(ToString::to_string).collect()
}

#[test]
fn long_and_wide_lines_are_cut_at_the_width() {
    assert_eq!(
        texts(&wrap_line(&Line::from("abcdefghijkl"), 10)),
        ["abcdefghij", "kl"]
    );
    assert_eq!(
        texts(&wrap_line(&Line::from("日本語のテキスト"), 10)),
        ["日本語のテ", "キスト"]
    );
}

#[test]
fn span_styles_carry_across_rows() {
    let line = Line::from(vec!["abc".red(), "def".into()]);
    assert_eq!(
        wrap_line(&line, 4),
        [
            Line::from(vec!["abc".red(), "d".into()]),
            Line::from(vec!["ef".into()]),
        ]
    );
}

#[test]
fn controls_become_visible_symbols() {
    let hostile = "\x1b[2J\x1b]0;title\x07\u{9b}31m\x7f";
    assert_eq!(
        texts(&wrap_line(&Line::from(hostile), 80)),
        ["␛[2J␛]0;title␇<U+009B>31m␡"]
    );
}

#[test]
fn tabs_are_spaces_to_the_next_stop() {
    assert_eq!(texts(&wrap_line(&Line::from("\tab\tc"), 80)), ["    ab  c"]);
}

/// Each row holds the source chars that start in it: a shown code point or
/// a tab counts as one char, so the transcript's anchors stay in the text.
#[test]
fn rows_count_the_source_chars_they_hold() {
    let rows = wrap_rows(&Line::from("ab\u{202e}cd\tef"), 6);
    let got: Vec<(String, usize)> = rows.iter().map(|(r, n)| (r.to_string(), *n)).collect();
    assert_eq!(
        got,
        [
            ("ab<U+2".to_string(), 3),
            ("02E>cd".to_string(), 2),
            ("    ef".to_string(), 3),
        ]
    );
}

#[test]
fn an_empty_line_is_one_empty_row() {
    assert_eq!(texts(&wrap_line(&Line::from(""), 10)), [""]);
}

/// An emoji with its variation selector is two cells, as ratatui draws it,
/// so a row never holds more than fits.
#[test]
fn emoji_sequences_take_the_cells_ratatui_gives_them() {
    assert_eq!(
        texts(&wrap_line(&Line::from("abc❤\u{fe0f}d"), 4)),
        ["abc", "❤\u{fe0f}d"]
    );
    assert_eq!(
        texts(&wrap_line(&Line::from("ab1\u{fe0f}\u{20e3}"), 3)),
        ["ab", "1\u{fe0f}\u{20e3}"]
    );
}

/// A tab at the end of a row is cut to the row, never carried over.
#[test]
fn a_tab_is_never_split_across_rows() {
    assert_eq!(
        texts(&wrap_line(&Line::from("abcde\tf"), 6)),
        ["abcde ", "f"]
    );
    assert_eq!(
        texts(&wrap_line(&Line::from("abcdef\tg"), 6)),
        ["abcdef", "    g"]
    );
}
