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
