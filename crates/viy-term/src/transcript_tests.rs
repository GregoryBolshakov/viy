use pretty_assertions::assert_eq;

use super::*;

fn cell(text: &str) -> Cell {
    text.split('\n')
        .map(|l| Line::from(l.to_string()))
        .collect()
}

fn numbered(n: usize) -> Vec<Cell> {
    (1..=n).map(|i| cell(&format!("line {i}"))).collect()
}

/// The rows `view` draws for `cells` in a `width` x `height` area.
fn draw(view: &mut TranscriptView, cells: &[Cell], width: u16, height: u16) -> Vec<String> {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    view.render(cells, area, &mut buf);
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn a_short_transcript_sits_at_the_bottom() {
    let mut view = TranscriptView::default();
    assert_eq!(
        draw(&mut view, &[cell("one\ntwo")], 10, 4),
        ["", "", "one", "two"]
    );
}

#[test]
fn it_follows_the_end_as_rows_arrive() {
    let mut view = TranscriptView::default();
    let mut cells = numbered(5);
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 3", "line 4", "line 5"]
    );
    cells.push(cell("line 6"));
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 4", "line 5", "line 6"]
    );
    assert!(view.is_following());
    assert_eq!(view.rows_below(), 0);
}

#[test]
fn scrolled_back_it_holds_still_while_rows_arrive() {
    let mut view = TranscriptView::default();
    let mut cells = numbered(10);
    draw(&mut view, &cells, 10, 3);
    view.scroll_up(4);
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 4", "line 5", "line 6"]
    );
    assert_eq!(view.rows_below(), 4);
    cells.extend(numbered(2));
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 4", "line 5", "line 6"]
    );
    assert_eq!(view.rows_below(), 6);
    assert!(!view.is_following());
}

#[test]
fn scrolling_down_to_the_end_follows_again() {
    let mut view = TranscriptView::default();
    let mut cells = numbered(10);
    draw(&mut view, &cells, 10, 3);
    view.scroll_up(5);
    draw(&mut view, &cells, 10, 3);
    view.scroll_down(2);
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 5", "line 6", "line 7"]
    );
    view.scroll_down(10);
    assert!(view.is_following());
    cells.push(cell("line 11"));
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 9", "line 10", "line 11"]
    );
}

#[test]
fn home_and_end() {
    let mut view = TranscriptView::default();
    let cells = numbered(10);
    draw(&mut view, &cells, 10, 3);
    view.home();
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 1", "line 2", "line 3"]
    );
    view.scroll_up(1);
    assert_eq!(draw(&mut view, &cells, 10, 3)[0], "line 1");
    view.end();
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 8", "line 9", "line 10"]
    );
}

#[test]
fn nothing_scrolls_when_everything_fits() {
    let mut view = TranscriptView::default();
    let cells = numbered(2);
    draw(&mut view, &cells, 10, 3);
    view.scroll_up(1);
    view.home();
    assert!(view.is_following());
}

#[test]
fn long_lines_wrap_at_the_width() {
    let mut view = TranscriptView::default();
    assert_eq!(
        draw(&mut view, &[cell("abcdefghij")], 4, 3),
        ["abcd", "efgh", "ij"]
    );
}

#[test]
fn a_resize_rewraps_and_keeps_the_text_at_the_top() {
    let mut view = TranscriptView::default();
    let cells = vec![
        cell("first"),
        cell("aaaa bbbb cccc dddd eeee"),
        cell("last one"),
        cell("tail 1"),
        cell("tail 2"),
        cell("tail 3"),
    ];
    // At 5 columns the long cell is five rows; read from its fourth.
    draw(&mut view, &cells, 5, 3);
    view.home();
    draw(&mut view, &cells, 5, 3);
    view.scroll_down(4);
    assert_eq!(draw(&mut view, &cells, 5, 3), ["dddd", "eeee", "last"]);
    // At 10 columns the row with "dddd" is the long cell's second; it
    // stays at the top.
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["cccc dddd", "eeee", "last one"]
    );
}

#[test]
fn a_reset_follows_the_new_transcript() {
    let mut view = TranscriptView::default();
    draw(&mut view, &numbered(10), 10, 3);
    view.scroll_up(3);
    view.reset();
    assert_eq!(
        draw(&mut view, &[cell("new chat")], 10, 3),
        ["", "", "new chat"]
    );
    assert!(view.is_following());
}

#[test]
fn controls_in_the_text_are_drawn_visible() {
    let mut view = TranscriptView::default();
    assert_eq!(draw(&mut view, &[cell("a\x1b[2Jb")], 10, 1), ["a␛[2Jb"]);
}
