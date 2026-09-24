use pretty_assertions::assert_eq;
use ratatui::layout::Position;

use super::*;

fn area(text: &str) -> TextArea {
    let mut area = TextArea::default();
    area.insert_str(text);
    area
}

#[test]
fn a_paste_keeps_its_newlines() {
    let area = area("one\r\ntwo\rthree\n");
    assert_eq!(area.text(), "one\ntwo\nthree\n");
}

#[test]
fn editing_moves_by_characters_not_bytes() {
    let mut area = area("née");
    area.move_left();
    area.move_left();
    area.backspace();
    area.insert_char('a');
    assert_eq!(area.text(), "aée");
    area.delete();
    assert_eq!(area.text(), "ae");
}

#[test]
fn home_and_end_stay_on_the_cursor_line() {
    let mut area = area("first\nsecond");
    area.move_home();
    area.insert_char('>');
    area.move_end();
    area.insert_char('<');
    assert_eq!(area.text(), "first\n>second<");
}

#[test]
fn layout_wraps_at_the_width_and_places_the_cursor() {
    let (rows, cursor) = area("abcdef\ngh").layout(4);
    assert_eq!(rows, ["abcd", "ef", "gh"]);
    assert_eq!(cursor, Position::new(2, 2));
}

#[test]
fn a_cursor_after_a_full_row_starts_the_next() {
    let (rows, cursor) = area("abcd").layout(4);
    assert_eq!(rows, ["abcd", ""]);
    assert_eq!(cursor, Position::new(0, 1));
}

#[test]
fn wide_characters_do_not_split_across_rows() {
    let (rows, cursor) = area("日本語").layout(5);
    assert_eq!(rows, ["日本", "語"]);
    assert_eq!(cursor, Position::new(2, 1));
}

#[test]
fn prefill_puts_the_cursor_at_the_end_and_take_empties() {
    let mut area = TextArea::default();
    area.set_text("cut message");
    area.insert_char('!');
    assert_eq!(area.take(), "cut message!");
    assert!(area.is_empty());
}

/// The cursor after an emoji sequence is two cells on, and hostile text is
/// laid out as drawn.
#[test]
fn the_cursor_counts_cells_as_drawn() {
    let mut area = TextArea::default();
    area.insert_str("❤\u{fe0f}x");
    area.move_left();
    assert_eq!(
        area.layout(10),
        (vec!["❤\u{fe0f}x".to_string()], Position::new(2, 0))
    );
    let mut area = TextArea::default();
    area.insert_str("a\x1b[2J\u{202e}b");
    assert_eq!(
        area.layout(40),
        (vec!["a␛[2J<U+202E>b".to_string()], Position::new(14, 0))
    );
}

#[test]
fn layout_wraps_words() {
    let (rows, cursor) = area("hello big world").layout(10);
    assert_eq!(rows, ["hello big ", "world"]);
    assert_eq!(cursor, Position::new(5, 1));
}

/// Left and Right step over a whole grapheme: an emoji with its selector,
/// a letter with its combining accent.
#[test]
fn moves_step_over_graphemes() {
    let mut area = area("a❤\u{fe0f}e\u{301}");
    area.move_left();
    assert_eq!(area.cursor(), 7);
    area.move_left();
    assert_eq!(area.cursor(), 1);
    area.backspace();
    assert_eq!(area.text(), "❤\u{fe0f}e\u{301}");
    area.delete();
    assert_eq!(area.text(), "e\u{301}");
}

#[test]
fn word_moves_and_deletions() {
    let mut area = area("cargo test --workspace  ");
    area.move_word_left();
    assert_eq!(&area.text()[area.cursor()..], "workspace  ");
    area.move_word_left();
    assert_eq!(&area.text()[area.cursor()..], "--workspace  ");
    area.move_word_right();
    assert_eq!(&area.text()[area.cursor()..], "workspace  ");
    area.move_end();
    area.delete_word_left();
    assert_eq!(area.text(), "cargo test --");
    area.move_home();
    area.delete_word_right();
    assert_eq!(area.text(), " test --");
}

/// Ctrl-K and Ctrl-U kill to the line's end or start, or the newline when
/// there is nothing else; Ctrl-Y yanks the last kill back.
#[test]
fn kill_and_yank() {
    let mut area = area("first\nsecond");
    area.move_home();
    area.kill_to_line_end();
    assert_eq!(area.text(), "first\n");
    area.kill_to_line_start();
    assert_eq!(area.text(), "first");
    area.kill_to_line_start();
    assert_eq!(area.text(), "");
    area.yank();
    assert_eq!(area.text(), "first");
    area.insert_str("!");
    area.delete_word_left();
    area.yank();
    assert_eq!(area.text(), "first!");
}

/// Up and Down move between screen rows, wrapped ones too, keeping the
/// column; at the first or last row they go to the start or end and say
/// so.
#[test]
fn up_and_down_move_by_screen_row_keeping_the_column() {
    // Rows at 9: "abcdefgh ", "ij", "xy", "longer ", "line".
    let mut area = area("abcdefgh ij\nxy\nlonger line");
    area.layout(9);
    let rest = |a: &TextArea| a.text()[a.cursor()..].to_string();
    assert!(area.move_up(9));
    assert_eq!(rest(&area), "er line", "column 4 of `longer `");
    assert!(area.move_up(9));
    assert_eq!(rest(&area), "\nlonger line", "`xy` is shorter: its end");
    assert!(area.move_up(9));
    assert_eq!(
        rest(&area),
        "\nxy\nlonger line",
        "back to column 4 is past `ij`"
    );
    assert!(area.move_up(9));
    assert_eq!(rest(&area), "efgh ij\nxy\nlonger line", "column 4 kept");
    assert!(!area.move_up(9));
    assert_eq!(area.cursor(), 0);
    for _ in 0..4 {
        assert!(area.move_down(9));
    }
    assert!(!area.move_down(9));
    assert_eq!(area.cursor(), area.text().len());
}
