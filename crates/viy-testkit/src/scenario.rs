//! The scenarios in `tests/scenarios/` (format: its README), the request
//! logs recorded from rp for them by `tools/drive_rp.py --record`
//! (implementation plan 3.4), and [`compare`], the Rust twin of
//! `tools/parity/compare_requests.py` (kb d031).

use std::io;
use std::path::PathBuf;

use serde_json::Value;

use crate::repo::repo_root;

pub fn dir() -> PathBuf {
    repo_root().join("tests").join("scenarios")
}

/// Loads `tests/scenarios/<name>.json`.
pub fn load(name: &str) -> io::Result<Value> {
    let text = std::fs::read_to_string(dir().join(format!("{name}.json")))?;
    serde_json::from_str(&text).map_err(io::Error::other)
}

/// A normalized request log: the meta header, then what fakellm received.
#[derive(Debug, Clone)]
pub struct RequestLog {
    pub meta: Value,
    pub entries: Vec<Value>,
}

impl RequestLog {
    /// The bodies of the chat-completions requests, in order.
    pub fn chat_bodies(&self) -> Vec<Value> {
        self.entries
            .iter()
            .filter(|e| e["path"] == "/v1/chat/completions")
            .filter_map(|e| e.get("body").cloned())
            .collect()
    }
}

/// Loads `tests/scenarios/<name>.rp.jsonl`, rp's requests for that scenario.
pub fn rp_log(name: &str) -> io::Result<RequestLog> {
    let text = std::fs::read_to_string(dir().join(format!("{name}.rp.jsonl")))?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let parse = |l: &str| serde_json::from_str::<Value>(l).map_err(io::Error::other);
    let meta = lines
        .next()
        .map(parse)
        .transpose()?
        .and_then(|mut first| first.get_mut("meta").map(Value::take))
        .ok_or_else(|| io::Error::other(format!("{name}.rp.jsonl: no meta header")))?;
    let entries = lines.map(parse).collect::<io::Result<_>>()?;
    Ok(RequestLog { meta, entries })
}

/// The names of the scenarios that have an rp recording.
pub fn recorded() -> io::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir())?
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".rp.jsonl"))
                .map(str::to_string)
        })
        .collect();
    names.sort();
    Ok(names)
}

