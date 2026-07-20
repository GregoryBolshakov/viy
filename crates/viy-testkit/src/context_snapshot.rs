//! Readable snapshots of the model context a chat sent, after Codex's
//! `core/tests/common/context_snapshot.rs`, for Chat Completions bodies.
//!
//! Requests are grouped into windows: a request whose messages extend the
//! previous request's, with the same settings (everything but `messages`),
//! joins its window and shows only its new messages. Anything else starts a
//! new window, and the header says why. Each message is one numbered item:
//! role, then its text indented, long lines clipped. A snapshot file is
//! compared with [`assert_snapshot`]; `UPDATE_SNAPSHOTS=1` rewrites it
//! (Codex uses insta, which viy does without).

use std::path::Path;

use serde_json::Value;

use crate::mock_llm::tools_mode;

const MAX_LINE_CHARS: usize = 160;
const MAX_ITEM_LINES: usize = 24;

#[derive(Debug, Clone, Default)]
pub struct ContextSnapshotOptions {
    full_text: bool,
    include_settings: bool,
    replacements: Vec<(String, String)>,
}

impl ContextSnapshotOptions {
    /// Every line of every message; the default stops an item at 24 lines.
    pub fn full_text(mut self) -> Self {
        self.full_text = true;
        self
    }

    /// Print each window's settings: the body without its messages, the
    /// tools by name.
    pub fn include_settings(mut self) -> Self {
        self.include_settings = true;
        self
    }

    /// Replace `from` with `to` in all text, e.g. a scratch path with
    /// `{WORKSPACE}`, as the drivers normalize their logs.
    pub fn replace(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.replacements.push((from.into(), to.into()));
        self
    }
}

/// Renders chat-completions request bodies, in order.
pub fn format_context_snapshot(
    scenario: &str,
    bodies: &[Value],
    options: &ContextSnapshotOptions,
) -> String {
    let mut out = format!("Scenario: {scenario}\n");
    let mut prev: Option<(&[Value], Value)> = None;
    let mut window = 0;
    for (n, body) in bodies.iter().enumerate() {
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let settings = settings_of(body);
        let shared = prev.as_ref().map_or(0, |(p, _)| {
            p.iter().zip(messages).take_while(|(a, b)| a == b).count()
        });
        let boundary = prev.as_ref().and_then(|(p, s)| {
            let mut why = Vec::new();
            if shared < p.len() {
                why.push(if shared == messages.len() {
                    format!("messages truncated at item {shared:02}")
                } else {
                    format!("messages diverged at item {shared:02}")
                });
            } else if shared == messages.len() {
                why.push("messages repeated".to_string());
            }
            if s != &settings {
                why.push(format!("settings changed ({})", changed_keys(s, &settings)));
            }
            (!why.is_empty()).then(|| why.join(", "))
        });
        let start = if prev.is_none() || boundary.is_some() {
            window += 1;
            out.push_str(&format!("\n## Window {window}"));
            if let Some(why) = &boundary {
                out.push_str(&format!(" (after request {n}: {why})"));
            }
            out.push('\n');
            if options.include_settings {
                out.push_str(&format!("settings: {}\n", settings_line(&settings)));
            }
            0
        } else {
            shared
        };
        let stream = if body["stream"] == Value::Bool(true) {
            "stream"
        } else {
            "no stream"
        };
        let tools = tools_mode(body);
        out.push_str(&format!(
            "-- request {} ({stream}, tools {}) --\n",
            n + 1,
            tools.as_str().unwrap_or_default()
        ));
        for (i, m) in messages.iter().enumerate().skip(start) {
            render_message(&mut out, i, m, options);
        }
        prev = Some((messages, settings));
    }
    apply_replacements(out, options)
}

fn apply_replacements(mut text: String, options: &ContextSnapshotOptions) -> String {
    for (from, to) in &options.replacements {
        text = text.replace(from.as_str(), to);
    }
    text
}

fn settings_of(body: &Value) -> Value {
    let mut s = body.clone();
    if let Some(obj) = s.as_object_mut() {
        obj.remove("messages");
    }
    s
}

