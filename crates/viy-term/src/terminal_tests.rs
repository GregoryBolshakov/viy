use pretty_assertions::assert_eq;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::widgets::Paragraph;

use super::*;
use crate::test_backend::VT100Backend;

fn text(text: &'static str) -> impl FnOnce(&mut Frame) {
    move |frame: &mut Frame| {
        let area = frame.area();
        frame.render_widget(Paragraph::new(text), area);
    }
}

#[test]
fn draws_the_frame_and_places_the_cursor() {
    let mut term = Terminal::new(VT100Backend::new(20, 3)).unwrap();
    term.draw(|frame| {
        frame.render_widget(Paragraph::new("transcript"), Rect::new(0, 0, 20, 1));
        frame.render_widget(Paragraph::new("› hi"), Rect::new(0, 2, 20, 1));
        frame.set_cursor_position((4, 2));
    })
    .unwrap();
    assert_eq!(term.backend().screen_rows(), ["transcript", "", "› hi▮"]);
}

#[test]
fn a_second_identical_frame_writes_nothing() {
    let mut term = Terminal::new(VT100Backend::new(20, 3)).unwrap();
    term.draw(text("same")).unwrap();
    let before = term.backend().bytes_written();
    term.draw(text("same")).unwrap();
    assert_eq!(term.backend().bytes_written(), before);
}

#[test]
fn clearing_repaints_every_cell() {
    let mut term = Terminal::new(VT100Backend::new(20, 2)).unwrap();
    term.draw(text("kept")).unwrap();
    term.clear().unwrap();
    assert_eq!(term.backend().screen_rows(), ["", ""]);
    term.draw(text("kept")).unwrap();
    assert_eq!(term.backend().screen_rows(), ["kept", ""]);
}

#[test]
fn a_resize_lays_the_frame_out_at_the_new_size() {
    let mut term = Terminal::new(VT100Backend::new(20, 2)).unwrap();
    term.draw(text("wide enough here")).unwrap();
    term.backend_mut().resize(8, 3);
    term.resize(Size::new(8, 3)).unwrap();
    assert_eq!(term.size(), Size::new(8, 3));
    term.draw(|frame| {
        let area = frame.area();
        frame.render_widget(
            Paragraph::new("wide enough here").wrap(ratatui::widgets::Wrap { trim: true }),
            area,
        );
    })
    .unwrap();
    assert_eq!(term.backend().screen_rows(), ["wide", "enough", "here"]);
}

#[test]
fn nothing_written_clears_the_scrollback() {
    let mut term = Terminal::new(VT100Backend::new(20, 2)).unwrap();
    term.draw(text("a")).unwrap();
    term.clear().unwrap();
    term.resize(Size::new(20, 2)).unwrap();
    term.draw(text("b")).unwrap();
    assert!(!term.backend().written().windows(4).any(|w| w == b"\x1b[3J"));
}
