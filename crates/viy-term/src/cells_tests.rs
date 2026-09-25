use std::time::Duration;

use pretty_assertions::assert_eq;
use ratatui::style::Color;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::ToolCallEndEvent;
use viy_protocol::ToolCallStatus;

use super::*;

fn texts(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .map(|r| r.line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn ended(status: ToolCallStatus, detail: Option<&str>) -> ToolCallEndEvent {
    ToolCallEndEvent {
        call_id: "c1".into(),
        status,
        detail: detail.map(str::to_string),
        duration_ms: 300,
        sandboxed: true,
        network: false,
    }
}

#[test]
fn your_message_wraps_behind_its_gutter() {
    let rows = Cell::user("explain how the tunnel guard restarts ssh\nthanks").rows(24, false);
    assert_eq!(
        texts(&rows),
        [
            "",
            "you › explain how the",
            "      tunnel guard",
            "      restarts ssh",
            "      thanks",
        ]
    );
}

#[test]
fn an_answer_is_markdown_after_a_blank_row() {
    let rows = Cell::answer("**bold** and `code`", true).rows(40, false);
    assert_eq!(texts(&rows), ["", "bold and code"]);
    let raw = Cell::answer("**bold**", false).rows(40, false);
    assert_eq!(texts(&raw), ["", "**bold**"]);
}

#[test]
fn notices_wrap_in_their_colour() {
    let notice = NoticeEvent {
        level: NoticeLevel::Warn,
        code: "x".into(),
        message: "tunnel down, reconnecting now".into(),
    };
    let rows = Cell::notice(&notice).rows(12, false);
    assert_eq!(texts(&rows), ["tunnel down,", "reconnecting", "now"]);
    assert_eq!(rows[0].line.spans[0].style.fg, Some(Color::Yellow));
}

#[test]
fn a_finished_card_is_one_row_with_its_result_on_the_right() {
    let mut card = ToolCard::new("c1", "run_shell", "rg -n tunnel_guard");
    card.end = Some(ended(ToolCallStatus::Completed, Some("exit 0")));
    let rows = Cell::Tool(card).rows(60, false);
    assert_eq!(
        texts(&rows),
        ["▸ run_shell  rg -n tunnel_guard       exit 0  sandbox  0.3 s"]
    );
    assert_eq!(rows[0].line.spans[0].style.fg, Some(Color::Green));
}

#[test]
fn a_long_summary_wraps_and_the_result_follows_it() {
    let mut card = ToolCard::new(
        "c1",
        "run_shell",
        "pytest -q tests/test_idle.py tests/test_pod.py",
    );
    card.end = Some(ended(ToolCallStatus::Failed, None));
    let rows = Cell::Tool(card).rows(30, false);
    assert_eq!(
        texts(&rows),
        [
            "▸ run_shell  pytest -q",
            "    tests/test_idle.py",
            "    tests/test_pod.py  sandbox",
            "    failed  0.3 s",
        ]
    );
    assert_eq!(rows[0].line.spans[0].style.fg, Some(Color::Red));
}

#[test]
fn output_shows_its_tail_folded_and_all_of_it_expanded() {
    let mut card = ToolCard::new("c1", "run_shell", "seq 5");
    card.output = "1\n2\n3\n4\n5\n".into();
    card.end = Some(ended(ToolCallStatus::Completed, Some("exit 0")));
    let cell = Cell::Tool(card);
    assert_eq!(
        texts(&cell.rows(50, false))[1..],
        [
            "  └ … +2 lines (Ctrl-T shows all)",
            "    3",
            "    4",
            "    5"
        ]
    );
    assert_eq!(
        texts(&cell.rows(50, true))[1..],
        ["  └ 1", "    2", "    3", "    4", "    5"]
    );
}

#[test]
fn a_running_card_spins_with_its_time() {
    let mut card = ToolCard::new("c1", "run_shell", "sleep 9");
    card.output = "a\nb\nc\nd".into();
    let running = Running {
        spinner: "⠹",
        elapsed: Duration::from_millis(4200),
    };
    let rows = card.rows(30, true, Some(running));
    assert_eq!(
        texts(&rows),
        [
            "⠹ run_shell  sleep 9     4.2 s",
            "  └ … +1 lines (Ctrl-T shows all)",
            "    b",
            "    c",
            "    d",
        ]
    );
}

#[test]
fn a_resumed_call_has_no_result() {
    let rows = Cell::Tool(ToolCard::new("c1", "read_file", "a.py")).rows(40, false);
    assert_eq!(texts(&rows), ["▸ read_file  a.py"]);
}

#[test]
fn reasoning_is_drawn_whole_or_folded() {
    let shown = Reasoning {
        text: "first I check\nthen".into(),
        took: Some(Duration::from_secs(12)),
        shown: true,
    };
    assert_eq!(
        texts(&shown.rows(40, false)),
        ["", "[thinking] first I check", "then"]
    );
    let folded = Reasoning {
        shown: false,
        ..shown.clone()
    };
    assert_eq!(
        texts(&folded.rows(40, false)),
        ["thought for 12.0 s · Ctrl-T shows it"]
    );
    assert_eq!(folded.rows(40, true), shown.rows(40, false));
    let resumed = Reasoning {
        text: String::new(),
        took: None,
        shown: false,
    };
    assert_eq!(texts(&resumed.rows(40, true)), ["thought"]);
}

#[test]
fn keys_grow_down_every_cell() {
    let mut card = ToolCard::new("c1", "run_shell", "x");
    card.output = "a\nb".into();
    let cells = [
        Cell::user("one two three four five six\nseven"),
        Cell::answer(
            "# h\n\ntext text text text text\n\n| a |\n|---|\n| b |",
            true,
        ),
        Cell::Tool(card),
    ];
    for cell in &cells {
        let rows = cell.rows(10, true);
        for pair in rows.windows(2) {
            assert!(pair[0].key < pair[1].key, "{pair:?}");
        }
    }
}
