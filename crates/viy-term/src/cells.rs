//! The transcript's entries (ui-plan 3; Codex's `history_cell`): your
//! messages behind a `you › ` gutter, answers as Markdown, the reasoning
//! trace, tool calls as cards, notices. A cell is drawn at a width when it
//! is laid out, so a resize rewraps it (a table is laid out again too),
//! and each row carries a key that grows down the cell, which the
//! transcript anchors its reading position to.
//!
//! Every row comes from [`wrap_spans`], [`wrap_rows`] or the Markdown
//! renderer, which draw each char that could act on the terminal visibly
//! (ui-plan 7).

use std::time::Duration;

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::ToolCallEndEvent;
use viy_protocol::ToolCallStatus;

use crate::markdown;
use crate::markdown::Row;
use crate::wrap::text_width;
use crate::wrap::wrap_rows;
use crate::wrap::wrap_spans;

/// Output lines a collapsed tool card shows: its last ones, as the live
/// tail showed them while it ran.
pub const TOOL_TAIL: usize = 3;
/// The gutter of your messages.
const YOU: &str = "you › ";

/// What goes before a cell's first row and before the others.
#[derive(Debug, Clone, PartialEq)]
pub struct Gutter {
    pub first: Span<'static>,
    pub rest: Span<'static>,
}

/// One entry of the transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// Lines word-wrapped behind an optional gutter.
    Lines {
        lines: Vec<Line<'static>>,
        gutter: Option<Gutter>,
        /// A blank row above it, between turns.
        blank: bool,
    },
    /// An answer: Markdown drawn (rp `/md on`) or its text as written.
    Answer {
        source: String,
        markdown: bool,
    },
    Reasoning(Reasoning),
    Tool(ToolCard),
}

/// The reasoning trace of a round.
#[derive(Debug, Clone, PartialEq)]
pub struct Reasoning {
    /// Empty for a resumed chat: the trace is not kept (kb d052).
    pub text: String,
    pub took: Option<Duration>,
    /// Drawn whole (rp `/think on`); one line otherwise, whole with Ctrl-T.
    pub shown: bool,
}

/// A tool call: its name, its arguments as the engine summed them up, its
/// output, and once it ended, how.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCard {
    pub call_id: String,
    pub tool: String,
    pub summary: String,
    pub output: String,
    pub end: Option<ToolCallEndEvent>,
}

/// How a running card is drawn: a spinner frame and the time so far.
#[derive(Debug, Clone, Copy)]
pub struct Running<'a> {
    pub spinner: &'a str,
    pub elapsed: Duration,
}

impl Cell {
    /// Lines as they are, word-wrapped.
    pub fn lines(lines: Vec<Line<'static>>) -> Self {
        Self::Lines {
            lines,
            gutter: None,
            blank: false,
        }
    }

    /// A message you sent, behind `you › `.
    pub fn user(text: &str) -> Self {
        Self::Lines {
            lines: text
                .split('\n')
                .map(|l| Line::from(l.to_string()))
                .collect(),
            gutter: Some(Gutter {
                first: YOU.cyan().bold(),
                rest: Span::raw(" ".repeat(YOU.chars().count())),
            }),
            blank: true,
        }
    }

    pub fn answer(source: &str, markdown: bool) -> Self {
        Self::Answer {
            source: source.to_string(),
            markdown,
        }
    }

    /// A notice in its level's colour.
    pub fn notice(notice: &NoticeEvent) -> Self {
        let style = notice_style(notice.level);
        Self::lines(
            notice
                .message
                .split('\n')
                .map(|l| Line::from(Span::styled(l.to_string(), style)))
                .collect(),
        )
    }

    /// The rows at `width`; `expanded` (Ctrl-T) draws tool output and
    /// reasoning whole.
    pub fn rows(&self, width: usize, expanded: bool) -> Vec<Row> {
        let width = width.max(1);
        match self {
            Self::Lines {
                lines,
                gutter,
                blank,
            } => lines_rows(lines, gutter.as_ref(), *blank, width),
            Self::Answer { source, markdown } => answer_rows(source, *markdown, width),
            Self::Reasoning(reasoning) => reasoning.rows(width, expanded),
            Self::Tool(card) => card.rows(width, expanded, None),
        }
    }
}

