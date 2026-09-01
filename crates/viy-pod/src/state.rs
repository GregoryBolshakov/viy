//! `pod.json`, the one pod's state, shared with rp until cutover (kb d006):
//! rpctl's `pod.load_state`, `save_state` and `clear_state`. It is a cache,
//! RunPod is the truth ([`crate::tracked::resolve`]). Every
//! read-modify-write holds `pod.json.lock`, the same flock rp takes, so
//! neither tool's update is lost to the other's; the file is replaced
//! whole, so a reader never sees half of one.
//!
//! rp's fallback to its old in-repo `.pod_state.json` is not ported: a file
//! from before `rp migrate` (kb d029).

use std::fs::File;
use std::io;

use serde_json::Map;
use serde_json::Value;
use viy_config::Paths;
use viy_config::atomic::write_atomically;

use crate::lock::open_lock_file;
use crate::lock::with_path;

/// The keys rp and viy write, in rpctl's spelling.
pub mod key {
    /// The tracked pod's id; [`crate::tracked::resolve`] caches it.
    pub const POD_ID: &str = "pod_id";
    /// The pod's name on RunPod.
    pub const NAME: &str = "name";
    /// USD per hour, as RunPod reported it.
    pub const COST_PER_HR: &str = "cost_per_hr";
    /// The network volume the pod mounts.
    pub const VOLUME_ID: &str = "volume_id";
    /// When `up` created the pod, Unix seconds.
    pub const CREATED_AT: &str = "created_at";
    /// Whether the pod was created reachable only through the SSH tunnel.
    pub const TUNNEL_ONLY: &str = "tunnel_only";
    /// The last fresh boot's length in seconds, to one decimal.
    pub const LAST_STARTUP_SECONDS: &str = "last_startup_seconds";
    /// The backgrounded tunnel's ssh pid, `null` once it is stopped.
    pub const TUNNEL_PID: &str = "tunnel_pid";
    /// The tunnel's local port, as a string.
    pub const TUNNEL_LOCAL_PORT: &str = "tunnel_local_port";
}

/// What `pod.json` holds: its keys in file order, unknown ones kept.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PodState(Map<String, Value>);

impl PodState {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }

    /// The tracked pod's id, if one is cached.
    pub fn pod_id(&self) -> Option<&str> {
        self.text(key::POD_ID)
    }

    /// The tracked pod's network volume, if recorded.
    pub fn volume_id(&self) -> Option<&str> {
        self.text(key::VOLUME_ID)
    }

    /// How the tracked pod was created: `None` for a pod from before tunnel
    /// mode, which recorded nothing. rp and viy write a bool; any other
    /// value is read as rp's `bool(...)` reads it, so both tools agree on it.
    pub fn tunnel_only(&self) -> Option<bool> {
        self.0.get(key::TUNNEL_ONLY).map(|v| match v {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64() != Some(0.0),
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
        })
    }

    fn text(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }
}

/// `pod.json` as it is now: empty when there is none, or it is not a JSON
/// object, as rp reads it.
pub fn load(paths: &Paths) -> io::Result<PodState> {
    let path = paths.pod_state_file();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PodState::default()),
        Err(e) => return Err(with_path(&path, e)),
    };
    Ok(match serde_json::from_slice(&bytes) {
        Ok(Value::Object(map)) => PodState(map),
        _ => PodState::default(),
    })
}

/// Sets `updates` in `pod.json` and returns the whole state after them.
/// Written as rp writes it, two-space indent and a newline at the end, but
/// with text as UTF-8 where rp's `json.dumps` writes `\uXXXX` escapes: the
/// same values to either reader (kb d030).
pub fn save<'a>(
    paths: &Paths,
    updates: impl IntoIterator<Item = (&'a str, Value)>,
) -> io::Result<PodState> {
    update(paths, |map| {
        for (k, v) in updates {
            map.insert(k.to_string(), v);
        }
    })
}

/// Changes `pod.json` as `change` says, under the same lock as [`save`],
/// and returns the whole state after it. Keys removed with
/// `Map::shift_remove` leave the others in their order, as Python's
/// `dict.pop` does.
pub fn update(paths: &Paths, change: impl FnOnce(&mut Map<String, Value>)) -> io::Result<PodState> {
    let _lock = state_lock(paths)?;
    let mut state = load(paths)?;
    change(&mut state.0);
    let mut text = serde_json::to_string_pretty(&state.0).map_err(io::Error::other)?;
    text.push('\n');
    write_atomically(&paths.pod_state_file(), &text)?;
    Ok(state)
}

/// Forgets the pod: `pod.json` removed.
pub fn clear(paths: &Paths) -> io::Result<()> {
    let _lock = state_lock(paths)?;
    let path = paths.pod_state_file();
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(with_path(&path, e)),
        _ => Ok(()),
    }
}

/// `pod.json.lock`, taken and held until the file is dropped.
fn state_lock(paths: &Paths) -> io::Result<File> {
    let path = paths.pod_state_lock_file();
    let file = open_lock_file(&path)?;
    file.lock().map_err(|e| with_path(&path, e))?;
    Ok(file)
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
