//! The scenarios in `tests/scenarios/` (format: its README), the request
//! logs recorded from rp for them by `tools/drive_rp.py --record`
//! (implementation plan 3.4), and [`compare`], the Rust twin of
//! `tools/parity/compare_requests.py` (kb d031), and the session logs rp
//! kept for them (`<name>.rp.session.jsonl`, WP 3.5).

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

/// Loads `tests/scenarios/<name>.rp.session.jsonl`, rp's session log for
/// that scenario as drive_rp normalizes it; `None` when rp kept none.
pub fn rp_session(name: &str) -> io::Result<Option<Vec<Value>>> {
    let path = dir().join(format!("{name}.rp.session.jsonl"));
    if !path.exists() {
        return Ok(None);
    }
    std::fs::read_to_string(path)?
        .lines()
        .map(|l| serde_json::from_str(l).map_err(io::Error::other))
        .collect::<io::Result<_>>()
        .map(Some)
}

/// A session log normalized as drive_rp's `session_lines`: no `ts` or
/// `created`, the session's id as `{ID}`, `subs` replaced.
pub fn session_events(path: &std::path::Path, subs: &[(&str, &str)]) -> io::Result<Vec<Value>> {
    let id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut subs = subs.to_vec();
    subs.push((&id, "{ID}"));
    std::fs::read_to_string(path)?
        .lines()
        .map(|l| {
            let mut ev = normalize(&serde_json::from_str(l).map_err(io::Error::other)?, &subs);
            if let Some(map) = ev.as_object_mut() {
                map.shift_remove("ts");
                map.shift_remove("created");
            }
            Ok(ev)
        })
        .collect()
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

/// `s` with each untrusted fence's random nonce (`<<<UNTRUSTED 1a2b3c4d`,
/// `UNTRUSTED 1a2b3c4d>>>`) as `{NONCE}`, as compare_requests.py does.
pub fn fence_nonces(s: &str) -> String {
    const MARK: &str = "UNTRUSTED ";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(MARK) {
        let after = &rest[i + MARK.len()..];
        let hex = after.len() >= 8
            && after.as_bytes()[..8]
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b));
        let ends = !after
            .as_bytes()
            .get(8)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
        out.push_str(&rest[..i + MARK.len()]);
        if hex && ends {
            out.push_str("{NONCE}");
            rest = &after[8..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `value` with each `(from, to)` replaced in every string, longest `from`
/// first, as compare_requests' `normalize` puts `{WORKSPACE}`, `{BASE}` and
/// `{TODAY}` in place of a run's paths and date, and the fence nonces
/// ([`fence_nonces`]).
pub fn normalize(value: &Value, subs: &[(&str, &str)]) -> Value {
    let mut subs: Vec<(&str, &str)> = subs
        .iter()
        .filter(|(f, _)| !f.is_empty())
        .copied()
        .collect();
    subs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    fn walk(v: &Value, subs: &[(&str, &str)]) -> Value {
        match v {
            Value::String(s) => Value::String(fence_nonces(
                &subs
                    .iter()
                    .fold(s.clone(), |s, (from, to)| s.replace(from, to)),
            )),
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
        for key in ["method", "path", "host"] {
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

/// The first difference between two session logs, as drive_rp's
/// `same_session` sees one (kb d031): the same events with their keys in
/// order, and a message's tool result or call arguments that hold JSON
/// compared as the JSON they hold (viy writes it compactly, kb d030).
pub fn session_diff(a: &[Value], b: &[Value]) -> Option<String> {
    if a.len() != b.len() {
        return Some(format!("{} events != {}", a.len(), b.len()));
    }
    let wrap = |e: &Value| {
        let mut e = e.clone();
        if e["message"].is_object() {
            let parsed = json_strings(&serde_json::json!({"messages": [e["message"].clone()]}));
            e["message"] = parsed["messages"][0].clone();
        }
        e
    };
    for (n, (x, y)) in a.iter().zip(b).enumerate() {
        let keys = |v: &Value| -> Vec<String> {
            v.as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k.clone())
                .collect()
        };
        if keys(x) != keys(y) {
            return Some(format!("event {n}: keys {:?} != {:?}", keys(x), keys(y)));
        }
        if let Some(d) = diff(&wrap(x), &wrap(y), &format!("event {n}"), false) {
            return Some(d);
        }
    }
    None
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
