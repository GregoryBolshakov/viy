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

/// A frame is one flush, inside a synchronized update, and the same frame
/// again, cursor included, writes nothing.
#[test]
fn a_frame_is_written_whole_in_a_synchronized_update() {
    let mut term = Terminal::new(VT100Backend::new(20, 3)).unwrap();
    let frame = |frame: &mut Frame| {
        frame.render_widget(Paragraph::new("text"), Rect::new(0, 0, 20, 1));
        frame.set_cursor_position((2, 2));
    };
    let before = term.backend().bytes_written();
    let flushes = term.backend().flushes();
    term.draw(frame).unwrap();
    assert_eq!(term.backend().flushes(), flushes + 1);
    let written = &term.backend().written()[before..];
    assert!(written.starts_with(b"\x1b[?2026h"), "{written:?}");
    assert!(written.ends_with(b"\x1b[?2026l"), "{written:?}");
    let before = term.backend().bytes_written();
    term.draw(frame).unwrap();
    assert_eq!(term.backend().bytes_written(), before);
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
    term.resize(Size::new(8, 3));
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

/// A resize writes nothing; the next frame rewrites every cell, so what the
/// terminal made of the old screen is gone without a blank screen between.
#[test]
fn a_resize_rewrites_every_cell_in_the_next_frame_without_clearing() {
    let mut term = Terminal::new(VT100Backend::new(20, 2)).unwrap();
    term.draw(text("old")).unwrap();
    // What a terminal may leave after a resize: cells viy did not write.
    term.backend_mut()
        .write_all(b"\x1b[1;5Hstray\x1b[2;1Hrow two")
        .unwrap();
    term.backend_mut().resize(24, 2);
    let before = term.backend().bytes_written();
    term.resize(Size::new(24, 2));
    assert_eq!(term.backend().bytes_written(), before);
    term.draw(text("new")).unwrap();
    assert_eq!(term.backend().screen_rows(), ["new", ""]);
    let frame = &term.backend().written()[before..];
    assert!(!frame.windows(4).any(|w| w == b"\x1b[2J"));
}

/// Ctrl-L's repaint, the same way.
#[test]
fn invalidating_repaints_every_cell_without_clearing() {
    let mut term = Terminal::new(VT100Backend::new(20, 2)).unwrap();
    term.draw(text("kept")).unwrap();
    term.backend_mut().write_all(b"\x1b[2;1Hstray").unwrap();
    let before = term.backend().bytes_written();
    term.invalidate();
    term.draw(text("kept")).unwrap();
    assert_eq!(term.backend().screen_rows(), ["kept", ""]);
    assert!(
        !term.backend().written()[before..]
            .windows(4)
            .any(|w| w == b"\x1b[2J")
    );
}

#[test]
fn nothing_written_clears_the_scrollback() {
    let mut term = Terminal::new(VT100Backend::new(20, 2)).unwrap();
    term.draw(text("a")).unwrap();
    term.clear().unwrap();
    term.resize(Size::new(20, 2));
    term.draw(text("b")).unwrap();
    assert!(!term.backend().written().windows(4).any(|w| w == b"\x1b[3J"));
}