fn changed_keys(old: &Value, new: &Value) -> String {
    let (Some(old), Some(new)) = (old.as_object(), new.as_object()) else {
        return String::new();
    };
    let mut keys: Vec<&str> = old
        .keys()
        .chain(new.keys())
        .filter(|k| old.get(*k) != new.get(*k))
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys.join(", ")
}

fn settings_line(settings: &Value) -> String {
    let Some(obj) = settings.as_object() else {
        return settings.to_string();
    };
    obj.iter()
        .map(|(k, v)| match (k.as_str(), v.as_array()) {
            ("tools", Some(tools)) => {
                let names: Vec<&str> = tools
                    .iter()
                    .filter_map(|t| t["function"]["name"].as_str())
                    .collect();
                format!("tools=[{}]", names.join(", "))
            }
            _ => format!("{k}={v}"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_message(out: &mut String, i: usize, m: &Value, options: &ContextSnapshotOptions) {
    let role = m["role"].as_str().unwrap_or("?");
    match m.get("tool_call_id").and_then(Value::as_str) {
        Some(id) => out.push_str(&format!("{i:02}:{role}[{id}]:\n")),
        None => out.push_str(&format!("{i:02}:{role}:\n")),
    }
    let mut lines = Vec::new();
    if let Some(r) = m.get("reasoning").and_then(Value::as_str) {
        lines.extend(r.split('\n').map(|l| format!("[reasoning] {l}")));
    }
    match &m["content"] {
        Value::String(s) => lines.extend(s.split('\n').map(str::to_string)),
        Value::Array(parts) => {
            for p in parts {
                match p["type"].as_str() {
                    Some("text") => lines.extend(
                        p["text"]
                            .as_str()
                            .unwrap_or_default()
                            .split('\n')
                            .map(str::to_string),
                    ),
                    Some(other) => lines.push(format!("<{other}>")),
                    None => lines.push(p.to_string()),
                }
            }
        }
        Value::Null => {}
        other => lines.push(other.to_string()),
    }
    for tc in m
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        lines.push(format!(
            "-> {}({}) {}",
            tc["function"]["name"].as_str().unwrap_or("?"),
            tc["id"].as_str().unwrap_or("?"),
            tc["function"]["arguments"].as_str().unwrap_or_default()
        ));
    }
    let total = lines.len();
    let keep = if options.full_text {
        total
    } else {
        total.min(MAX_ITEM_LINES)
    };
    for line in &lines[..keep] {
        out.push_str("    ");
        out.push_str(&clip(line));
        out.push('\n');
    }
    if keep < total {
        out.push_str(&format!("    ... ({} more lines)\n", total - keep));
    }
}

fn clip(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((at, _)) => format!("{}... ({} chars)", &line[..at], line.chars().count()),
        None => line.to_string(),
    }
}

/// Compares `actual` with the snapshot file at `path`; with
/// `UPDATE_SNAPSHOTS=1` in the environment, writes it instead.
pub fn assert_snapshot(path: &Path, actual: &str) -> std::io::Result<()> {
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some_and(|v| v == "1") {
        return std::fs::write(path, actual);
    }
    let expected = std::fs::read_to_string(path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "{}: {e}; write it with UPDATE_SNAPSHOTS=1 cargo test",
                path.display()
            ),
        )
    })?;
    if expected != actual {
        return Err(std::io::Error::other(format!(
            "{} differs from the snapshot (rewrite with UPDATE_SNAPSHOTS=1):\n{}",
            path.display(),
            first_difference(&expected, actual)
        )));
    }
    Ok(())
}

fn first_difference(expected: &str, actual: &str) -> String {
    let (e, a): (Vec<&str>, Vec<&str>) = (expected.lines().collect(), actual.lines().collect());
    let n = e.iter().zip(&a).take_while(|(x, y)| x == y).count();
    format!(
        "line {}:\n  snapshot: {}\n  actual:   {}",
        n + 1,
        e.get(n).unwrap_or(&"<end>"),
        a.get(n).unwrap_or(&"<end>")
    )
}

#[cfg(test)]
#[path = "context_snapshot_tests.rs"]
mod tests;
