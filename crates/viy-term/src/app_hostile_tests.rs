//! The hostile-string corpus through every widget (ui-plan 7, flags F36):
//! the emulated screen shows exactly the frame drawn, nothing set the
//! title, touched the clipboard, rang, asked for a report or changed a
//! mode, and the bytes written hold only the renderer's sequences.

use pretty_assertions::assert_eq;
use viy_protocol::Answer;
use viy_protocol::Choices;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::TitleChangedEvent;
use viy_protocol::ToolCallBeginEvent;
use viy_protocol::ToolCallEndEvent;
use viy_protocol::ToolCallOutputDeltaEvent;
use viy_protocol::ToolCallStatus;
use viy_testkit::hostile::CORPUS;
use viy_testkit::hostile::foreign_controls;
use viy_testkit::screen::cell_text;
use viy_testkit::screen::dump;

use super::*;

const WIDTH: u16 = 72;
const HEIGHT: u16 = 30;

/// Shows one string in a widget.
type Show = fn(&mut Fixture, &str);

/// Each widget, given one string.
const WIDGETS: &[(&str, Show)] = &[
    ("user message", |f, text| f.app.event(started(text))),
    ("answer being written", |f, text| {
        f.app.event(started("q"));
        f.app.event(delta(text));
    }),
    ("answer finished", |f, text| {
        f.app.event(started("q"));
        f.app.event(delta(text));
        f.app.event(complete());
    }),
    ("answer in one-char pieces", |f, text| {
        f.app.event(started("q"));
        for c in text.chars() {
            f.app.event(delta(&c.to_string()));
        }
    }),
    ("notice", |f, text| {
        for level in [NoticeLevel::Info, NoticeLevel::Warn, NoticeLevel::Error] {
            f.app.event(EventMsg::Notice(NoticeEvent {
                level,
                code: "test".into(),
                message: text.into(),
            }));
        }
    }),
    ("failed turn", |f, text| {
        f.app.event(started("q"));
        f.app.event(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "1".into(),
            outcome: TurnOutcome::Failed {
                message: text.into(),
            },
        }));
    }),
    ("tool call", |f, text| {
        f.app.event(started("q"));
        f.app.event(EventMsg::ToolCallBegin(ToolCallBeginEvent {
            call_id: "c1".into(),
            tool: text.into(),
            summary: text.into(),
        }));
    }),
    ("tool output, running and ended", |f, text| {
        f.app.event(started("q"));
        f.app.event(EventMsg::ToolCallBegin(ToolCallBeginEvent {
            call_id: "c1".into(),
            tool: "run_shell".into(),
            summary: "x".into(),
        }));
        f.app
            .event(EventMsg::ToolCallOutputDelta(ToolCallOutputDeltaEvent {
                call_id: "c1".into(),
                chunk: text.into(),
            }));
        f.screen();
        f.app.event(EventMsg::ToolCallEnd(ToolCallEndEvent {
            call_id: "c1".into(),
            status: ToolCallStatus::Failed,
            detail: Some(text.into()),
            duration_ms: 5,
            sandboxed: true,
            network: true,
        }));
        f.ctrl('t');
    }),
    ("reasoning, live and kept", |f, text| {
        f.app.event(started("q"));
        f.app.event(EventMsg::ReasoningContentDelta(DeltaEvent {
            delta: text.into(),
        }));
        f.screen();
        f.app.event(delta("a"));
    }),
    ("markdown around it", |f, text| {
        f.app.event(started("q"));
        let one = text.replace('\n', " ");
        f.app.event(delta(&format!(
            "# {one}\n\n[{one}]({one}) ![{one}]({one}) `{one}` *{one}* $${one}$$\n\n\
             | {one} | b |\n|---|---|\n| {one} | b |\n\n> {one}\n\n- [x] {one}\n\n\
             ```{one}\n{text}\n```\n"
        )));
        f.app.event(complete());
    }),
    ("title", |f, text| {
        f.app.event(EventMsg::TitleChanged(TitleChangedEvent {
            title: text.into(),
        }));
    }),
    ("question", |f, text| {
        f.app.event(EventMsg::QuestionRequest(QuestionRequestEvent {
            question_id: "q1".into(),
            question: Question {
                kind: Some(QuestionKind::Destructive),
                prompt: text.into(),
                choices: Choices::YesNo,
                default: Answer::No,
                unattended: Answer::No,
                options: Vec::new(),
                reason: vec![NoticeEvent {
                    level: NoticeLevel::Warn,
                    code: "test".into(),
                    message: text.into(),
                }],
            },
        }));
    }),
    ("history", |f, text| {
        f.app
            .event(EventMsg::HistoryRewritten(HistoryRewrittenEvent {
                reason: RewriteReason::Resume,
                items: vec![
                    HistoryItem::User { text: text.into() },
                    HistoryItem::Agent { text: text.into() },
                    HistoryItem::ToolCall {
                        call_id: "c1".into(),
                        tool: text.into(),
                        summary: text.into(),
                    },
                    HistoryItem::Notice(NoticeEvent {
                        level: NoticeLevel::Warn,
                        code: "test".into(),
                        message: text.into(),
                    }),
                ],
            }));
    }),
    ("paste into the input", |f, text| {
        f.input(Input::Paste(text.into()));
    }),
    ("typed into the input", |f, text| {
        for c in text.chars().filter(|&c| c != '\n') {
            f.key(KeyCode::Char(c));
        }
    }),
];