fn blank_row() -> Row {
    Row {
        line: Line::default(),
        key: (0, 0),
    }
}

/// `lines` word-wrapped behind `gutter`; keys are (line + 1, byte offset).
fn lines_rows(
    lines: &[Line<'static>],
    gutter: Option<&Gutter>,
    blank: bool,
    width: usize,
) -> Vec<Row> {
    let mut rows = Vec::new();
    if blank {
        rows.push(blank_row());
    }
    let gutter_width = gutter.map_or(0, |g| text_width(&g.first.content));
    let room = width.saturating_sub(gutter_width).max(1);
    for (i, line) in lines.iter().enumerate() {
        for (k, row) in wrap_spans(&line.spans, room).into_iter().enumerate() {
            let mut spans = Vec::new();
            if let Some(g) = gutter {
                spans.push(if i == 0 && k == 0 {
                    g.first.clone()
                } else {
                    g.rest.clone()
                });
            }
            spans.extend(row.spans);
            rows.push(Row {
                line: Line::from(spans).style(line.style),
                key: (i + 1, row.start),
            });
        }
    }
    rows
}

fn answer_rows(source: &str, markdown: bool, width: usize) -> Vec<Row> {
    let mut rows = vec![blank_row()];
    if markdown {
        rows.extend(markdown::render(source, width).into_iter().map(|row| Row {
            line: row.line,
            key: (row.key.0 + 1, row.key.1),
        }));
    } else {
        let lines: Vec<Line<'static>> = source
            .trim_end_matches('\n')
            .split('\n')
            .map(|l| Line::from(l.to_string()))
            .collect();
        rows.extend(lines_rows(&lines, None, false, width));
    }
    rows
}

impl Reasoning {
    pub fn rows(&self, width: usize, expanded: bool) -> Vec<Row> {
        if (self.shown || expanded) && !self.text.trim().is_empty() {
            let style = Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC);
            let lines: Vec<Line<'static>> = self
                .text
                .trim_matches('\n')
                .split('\n')
                .enumerate()
                .map(|(i, l)| {
                    let mut spans = Vec::new();
                    if i == 0 {
                        spans.push("[thinking] ".dim());
                    }
                    spans.push(Span::styled(l.to_string(), style));
                    Line::from(spans)
                })
                .collect();
            return lines_rows(&lines, None, true, width);
        }
        let took = self
            .took
            .map(|t| format!(" for {}", seconds(t)))
            .unwrap_or_default();
        let text = if self.text.trim().is_empty() {
            format!("thought{took}")
        } else {
            format!("thought{took} · Ctrl-T shows it")
        };
        lines_rows(&[Line::from(text.dim().italic())], None, false, width)
    }
}

/// `4.2 s`.
/// A notice's colour, by its level.
pub fn notice_style(level: NoticeLevel) -> Style {
    match level {
        NoticeLevel::Info => Style::new().dim(),
        NoticeLevel::Warn => Style::new().yellow(),
        NoticeLevel::Error => Style::new().red(),
    }
}

pub fn seconds(t: Duration) -> String {
    format!("{:.1} s", t.as_secs_f64())
}

impl ToolCard {
    pub fn new(call_id: &str, tool: &str, summary: &str) -> Self {
        Self {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            summary: summary.to_string(),
            output: String::new(),
            end: None,
        }
    }

