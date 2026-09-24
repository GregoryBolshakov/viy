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

fn drawn(rows: &[TextRow]) -> Vec<String> {
    rows.iter().map(TextRow::drawn).collect()
}

#[test]
fn word_wrap_breaks_after_spaces_and_keeps_byte_ranges() {
    let text = "the quick brown fox";
    let rows = word_wrap(text, 10);
    assert_eq!(drawn(&rows), ["the quick ", "brown fox"]);
    let ranges: Vec<_> = rows.iter().map(|r| r.range.clone()).collect();
    assert_eq!(ranges, [0..10, 10..19]);
    assert_eq!(&text[rows[1].range.clone()], "brown fox");
    assert_eq!(
        rows.iter().map(|r| r.ends_line).collect::<Vec<_>>(),
        [false, true]
    );
}

/// A word longer than the row is cut between graphemes; spaces past the
/// width hang at the end of their row, so the next starts with a word.
#[test]
fn long_words_are_cut_and_spaces_hang() {
    assert_eq!(drawn(&word_wrap("abcdefghij", 4)), ["abcd", "efgh", "ij"]);
    assert_eq!(drawn(&word_wrap("abcd    efg", 6)), ["abcd    ", "efg"]);
    assert_eq!(drawn(&word_wrap("日本語の文", 5)), ["日本", "語の", "文"]);
}

#[test]
fn lines_and_empty_lines_each_give_rows() {
    let text = "one\n\nthree";
    let rows = word_wrap(text, 10);
    assert_eq!(drawn(&rows), ["one", "", "three"]);
    let ranges: Vec<_> = rows.iter().map(|r| r.range.clone()).collect();
    assert_eq!(ranges, [0..3, 4..4, 5..10]);
}

/// What is drawn is sanitized, and measured as drawn: an escape is `␛`, a
/// bidi override eight cells, and neither is ever split.
#[test]
fn word_wrap_measures_what_is_drawn() {
    let text = "a\u{1b}[2J \u{202e}b";
    let rows = word_wrap(text, 10);
    assert_eq!(drawn(&rows), ["a␛[2J ", "<U+202E>b"]);
    assert_eq!(rows[1].cells[0].range, 6..9);
    assert_eq!(rows[1].cells[1].column, 8);
    // Wider than the row: on a row of its own, whole.
    assert_eq!(drawn(&word_wrap(text, 6)), ["a␛[2J ", "<U+202E>", "b"]);
}

#[test]
fn columns_and_positions_map_both_ways() {
    let rows = word_wrap("ab\tcd ef", 6);
    assert_eq!(drawn(&rows), ["ab  cd ", "ef"]);
    let row = &rows[0];
    assert_eq!(row.column_of(3), 4, "after the tab");
    assert_eq!(row.pos_at(3), 2, "inside the tab: the tab");
    assert_eq!(row.pos_at(4), 3);
    // A row that wraps never gives its end, which shows on the next row.
    assert_eq!(row.pos_at(40), 5);
    assert_eq!(rows[1].pos_at(40), 8);
    assert_eq!(row_of(&rows, 5), 0);
    assert_eq!(row_of(&rows, 6), 1);
}
