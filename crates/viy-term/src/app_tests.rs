use pretty_assertions::assert_eq;
use viy_protocol::Answer;
use viy_protocol::DeltaEvent;
use viy_protocol::HistoryRewrittenEvent;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::QuestionKind;
use viy_protocol::QuestionRequestEvent;
use viy_protocol::QuestionResolvedEvent;
use viy_protocol::RewriteReason;
use viy_protocol::TurnAbortReason;
use viy_protocol::TurnAbortedEvent;
use viy_protocol::TurnCompleteEvent;
use viy_protocol::TurnStartedEvent;

use super::*;
use crate::test_backend::VT100Backend;

struct Fixture {
    app: App,
    term: Terminal<VT100Backend>,
    ops: mpsc::UnboundedReceiver<Op>,
    /// When the next key is pressed: keys come at a typist's pace, not as
    /// a keystroke paste.
    clock: Instant,
}

fn fixture(width: u16, height: u16) -> Fixture {
    let (tx, ops) = mpsc::unbounded_channel();
    Fixture {
        app: App::new(tx, History::default()),
        term: Terminal::new(VT100Backend::new(width, height)).unwrap(),
        ops,
        clock: Instant::now(),
    }
}

impl Fixture {
    /// The screen's rows, `▮` at viy's caret (the terminal's own cursor
    /// stays hidden).
    fn screen(&mut self) -> Vec<String> {
        self.app.draw_at(&mut self.term, self.clock);
        self.rows()
    }

    /// The screen as it stands, `▮` at the caret drawn last.
    fn rows(&self) -> Vec<String> {
        let mut rows = self.term.backend().screen_rows();
        if let Some(at) = self.app.caret.filter(|at| usize::from(at.y) < rows.len()) {
            let row = &mut rows[usize::from(at.y)];
            let mut chars: Vec<char> = row.chars().collect();
            let x = usize::from(at.x);
            if chars.len() <= x {
                chars.resize(x + 1, ' ');
            }
            chars[x] = '▮';
            *row = chars.into_iter().collect();
        }
        rows
    }

    fn ops(&mut self) -> Vec<Op> {
        std::iter::from_fn(|| self.ops.try_recv().ok()).collect()
    }

    fn key(&mut self, code: KeyCode) -> Option<Flow> {
        self.press(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(&mut self, c: char) -> Option<Flow> {
        self.press(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    /// A key pressed 50 ms after the last.
    fn press(&mut self, key: KeyEvent) -> Option<Flow> {
        self.clock += Duration::from_millis(50);
        self.app.dirty = true;
        self.app.key(key, self.clock)
    }

    fn input(&mut self, input: Input) -> Option<Flow> {
        self.app.on_input(input, &mut self.term)
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(KeyCode::Char(c));
        }
    }
}

fn started(text: &str) -> EventMsg {
    EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "1".into(),
        user_text: text.into(),
    })
}

fn delta(text: &str) -> EventMsg {
    EventMsg::AgentMessageContentDelta(DeltaEvent { delta: text.into() })
}

fn complete() -> EventMsg {
    EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "1".into(),
        outcome: TurnOutcome::Answered,
    })
}

fn notice(text: &str) -> EventMsg {
    EventMsg::Notice(NoticeEvent {
        level: NoticeLevel::Info,
        code: "test".into(),
        message: text.into(),
    })
}

fn event(msg: EventMsg) -> Event {
    Event {
        id: "1".into(),
        msg,
    }
}

fn numbered(f: &mut Fixture, n: usize) {
    for i in 1..=n {
        f.app.event(notice(&format!("line {i}")));
    }
}

#[test]
fn an_idle_chat_is_the_input_and_the_footer() {
    let mut f = fixture(30, 5);
    assert_eq!(f.screen(), ["", "", "", "› ▮", ""]);
}