    /// One header row (wrapped when long) and the output: its last
    /// [`TOOL_TAIL`] lines, or all of them when `expanded`. Keys are the
    /// row numbers.
    pub fn rows(&self, width: usize, expanded: bool, running: Option<Running<'_>>) -> Vec<Row> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let (marker, meta) = match (running, &self.end) {
            (Some(run), _) => (
                run.spinner.to_string().magenta(),
                vec![seconds(run.elapsed)],
            ),
            (None, Some(end)) => (status_marker(end.status), end_meta(end)),
            (None, None) => ("▸".dim(), Vec::new()),
        };
        let mut head = vec![
            marker,
            Span::raw(" "),
            self.tool.clone().bold(),
            Span::raw("  "),
            Span::raw(self.summary.replace('\n', " ")),
        ];
        let meta = meta.join("  ");
        let head_width: usize = head.iter().map(|s| text_width(&s.content)).sum();
        let meta_width = text_width(&meta);
        if !meta.is_empty() && head_width + 2 + meta_width <= width {
            head.push(Span::raw(" ".repeat(width - head_width - meta_width)));
            head.push(meta.dim());
            lines.extend(
                wrap_spans(&head, width)
                    .into_iter()
                    .map(|r| Line::from(r.spans)),
            );
        } else {
            if !meta.is_empty() {
                head.push(Span::raw("  "));
                head.push(meta.dim());
            }
            for (k, row) in wrap_spans(&head, width.saturating_sub(4).max(1))
                .into_iter()
                .enumerate()
            {
                let mut spans = if k == 0 {
                    Vec::new()
                } else {
                    vec![Span::raw("    ")]
                };
                spans.extend(row.spans);
                lines.push(Line::from(spans));
            }
        }
        lines.extend(self.output_lines(width, expanded && running.is_none()));
        lines
            .into_iter()
            .enumerate()
            .map(|(n, line)| Row { line, key: (n, 0) })
            .collect()
    }

    /// The output under the header: `  └ ` before the first row, cut
    /// between graphemes (logs and code keep their columns).
    fn output_lines(&self, width: usize, all: bool) -> Vec<Line<'static>> {
        let text = self.output.trim_end_matches('\n');
        if text.is_empty() {
            return Vec::new();
        }
        let lines: Vec<&str> = text.split('\n').collect();
        let shown = if all {
            lines.len()
        } else {
            TOOL_TAIL.min(lines.len())
        };
        let hidden = lines.len() - shown;
        let mut rows: Vec<Line<'static>> = Vec::new();
        if hidden > 0 {
            rows.push(Line::from(
                format!("… +{hidden} lines (Ctrl-T shows all)").dim(),
            ));
        }
        let room = width.saturating_sub(4).max(1);
        for line in &lines[hidden..] {
            for (row, _) in wrap_rows(&Line::from(line.to_string().dim()), room) {
                rows.push(row);
            }
        }
        rows.into_iter()
            .enumerate()
            .map(|(i, row)| {
                let prefix = if i == 0 { "  └ " } else { "    " };
                let mut spans = vec![prefix.dim()];
                spans.extend(row.spans);
                Line::from(spans)
            })
            .collect()
    }
}

/// `▸` in the colour of how the call ended (Codex's green and red bullets).
fn status_marker(status: ToolCallStatus) -> Span<'static> {
    let style = match status {
        ToolCallStatus::Completed => Style::new().fg(Color::Green),
        ToolCallStatus::Failed => Style::new().fg(Color::Red),
        _ => Style::new().fg(Color::Yellow),
    };
    Span::styled("▸", style)
}

/// What the right of a finished card says: the result in a few words, the
/// sandbox and network, how it ended unless it completed, and the time.
fn end_meta(end: &ToolCallEndEvent) -> Vec<String> {
    let mut meta = Vec::new();
    if let Some(detail) = end.detail.as_ref().filter(|d| !d.is_empty()) {
        meta.push(detail.replace('\n', " "));
    }
    if end.sandboxed {
        meta.push("sandbox".to_string());
    }
    if end.network {
        meta.push("network".to_string());
    }
    match end.status {
        ToolCallStatus::Completed => {}
        ToolCallStatus::Failed => meta.push("failed".to_string()),
        ToolCallStatus::Declined => meta.push("declined".to_string()),
        ToolCallStatus::Interrupted => meta.push("interrupted".to_string()),
        _ => meta.push("ended".to_string()),
    }
    meta.push(seconds(Duration::from_millis(end.duration_ms)));
    meta
}

#[cfg(test)]
#[path = "cells_tests.rs"]
mod tests;
