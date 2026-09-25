use pretty_assertions::assert_eq;
use ratatui::style::Color;
use viy_protocol::BackgroundEvent;
use viy_protocol::EffortChangedEvent;
use viy_protocol::ModeChangedEvent;
use viy_protocol::TitleChangedEvent;

use super::*;

fn text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The span holding `part`, for its colour.
fn colour(line: &Line, part: &str) -> Option<Color> {
    line.spans
        .iter()
        .find(|s| s.content.contains(part))
        .and_then(|s| s.style.fg)
}

fn pod(state: PodState) -> PodStatusEvent {
    PodStatusEvent {
        backend: "runpod".into(),
        model: None,
        state,
        gpu: Some("A100".into()),
        price_usd_per_hour: Some(1.79),
        spent_usd: None,
        idle_seconds_left: Some(52 * 60),
        tunnel_up: Some(true),
    }
}

fn full(now: Instant) -> Status {
    let mut s = Status::default();
    for msg in [
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
        EventMsg::PodStatus(pod(PodState::Ready)),
    ] {
        assert!(s.event(&msg, now));
    }
    s
}

#[test]
fn the_layout_of_ui_plan_3() {
    let now = Instant::now();
    let line = full(now).line(Counts::default(), 100, now);
    assert_eq!(
        text(&line),
        "⏵⏵ auto-yes │ effort medium │ ctx 41% 107K/262K │ A100 1.79$/h │ idle 52m │ tunnel ok"
    );
    assert_eq!(colour(&line, "auto-yes"), Some(Color::Yellow));
}

#[test]
fn an_idle_chat_without_statuses_is_empty() {
    let now = Instant::now();
    assert_eq!(
        text(&Status::default().line(Counts::default(), 80, now)),
        ""
    );
    // The normal mode is not named, as rp's prompt does not name it.
    let mut s = Status::default();
    s.event(
        &EventMsg::ModeChanged(ModeChangedEvent {
            mode: "normal".into(),
        }),
        now,
    );
    assert_eq!(text(&s.line(Counts::default(), 80, now)), "");
}

#[test]
fn narrow_drops_the_least_important_first() {
    let now = Instant::now();
    let mut s = full(now);
    s.title = Some("Pod costs".into());
    let counts = Counts {
        questions: 2,
        queued: 1,
        rows_below: 21,
    };
    let wide = text(&s.line(counts, 200, now));
    assert_eq!(
        wide,
        "⏵⏵ auto-yes │ effort medium │ ctx 41% 107K/262K │ A100 1.79$/h │ idle 52m │ \
         tunnel ok │ 2 questions waiting │ 1 queued │ Pod costs │ ↓ 21 rows (End)"
    );
    let mut last = usize::MAX;
    for width in (0..=140).rev() {
        let line = s.line(counts, width, now);
        let shown = text(&line);
        assert!(text_width(&shown) <= width, "{width}: {shown}");
        let n = line.spans.len();
        assert!(n <= last, "{width}: an item came back");
        last = n;
    }
    assert_eq!(
        text(&s.line(counts, 62, now)),
        "⏵⏵ auto-yes │ 2 questions waiting │ 1 queued │ ↓ 21 rows (End)"
    );
    assert_eq!(
        text(&s.line(counts, 61, now)),
        "⏵⏵ auto-yes │ 2 questions waiting │ ↓ 21 rows (End)"
    );
    assert_eq!(
        text(&s.line(counts, 40, now)),
        "2 questions waiting │ ↓ 21 rows (End)"
    );
    assert_eq!(text(&s.line(counts, 10, now)), "");
}

#[test]
fn the_gauge_takes_rps_prompt_colours() {
    let now = Instant::now();
    let gauge = |used, budget, auto| {
        let mut s = Status::default();
        s.event(
            &EventMsg::TokenCount(TokenCountEvent {
                used,
                budget,
                auto_compact_at: auto,
            }),
            now,
        );
        let line = s.line(Counts::default(), 80, now);
        (text(&line), line.spans.first().and_then(|x| x.style.fg))
    };
    let shown = |used, auto| gauge(used, 100_000, auto);
    assert_eq!(shown(950, Some(85_000)), ("ctx 1% 950/100K".into(), None));
    assert_eq!(
        shown(60_000, Some(85_000)),
        ("ctx 60% 60K/100K".into(), Some(Color::Yellow))
    );
    assert_eq!(
        shown(85_000, Some(85_000)),
        ("ctx 85% 85K/100K".into(), Some(Color::Red))
    );
    // Auto-compaction off: red only at the budget.
    assert_eq!(
        shown(90_000, None),
        ("ctx 90% 90K/100K".into(), Some(Color::Yellow))
    );
    assert_eq!(
        shown(100_000, None),
        ("ctx 100% 100K/100K".into(), Some(Color::Red))
    );
    // No budget known yet: no gauge.
    assert_eq!(gauge(5, 0, None), (String::new(), None));
}