/// The terminal's cursor stays hidden at the top left, where a terminal
/// made shorter has nothing to scroll up for (GNOME Terminal scrolls its
/// screen to keep the cursor in view before viy repaints); the input box
/// shows viy's own caret, a blinking reversed block.
#[test]
fn the_caret_is_drawn_and_the_cursor_waits_at_the_top_left() {
    let mut f = fixture(30, 5);
    assert_eq!(f.screen(), ["", "", "", "› ▮", ""]);
    let screen = f.term.backend().vt100().screen();
    assert!(screen.hide_cursor());
    assert_eq!(screen.cursor_position(), (0, 0));
    let caret = screen.cell(3, 2).unwrap();
    assert!(caret.inverse());
    f.term.backend_mut().resize(30, 4);
    f.input(Input::Resize(30, 4));
    assert_eq!(f.screen(), ["", "", "› ▮", ""]);
    assert_eq!(f.term.backend().vt100().screen().cursor_position(), (0, 0));
}

#[test]
fn enter_sends_the_line_and_the_turn_shows_it() {
    let mut f = fixture(40, 6);
    f.type_text("hello");
    assert_eq!(f.screen()[4], "› hello▮");
    f.key(KeyCode::Enter);
    assert_eq!(
        f.ops(),
        [Op::UserTurn {
            text: "hello".into()
        }]
    );
    f.app.event(started("hello"));
    let screen = f.screen();
    assert_eq!(screen[..3], ["", "", "you › hello"]);
    assert!(screen[3].starts_with("⠋ waiting · 0 s"), "{screen:?}");
    assert_eq!(screen[4..], ["› ▮", ""]);
}

#[test]
fn a_slash_line_is_a_command_and_a_blank_one_nothing() {
    let mut f = fixture(30, 4);
    f.type_text("   ");
    f.key(KeyCode::Enter);
    assert_eq!(f.ops(), []);
    f.ctrl('c');
    f.type_text("/stats");
    f.key(KeyCode::Enter);
    assert_eq!(
        f.ops(),
        [Op::Command {
            text: "/stats".into()
        }]
    );
}

#[test]
fn ctrl_j_and_a_trailing_backslash_add_a_line() {
    let mut f = fixture(30, 6);
    f.type_text("one");
    f.ctrl('j');
    f.type_text("two\\");
    f.key(KeyCode::Enter);
    f.type_text("three");
    assert_eq!(f.screen()[2..], ["› one", "  two", "  three▮", ""]);
    f.key(KeyCode::Enter);
    assert_eq!(
        f.ops(),
        [Op::UserTurn {
            text: "one\ntwo\nthree".into()
        }]
    );
}

#[test]
fn a_paste_is_inserted_and_never_sends() {
    let mut f = fixture(30, 5);
    f.input(Input::Paste("a\r\nb".into()));
    assert_eq!(f.ops(), []);
    assert_eq!(f.screen()[2..], ["› a", "  b▮", ""]);
}

#[test]
fn finished_blocks_move_up_and_the_open_one_streams_below() {
    let mut f = fixture(40, 11);
    f.app.event(started("explain"));
    f.app.event(delta("First **para"));
    f.app.event(delta("graph**.\n\nSecond "));
    let screen = f.screen();
    // One block so far: all of it the transcript's live tail, above the
    // activity line.
    assert_eq!(
        screen[..8],
        [
            "",
            "",
            "",
            "you › explain",
            "",
            "First paragraph.",
            "",
            "Second"
        ]
    );
    assert!(screen[8].contains("writing ·"), "{screen:?}");
    assert_eq!(screen[9..], ["› ▮", ""]);
    f.app.event(delta("one.\nmore"));
    let screen = f.screen();
    let streamed = [
        "",
        "you › explain",
        "",
        "First paragraph.",
        "",
        "Second one.",
        "more",
    ];
    assert_eq!(screen[1..8], streamed);
    assert!(screen[8].contains("writing ·"), "{screen:?}");
    // The finished block moved up to the row it was drawn at.
    f.app.event(complete());
    let screen = f.screen();
    assert_eq!(screen[2..9], streamed);
    assert_eq!(screen[9..], ["› ▮", ""]);
}

#[test]
fn a_blank_line_inside_a_code_fence_keeps_the_block_live() {
    let mut f = fixture(40, 12);
    f.app.event(started("code"));
    f.app.event(delta("```\nfn a() {}\n\nfn b() {}\n"));
    let screen = f.screen();
    assert_eq!(
        screen[..9],
        [
            "",
            "",
            "you › code",
            "",
            "╭─",
            "fn a() {}",
            "",
            "fn b() {}",
            "╰─"
        ]
    );
    assert!(screen[9].contains("writing ·"), "{screen:?}");
    f.app.event(delta("```\n\nAfter.\n"));
    let screen = f.screen();
    assert_eq!(
        screen[..9],
        [
            "you › code",
            "",
            "╭─",
            "fn a() {}",
            "",
            "fn b() {}",
            "╰─",
            "",
            "After."
        ]
    );
}

