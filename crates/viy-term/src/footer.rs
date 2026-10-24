//! The footer (ui-plan 3): the statuses rp prints now and then, kept on
//! one line under the input. Each item is there only when it has something
//! to say, so an idle chat with no pod shows little:
//!
//! ```text
//! ⏵⏵ auto-yes │ effort medium │ ctx 41% 107K/262K │ A100 1.79$/h │ idle 52m │ tunnel ok
//! ```
//!
//! The values come from the engine's events ([`Status::event`]); the
//! footer computes nothing but the idle countdown since the last
//! [`PodStatusEvent`]. When the line is too narrow the least important
//! items go first, as in Codex's footer: what you must act on (questions
//! waiting, rows below, messages queued, an `auto-yes` mode) stays longest.
//!
//! The context gauge takes rp's prompt colours: yellow from 60% of the
//! budget, red from the auto-compact point. The idle countdown turns yellow
//! when rp's reaper would warn (10 minutes before it takes the pod down).

use std::time::Duration;

use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use tokio::time::Instant;
use viy_protocol::BackgroundState;
use viy_protocol::BackgroundTask;
use viy_protocol::EventMsg;
use viy_protocol::PodState;
use viy_protocol::PodStatusEvent;
use viy_protocol::TokenCountEvent;

use crate::wrap::text_width;

/// Between two items.
const SEPARATOR: &str = " │ ";
/// rp's reaper warns this long before it takes an idle pod down
/// (`idle.WARN_BEFORE_S`).
const REAP_WARNING: Duration = Duration::from_secs(10 * 60);
/// rp's prompt shows its gauge from here (`chat._prompt`).
const GAUGE_WARNING: f64 = 0.6;

/// What the engine has said, as the footer shows it.
#[derive(Debug, Default)]
pub struct Status {
    pub mode: Option<String>,
    pub effort: Option<String>,
    pub context: Option<TokenCountEvent>,
    /// The last pod status and when it came.
    pub pod: Option<(PodStatusEvent, Instant)>,
    /// Background work running, in the order it began.
    pub background: Vec<(BackgroundTask, Option<String>)>,
    /// The chat's name: its title, else its first message (rp's
    /// `_tab_label`).
    pub title: Option<String>,
}

/// What the app adds: counts it keeps itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct Counts {
    /// Questions waiting behind the one shown.
    pub questions: usize,
    /// Messages queued for the turn running to end.
    pub queued: usize,
    /// Transcript rows below the screen.
    pub rows_below: usize,
}

impl Status {
    /// Takes in `msg` if it is a status; true if it was.
    pub fn event(&mut self, msg: &EventMsg, now: Instant) -> bool {
        match msg {
            EventMsg::ModeChanged(e) => self.mode = Some(e.label.clone()),
            EventMsg::EffortChanged(e) => self.effort = Some(e.effort.clone()),
            EventMsg::TokenCount(e) => self.context = Some(e.clone()),
            EventMsg::PodStatus(e) => self.pod = Some((e.clone(), now)),
            EventMsg::TitleChanged(e) => {
                self.title = (!e.title.is_empty()).then(|| e.title.clone());
            }
            EventMsg::Background(e) => {
                self.background.retain(|(task, _)| *task != e.task);
                if matches!(e.state, BackgroundState::Started | BackgroundState::Running) {
                    self.background.push((e.task.clone(), e.progress.clone()));
                }
            }
            _ => return false,
        }
        true
    }

    /// Whether a pod command runs in the background (kb d041).
    pub fn pod_work(&self) -> bool {
        self.background
            .iter()
            .any(|(task, _)| matches!(task, BackgroundTask::Pod { .. }))
    }

    /// When the idle countdown next changes what it shows, if one runs.
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        let left = self.idle_left(now)?;
        if left.is_zero() {
            return None;
        }
        // The countdown shows whole minutes, rounded up: it changes when
        // `left` drops to the minute below.
        let into = left.as_millis() % 60_000;
        let into = if into == 0 { 60_000 } else { into };
        Some(now + Duration::from_millis(u64::try_from(into).unwrap_or(60_000)))
    }

    fn idle_left(&self, now: Instant) -> Option<Duration> {
        let (pod, at) = self.pod.as_ref()?;
        let left = Duration::from_secs(pod.idle_seconds_left?);
        Some(left.saturating_sub(now.saturating_duration_since(*at)))
    }

    /// The footer for `width` columns.
    pub fn line(&self, counts: Counts, width: usize, now: Instant) -> Line<'static> {
        let mut items = self.items(counts, now);
        let total = |items: &[Item]| {
            items.iter().map(Item::width).sum::<usize>()
                + text_width(SEPARATOR) * items.len().saturating_sub(1)
        };
        while total(&items) > width && !items.is_empty() {
            // The least important; of equals, the rightmost.
            let drop = items
                .iter()
                .enumerate()
                .min_by_key(|(i, item)| (item.rank, std::cmp::Reverse(*i)))
                .map(|(i, _)| i)
                .unwrap_or(0);
            items.remove(drop);
        }
        let mut spans = Vec::new();
        for (i, item) in items.into_iter().enumerate() {
            if i > 0 {
                spans.push(SEPARATOR.dim());
            }
            spans.push(item.span);
        }
        Line::from(spans)
    }

    /// The items in the order shown; each with its rank, higher kept
    /// longer.
    fn items(&self, counts: Counts, now: Instant) -> Vec<Item> {
        let mut items = Vec::new();
        if let Some(label) = self.mode.as_deref().filter(|l| !l.is_empty()) {
            // rp's prompt label: none for the normal mode.
            items.push(Item::new(8, label.to_string(), Style::new().yellow()));
        }
        if let Some(effort) = &self.effort {
            items.push(Item::new(2, format!("effort {effort}"), dim()));
        }
        if let Some(gauge) = self.context.as_ref().and_then(gauge) {
            items.push(gauge);
        }
        if let Some((pod, _)) = &self.pod {
            items.extend(pod_items(pod, self.idle_left(now)));
        }
        if !self.background.is_empty() {
            let work: Vec<String> = self
                .background
                .iter()
                .map(|(task, progress)| {
                    let name = match task {
                        BackgroundTask::Title => "naming the chat".to_string(),
                        BackgroundTask::Compaction => "compacting".to_string(),
                        BackgroundTask::Helper { n } => format!("explore {n}"),
                        BackgroundTask::Pod { command, .. } => format!("/{command}"),
                        _ => "working".to_string(),
                    };
                    match progress {
                        Some(p) => format!("{name} {p}"),
                        None => name,
                    }
                })
                .collect();
            items.push(Item::new(3, format!("⟳ {}", work.join(", ")), dim()));
        }
        if counts.questions > 0 {
            let s = if counts.questions == 1 { "" } else { "s" };
            items.push(Item::new(
                10,
                format!("{} question{s} waiting", counts.questions),
                Style::new().yellow(),
            ));
        }
        if counts.queued > 0 {
            items.push(Item::new(
                7,
                format!("{} queued", counts.queued),
                Style::new().cyan(),
            ));
        }
        if let Some(title) = &self.title {
            items.push(Item::new(1, title.clone(), dim()));
        }
        if counts.rows_below > 0 {
            items.push(Item::new(
                9,
                format!("↓ {} rows (End)", counts.rows_below),
                Style::new().cyan(),
            ));
        }
        items
    }
}

