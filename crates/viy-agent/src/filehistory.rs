//! What write_file overwrote, so `/rewind` can put files back (rpctl
//! filehistory.py, with chat.py's `_file_actions`).
//!
//! Every write_file call records the file as it was before the write, keyed
//! by the tool call's id. The id is in the history, so the turns `/rewind`
//! drops name exactly the writes to undo, and compaction or a resume
//! changes nothing.
//!
//! A saved chat keeps its records next to its log, in `<id>.files/`, the
//! layout rp reads and writes:
//!
//! - `writes.jsonl`: one line per write, `{id, path, before, after, ts}`;
//!   `before` and `after` are SHA-1s of the content, `before` null when
//!   write_file created the file;
//! - `<sha1>`: the content of each `before`, stored once.
//!
//! A chat without a session log keeps the records in memory for the run.
//!
//! Only write_file is recorded. What run_shell or ask_claude change is not,
//! as with Claude Code's own checkpoints and its Bash tool. Codex keeps no
//! such record: its rollback drops turns only (`drop_last_n_user_turns`),
//! and its git snapshots ("undo") are gone at 970111f.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;
use viy_tools::resolve::resolve_checked;

use crate::files::sha1_hex;

const WRITES: &str = "writes.jsonl";

/// One write_file call: what the file was before it and what it left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    pub path: String,
    /// `None`: write_file created the file.
    pub before: Option<String>,
    pub after: String,
    /// Seconds since the epoch.
    #[serde(default)]
    pub ts: f64,
}

/// Undoing one file: as it was before the first of the writes undone, and
/// as the last of them left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub path: PathBuf,
    /// `None`: the file did not exist.
    pub before: Option<String>,
    pub after: String,
}

/// What undoing a [`Planned`] file takes, or took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Its old content goes back.
    Restore,
    /// write_file created it; it goes.
    Delete,
    /// Already as it was.
    Same,
    /// Something else wrote it since, or a link now stands in its path:
    /// left alone.
    Changed,
    /// Its old content was not kept: left alone.
    Lost,
    /// Reading or undoing it failed, and why.
    Failed(String),
}

impl Action {
    /// Whether undoing it changes the file.
    pub fn changes(&self) -> bool {
        matches!(self, Self::Restore | Self::Delete)
    }
}

/// A chat's record of what write_file overwrote: in `<id>.files/`, or in
/// memory.
#[derive(Debug, Clone, Default)]
pub struct FileHistory {
    folder: Option<PathBuf>,
    mem: Vec<Row>,
    blobs: HashMap<String, Vec<u8>>,
}