#[test]
fn notices_and_failures_go_to_the_transcript() {
    let mut f = fixture(40, 6);
    f.app.event(started("hi"));
    f.app.event(notice("[context: 3% of 262k]"));
    f.app.event(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "1".into(),
        outcome: TurnOutcome::Failed {
            message: "the request failed".into(),
        },
    }));
    assert_eq!(
        f.screen(),
        [
            "",
            "you › hi",
            "[context: 3% of 262k]",
            "✗ the request failed",
            "› ▮",
            ""
        ]
    );
}

#[test]
fn ctrl_c_interrupts_a_turn_then_clears_the_input_then_closes() {
    let mut f = fixture(40, 6);
    f.app.event(started("hi"));
    f.app.event(delta("partial"));
    assert_eq!(f.ctrl('c'), None);
    assert_eq!(f.ops(), [Op::Interrupt]);
    f.app.event(EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: "1".into(),
        reason: TurnAbortReason::Interrupted,
    }));
    assert_eq!(f.screen()[1..4], ["", "partial", "■ interrupted"]);
    f.type_text("draft");
    f.ctrl('c');
    assert_eq!(f.ops(), []);
    assert_eq!(f.screen()[4], "› ▮");
    f.ctrl('c');
    assert_eq!(f.ops(), [Op::Shutdown]);
    assert_eq!(f.screen()[3..], ["closing the chat…", "› ▮", ""]);
    // A second Ctrl-C while closing leaves at once.
    assert_eq!(f.ctrl('c'), Some(Flow::Exit));
}

#[test]
fn ctrl_d_closes_only_at_an_empty_idle_prompt() {
    let mut f = fixture(30, 4);
    f.type_text("x");
    f.ctrl('d');
    assert_eq!(f.ops(), []);
    f.key(KeyCode::Backspace);
    f.ctrl('d');
    assert_eq!(f.ops(), [Op::Shutdown]);
}

#[test]
fn ctrl_z_suspends() {
    let mut f = fixture(30, 4);
    assert_eq!(f.ctrl('z'), Some(Flow::Suspend));
}

#[test]
fn lines_sent_during_a_turn_are_shown_queued() {
    let mut f = fixture(40, 6);
    f.type_text("one");
    f.key(KeyCode::Enter);
    f.app.event(started("one"));
    f.type_text("two");
    f.key(KeyCode::Enter);
    let screen = f.screen();
    assert_eq!(screen[3], "↳ two", "{screen:?}");
    assert_eq!(screen[5], "1 queued");
    f.app.event(complete());
    f.app.event(started("two"));
    let screen = f.screen();
    assert_eq!(screen[5], "");
    assert!(!screen.iter().any(|r| r.starts_with('↳')), "{screen:?}");
}