/// One item of the footer.
struct Item {
    rank: u8,
    span: Span<'static>,
}

impl Item {
    fn new(rank: u8, text: String, style: Style) -> Self {
        Self {
            rank,
            span: Span::styled(text, style),
        }
    }

    fn width(&self) -> usize {
        text_width(&self.span.content)
    }
}

fn dim() -> Style {
    Style::new().dim()
}

/// `ctx 41% 107K/262K`, coloured as rp's prompt gauge.
fn gauge(c: &TokenCountEvent) -> Option<Item> {
    if c.budget == 0 {
        return None;
    }
    let frac = c.used as f64 / c.budget as f64;
    let style = match c.auto_compact_at {
        Some(at) if c.used >= at => Style::new().red(),
        None if c.used >= c.budget => Style::new().red(),
        _ if frac >= GAUGE_WARNING => Style::new().yellow(),
        _ => dim(),
    };
    let text = format!(
        "ctx {:.0}% {}/{}",
        frac * 100.0,
        tokens(c.used),
        tokens(c.budget)
    );
    Some(Item::new(5, text, style))
}

/// The pod's items: where it is, its price, the idle countdown, the tunnel.
fn pod_items(pod: &PodStatusEvent, idle_left: Option<Duration>) -> Vec<Item> {
    let mut items = Vec::new();
    if pod.backend != "runpod" {
        let text = match &pod.model {
            Some(model) => format!("{} {model}", pod.backend),
            None => pod.backend.clone(),
        };
        items.push(Item::new(4, text, dim()));
        return items;
    }
    match &pod.state {
        PodState::Absent => items.push(Item::new(6, "no pod (/up)".into(), dim())),
        PodState::Booting { stage } => items.push(Item::new(
            6,
            format!("pod booting: {stage}"),
            Style::new().yellow(),
        )),
        PodState::Stopping => {
            items.push(Item::new(6, "pod stopping".into(), Style::new().yellow()));
        }
        // Ready, and any state added later.
        _ => {
            let mut text = pod.gpu.clone().unwrap_or_else(|| "pod".into());
            if let Some(price) = pod.price_usd_per_hour {
                text.push_str(&format!(" {price:.2}$/h"));
            }
            if let Some(spent) = pod.spent_usd {
                text.push_str(&format!(" · {spent:.2}$"));
            }
            // A ready pod's price says less than the gauge; a pod that is
            // not ready says more.
            items.push(Item::new(4, text, dim()));
            if let Some(left) = idle_left {
                let style = if left <= REAP_WARNING {
                    Style::new().yellow()
                } else {
                    dim()
                };
                items.push(Item::new(4, format!("idle {}", minutes(left)), style));
            }
            match pod.tunnel_up {
                Some(true) => items.push(Item::new(3, "tunnel ok".into(), dim())),
                Some(false) => items.push(Item::new(7, "tunnel down".into(), Style::new().red())),
                None => {}
            }
        }
    }
    items
}

/// `52m`, `1h05m`, rounded up to the minute; `0m` once it has run out.
fn minutes(left: Duration) -> String {
    let m = left.as_secs().div_ceil(60);
    if m >= 60 {
        format!("{}h{:02}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

/// A token count as Codex's `format_tokens_compact` writes it: `950`,
/// `1.5K`, `107K`, `1.05M`.
pub fn tokens(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let (scaled, suffix) = if n >= 1_000_000_000 {
        (n as f64 / 1e9, "B")
    } else if n >= 1_000_000 {
        (n as f64 / 1e6, "M")
    } else {
        (n as f64 / 1e3, "K")
    };
    let decimals = if scaled < 10.0 {
        2
    } else if scaled < 100.0 {
        1
    } else {
        0
    };
    let mut s = format!("{scaled:.decimals$}");
    if s.contains('.') {
        s.truncate(s.trim_end_matches('0').trim_end_matches('.').len());
    }
    format!("{s}{suffix}")
}

#[cfg(test)]
#[path = "footer_tests.rs"]
mod tests;
