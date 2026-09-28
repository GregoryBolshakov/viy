//! The footer and the tab title (WP 3.10): what they show is what the
//! engine's events said.

use pretty_assertions::assert_eq;
use viy_protocol::BackgroundEvent;
use viy_protocol::BackgroundState;
use viy_protocol::BackgroundTask;
use viy_protocol::EffortChangedEvent;
use viy_protocol::ModeChangedEvent;
use viy_protocol::PodState;
use viy_protocol::PodStatusEvent;
use viy_protocol::TitleChangedEvent;
use viy_protocol::TokenCountEvent;
use viy_testkit::hostile::CORPUS;
use viy_testkit::hostile::foreign_controls;

use super::*;
use crate::tabtitle;
use crate::tabtitle::Encoding;

fn statuses() -> Vec<EventMsg> {
    vec![
        EventMsg::ModeChanged(ModeChangedEvent {
            mode: "auto-yes".into(),
        }),
        EventMsg::EffortChanged(EffortChangedEvent {
            effort: "medium".into(),
        }),
        EventMsg::TokenCount(TokenCountEvent {
            used: 107_000,
            budget: 262_000,
            auto_compact_at: Some(222_700),
        }),
        EventMsg::PodStatus(PodStatusEvent {
            backend: "runpod".into(),
            model: None,
            state: PodState::Ready,
            gpu: Some("A100".into()),
            price_usd_per_hour: Some(1.79),
            spent_usd: Some(0.61),
            idle_seconds_left: Some(52 * 60),
            tunnel_up: Some(true),
        }),
        EventMsg::Background(BackgroundEvent {
            task: BackgroundTask::Title,
            state: BackgroundState::Started,
            progress: None,
        }),
        EventMsg::TitleChanged(TitleChangedEvent {
            title: "Pod costs".into(),
        }),
    ]
}

#[test]
fn the_footer_shows_the_engines_statuses() {
    let mut f = fixture(132, 6);
    for msg in statuses() {
        f.app.event(msg);
    }
    insta::assert_snapshot!(f.screen().join("\n"));
    // Narrow, the least important go first; what is left still fits.
    let mut f = fixture(60, 6);
    for msg in statuses() {
        f.app.event(msg);
    }
    f.app.event(started("q"));
    f.type_text("next");
    f.key(KeyCode::Enter);
    let footer = f.screen().pop().unwrap();
    assert_eq!(footer, "⏵⏵ auto-yes │ ctx 41% 107K/262K │ 1 queued");
}

/// A status ends nothing: a turn running goes on.
#[test]
fn statuses_mid_turn_leave_the_turn_alone() {
    let mut f = fixture(80, 8);
    f.app.event(started("q"));
    f.app.event(delta("half an answer"));
    for msg in statuses() {
        f.app.event(msg);
    }
    let screen = f.screen();
    assert!(
        screen.iter().any(|r| r.contains("half an answer")),
        "{screen:#?}"
    );
    // At 80 columns the effort is the first to go.
    assert_eq!(
        screen[7],
        "⏵⏵ auto-yes │ ctx 41% 107K/262K │ A100 1.79$/h · 0.61$ │ idle 52m │ tunnel ok"
    );
}

/// The titles written, in order, read from the bytes. The terminal saw
/// nothing else beyond the cells: OSC 0 sets the icon name and the title
/// (vt100 reports a title holding `;` as an OSC of more parameters, where
/// xterm and VTE take all after `0;` as the title).
fn titles(f: &Fixture) -> Vec<String> {
    let backend = f.term.backend();
    for e in backend.vt100().callbacks().foreign() {
        assert!(
            e.starts_with("title ") || e.starts_with("icon name ") || e.starts_with("osc [\"0\","),
            "{e}"
        );
    }
    let written = String::from_utf8_lossy(backend.written()).into_owned();
    written
        .split("\x1b]0;")
        .skip(1)
        .map(|rest| rest.split('\x07').next().unwrap_or_default().to_string())
        .collect()
}

/// rp's `test_untouched_until_opened`: a chat that did not turn the tab
/// title on never writes one, through a whole turn.
#[test]
fn the_tab_title_is_untouched_unless_turned_on() {
    let mut f = fixture(40, 6);
    f.screen();
    f.app.event(started("hello there"));
    f.screen();
    f.app.event(EventMsg::TitleChanged(TitleChangedEvent {
        title: "x".into(),
    }));
    f.app.event(complete());
    f.screen();
    assert_eq!(titles(&f), Vec::<String>::new());
}

/// rp's `test_spins_while_busy_and_restores_on_close` and
/// `test_turn_spins_and_approval_prompt_pauses` (the pause comes with WP
/// 3.11's question boxes): idle before, a spinner frame per frame drawn
/// during the turn, idle after, each written once.
#[test]
fn the_tab_title_spins_during_a_turn() {
    let mut f = fixture(40, 6);
    f.app.tab_title(Encoding::Unicode);
    f.screen();
    f.screen();
    assert_eq!(titles(&f), ["✳ viy chat"]);
    f.app.event(started("hello there"));
    f.app.event(EventMsg::TitleChanged(TitleChangedEvent {
        title: "hello there".into(),
    }));
    for _ in 0..3 {
        f.screen();
        if let Some(turn) = &mut f.app.turn {
            turn.started -= SPINNER_FRAME;
        }
    }
    f.app.event(complete());
    f.screen();
    assert_eq!(
        titles(&f),
        [
            "✳ viy chat",
            "⠋ hello there",
            "⠙ hello there",
            "⠹ hello there",
            "✳ hello there"
        ]
    );
    // Written with the frame: each OSC 0 sits in the frame's bytes.
    let written = String::from_utf8_lossy(f.term.backend().written()).into_owned();
    assert_eq!(written.matches("\x1b]0;").count(), 5);
    // After Ctrl-Z the terminal shows the shell's title: written again.
    f.app.retitle();
    f.screen();
    assert_eq!(titles(&f).last().unwrap(), "✳ hello there");
    assert_eq!(titles(&f).len(), 6);
}

