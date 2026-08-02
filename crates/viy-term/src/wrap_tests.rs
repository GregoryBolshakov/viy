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
        ["␛[2J␛]0;title␇\u{fffd}31m␡"]
    );
}

#[test]
fn an_empty_line_is_one_empty_row() {
    assert_eq!(texts(&wrap_line(&Line::from(""), 10)), [""]);
}