/// Whether `digest` can name a blob: a SHA-1 in hex, nothing that walks
/// out of the folder.
fn is_digest(digest: &str) -> bool {
    digest.len() == 40 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

impl FileHistory {
    /// Records kept for this run only.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Records in `folder` (a session's `files_dir`).
    pub fn at(folder: PathBuf) -> Self {
        Self {
            folder: Some(folder),
            ..Self::default()
        }
    }

    /// Remembers that call `call_id` replaced `before` (`None`: the file
    /// did not exist) at `path` by content hashing to `after`. A call
    /// without an id is not recorded.
    pub fn record(
        &mut self,
        call_id: &str,
        path: &Path,
        before: Option<&[u8]>,
        after: &str,
    ) -> io::Result<()> {
        if call_id.is_empty() {
            return Ok(());
        }
        let shown = path
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the path is not UTF-8"))?;
        let row = Row {
            id: call_id.into(),
            path: shown.into(),
            before: before.map(sha1_hex),
            after: after.into(),
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64()),
        };
        let Some(folder) = &self.folder else {
            if let (Some(data), Some(digest)) = (before, &row.before) {
                self.blobs.insert(digest.clone(), data.to_vec());
            }
            self.mem.push(row);
            return Ok(());
        };
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(folder)?;
        if let (Some(data), Some(digest)) = (before, &row.before) {
            let blob = folder.join(digest);
            if !blob.exists() {
                let tmp = folder.join(format!("{digest}.{}.tmp", std::process::id()));
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)?;
                file.write_all(data)?;
                file.sync_all()?;
                fs::rename(&tmp, &blob)?;
            }
        }
        let mut line = serde_json::to_vec(&row).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(folder.join(WRITES))?;
        // One write of the whole line, so a crash leaves at most a partial
        // last line, which `rows` skips.
        file.write_all(&line)?;
        file.sync_all()
    }

    /// Every write recorded, oldest first. A line cut short (a crash while
    /// writing it) ends the list; one that does not parse is skipped.
    pub fn rows(&self) -> Vec<Row> {
        let Some(folder) = &self.folder else {
            return self.mem.clone();
        };
        let Ok(data) = fs::read(folder.join(WRITES)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut rest = data.as_slice();
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            let (line, more) = rest.split_at(end);
            rest = &more[1..];
            if let Ok(row) = serde_json::from_slice::<Row>(line)
                && !row.id.is_empty()
                && !row.path.is_empty()
            {
                out.push(row);
            }
        }
        out
    }

    /// The content stored under `digest`, or `None` when it is missing.
    pub fn blob(&self, digest: &str) -> Option<Vec<u8>> {
        let Some(folder) = &self.folder else {
            return self.blobs.get(digest).cloned();
        };
        if !is_digest(digest) {
            return None;
        }
        fs::read(folder.join(digest)).ok()
    }

    /// What undoing the writes made by `call_ids` takes, one entry per file
    /// in the order first written.
    pub fn plan<'a>(&self, call_ids: impl IntoIterator<Item = &'a str>) -> Vec<Planned> {
        let ids: std::collections::HashSet<&str> = call_ids.into_iter().collect();
        let mut files: Vec<Planned> = Vec::new();
        for row in self.rows() {
            if !ids.contains(row.id.as_str()) {
                continue;
            }
            let path = PathBuf::from(&row.path);
            match files.iter_mut().find(|f| f.path == path) {
                Some(f) => f.after = row.after,
                None => files.push(Planned {
                    path,
                    before: row.before,
                    after: row.after,
                }),
            }
        }
        files
    }

    /// What undoing each of `files` takes (rp's `_file_actions`): restore,
    /// delete (write_file created it), same (already as it was), changed
    /// (something else wrote it since: left alone) or lost (its old content
    /// is gone). With `apply`, does it; a failure becomes
    /// [`Action::Failed`]. The caller holds the workspace lock.
    ///
    /// The paths were resolved when written. One that no longer resolves
    /// to itself (a link put in its place or in a folder above it since)
    /// counts as changed, so undoing never writes or deletes through a
    /// link to somewhere else.
    pub fn actions(&self, files: &[Planned], apply: bool) -> Vec<Action> {
        files.iter().map(|f| self.action(f, apply)).collect()
    }

    fn action(&self, f: &Planned, apply: bool) -> Action {
        let path = &f.path;
        let now = match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Action::Failed(e.to_string()),
            Ok(_) if resolve_checked(path).as_ref() != Ok(path) => return Action::Changed,
            Ok(meta) if !meta.is_file() => None,
            Ok(_) => match fs::read(path) {
                Ok(data) => Some(sha1_hex(&data)),
                Err(e) => return Action::Failed(e.to_string()),
            },
        };
        let mut data = None;
        let action = if now == f.before {
            Action::Same
        } else if now.as_deref() != Some(f.after.as_str()) {
            Action::Changed
        } else if f.before.is_none() {
            Action::Delete
        } else {
            data = f
                .before
                .as_deref()
                .and_then(|d| self.blob(d))
                .filter(|b| Some(sha1_hex(b)) == f.before);
            if data.is_some() {
                Action::Restore
            } else {
                Action::Lost
            }
        };
        if !apply || !action.changes() {
            return action;
        }
        let done = match data {
            None => fs::remove_file(path),
            Some(data) => fs::write(path, data),
        };
        match done {
            Ok(()) => action,
            Err(e) => Action::Failed(e.to_string()),
        }
    }
}

#[cfg(test)]
#[path = "filehistory_tests.rs"]
mod tests;
