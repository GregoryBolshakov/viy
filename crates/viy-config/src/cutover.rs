//! The record `viy migrate --finish` keeps of the cutover from rp,
//! `cutover.json` in viy's state directory, as Codex keeps a ledger of the
//! sessions it imported from another agent
//! (`external-agent-migration/src/sessions/ledger.rs`): which projects'
//! input histories are in, whether the pod state has moved to viy's state
//! directory, and when the cutover finished. [`crate::Paths::from_env`]
//! reads it to know where the pod state is (kb d135).

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;
use serde_json::json;

use crate::atomic::write_atomically;

/// The record's file name in viy's state directory.
pub const FILE: &str = "cutover.json";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Record {
    /// The project roots whose `.rpctl/history` was imported, in order.
    pub histories: Vec<PathBuf>,
    /// The pod state lives in viy's state directory, no longer in rp's.
    pub pod_state_moved: bool,
    /// When the cutover finished, Unix seconds.
    pub finished: Option<f64>,
}

impl Record {
    /// The record in `state_dir`; the empty one when there is none. A file
    /// that is there but unreadable or not a record is an error: guessing
    /// where the pod state is could hide a running pod from viy.
    pub fn load(state_dir: &Path) -> io::Result<Self> {
        let path = state_dir.join(FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
        };
        let bad = |why: &str| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {why}", path.display()),
            )
        };
        let value: Value = serde_json::from_str(&text).map_err(|e| bad(&e.to_string()))?;
        let Some(map) = value.as_object() else {
            return Err(bad("not a JSON object"));
        };
        let histories = match map.get("histories") {
            None => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| v.as_str().map(PathBuf::from))
                .collect::<Option<_>>()
                .ok_or_else(|| bad("histories holds something that is not a path"))?,
            Some(_) => return Err(bad("histories is not a list")),
        };
        let pod_state_moved = match map.get("pod_state") {
            None => false,
            Some(Value::String(s)) if s == "moved" => true,
            Some(_) => return Err(bad("pod_state is not \"moved\"")),
        };
        let finished = match map.get("finished") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_f64().ok_or_else(|| bad("finished is not a number"))?),
        };
        Ok(Self {
            histories,
            pod_state_moved,
            finished,
        })
    }

    /// Writes the record to `state_dir` in one step, owner-only.
    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        let mut map = serde_json::Map::new();
        map.insert(
            "histories".into(),
            json!(
                self.histories
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            ),
        );
        if self.pod_state_moved {
            map.insert("pod_state".into(), json!("moved"));
        }
        if let Some(t) = self.finished {
            map.insert("finished".into(), json!(t));
        }
        let mut text = Value::Object(map).to_string();
        text.push('\n');
        write_atomically(&state_dir.join(FILE), &text)
    }
}

#[cfg(test)]
#[path = "cutover_tests.rs"]
mod tests;
