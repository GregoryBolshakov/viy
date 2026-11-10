//! Context management (rpctl chat.py `_maybe_elide_tool_results`,
//! `_elide_before`, `_apply_elision`; compaction comes with WP 6.2).
//!
//! Tool output is most of a chat's history (75% in Gregory's saved chats,
//! rpctl's comment; 81-93% of real chats, rpctl kb f101) and rarely needed
//! once its turn is over: the model can run the tool again. Once the
//! history passes TOOL_ELIDE_THRESHOLD of the prompt budget, the tool
//! results and write_file contents of the turns before the last
//! TOOL_ELIDE_KEEP_TURNS are replaced by a short note, in one batch per
//! crossing (each change to old messages costs a prefix-cache miss from
//! there on). The session log keeps the originals: the next checkpoint
//! writes the new history as a `replace`. Codex keeps the same split: tool
//! output is truncated in the live history, the rollout keeps it whole
//! (core/src/context_manager/history.rs).
//!
//! The elision itself ([`elide_before`]) is a pure function of the
//! history, so compaction (WP 6.2) can use it for its trim.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value;
use serde_json::json;
use viy_protocol::NoticeLevel;

use super::Chat;
use super::notice;
use crate::engine::TurnContext;

/// Tool output and write_file contents shorter than this are kept: the
/// note replacing them would not be much shorter.
pub const ELIDE_MIN_CHARS: usize = 400;
/// How much of an elided tool result its note keeps: enough for the path,
/// the exit code or the first lines, so the model knows what the call
/// found.
pub const ELIDE_HEAD_CHARS: usize = 200;

/// The tools whose results stay: explore's and ask_claude's are already
/// the short, distilled form, and the only way back to them is running
/// the helpers or Claude again for minutes.
const KEPT_TOOLS: [&str; 2] = ["explore", "ask_claude"];