/// The hostile corpus as the chat's name: the title written is the
/// cleaned name and nothing else reaches the terminal.
#[test]
fn a_hostile_title_is_cleaned_before_the_tab_shows_it() {
    for text in CORPUS {
        let mut f = fixture(72, 8);
        f.app.tab_title(Encoding::Unicode);
        f.app.event(EventMsg::TitleChanged(TitleChangedEvent {
            title: (*text).into(),
        }));
        f.screen();
        let want = tabtitle::title(Some(text), None, Encoding::Unicode);
        assert!(!want.chars().any(char::is_control), "{text:?}: {want:?}");
        assert_eq!(titles(&f), std::slice::from_ref(&want), "{text:?}");
        let written =
            String::from_utf8_lossy(f.term.backend().written()).replace(&tabtitle::osc(&want), "");
        assert_eq!(
            foreign_controls(written.as_bytes()),
            Vec::<String>::new(),
            "{text:?}"
        );
    }
}

/// Enter without a ready pod (kb d041): the engine refuses the line, and
/// it is back in the input box, ahead of anything typed since; nothing of
/// it is in the transcript but the reason.
#[test]
fn a_refused_line_goes_back_into_the_input_box() {
    let mut f = fixture(70, 8);
    f.type_text("what is 2+2");
    f.key(KeyCode::Enter);
    assert_eq!(
        f.ops(),
        [Op::UserTurn {
            text: "what is 2+2".into()
        }]
    );
    f.type_text("and");
    f.app
        .event(EventMsg::TurnRefused(viy_protocol::TurnRefusedEvent {
            text: "what is 2+2".into(),
            message: "no pod is running: /up starts one.".into(),
        }));
    let screen = f.screen();
    assert!(
        screen.contains(&"no pod is running: /up starts one.".to_string()),
        "{screen:#?}"
    );
    assert!(
        !screen.iter().any(|l| l.starts_with("you ›")),
        "{screen:#?}"
    );
    assert!(screen.contains(&"› what is 2+2".to_string()), "{screen:#?}");
    assert!(screen.contains(&"  and▮".to_string()), "{screen:#?}");
    // Nothing is queued any more.
    assert!(!f.screen().last().unwrap().contains("queued"));
}

/// Ctrl-C with no turn stops a pod command running beside the chat; only
/// with nothing running does it close the chat.
#[test]
fn ctrl_c_stops_pod_work_before_it_closes_the_chat() {
    let mut f = fixture(70, 8);
    let up = BackgroundTask::Pod {
        command: "up".into(),
        job: 1,
    };
    let background = |state| {
        EventMsg::Background(BackgroundEvent {
            task: up.clone(),
            state,
            progress: None,
        })
    };
    f.app.event(background(BackgroundState::Started));
    assert!(f.screen().last().unwrap().contains("⟳ /up"));
    f.ctrl('c');
    assert_eq!(f.ops(), [Op::Interrupt]);
    f.app.event(background(BackgroundState::Failed));
    f.ctrl('c');
    assert_eq!(f.ops(), [Op::Shutdown]);
}

/// Lines refused one after another come back in the order they were sent,
/// ahead of what was typed since.
#[test]
fn refused_lines_come_back_in_order() {
    let mut f = fixture(70, 10);
    for line in ["first", "second"] {
        f.type_text(line);
        f.key(KeyCode::Enter);
    }
    f.type_text("third");
    let refused = |text: &str| {
        EventMsg::TurnRefused(viy_protocol::TurnRefusedEvent {
            text: text.into(),
            message: "no pod is running: /up starts one.".into(),
        })
    };
    f.app.event(refused("first"));
    f.app.event(refused("second"));
    assert_eq!(f.app.composer.text(), "first\nsecond\nthird");
}

/// Ctrl-C while a line is admitted (sent, not started) stops it, and the
/// line comes back to the input box.
#[test]
fn ctrl_c_while_a_line_is_admitted_gives_it_back() {
    let mut f = fixture(70, 8);
    f.type_text("hello");
    f.key(KeyCode::Enter);
    f.ops();
    f.ctrl('c');
    assert_eq!(f.ops(), [Op::Interrupt]);
    f.app
        .event(EventMsg::TurnAborted(viy_protocol::TurnAbortedEvent {
            turn_id: "1".into(),
            reason: viy_protocol::TurnAbortReason::Interrupted,
        }));
    assert_eq!(f.app.composer.text(), "hello");
    assert!(!f.screen().iter().any(|l| l.contains("interrupted")));
}

/// A draft taller than the screen shows the rows around the cursor: the
/// footer stays (a pasted 268 KB message pushed it off, seen live).
#[test]
fn a_tall_draft_leaves_the_footer_on_screen() {
    let mut f = fixture(60, 10);
    f.app.event(EventMsg::EffortChanged(EffortChangedEvent {
        effort: "medium".into(),
    }));
    let draft: Vec<String> = (1..=100).map(|n| format!("line {n}")).collect();
    f.app.composer.set_text(&draft.join("\n"));
    let screen = f.screen();
    assert_eq!(
        screen.last().map(String::as_str),
        Some("effort medium"),
        "{screen:#?}"
    );
    // The cursor, at the end, is on screen with the last rows.
    assert!(
        screen.iter().any(|l| l.contains("line 100▮")),
        "{screen:#?}"
    );
}