/// What the terminal shows is the frame drawn, and nothing else happened.
fn assert_untouched(f: &Fixture, what: &str) {
    let backend = f.term.backend();
    let parser = backend.vt100();
    let screen = parser.screen();
    let shown = dump(screen);
    assert_eq!(
        parser.callbacks().foreign(),
        Vec::<String>::new(),
        "{what}: side effects\n{shown}"
    );
    assert_eq!(
        foreign_controls(backend.written()),
        Vec::<String>::new(),
        "{what}: bytes\n{shown}"
    );
    assert!(!screen.alternate_screen(), "{what}: alternate screen");
    assert!(!screen.bracketed_paste(), "{what}: bracketed paste");
    assert!(!screen.application_cursor(), "{what}: application cursor");
    assert!(!screen.application_keypad(), "{what}: application keypad");
    assert_eq!(
        screen.mouse_protocol_mode(),
        vt100::MouseProtocolMode::None,
        "{what}: mouse"
    );
    let (frame, cursor) = f.term.last_frame();
    for y in 0..frame.area.height {
        for x in 0..frame.area.width {
            let Some(cell) = screen.cell(y, x) else {
                panic!("{what}: no cell at {x},{y}");
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let drawn = frame[(x, y)].symbol();
            assert_eq!(cell_text(cell), drawn, "{what}: cell {x},{y}\n{shown}");
        }
    }
    match cursor {
        Some(p) => {
            assert!(!screen.hide_cursor(), "{what}: cursor hidden\n{shown}");
            assert_eq!(
                screen.cursor_position(),
                (p.y, p.x),
                "{what}: cursor\n{shown}"
            );
        }
        None => assert!(screen.hide_cursor(), "{what}: cursor shown\n{shown}"),
    }
}

#[test]
fn hostile_strings_through_every_widget_leave_the_terminal_alone() {
    for (name, show) in WIDGETS {
        for text in CORPUS {
            let mut f = fixture(WIDTH, HEIGHT);
            f.screen();
            show(&mut f, text);
            let rows = f.screen();
            let what = format!("{name} with {text:?}");
            assert_untouched(&f, &what);
            if text.contains('\x1b') {
                assert!(
                    rows.iter().any(|r| r.contains('␛')),
                    "{what}: no ␛\n{}",
                    rows.join("\n")
                );
            }
        }
    }
}

/// The whole corpus in one answer, as one stream: an escape cut between
/// two pieces stays visible.
#[test]
fn the_whole_corpus_in_one_stream() {
    let mut f = fixture(WIDTH, HEIGHT);
    stream_corpus(&mut f);
    insta::assert_snapshot!("corpus_streamed", f.screen().join("\n"));
}

/// Scrolling back through it redraws every page the same way.
#[test]
fn the_corpus_scrolled_back_page_by_page() {
    let mut f = fixture(WIDTH, 8);
    stream_corpus(&mut f);
    for _ in 0..20 {
        f.key(KeyCode::PageUp);
        f.screen();
        assert_untouched(&f, "scrolled back");
    }
    // At the top: the user cell, its blank line first.
    assert_eq!(f.screen()[..2], ["", "you › show me"]);
    for _ in 0..20 {
        f.key(KeyCode::PageDown);
        f.screen();
        assert_untouched(&f, "scrolled forward");
    }
}

fn stream_corpus(f: &mut Fixture) {
    f.app.event(started("show me"));
    for text in CORPUS {
        f.app.event(delta(text));
        f.screen();
        assert_untouched(f, &format!("streaming {text:?}"));
    }
    f.app.event(delta("\n\nthe end"));
    f.app.event(complete());
    f.screen();
    assert_untouched(f, "finished");
}

/// The input box: a hostile paste is shown, the cursor stays at its end,
/// and Enter sends the text unchanged, since the engine, not the screen,
/// decides what reaches the model.
#[test]
fn a_hostile_paste_is_shown_in_the_input_and_sent_unchanged() {
    let mut f = fixture(WIDTH, 12);
    let text = "rm -rf ~\x1b[2K\x1b[1A\rls\u{202e}txt.exe\tx";
    f.input(Input::Paste(text.into()));
    insta::assert_snapshot!("hostile_paste", f.screen().join("\n"));
    assert_untouched(&f, "paste");
    f.key(KeyCode::Enter);
    assert_eq!(
        f.ops(),
        [Op::UserTurn {
            text: "rm -rf ~\x1b[2K\x1b[1A\nls\u{202e}txt.exe\tx".into()
        }]
    );
}

/// A widget that put an unsanitized string into the buffer is still caught
/// by the terminal's last check.
#[test]
fn the_terminal_scrubs_a_cell_no_widget_cleaned() {
    let mut term = Terminal::new(VT100Backend::new(20, 2)).unwrap();
    term.draw(|frame| {
        frame.buffer_mut()[(0, 0)].set_symbol("\x1b]0;t\x07");
        frame.buffer_mut()[(1, 0)].set_symbol("\u{202e}");
        frame.buffer_mut()[(2, 0)].set_symbol("ok");
    })
    .unwrap();
    let backend = term.backend();
    assert_eq!(backend.vt100().callbacks().foreign(), Vec::<String>::new());
    assert_eq!(foreign_controls(backend.written()), Vec::<String>::new());
    assert_eq!(backend.screen_rows()[0], "\u{fffd}\u{fffd}ok");
}