/// A history with old tool output elided, and what changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Elision {
    pub messages: Vec<Value>,
    /// Outputs and write_file contents elided.
    pub n: usize,
    /// Characters saved (negative only if a note outgrew its text).
    pub saved: isize,
    /// Write-guard keys whose contents leave the context.
    pub forget: HashSet<PathBuf>,
    /// Write-guard keys a turn still in full read or wrote.
    pub kept: HashSet<PathBuf>,
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// `messages` with the tool results and write_file contents before index
/// `cut` replaced by a note (rp's `_elide_before`). `call_path` is the
/// write guard's key for a read_file or write_file call.
pub fn elide_before(
    messages: &[Value],
    cut: usize,
    call_path: &dyn Fn(&Value) -> Option<PathBuf>,
) -> Elision {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut paths: HashMap<String, Option<PathBuf>> = HashMap::new();
    let (mut saved, mut n) = (0isize, 0usize);
    let mut forget = HashSet::new();
    let mut out = Vec::with_capacity(messages.len());
    let id_of = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    for (i, m) in messages.iter().enumerate() {
        if i >= cut {
            out.push(m.clone());
            continue;
        }
        for tc in m["tool_calls"].as_array().into_iter().flatten() {
            let name = tc["function"]["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("tool");
            names.insert(id_of(&tc["id"]), name.to_string());
            paths.insert(id_of(&tc["id"]), call_path(tc));
        }
        let call_id = id_of(&m["tool_call_id"]);
        let name = names.get(&call_id).map(String::as_str);
        let content = m["content"].as_str().unwrap_or_default();
        if m["role"] == "tool"
            && chars(content) > ELIDE_MIN_CHARS
            && !name.is_some_and(|n| KEPT_TOOLS.contains(&n))
        {
            if name == Some("read_file")
                && let Some(Some(p)) = paths.get(&call_id)
            {
                forget.insert(p.clone());
            }
            let head: String = content.chars().take(ELIDE_HEAD_CHARS).collect();
            let head = head.split_whitespace().collect::<Vec<_>>().join(" ");
            let note = format!(
                "[elided: this {} result ({} chars) was removed to save context. It began: \
                 {head} ... Run the tool again if you need it.]",
                name.unwrap_or("tool"),
                chars(content)
            );
            saved += chars(content) as isize - chars(&note) as isize;
            n += 1;
            let mut m = m.clone();
            m["content"] = Value::String(note);
            out.push(m);
            continue;
        }
        let Some(calls) = m["tool_calls"].as_array().filter(|c| !c.is_empty()) else {
            out.push(m.clone());
            continue;
        };
        let mut new_calls = Vec::with_capacity(calls.len());
        for tc in calls {
            let mut tc = tc.clone();
            if tc["function"]["name"] == "write_file"
                && let Some(raw) = tc["function"]["arguments"].as_str()
                && let Ok(Value::Object(mut args)) =
                    serde_json::from_str::<Value>(if raw.is_empty() { "{}" } else { raw })
                && let Some(body) = args.get("content").and_then(Value::as_str)
                && chars(body) > ELIDE_MIN_CHARS
            {
                let note = format!(
                    "[elided: {} chars written; read_file to see the file]",
                    chars(body)
                );
                args.insert("content".into(), Value::String(note));
                let new = Value::Object(args).to_string();
                saved += chars(raw) as isize - chars(&new) as isize;
                n += 1;
                if let Some(p) = call_path(&tc) {
                    forget.insert(p);
                }
                tc["function"]["arguments"] = Value::String(new);
            }
            new_calls.push(tc);
        }
        let mut m = m.clone();
        m["tool_calls"] = Value::Array(new_calls);
        out.push(m);
    }
    let kept = messages[cut.min(messages.len())..]
        .iter()
        .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
        .filter_map(call_path)
        .collect();
    Elision {
        messages: out,
        n,
        saved,
        forget,
        kept,
    }
}

/// TOOL_ELIDE_THRESHOLD and TOOL_ELIDE_KEEP_TURNS; both rpctl's defaults
/// (0.6, 2) when either is not a number.
pub fn elide_settings(threshold: Option<&str>, keep: Option<&str>) -> (f64, usize) {
    let threshold = threshold.unwrap_or("0.6").trim().parse::<f64>();
    let keep = keep.unwrap_or("2").trim().parse::<usize>();
    match (threshold, keep) {
        (Ok(t), Ok(k)) => (t, k),
        _ => (0.6, 2),
    }
}

impl Chat {
    /// Before a turn: once the history with `content` passes the elision
    /// threshold, elide the old tool output, and say so (rp's
    /// `_maybe_elide_tool_results`).
    pub(crate) fn maybe_elide_tool_results(&mut self, ctx: &TurnContext, content: &Value) {
        if self.messages.is_empty() || self.elide_threshold <= 0.0 {
            return;
        }
        let (_, frac) = self.context_state(&[json!({"role": "user", "content": content})]);
        if frac < self.elide_threshold {
            return;
        }
        let users: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "user")
            .map(|(i, _)| i)
            .collect();
        if users.len() <= self.elide_keep_turns {
            return;
        }
        let cut = if self.elide_keep_turns == 0 {
            self.messages.len()
        } else {
            users[users.len() - self.elide_keep_turns]
        };
        let el = {
            let files = &self.files;
            elide_before(&self.messages, cut, &|tc| files.call_path(tc))
        };
        if el.n == 0 {
            return;
        }
        let saved_tokens = (el.saved as f64 / self.estimator.chars_per_token) as i64;
        let n = el.n;
        self.apply_elision(el);
        notice(
            ctx,
            NoticeLevel::Info,
            "elision",
            format!(
                "  [context {:.0}% of budget: elided {n} old tool outputs, ~{saved_tokens} \
                 tokens; the session log keeps them]",
                frac * 100.0
            ),
        );
    }

    /// Takes the elided history (rp's `_apply_elision`). The write guard
    /// trusts that a file read in this chat is known to the model; once
    /// its contents are elided that is no longer true, so the model must
    /// read it again before overwriting it, unless a turn still in full
    /// read or wrote it.
    pub(crate) fn apply_elision(&mut self, el: Elision) {
        self.messages = el.messages;
        let gone: Vec<PathBuf> = el.forget.difference(&el.kept).cloned().collect();
        self.files.forget_paths(&gone);
    }
}

#[cfg(test)]
#[path = "compact_tests.rs"]
mod tests;
