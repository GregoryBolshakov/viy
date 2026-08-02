use pretty_assertions::assert_eq;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use super::*;
use crate::terminal::Frame;
use crate::test_backend::VT100Backend;

fn terminal(width: u16, height: u16, viewport: Rect) -> Terminal<VT100Backend> {
    let mut term = Terminal::with_size_and_cursor(
        VT100Backend::new(width, height),
        Size::new(width, height),
        Position::new(0, viewport.y),
    );
    term.set_viewport_area(viewport);
    term
}

fn live(frame: &mut Frame) {
    let area = frame.area();
    frame.render_widget(Paragraph::new("> input\nfooter"), area);
}

fn lines(texts: &[&str]) -> Vec<Line<'static>> {
    texts.iter().map(|t| Line::from(t.to_string())).collect()
}

#[test]
fn lines_go_above_a_viewport_at_the_bottom() {
    let mut term = terminal(20, 6, Rect::new(0, 4, 20, 2));
    term.draw(live).unwrap();
    insert_history_lines(&mut term, &lines(&["one", "two"])).unwrap();
    term.draw(live).unwrap();
    assert_eq!(
        term.backend().screen_rows(),
        ["", "", "one", "two", "> input", "footer"]
    );
}

#[test]
fn more_lines_than_rows_above_scroll_only_the_region_above() {
    let mut term = terminal(20, 5, Rect::new(0, 3, 20, 2));
    term.draw(live).unwrap();
    let many: Vec<String> = (1..=7).map(|n| format!("line {n}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    insert_history_lines(&mut term, &lines(&many)).unwrap();
    term.draw(live).unwrap();
    assert_eq!(
        term.backend().screen_rows(),
        ["line 5", "line 6", "line 7", "> input", "footer"]
    );
}

#[test]
fn a_viewport_below_the_shell_prompt_moves_down_as_lines_arrive() {
    let mut term = terminal(20, 8, Rect::new(0, 1, 20, 2));
    write!(term.backend_mut(), "$ viy").unwrap();
    term.draw(live).unwrap();
    insert_history_lines(&mut term, &lines(&["a", "b", "c"])).unwrap();
    term.draw(live).unwrap();
    assert_eq!(term.viewport_area(), Rect::new(0, 4, 20, 2));
    assert_eq!(
        term.backend().screen_rows(),
        ["$ viy", "a", "b", "c", "> input", "footer", "", ""]
    );
}

#[test]
fn long_and_wide_lines_are_cut_at_the_screen_width() {
    let mut term = terminal(10, 6, Rect::new(0, 4, 10, 2));
    term.draw(live).unwrap();
    insert_history_lines(&mut term, &lines(&["abcdefghijkl", "日本語のテキスト"])).unwrap();
    term.draw(live).unwrap();
    assert_eq!(
        term.backend().screen_rows(),
        [
            "abcdefghij",
            "kl",
            "日本語のテ",
            "キスト",
            "> input",
            "footer"
        ]
    );
}

#[test]
fn styles_reach_the_screen_and_are_reset_after_each_span() {
    let mut term = terminal(20, 4, Rect::new(0, 3, 20, 1));
    insert_history_lines(
        &mut term,
        &[Line::from(vec!["warn".yellow().bold(), " plain".into()])],
    )
    .unwrap();
    let screen = term.backend().vt100().screen();
    let warn = screen.cell(2, 0).unwrap();
    assert_eq!(warn.fgcolor(), vt100::Color::Idx(3));
    assert!(warn.bold());
    let plain = screen.cell(2, 5).unwrap();
    assert_eq!(plain.fgcolor(), vt100::Color::Default);
    assert!(!plain.bold());
}

#[test]
fn escape_sequences_in_text_are_shown_not_obeyed() {
    let mut term = terminal(40, 4, Rect::new(0, 3, 40, 1));
    term.draw(|frame| {
        let area = frame.area();
        frame.render_widget(Paragraph::new("question"), area);
    })
    .unwrap();
    let hostile = "\x1b[2J\x1b[Hgone\x1b]0;title\x07\u{9b}31m";
    insert_history_lines(&mut term, &lines(&[hostile])).unwrap();
    // The C1 CSI became U+FFFD, which vt100 (unlike real terminals) does
    // not print.
    assert_eq!(
        term.backend().screen_rows(),
        ["", "", "␛[2J␛[Hgone␛]0;title␇31m", "question"]
    );
}

#[test]
fn wrap_line_keeps_span_styles_across_rows() {
    let line = Line::from(vec!["abc".red(), "def".into()]);
    let rows = wrap_line(&line, 4);
    assert_eq!(
        rows,
        [
            Line::from(vec!["abc".red(), "d".into()]),
            Line::from(vec!["ef".into()]),
        ]
    );
}

#[test]
fn a_viewport_at_the_top_takes_lines_into_the_rows_it_leaves() {
    // After `clear`, or a repaint: the viewport starts on row 0.
    let mut term = terminal(20, 6, Rect::new(0, 0, 20, 2));
    term.draw(live).unwrap();
    insert_history_lines(&mut term, &lines(&["first"])).unwrap();
    term.draw(live).unwrap();
    insert_history_lines(&mut term, &lines(&["second", "third"])).unwrap();
    term.draw(live).unwrap();
    assert_eq!(
        term.backend().screen_rows(),
        ["first", "second", "third", "> input", "footer", ""]
    );
}

#[test]
fn lines_fill_the_gap_first_then_scroll_above_the_viewport() {
    let mut term = terminal(20, 6, Rect::new(0, 2, 20, 2));
    term.draw(live).unwrap();
    insert_history_lines(&mut term, &lines(&["a", "b", "c", "d", "e"])).unwrap();
    term.draw(live).unwrap();
    assert_eq!(term.viewport_area(), Rect::new(0, 4, 20, 2));
    assert_eq!(
        term.backend().screen_rows(),
        ["b", "c", "d", "e", "> input", "footer"]
    );
}
