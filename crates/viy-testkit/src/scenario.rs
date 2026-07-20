//! The scenarios in `tests/scenarios/` (format: its README) and the request
//! logs recorded from rp for them by `tools/drive_rp.py --record`
//! (implementation plan 3.4).

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
