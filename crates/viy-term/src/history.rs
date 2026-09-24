//! The input history (ui-plan 5): what was sent from the input box, per
//! project, recalled with Up and Down.
//!
//! rp keeps readline's history in `.rpctl/history`, read at chat start and
//! written whole at exit, 2000 entries. viy keeps its own,
//! `.viy/history` ([`viy_config::Project::history_file`] gives the path),
//! as Codex keeps `history.jsonl`
//! (`codex-rs/message-history/src/lib.rs`): one JSON object per line,
//! `{"ts": <unix seconds>, "text": "..."}`, so a message with newlines is
//! one entry; each entry appended as it is sent, in one write under an
//! exclusive lock, owner-only, so two chats in one project both keep
//! theirs. The newest 2000 are kept, rp's length. rp's file is copied in by
//! `viy migrate --finish` at cutover (kb d043), not read here.
//!
//! Recall follows Codex's `chat_composer_history.rs`: Up and Down browse
//! when the box is empty, or still holds the entry they last recalled with
//! the cursor at its start or end; otherwise they move the cursor.

use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use serde_json::json;

/// Entries kept: rp's `readline.set_history_length(2000)`.
pub const MAX_ENTRIES: usize = 2000;

/// The file is cut back to [`MAX_ENTRIES`] once it holds this many more,
/// so a cut is not a rewrite on every message.
const SLACK: usize = 200;

#[derive(Debug, Default)]
pub struct History {
    /// Where entries are appended; none keeps them in memory only.
    path: Option<PathBuf>,
    /// Oldest first.
    entries: Vec<String>,
    /// The entry being shown, while browsing.
    at: Option<usize>,
    /// What the last recall put in the box.
    recalled: Option<String>,
}

impl History {
    /// The history in `path`: its newest [`MAX_ENTRIES`] entries; empty
    /// when it is missing or unreadable, and lines that are not entries are
    /// skipped.
    pub fn load(path: PathBuf) -> Self {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut entries: Vec<String> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|v| v.get("text")?.as_str().map(str::to_string))
            .collect();
        let extra = entries.len().saturating_sub(MAX_ENTRIES);
        entries.drain(..extra);
        Self {
            path: Some(path),
            entries,
            at: None,
            recalled: None,
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Adds what was sent, unless it repeats the newest entry, and appends
    /// it to the file. Browsing starts again from the newest.
    pub fn record(&mut self, text: &str) -> io::Result<()> {
        self.at = None;
        self.recalled = None;
        if text.trim().is_empty() || self.entries.last().is_some_and(|e| e == text) {
            return Ok(());
        }
        self.entries.push(text.to_string());
        let extra = self.entries.len().saturating_sub(MAX_ENTRIES);
        self.entries.drain(..extra);
        match &self.path {
            Some(path) => append(path, text),
            None => Ok(()),
        }
    }

    /// Whether Up and Down browse, for the box's `text` and `cursor`.
    pub fn browses(&self, text: &str, cursor: usize) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        if text.is_empty() {
            return true;
        }
        (cursor == 0 || cursor == text.len()) && self.recalled.as_deref() == Some(text)
    }

    /// The entry before the one shown, or the newest; `None` at the oldest.
    pub fn older(&mut self) -> Option<String> {
        let next = match self.at {
            None => self.entries.len().checked_sub(1)?,
            Some(i) => i.checked_sub(1)?,
        };
        self.show(Some(next))
    }

    /// The entry after the one shown; past the newest, an empty box.
    pub fn newer(&mut self) -> Option<String> {
        let i = self.at?;
        if i + 1 < self.entries.len() {
            self.show(Some(i + 1))
        } else {
            self.show(None)
        }
    }

    fn show(&mut self, at: Option<usize>) -> Option<String> {
        self.at = at;
        let text = at
            .and_then(|i| self.entries.get(i).cloned())
            .unwrap_or_default();
        self.recalled = Some(text.clone());
        Some(text)
    }
}

/// Appends one entry in one write, under an exclusive lock, and cuts the
/// file back to the newest [`MAX_ENTRIES`] when it has grown [`SLACK`]
/// past them.
fn append(path: &PathBuf, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut line = json!({"ts": ts, "text": text}).to_string();
    line.push('\n');
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    file.lock()?;
    file.write_all(line.as_bytes())?;
    let mut all = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut all)?;
    let count = all.lines().count();
    if count > MAX_ENTRIES + SLACK {
        let keep: String = all
            .lines()
            .skip(count - MAX_ENTRIES)
            .flat_map(|l| [l, "\n"])
            .collect();
        file.set_len(0)?;
        file.write_all(keep.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