#[test]
fn page_keys_and_the_wheel_scroll_the_transcript() {
    let mut f = fixture(30, 6);
    numbered(&mut f, 20);
    assert_eq!(
        f.screen(),
        ["line 17", "line 18", "line 19", "line 20", "› ▮", ""]
    );
    f.key(KeyCode::PageUp);
    assert_eq!(
        f.screen(),
        [
            "line 14",
            "line 15",
            "line 16",
            "line 17",
            "› ▮",
            "↓ 3 rows (End)"
        ]
    );
    f.input(Input::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(f.screen()[0], "line 11");
    // Rows arriving while scrolled back leave the view where it is.
    f.app.event(notice("line 21"));
    assert_eq!(f.screen()[5], "↓ 7 rows (End)");
    f.key(KeyCode::End);
    assert_eq!(
        f.screen(),
        ["line 18", "line 19", "line 20", "line 21", "› ▮", ""]
    );
}

#[test]
fn a_rewritten_history_is_drawn_from_its_items() {
    let mut f = fixture(30, 7);
    numbered(&mut f, 3);
    f.app
        .event(EventMsg::HistoryRewritten(HistoryRewrittenEvent {
            reason: RewriteReason::Resume,
            items: vec![
                HistoryItem::User { text: "q".into() },
                HistoryItem::Reasoning { text: "hm".into() },
                HistoryItem::Agent { text: "a".into() },
            ],
        }));
    assert_eq!(
        f.screen(),
        [
            "",
            "you › q",
            "thought · Ctrl-T shows it",
            "",
            "a",
            "› ▮",
            ""
        ]
    );
}

#[test]
fn the_block_being_written_scrolls_with_the_transcript() {
    let mut f = fixture(30, 6);
    numbered(&mut f, 5);
    f.app.event(started("long"));
    f.app.event(delta("a\nb\nc\nd\ne\nf"));
    let screen = f.screen();
    assert_eq!(screen[..3], ["d", "e", "f"]);
    assert!(screen[3].contains("writing ·"), "{screen:?}");
    f.key(KeyCode::PageUp);
    let screen = f.screen();
    assert_eq!(screen[..3], ["b", "c", "d"]);
    assert!(screen[3].contains("writing ·"), "{screen:?}");
    assert_eq!(screen[5], "↓ 2 rows (End)");
    // The block grows below the view, which holds still, and moving up
    // into the transcript when it ends moves it no row.
    f.app.event(delta("\ng"));
    assert_eq!(f.screen()[..3], ["b", "c", "d"]);
    assert_eq!(f.rows()[5], "↓ 3 rows (End)");
    f.app.event(delta("\n\nnext\n"));
    assert_eq!(f.screen()[..3], ["b", "c", "d"]);
    assert_eq!(f.rows()[5], "↓ 5 rows (End)");
    f.key(KeyCode::End);
    assert_eq!(f.screen()[..3], ["g", "", "next"]);
    // Followed again, the tail's new rows show as they come.
    f.app.event(delta("more\n"));
    assert_eq!(f.screen()[..3], ["", "next", "more"]);
}

#[test]
fn reasoning_and_running_tools_scroll_with_the_transcript() {
    use viy_protocol::ToolCallBeginEvent;
    use viy_protocol::ToolCallOutputDeltaEvent;

    let mut f = fixture(40, 7);
    numbered(&mut f, 3);
    f.app.event(started("go"));
    f.app.event(EventMsg::ReasoningContentDelta(DeltaEvent {
        delta: "one\ntwo\nthree\nfour".into(),
    }));
    let screen = f.screen();
    assert_eq!(screen[..4], ["[thinking] one", "two", "three", "four"]);
    assert!(screen[4].contains("thinking ·"), "{screen:?}");
    f.app.event(EventMsg::ToolCallBegin(ToolCallBeginEvent {
        call_id: "c1".into(),
        tool: "run_shell".into(),
        summary: "ls".into(),
    }));
    f.app
        .event(EventMsg::ToolCallOutputDelta(ToolCallOutputDeltaEvent {
            call_id: "c1".into(),
            chunk: "x\n".into(),
        }));
    let screen = f.screen();
    assert!(screen[2].contains("run_shell  ls"), "{screen:?}");
    assert_eq!(screen[3], "  └ x");
    assert!(screen[4].contains("running tools ·"), "{screen:?}");
    f.key(KeyCode::PageUp);
    let screen = f.screen();
    assert_eq!(screen[..4], ["", "[thinking] one", "two", "three"]);
    assert!(screen[4].contains("running tools ·"), "{screen:?}");
}

#[test]
fn ctrl_l_rewrites_every_cell() {
    let mut f = fixture(10, 3);
    f.screen();
    let before = f.term.backend().bytes_written();
    f.screen();
    assert_eq!(f.term.backend().bytes_written(), before);
    f.ctrl('l');
    f.screen();
    let written = &f.term.backend().written()[before..];
    // 30 cells, each written again; no clear.
    assert!(written.len() >= 30, "{written:?}");
    assert!(!written.windows(4).any(|w| w == b"\x1b[2J"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn deltas_are_batched_into_at_most_thirty_frames_a_second() {
    let mut f = fixture(40, 6);
    let (input_tx, mut input) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let driver = async move {
        for i in 0..100 {
            events_tx.send(event(notice(&format!("line {i}")))).unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        events_tx.send(event(EventMsg::ShutdownComplete)).unwrap();
        drop(input_tx);
    };
    let started = Instant::now();
    let (flow, ()) = tokio::join!(f.app.run(&mut f.term, &mut input, &mut events), driver);
    assert_eq!(flow.unwrap(), Flow::Exit);
    let elapsed = started.elapsed();
    let frames = f
        .term
        .backend()
        .written()
        .windows(8)
        .filter(|w| w == b"\x1b[?2026h")
        .count();
    let allowed = (elapsed.as_secs_f64() * 30.0).ceil() as usize + 1;
    assert!(
        (2..=allowed).contains(&frames),
        "{frames} frames in {elapsed:?}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_resize_reflows_in_one_frame_without_clearing() {
    let mut f = fixture(20, 5);
    f.app.event(notice("aaaa bbbb cccc dddd"));
    assert_eq!(f.screen()[..3], ["", "", "aaaa bbbb cccc dddd"]);
    let before = f.term.backend().bytes_written();
    f.term.backend_mut().resize(10, 5);
    let (input_tx, mut input) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let driver = async move {
        input_tx.send(Input::Resize(10, 5)).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        events_tx.send(event(EventMsg::ShutdownComplete)).unwrap();
    };
    let (flow, ()) = tokio::join!(f.app.run(&mut f.term, &mut input, &mut events), driver);
    assert_eq!(flow.unwrap(), Flow::Exit);
    assert_eq!(f.rows(), ["", "aaaa bbbb", "cccc dddd", "› ▮", ""]);
    let written = &f.term.backend().written()[before..];
    assert_eq!(
        written.windows(8).filter(|w| w == b"\x1b[?2026h").count(),
        1
    );
    assert!(!written.windows(4).any(|w| w == b"\x1b[2J"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_hangup_closes_the_chat_without_drawing() {
    let mut f = fixture(20, 4);
    f.screen();
    let before = f.term.backend().bytes_written();
    let (input_tx, mut input) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    input_tx.send(Input::Hangup).unwrap();
    let driver = async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        events_tx.send(event(notice("saved"))).unwrap();
        events_tx.send(event(EventMsg::ShutdownComplete)).unwrap();
    };
    let (flow, ()) = tokio::join!(f.app.run(&mut f.term, &mut input, &mut events), driver);
    assert!(flow.is_err());
    assert_eq!(f.ops(), [Op::Shutdown]);
    assert_eq!(f.term.backend().bytes_written(), before);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sigterm_closes_the_chat_once_the_engine_has_saved_it() {
    let mut f = fixture(30, 5);
    let (input_tx, mut input) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    input_tx.send(Input::Terminate).unwrap();
    let driver = async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        events_tx.send(event(EventMsg::ShutdownComplete)).unwrap();
    };
    let (flow, ()) = tokio::join!(f.app.run(&mut f.term, &mut input, &mut events), driver);
    assert_eq!(flow.unwrap(), Flow::Exit);
    assert_eq!(f.ops(), [Op::Shutdown]);
    assert_eq!(f.rows(), ["", "", "closing the chat…", "› ▮", ""]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_loop_ends_when_the_engine_stops() {
    let mut f = fixture(30, 4);
    let (_input_tx, mut input) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
    drop(events_tx);
    let flow = f.app.run(&mut f.term, &mut input, &mut events).await;
    assert_eq!(flow.unwrap(), Flow::Exit);
}

#[path = "app_hostile_tests.rs"]
mod hostile;

#[path = "app_status_tests.rs"]
mod status;

#[path = "app_question_tests.rs"]
mod question;

#[path = "app_repaint_tests.rs"]
mod repaint;

/// Typing `/` opens the popup of commands above the input; Tab takes the
/// choice, and its values follow.
#[test]
fn the_slash_popup_is_drawn_above_the_input() {
    let mut f = fixture(60, 8);
    f.type_text("/ef");
    let screen = f.screen();
    assert_eq!(
        screen[5], "  /effort [xhigh|medium|low]  how hard the model thinks",
        "{screen:?}"
    );
    assert_eq!(screen[6], "› /ef▮");
    f.key(KeyCode::Tab);
    let screen = f.screen();
    assert_eq!(
        screen[3..6],
        [
            "  xhigh   how hard the model thinks",
            "  medium  how hard the model thinks",
            "  low     how hard the model thinks",
        ],
        "{screen:?}"
    );
    assert_eq!(screen[6], "› /effort ▮");
}

/// Shift+Tab asks the engine for the next mode; the mode it names shows
/// in the footer unless it is the normal one.
#[test]
fn shift_tab_asks_for_the_next_mode_and_the_footer_shows_it() {
    let mut f = fixture(40, 4);
    f.key(KeyCode::BackTab);
    assert_eq!(f.ops(), [Op::CycleMode { question: None }]);
    f.app
        .event(EventMsg::ModeChanged(viy_protocol::ModeChangedEvent {
            mode: "auto-yes".into(),
            label: "⏵⏵ auto-yes".into(),
        }));
    assert_eq!(f.screen()[3], "⏵⏵ auto-yes");
    f.app
        .event(EventMsg::ModeChanged(viy_protocol::ModeChangedEvent {
            mode: "normal".into(),
            label: "".into(),
        }));
    assert_eq!(f.screen()[3], "");
}

/// rpctl test_modes PromptShiftTabTests.test_shift_tab_keeps_the_typed_text:
/// Shift+Tab while typing changes the mode and keeps the draft.
#[test]
fn shift_tab_keeps_the_typed_text() {
    let mut f = fixture(40, 4);
    for c in "hel".chars() {
        f.key(KeyCode::Char(c));
    }
    f.key(KeyCode::BackTab);
    for c in "lo".chars() {
        f.key(KeyCode::Char(c));
    }
    assert_eq!(f.ops(), [Op::CycleMode { question: None }]);
    assert!(
        f.screen().iter().any(|row| row.starts_with("› hello")),
        "{:?}",
        f.screen()
    );
}

/// A message sent goes into the history; Up brings it back.
#[test]
fn up_brings_back_what_was_sent() {
    let mut f = fixture(40, 4);
    f.type_text("first message");
    f.key(KeyCode::Enter);
    f.ops();
    f.key(KeyCode::Up);
    assert_eq!(f.screen()[2], "› first message▮");
}

/// A turn with every kind of cell: reasoning, an answer in Markdown, a
/// tool call with output, a notice, and the answer after it.
fn a_whole_turn(f: &mut Fixture) {
    use viy_protocol::ToolCallBeginEvent;
    use viy_protocol::ToolCallEndEvent;
    use viy_protocol::ToolCallOutputDeltaEvent;
    use viy_protocol::ToolCallStatus;

    f.app.event(started("which GPU?"));
    f.app.event(EventMsg::ReasoningContentDelta(DeltaEvent {
        delta: "The user wants a table.".into(),
    }));
    f.app.event(delta(
        "## Prices\n\n| GPU | $/h |\n|:--|--:|\n| A100 | 1.79 |\n",
    ));
    f.app.event(EventMsg::ToolCallBegin(ToolCallBeginEvent {
        call_id: "c1".into(),
        tool: "run_shell".into(),
        summary: "seq 6".into(),
    }));
    f.app
        .event(EventMsg::ToolCallOutputDelta(ToolCallOutputDeltaEvent {
            call_id: "c1".into(),
            chunk: "1\n2\n3\n4\n5\n6\n".into(),
        }));
    let running = f.screen().join("\n");
    assert!(running.contains("run_shell  seq 6"), "{running}");
    assert!(
        running.contains("  └ … +3 lines (Ctrl-T shows all)"),
        "{running}"
    );
    f.app.event(EventMsg::ToolCallEnd(ToolCallEndEvent {
        call_id: "c1".into(),
        status: ToolCallStatus::Completed,
        detail: Some("exit 0".into()),
        duration_ms: 40,
        sandboxed: true,
        network: false,
    }));
    f.app.event(notice("review: low -- prints numbers"));
    f.app.event(delta("Take the **A100**."));
    f.app.event(complete());
}

#[test]
fn a_whole_turn_folded() {
    let mut f = fixture(48, 22);
    a_whole_turn(&mut f);
    insta::assert_snapshot!(f.screen().join("\n"));
}

#[test]
fn ctrl_t_draws_tool_output_whole() {
    let mut f = fixture(48, 26);
    a_whole_turn(&mut f);
    f.ctrl('t');
    insta::assert_snapshot!(f.screen().join("\n"));
    f.ctrl('t');
    assert!(f.screen().join("\n").contains("… +3 lines"));
}