#[test]
fn the_pod_in_each_state() {
    let now = Instant::now();
    let shown = |e: PodStatusEvent| {
        let mut s = Status::default();
        s.event(&EventMsg::PodStatus(e), now);
        text(&s.line(Counts::default(), 120, now))
    };
    assert_eq!(shown(pod(PodState::Absent)), "no pod (/up)");
    assert_eq!(
        shown(pod(PodState::Booting {
            stage: "pulling the image".into()
        })),
        "pod booting: pulling the image"
    );
    assert_eq!(shown(pod(PodState::Stopping)), "pod stopping");
    let mut ready = pod(PodState::Ready);
    ready.spent_usd = Some(0.61);
    ready.tunnel_up = Some(false);
    ready.idle_seconds_left = Some(65 * 60);
    assert_eq!(
        shown(ready),
        "A100 1.79$/h · 0.61$ │ idle 1h05m │ tunnel down"
    );
    let hosted = PodStatusEvent {
        backend: "openai".into(),
        model: Some("gpt-x".into()),
        state: PodState::Absent,
        gpu: None,
        price_usd_per_hour: None,
        spent_usd: None,
        idle_seconds_left: None,
        tunnel_up: None,
    };
    assert_eq!(shown(hosted), "openai gpt-x");
}

#[test]
fn the_idle_countdown_runs_from_the_event() {
    let at = Instant::now();
    let mut s = Status::default();
    s.event(&EventMsg::PodStatus(pod(PodState::Ready)), at);
    let idle = |now: Instant| {
        let line = s.line(Counts::default(), 120, now);
        let span = line.spans.iter().find(|x| x.content.starts_with("idle"));
        span.map(|x| (x.content.to_string(), x.style.fg))
    };
    assert_eq!(idle(at), Some(("idle 52m".into(), None)));
    // Rounded up: 51 min 59 s left shows 52m.
    assert_eq!(
        idle(at + Duration::from_secs(1)),
        Some(("idle 52m".into(), None))
    );
    assert_eq!(
        idle(at + Duration::from_secs(60)),
        Some(("idle 51m".into(), None))
    );
    assert_eq!(
        idle(at + Duration::from_secs(42 * 60)),
        Some(("idle 10m".into(), Some(Color::Yellow)))
    );
    assert_eq!(
        idle(at + Duration::from_secs(3600)),
        Some(("idle 0m".into(), Some(Color::Yellow)))
    );
    // The next frame is due when the minute shown changes.
    assert_eq!(s.next_change(at), Some(at + Duration::from_secs(60)));
    assert_eq!(
        s.next_change(at + Duration::from_secs(1)),
        Some(at + Duration::from_secs(60))
    );
    assert_eq!(s.next_change(at + Duration::from_secs(3600)), None);
}

#[test]
fn background_work_shows_while_it_runs() {
    let now = Instant::now();
    let mut s = Status::default();
    let bg = |task, state, progress: Option<&str>| {
        EventMsg::Background(BackgroundEvent {
            task,
            state,
            progress: progress.map(str::to_string),
        })
    };
    s.event(
        &bg(BackgroundTask::Title, BackgroundState::Started, None),
        now,
    );
    s.event(
        &bg(
            BackgroundTask::Helper { n: 2 },
            BackgroundState::Running,
            Some("3/7"),
        ),
        now,
    );
    s.event(
        &bg(BackgroundTask::Compaction, BackgroundState::Started, None),
        now,
    );
    assert_eq!(
        text(&s.line(Counts::default(), 80, now)),
        "⟳ naming the chat, explore 2 3/7, compacting"
    );
    s.event(&bg(BackgroundTask::Title, BackgroundState::Done, None), now);
    s.event(
        &bg(BackgroundTask::Compaction, BackgroundState::Failed, None),
        now,
    );
    assert_eq!(text(&s.line(Counts::default(), 80, now)), "⟳ explore 2 3/7");
}

#[test]
fn the_title_is_the_last_thing_shown() {
    let now = Instant::now();
    let mut s = Status::default();
    s.event(
        &EventMsg::TitleChanged(TitleChangedEvent {
            title: "Pod costs".into(),
        }),
        now,
    );
    let counts = Counts {
        queued: 2,
        ..Counts::default()
    };
    assert_eq!(text(&s.line(counts, 80, now)), "2 queued │ Pod costs");
    // A new chat has no name.
    s.event(
        &EventMsg::TitleChanged(TitleChangedEvent {
            title: String::new(),
        }),
        now,
    );
    assert_eq!(text(&s.line(counts, 80, now)), "2 queued");
    assert!(!s.event(&EventMsg::ShutdownComplete, now));
}

#[test]
fn tokens_as_codex_writes_them() {
    for (n, shown) in [
        (0, "0"),
        (999, "999"),
        (1000, "1K"),
        (1500, "1.5K"),
        (12_345, "12.3K"),
        (107_000, "107K"),
        (262_144, "262K"),
        (1_050_000, "1.05M"),
        (2_000_000_000, "2B"),
    ] {
        assert_eq!(tokens(n), shown, "{n}");
    }
}