/// `value` with each `(from, to)` replaced in every string, longest `from`
/// first, as compare_requests' `normalize` puts `{WORKSPACE}`, `{BASE}` and
/// `{TODAY}` in place of a run's paths and date.
pub fn normalize(value: &Value, subs: &[(&str, &str)]) -> Value {
    let mut subs: Vec<(&str, &str)> = subs
        .iter()
        .filter(|(f, _)| !f.is_empty())
        .copied()
        .collect();
    subs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    fn walk(v: &Value, subs: &[(&str, &str)]) -> Value {
        match v {
            Value::String(s) => Value::String(
                subs.iter()
                    .fold(s.clone(), |s, (from, to)| s.replace(from, to)),
            ),
            Value::Array(items) => Value::Array(items.iter().map(|x| walk(x, subs)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, x)| {
                        (
                            walk(&Value::String(k.clone()), subs)
                                .as_str()
                                .unwrap_or(k)
                                .to_string(),
                            walk(x, subs),
                        )
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    walk(value, &subs)
}

/// The first difference between two request logs (fakellm's entries), or
/// `None`: the same requests in order, with the same method, path and
/// Authorization header, and bodies equal in content. Key order counts only
/// inside `tools`; strings holding JSON (tool results, call arguments) are
/// compared parsed; a User-Agent naming viy is a difference.
pub fn compare(a: &[Value], b: &[Value]) -> Option<String> {
    for (n, (x, y)) in a.iter().zip(b).enumerate() {
        let at = format!("request {n} ({} {})", text(&x["method"]), text(&x["path"]));
        for (side, e) in [("A", x), ("B", y)] {
            if text(&e["headers"]["user-agent"])
                .to_lowercase()
                .contains("viy")
            {
                return Some(format!("{at}: {side}'s User-Agent names viy"));
            }
            if !e["mismatch"].is_null() {
                return Some(format!(
                    "{at}: {side} hit a scenario mismatch: {}",
                    e["mismatch"]
                ));
            }
        }
        for key in ["method", "path"] {
            if x[key] != y[key] {
                return Some(format!("{at}: {key} {} != {}", x[key], y[key]));
            }
        }
        if x["headers"]["authorization"] != y["headers"]["authorization"] {
            return Some(format!("{at}: Authorization headers differ"));
        }
        if let Some(d) = diff(
            &json_strings(&x["body"]),
            &json_strings(&y["body"]),
            "body",
            false,
        ) {
            return Some(format!("{at}: {d}"));
        }
    }
    (a.len() != b.len()).then(|| {
        let (side, extra) = if a.len() > b.len() {
            ("A", a)
        } else {
            ("B", b)
        };
        let first = &extra[a.len().min(b.len())];
        format!(
            "{side} has {} more request(s), the first: {} {}",
            a.len().abs_diff(b.len()),
            text(&first["method"]),
            text(&first["path"])
        )
    })
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// `body` with tool results and call arguments that hold JSON parsed.
fn json_strings(body: &Value) -> Value {
    let parse = |v: &Value| {
        v.as_str()
            .filter(|s| s.trim_start().starts_with(['{', '[']))
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .map(|parsed| serde_json::json!({"$json": parsed}))
    };
    let mut body = body.clone();
    for m in body["messages"].as_array_mut().into_iter().flatten() {
        if m["role"] == "tool"
            && let Some(parsed) = parse(&m["content"])
        {
            m["content"] = parsed;
        }
        for tc in m["tool_calls"].as_array_mut().into_iter().flatten() {
            if let Some(parsed) = parse(&tc["function"]["arguments"]) {
                tc["function"]["arguments"] = parsed;
            }
        }
    }
    body
}

fn diff(a: &Value, b: &Value, path: &str, ordered: bool) -> Option<String> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            (x.as_f64() != y.as_f64()).then(|| format!("{path}: {x} != {y}"))
        }
        (Value::Object(x), Value::Object(y)) => {
            if ordered && !x.keys().eq(y.keys()) {
                return Some(format!(
                    "{path}: key order differs: {:?} vs {:?}",
                    x.keys().collect::<Vec<_>>(),
                    y.keys().collect::<Vec<_>>()
                ));
            }
            if let Some(k) = x.keys().find(|k| !y.contains_key(*k)) {
                return Some(format!("{path}.{k}: only in A"));
            }
            if let Some(k) = y.keys().find(|k| !x.contains_key(*k)) {
                return Some(format!("{path}.{k}: only in B"));
            }
            x.iter().find_map(|(k, v)| {
                diff(
                    v,
                    &y[k],
                    &format!("{path}.{k}"),
                    ordered || (path == "body" && k == "tools"),
                )
            })
        }
        (Value::Array(x), Value::Array(y)) => x
            .iter()
            .zip(y)
            .enumerate()
            .find_map(|(i, (v, w))| diff(v, w, &format!("{path}[{i}]"), ordered))
            .or_else(|| {
                (x.len() != y.len()).then(|| format!("{path}: {} vs {} items", x.len(), y.len()))
            }),
        (Value::String(x), Value::String(y)) if x != y => {
            let i = x.chars().zip(y.chars()).take_while(|(p, q)| p == q).count();
            let around = |s: &str| {
                s.chars()
                    .skip(i.saturating_sub(40))
                    .take(120)
                    .collect::<String>()
            };
            Some(format!(
                "{path}: strings differ at char {i}:\n  A: ...{:?}\n  B: ...{:?}",
                around(x),
                around(y)
            ))
        }
        _ if std::mem::discriminant(a) != std::mem::discriminant(b) => {
            Some(format!("{path}: types differ, {a} vs {b}"))
        }
        _ => (a != b).then(|| format!("{path}: {a} != {b}")),
    }
}

#[cfg(test)]
#[path = "scenario_tests.rs"]
mod tests;
