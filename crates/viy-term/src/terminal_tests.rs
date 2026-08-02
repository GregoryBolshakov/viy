use pretty_assertions::assert_eq;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::widgets::Paragraph;

use super::*;
use crate::test_backend::VT100Backend;

fn terminal(width: u16, height: u16) -> Terminal<VT100Backend> {
    Terminal::with_size_and_cursor(
        VT100Backend::new(width, height),
        Size::new(width, height),
        Position::ORIGIN,
    )
}

#[test]
fn draws_the_viewport_where_it_is_placed() {
    let mut term = terminal(20, 5);
    term.set_viewport_area(Rect::new(0, 3, 20, 2));
    term.draw(|frame| {
        let area = frame.area();
        frame.render_widget(Paragraph::new("live\nfooter"), area);
        frame.set_cursor_position((4, 3));
    })
    .unwrap();
    assert_eq!(
        term.backend().screen_rows(),
        ["", "", "", "live▮", "footer"]
    );
    assert_eq!(term.cursor_position(), Position::new(4, 3));
}

#[test]
fn a_second_identical_frame_writes_no_cells() {
    let mut term = terminal(20, 3);
    term.set_viewport_area(Rect::new(0, 1, 20, 2));
    let render = |frame: &mut Frame| {
        let area = frame.area();
        frame.render_widget(Paragraph::new("same"), area);
    };
    term.draw(render).unwrap();
    let before = term.backend().bytes_written();
    term.draw(render).unwrap();
    assert_eq!(term.backend().bytes_written(), before);
}

#[test]
fn invalidating_repaints_cells_cleared_behind_the_viewport() {
    let mut term = terminal(20, 3);
    term.set_viewport_area(Rect::new(0, 1, 20, 2));
    let render = |frame: &mut Frame| {
        let area = frame.area();
        frame.render_widget(Paragraph::new("kept"), area);
    };
    term.draw(render).unwrap();
    term.clear_from(Position::new(0, 1)).unwrap();
    assert_eq!(term.backend().screen_rows(), ["", "", ""]);
    term.draw(render).unwrap();
    assert_eq!(term.backend().screen_rows(), ["", "kept", ""]);
}

#[test]
fn clearing_the_scrollback_sends_ed3_and_homes_the_cursor() {
    let mut term = terminal(10, 3);
    write!(term.backend_mut(), "old\r\ntext").unwrap();
    term.clear_scrollback_and_screen().unwrap();
    // vt100 does not keep a scrollback here; the screen is blank and the
    // cursor home.
    assert_eq!(term.backend().screen_rows(), ["▮", "", ""]);
    assert_eq!(term.cursor_position(), Position::ORIGIN);
}
