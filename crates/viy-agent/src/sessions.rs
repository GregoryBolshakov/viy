//! Automatic chat sessions: rpctl's `sessions.py`. Every chat is an
//! append-only JSONL log in viy's sessions directory, one JSON event per
//! line, in rp's format, so a chat moves between the tools as it is:
//!
//! ```text
//!   meta     {id, created, project, model, source?}   first line, written lazily
//!   msg      {message}                                a message joined the history
//!   replace  {messages}                               history rewritten (compact, load)
//!   system   {text}                                   system prompt changed
//!   title    {text, auto, by?}                        auto title or /rename; last wins
//!                                                     (by: "model" for a generated one)
//!   end      {reason}                                 clean exit (informational)
//! ```
//!
//! Every event is one write of a whole line followed by fsync, so a crash
//! or a closed terminal loses at most what was in flight. A torn final line
//! is ignored on replay and trimmed before a resumed session appends again.
//! A `replace` only changes the live history: everything before it stays in
//! the file, so compaction never destroys anything. One writer per log: an
//! exclusive `flock`, as rp takes, refuses a second.
//!
//! With CHAT_REDACT=1 the chat gives the log a [`Redactor`], and message,
//! system and title events are masked on their way to disk.
//!
//! Listing reads a small `<id>.meta.json` sidecar per chat instead of
//! replaying every log; a missing or stale sidecar is rebuilt from the log.
//! `<id>.files/` holds what write_file overwrote, for /rewind.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use rand::TryRngCore;
use rand::rngs::OsRng;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_config::redact::Redactor;
use viy_time::TimeProvider;

pub const TITLE_CHARS: usize = 60;

/// A text part of a user message that the client added, not the user (the
/// /mount note, the /ssh note). Titles and /rewind leave it out. rp's words,
/// kept: the model reads them.
pub const CLIENT_NOTE_PREFIX: &str = "[viy chat: ";
/// rp's prefix (flag F21), in the chats rp saved.
pub const RP_CLIENT_NOTE_PREFIX: &str = "[rp chat: ";

/// Whether a user message's text part is a note of the client's, viy's or
/// one rp wrote.
pub fn is_client_note(text: &str) -> bool {
    text.starts_with(CLIENT_NOTE_PREFIX) || text.starts_with(RP_CLIENT_NOTE_PREFIX)
}

/// How a compaction's summary opens.
pub const SUMMARY_PREFIX: &str = "[Summary of the conversation";

/// What stands where a compaction summarised the messages before it away
/// ([`State::earlier`]).
pub fn compacted() -> Value {
    json!({"compacted": true})
}

/// The events a [`Redactor`] masks.
const REDACTED_EVENTS: [&str; 4] = ["msg", "replace", "system", "title"];

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Another chat holds this session open.
    #[error("chat {0} is open in another viy chat")]
    Busy(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Seconds since the epoch, as rp's `time.time()` stamps events.
fn seconds(t: DateTime<Utc>) -> f64 {
    t.timestamp() as f64 + f64::from(t.timestamp_subsec_nanos()) / 1e9
}

/// Python's truth of a JSON value, for fields rp tests with `if`.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The searchable, readable text of one message; images are skipped.
pub fn message_text(m: &Value, with_tools: bool) -> String {
    let content = match &m["content"] {
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .map(|p| p["text"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" "),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let mut parts = vec![content];
    if with_tools {
        for tc in m["tool_calls"].as_array().into_iter().flatten() {
            let f = &tc["function"];
            parts.push(format!(
                "{}({})",
                f["name"].as_str().unwrap_or_default(),
                f["arguments"].as_str().unwrap_or_default()
            ));
        }
    }
    parts.retain(|p| !p.is_empty());
    parts.join("\n")
}

/// `text` cut to `max` characters, the last one an ellipsis.
fn cut(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let mut kept: String = text.chars().take(max - 1).collect();
    kept.push('…');
    kept
}

/// The first user message, flattened and cut short. No model call.
pub fn default_title(messages: &[Value]) -> Option<String> {
    for m in messages.iter().filter(|m| m["role"] == "user") {
        let mut m = m.clone();
        if let Some(parts) = m["content"].as_array() {
            let kept: Vec<Value> = parts
                .iter()
                .filter(|p| !p["text"].as_str().is_some_and(is_client_note))
                .cloned()
                .collect();
            m = json!({"content": kept});
        }
        let text = message_text(&m, false)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.starts_with(SUMMARY_PREFIX) {
            continue;
        }
        if !text.is_empty() {
            return Some(cut(text, TITLE_CHARS));
        }
    }
    None
}

/// Every parseable event in a log. A corrupt line is skipped; a torn final
/// line, from a killed writer, ends the read.
pub fn read_events(path: &Path) -> io::Result<Vec<Map<String, Value>>> {
    let data = fs::read(path)?;
    let mut events = Vec::new();
    let mut rest = data.as_slice();
    while let Some(end) = rest.iter().position(|&b| b == b'\n') {
        if let Ok(Value::Object(ev)) = serde_json::from_slice(&rest[..end]) {
            events.push(ev);
        }
        rest = &rest[end + 1..];
    }
    Ok(events)
}

/// Whether `msgs` opens with the summary and acknowledgement a compaction
/// puts in place of the turns it summarised.
pub fn opens_with_summary(msgs: &[Value]) -> bool {
    msgs.len() >= 2
        && msgs[0]["role"] == "user"
        && msgs[0]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with(SUMMARY_PREFIX))
}

/// The messages a `replace` from history `old` to `new` summarised away,
/// then a [`compacted`] marker; nothing when it was no new compaction (an
/// elision or a /rewind keeps the summary that opens the history).
pub fn compacted_away(old: &[Value], new: &[Value]) -> Vec<Value> {
    if !opens_with_summary(new) || (opens_with_summary(old) && old[0] == new[0]) {
        return Vec::new();
    }
    // The previous summary already left its marker.
    let old = if opens_with_summary(old) {
        &old[2..]
    } else {
        old
    };
    // The newest turns stay verbatim after the summary.
    let kept = new.len() - 2;
    let mut away = old[..old.len().saturating_sub(kept)].to_vec();
    away.push(compacted());
    away
}

/// A session's state, rebuilt from its log.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub id: String,
    /// The `meta` event, whole.
    pub meta: Map<String, Value>,
    pub system: Option<String>,
    pub messages: Vec<Value>,
    /// The messages compactions summarised away, each run followed by a
    /// [`compacted`] marker: no longer in `messages`, but /resume shows them.
    pub earlier: Vec<Value>,
    pub title: Option<String>,
    pub title_auto: bool,
    pub title_by_model: bool,
    /// The last event's time.
    pub updated: Option<f64>,
}

impl State {
    fn meta_str(&self, key: &str) -> Option<String> {
        self.meta
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

/// Rebuilds a session's state from its log.
pub fn replay(path: &Path) -> io::Result<State> {
    let mut st = State {
        id: stem(path),
        meta: Map::new(),
        system: None,
        messages: Vec::new(),
        earlier: Vec::new(),
        title: None,
        title_auto: true,
        title_by_model: false,
        updated: None,
    };
    for ev in read_events(path)? {
        match ev.get("t").and_then(Value::as_str) {
            Some("meta") => st.meta = ev.clone(),
            Some("msg") => {
                if let Some(m @ Value::Object(_)) = ev.get("message") {
                    st.messages.push(m.clone());
                }
            }
            Some("replace") => {
                let new = ev
                    .get("messages")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                st.earlier.extend(compacted_away(&st.messages, &new));
                st.messages = new;
            }
            Some("system") => st.system = ev.get("text").and_then(Value::as_str).map(Into::into),
            Some("title") => {
                if let Some(text) = ev
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    let auto = ev.get("auto").is_some_and(truthy);
                    // A title the user chose is never displaced by an
                    // automatic one.
                    if !(auto && !st.title_auto) {
                        st.title = Some(text.to_string());
                        st.title_auto = auto;
                        st.title_by_model = auto && ev.get("by") == Some(&json!("model"));
                    }
                }
            }
            _ => {}
        }
        if let Some(ts) = ev.get("ts").filter(|ts| truthy(ts)).and_then(Value::as_f64) {
            st.updated = Some(ts);
        }
    }
    Ok(st)
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A session's listing row: its sidecar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub title: String,
    pub title_auto: bool,
    pub project: Option<String>,
    pub created: Option<f64>,
    pub updated: Option<f64>,
    pub messages: usize,
    /// A one-shot chat (`viy exec`), which the resume pickers leave out
    /// unless asked (Codex's non-interactive sessions).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exec: bool,
    /// Where a search found its terms in the chat's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// How long ago `ts` was, as a listing says it (rp's `_age`): `just now`,
/// `5m ago`, `3h ago`, `2d ago`, then the local date.
pub fn age(ts: Option<f64>, now: DateTime<Utc>) -> String {
    let Some(ts) = ts.filter(|&t| t != 0.0) else {
        return "?".into();
    };
    let d = seconds(now) - ts;
    if d < 60.0 {
        "just now".into()
    } else if d < 3600.0 {
        format!("{}m ago", (d / 60.0) as i64)
    } else if d < 86400.0 {
        format!("{}h ago", (d / 3600.0) as i64)
    } else if d < 7.0 * 86400.0 {
        format!("{}d ago", (d / 86400.0) as i64)
    } else {
        DateTime::from_timestamp(ts as i64, 0)
            .map(|t| t.with_timezone(&Local).format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "?".into())
    }
}

/// Where a chat is filed, as a listing names it (rp's `_project_label`):
/// `this project`, a path under home as `~/...`, `?` for none.
pub fn project_label(project: Option<&str>, here: Option<&str>, home: &Path) -> String {
    let Some(project) = project.filter(|p| !p.is_empty()) else {
        return "?".into();
    };
    if Some(project) == here {
        return "this project".into();
    }
    match Path::new(project).strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => project.to_string(),
    }
}

/// How a listing shows one chat (rp's `_print_rows` without its colours),
/// numbered from 1: `*` marks `current`, the project shown with
/// `all_projects`; a search hit's snippet on a line below it.
pub fn row_lines(row: &Summary, number: usize, view: &RowView<'_>) -> Vec<String> {
    let mark = if view.current == Some(row.id.as_str()) {
        "*"
    } else {
        " "
    };
    let mut line = format!(
        "{mark}{number:>3}. {}  {} · {} msgs · {}",
        row.title,
        age(row.updated, view.now),
        row.messages,
        row.id
    );
    if view.all_projects {
        line.push_str("  ");
        line.push_str(&project_label(row.project.as_deref(), view.here, view.home));
    }
    let mut lines = vec![line];
    if let Some(snippet) = &row.snippet {
        lines.push(format!("       {snippet}"));
    }
    lines
}

/// What a listing needs besides the rows.
pub struct RowView<'a> {
    pub all_projects: bool,
    pub current: Option<&'a str>,
    /// This project's root.
    pub here: Option<&'a str>,
    pub home: &'a Path,
    pub now: DateTime<Utc>,
}

/// Rows a listing shows at most; rp's `_print_rows` limit.
pub const ROWS_SHOWN: usize = 25;

/// A whole listing: each row, then how many more there are.
pub fn listing(rows: &[Summary], view: &RowView<'_>) -> Vec<String> {
    let mut lines: Vec<String> = rows
        .iter()
        .take(ROWS_SHOWN)
        .enumerate()
        .flat_map(|(i, row)| row_lines(row, i + 1, view))
        .collect();
    if rows.len() > ROWS_SHOWN {
        lines.push(format!(
            "  ... {} more; narrow with a search",
            rows.len() - ROWS_SHOWN
        ));
    }
    lines
}

fn mtime(path: &Path) -> io::Result<std::time::SystemTime> {
    fs::metadata(path)?.modified()
}

fn summary_of(st: &State, path: &Path) -> Summary {
    let created = st.meta.get("created").and_then(Value::as_f64);
    let updated = st
        .updated
        .or(created)
        .or_else(|| mtime(path).ok().map(|t| seconds(DateTime::<Utc>::from(t))));
    Summary {
        id: st.id.clone(),
        title: st
            .title
            .clone()
            .or_else(|| default_title(&st.messages))
            .unwrap_or_else(|| "(empty)".into()),
        title_auto: st.title_auto,
        project: st.meta_str("project"),
        created,
        updated,
        messages: st.messages.len(),
        exec: st.meta.get("exec").is_some_and(truthy),
        snippet: None,
    }
}

/// Writes `data` as JSON through a temporary file, mode 0600.
fn write_json_atomic(path: &Path, data: &impl Serialize) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    fs::write(&tmp, serde_json::to_vec(data).map_err(io::Error::other)?)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path)
}

fn sidecar(path: &Path) -> PathBuf {
    path.with_extension("meta.json")
}

/// Where a session keeps the file contents /rewind can restore.
pub fn files_dir(path: &Path) -> PathBuf {
    path.with_extension("files")
}

/// A session's listing row, from its sidecar when that is current.
pub fn summary(path: &Path) -> io::Result<Summary> {
    let side = sidecar(path);
    if let (Ok(side_time), Ok(log_time)) = (mtime(&side), mtime(path))
        && side_time >= log_time
        && let Ok(row) = fs::read(&side)
            .map_err(drop)
            .and_then(|b| serde_json::from_slice(&b).map_err(drop))
    {
        return Ok(row);
    }
    let row = summary_of(&replay(path)?, path);
    // The log is the record; the sidecar is only a cache.
    let _ = write_json_atomic(&side, &row);
    Ok(row)
}

/// Whether a chat recorded under `chat_project` belongs to `scope`.
///
/// Projects are a tree: a project marker can sit at any level, so one
/// project can be nested inside another. A chat belongs to a scope when its
/// project is that scope or lies on the same root-to-leaf line: nested
/// inside the scope (a chat started in a sub-folder shows from the parent)
/// or an ancestor of it (a chat recorded against a parent still shows from
/// the sub-project). Sibling projects stay separate. `scope` `None` means
/// every project; a chat with no project matches nothing else.
pub fn in_scope(chat_project: Option<&str>, scope: Option<&str>) -> bool {
    let Some(scope) = scope else {
        return true;
    };
    let Some(project) = chat_project.filter(|p| !p.is_empty()) else {
        return false;
    };
    let (a, b) = (Path::new(project), Path::new(scope));
    a.starts_with(b) || b.starts_with(a)
}

/// Up to `width` characters of `text` around the first `term`, whitespace
/// folded.
fn snippet(text: &str, term: &str, width: usize) -> String {
    // The term is found in the lowercased text; each lowercased character
    // remembers which character of `text` it came from.
    let mut lowered = String::new();
    let mut origin = Vec::new();
    for (i, c) in text.chars().enumerate() {
        for l in c.to_lowercase() {
            lowered.push(l);
            origin.push(i);
        }
    }
    let Some(at) = lowered.find(term) else {
        return String::new();
    };
    let i = origin[lowered[..at].chars().count()];
    let start = i.saturating_sub(width / 3);
    let window: String = text.chars().skip(start).take(width).collect();
    let folded = window.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{}{folded}…", if start > 0 { "…" } else { "" })
}

/// Every chat's log: viy's sessions directory.
#[derive(Clone)]
pub struct Sessions {
    dir: PathBuf,
    clock: Arc<dyn TimeProvider>,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions").field("dir", &self.dir).finish()
    }
}

impl Sessions {
    /// The logs in `dir`, stamped by `clock`.
    pub fn new(dir: PathBuf, clock: Arc<dyn TimeProvider>) -> Self {
        Self { dir, clock }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The directory, made private; errors surface at the first write.
    fn ensure_dir(&self) -> &Path {
        if fs::create_dir_all(&self.dir).is_ok() {
            let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700));
        }
        &self.dir
    }

    pub fn path_of(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    /// A fresh id: the local time and four random hex digits.
    fn new_id(&self, at: DateTime<Utc>) -> String {
        static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
        let mut bytes = [0u8; 2];
        if OsRng.try_fill_bytes(&mut bytes).is_err() {
            // Without the OS's randomness, a counter keeps this process's
            // ids apart; the create_new in `create_file` keeps the rest.
            bytes = NEXT
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .to_be_bytes();
        }
        format!(
            "{}-{:02x}{:02x}",
            at.with_timezone(&Local).format("%Y%m%d-%H%M%S"),
            bytes[0],
            bytes[1]
        )
    }

    /// A new session's log. Nothing touches disk until there is something
    /// to keep.
    pub fn create(&self, project: Option<&str>, model: Option<&str>) -> SessionLog {
        self.ensure_dir();
        let id = self.new_id(self.clock.now());
        SessionLog::bare(self, id, project, model)
    }

    /// Reopens a session for appending; [`SessionError::Busy`] when another
    /// chat has it.
    pub fn open(&self, id: &str) -> Result<(SessionLog, State), SessionError> {
        let st = replay(&self.path_of(id))?;
        let mut log = SessionLog::from_state(self, id, &st);
        log.acquire()?;
        // Trim a torn final line, or the next event would be glued onto it.
        let data = fs::read(&log.path)?;
        if !data.is_empty()
            && !data.ends_with(b"\n")
            && let Some(file) = &log.file
        {
            let keep = data.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            file.set_len(keep as u64)?;
        }
        Ok((log, st))
    }

    /// A copy of session `id` to go on in, when another chat has it open
    /// or for `viy exec fork`: its history, project and model, named after
    /// it. The original is only read.
    pub fn copy(
        &self,
        id: &str,
        redact: Option<Redactor>,
        exec: bool,
    ) -> io::Result<(SessionLog, State)> {
        let st = replay(&self.path_of(id))?;
        let mut source = SessionLog::from_state(self, id, &st);
        source.redact = redact;
        // The copy is one-shot when `viy exec fork` makes it.
        source.exec = exec;
        let twin = source.fork(st.system.as_deref(), &st.messages, None)?;
        Ok((twin, st))
    }

    /// The store's clock.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// A search for `query`, or every row when it is empty (rp's `_find`).
    pub fn find(&self, query: &str, scope: Option<&str>) -> Vec<Summary> {
        if query.is_empty() {
            self.list(scope)
        } else {
            self.search(query, scope)
        }
    }

    /// Listing rows, newest first; only those in `scope`'s tree when given.
    pub fn list(&self, scope: Option<&str>) -> Vec<Summary> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        paths.sort();
        let mut rows: Vec<Summary> = paths
            .iter()
            .filter_map(|p| summary(p).ok())
            .filter(|r| in_scope(r.project.as_deref(), scope))
            .collect();
        rows.sort_by(|a, b| {
            b.updated
                .unwrap_or(0.0)
                .total_cmp(&a.updated.unwrap_or(0.0))
        });
        rows
    }

    /// Rows whose title, or any message ever in the log, contains every
    /// term. Title hits rank first, then recency; content hits carry a
    /// snippet. The whole log is searched, so text that compaction
    /// summarised away still finds its chat.
    pub fn search(&self, query: &str, scope: Option<&str>) -> Vec<Summary> {
        let query = query.to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        let rows = self.list(scope);
        if terms.is_empty() {
            return rows;
        }
        let (mut titled, mut content) = (Vec::new(), Vec::new());
        for row in rows {
            let title = row.title.to_lowercase();
            if terms.iter().all(|t| title.contains(t)) {
                titled.push(row);
                continue;
            }
            let Ok(events) = read_events(&self.path_of(&row.id)) else {
                continue;
            };
            let mut texts = Vec::new();
            for ev in &events {
                match ev.get("t").and_then(Value::as_str) {
                    Some("msg") => {
                        texts.push(message_text(
                            ev.get("message").unwrap_or(&Value::Null),
                            true,
                        ));
                    }
                    Some("replace") => texts.extend(
                        ev.get("messages")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .map(|m| message_text(m, true)),
                    ),
                    Some("system") => texts.push(
                        ev.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    _ => {}
                }
            }
            let blob = texts.join("\n");
            let low = blob.to_lowercase();
            if terms.iter().all(|t| low.contains(t) || title.contains(t)) {
                let hit = terms.iter().find(|t| low.contains(*t));
                content.push(Summary {
                    snippet: Some(hit.map(|t| snippet(&blob, t, 90)).unwrap_or_default()),
                    ..row
                });
            }
        }
        titled.extend(content);
        titled
    }

    /// The chat `target` names, as Codex's session commands take one: its
    /// id, else the exact name of one interactive chat.
    pub fn lookup(&self, target: &str) -> Result<Summary, LookupError> {
        let exact = self.path_of(target);
        if exact.is_file() {
            return summary(&exact).map_err(|_| LookupError::Missing);
        }
        let named: Vec<Summary> = self
            .list(None)
            .into_iter()
            .filter(|r| !r.exec && r.title == target)
            .collect();
        match named.as_slice() {
            [] => Err(LookupError::Missing),
            [one] => Ok(one.clone()),
            [a, b, ..] => Err(LookupError::Ambiguous(a.id.clone(), b.id.clone())),
        }
    }

    /// The archived sessions: `archived_sessions/` beside these, as
    /// Codex's `ARCHIVED_SESSIONS_SUBDIR`.
    pub fn archived(&self) -> Sessions {
        Sessions {
            dir: self.dir.with_file_name("archived_sessions"),
            clock: Arc::clone(&self.clock),
        }
    }

    /// Codex's `archive`: session `id` out of the lists, kept whole.
    pub fn archive(&self, id: &str) -> Result<(), SessionError> {
        self.move_to(id, &self.archived())
    }

    /// Codex's `unarchive`: archived session `id` back in this store.
    pub fn unarchive(&self, id: &str) -> Result<(), SessionError> {
        let archived = self.archived();
        archived.move_to(id, self)
    }

    /// Codex's `delete`: session `id` removed for good, its files with it.
    pub fn delete(&self, id: &str) -> Result<(), SessionError> {
        let src = self.path_of(id);
        let held = File::open(&src)?;
        lock(&held, id)?;
        fs::remove_file(&src)?;
        drop(held);
        remove_if_there(&sidecar(&src))?;
        let files = files_dir(&src);
        if files.is_dir() {
            fs::remove_dir_all(&files)?;
        }
        Ok(())
    }

    /// Moves session `id` (log, sidecar, files) to `to`; refused while a
    /// chat holds it.
    fn move_to(&self, id: &str, to: &Sessions) -> Result<(), SessionError> {
        let src = self.path_of(id);
        let held = File::open(&src)?;
        lock(&held, id)?;
        let dest = to.ensure_dir();
        fs::rename(&src, dest.join(src.file_name().unwrap_or_default()))?;
        drop(held);
        // The sidecar is a cache; the next listing there builds its own.
        remove_if_there(&sidecar(&src))?;
        let files = files_dir(&src);
        if files.is_dir() {
            fs::rename(&files, dest.join(files.file_name().unwrap_or_default()))?;
        }
        Ok(())
    }
}

/// Whether `text` has the shape of a session id (`20260718-135451-84e1`,
/// rp's and viy's), as Codex tells a UUID from a name.
pub fn is_session_id(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 20
        && b[8] == b'-'
        && b[15] == b'-'
        && b[..8].iter().chain(&b[9..15]).all(u8::is_ascii_digit)
        && b[16..].iter().all(u8::is_ascii_hexdigit)
}

/// Why [`Sessions::lookup`] found no one chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    Missing,
    /// Two of the chats with that name.
    Ambiguous(String, String),
}

fn remove_if_there(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Takes the exclusive lock on a session's file, or says who has it.
fn lock(file: &File, id: &str) -> Result<(), SessionError> {
    // A lock this process just let go of can still be held for a moment by
    // a child it is starting (a sandboxed command): between fork and exec
    // the child has a copy of every descriptor, and a flock lives as long
    // as any copy. So a few short tries before the log counts as another
    // chat's.
    for _ in 0..LOCK_TRIES {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(fs::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(fs::TryLockError::WouldBlock) => Err(SessionError::Busy(id.to_string())),
        Err(fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Fresh ids a new log may take when its id is already on disk.
const ID_TRIES: usize = 16;

/// Tries before a held log counts as another chat's (20 ms apart).
const LOCK_TRIES: usize = 10;

/// The live log of one chat, from [`Sessions::create`] or
/// [`Sessions::open`]. Nothing touches disk until there is something to
/// keep: a fresh session with no messages leaves no file behind.
pub struct SessionLog {
    store: Sessions,
    id: String,
    path: PathBuf,
    project: Option<String>,
    model: Option<String>,
    /// Open, appending and locked, once the log exists.
    file: Option<File>,
    /// The history as the log has it, before any masking.
    persisted: Vec<Value>,
    system: Option<String>,
    title: Option<String>,
    title_auto: bool,
    /// The model has already named this chat.
    title_by_model: bool,
    /// A one-shot chat (`viy exec`).
    exec: bool,
    redact: Option<Redactor>,
    created: Option<f64>,
    clock: Arc<dyn TimeProvider>,
}

impl std::fmt::Debug for SessionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLog")
            .field("id", &self.id)
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

impl SessionLog {
    fn bare(store: &Sessions, id: String, project: Option<&str>, model: Option<&str>) -> Self {
        Self {
            path: store.path_of(&id),
            store: store.clone(),
            id,
            project: project.filter(|p| !p.is_empty()).map(Into::into),
            model: model.map(Into::into),
            file: None,
            persisted: Vec::new(),
            system: None,
            title: None,
            title_auto: true,
            title_by_model: false,
            exec: false,
            redact: None,
            created: None,
            clock: Arc::clone(&store.clock),
        }
    }

    /// Session `id` as `st` replayed it, not yet open for appending.
    fn from_state(store: &Sessions, id: &str, st: &State) -> Self {
        let mut log = Self::bare(
            store,
            id.to_string(),
            st.meta_str("project").as_deref(),
            st.meta_str("model").as_deref(),
        );
        log.created = st.meta.get("created").and_then(Value::as_f64);
        log.persisted = st.messages.clone();
        log.system = st.system.clone();
        log.title = st.title.clone();
        log.title_auto = st.title_auto;
        log.title_by_model = st.title_by_model;
        log.exec = st.meta.get("exec").is_some_and(truthy);
        log
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Whether the title is automatic, not one the user chose.
    pub fn title_auto(&self) -> bool {
        self.title_auto
    }

    pub fn title_by_model(&self) -> bool {
        self.title_by_model
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    /// Marks a chat not yet written as one-shot (`viy exec`): the resume
    /// pickers leave it out unless asked.
    pub fn mark_exec(&mut self) {
        self.exec = true;
    }

    /// Masks what goes to disk from now on (CHAT_REDACT).
    pub fn set_redactor(&mut self, redact: Option<Redactor>) {
        self.redact = redact;
    }

    /// Whether the log has been written.
    pub fn exists(&self) -> bool {
        self.file.is_some()
    }

    pub fn files_dir(&self) -> PathBuf {
        files_dir(&self.path)
    }

    /// A new session that starts with this history, named `title` or after
    /// this one.
    pub fn fork(
        &self,
        system: Option<&str>,
        messages: &[Value],
        title: Option<&str>,
    ) -> io::Result<SessionLog> {
        let mut twin = self
            .store
            .create(self.project.as_deref(), self.model.as_deref());
        match title.filter(|t| !t.is_empty()) {
            Some(title) => {
                twin.title = Some(title.to_string());
                twin.title_auto = false;
            }
            None => {
                twin.title = self.title.as_ref().map(|t| format!("{t} (fork)"));
                twin.title_auto = self.title_auto;
            }
        }
        // A model title would describe the start both chats share, not
        // where the fork goes, so the fork does not ask for one.
        twin.title_by_model = twin.title.is_some();
        twin.redact = self.redact.clone();
        twin.exec = self.exec;
        twin.sync(system, messages, true)?;
        if self.files_dir().is_dir() {
            // The copied history names the same writes, so /rewind still
            // works.
            copy_tree(&self.files_dir(), &twin.files_dir())?;
        }
        if let Some(title) = twin.title.clone() {
            let mut ev = json!({"t": "title", "text": title, "auto": twin.title_auto});
            if twin.title_by_model {
                ev["by"] = json!("model");
            }
            twin.append(ev)?;
            twin.write_sidecar();
        }
        Ok(twin)
    }

    fn acquire(&mut self) -> Result<(), SessionError> {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.path)?;
        lock(&file, &self.id)?;
        self.file = Some(file);
        Ok(())
    }

    /// Writes one event as one line, stamped and masked, and syncs it to
    /// disk.
    fn append(&mut self, mut event: Value) -> io::Result<()> {
        if let Some(map) = event.as_object_mut()
            && !map.contains_key("ts")
        {
            map.insert("ts".into(), json!(seconds(self.clock.now())));
        }
        if let Some(redact) = &self.redact
            && event["t"]
                .as_str()
                .is_some_and(|t| REDACTED_EVENTS.contains(&t))
        {
            event = redact.value(&event);
        }
        let mut line = serde_json::to_vec(&event).map_err(io::Error::other)?;
        line.push(b'\n');
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("the session log is not open"))?;
        file.write_all(&line)?;
        file.sync_all()
    }

    fn create_file(&mut self) -> io::Result<()> {
        // A new log is never appended to an existing one: two chats started
        // in the same second can draw the same id, and the second then
        // takes a fresh one.
        let mut tries = 0;
        let file = loop {
            match OpenOptions::new()
                .append(true)
                .create_new(true)
                .mode(0o600)
                .open(&self.path)
            {
                Ok(file) => break file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tries < ID_TRIES => {
                    tries += 1;
                    self.id = self.store.new_id(self.clock.now());
                    self.path = self.store.path_of(&self.id);
                }
                Err(e) => return Err(e),
            }
        };
        lock(&file, &self.id).map_err(|e| match e {
            SessionError::Io(e) => e,
            busy => io::Error::new(io::ErrorKind::WouldBlock, busy.to_string()),
        })?;
        self.file = Some(file);
        let created = seconds(self.clock.now());
        self.created = Some(created);
        let mut ev = json!({
            "t": "meta",
            "id": self.id,
            "created": created,
            "project": self.project,
            "model": self.model,
        });
        if self.exec {
            ev["exec"] = json!(true);
        }
        self.append(ev)
    }

    /// Brings the log in line with the live history: appends new messages
    /// when the history only grew, writes one `replace` when it was
    /// rewritten. True when anything was written.
    pub fn sync(
        &mut self,
        system: Option<&str>,
        messages: &[Value],
        force: bool,
    ) -> io::Result<bool> {
        if !self.exists() {
            if messages.is_empty() && !force {
                return Ok(false);
            }
            self.create_file()?;
        }
        let mut wrote = false;
        if system != self.system.as_deref() {
            self.append(json!({"t": "system", "text": system}))?;
            self.system = system.map(Into::into);
            wrote = true;
        }
        let n = self.persisted.len();
        if messages.len() >= n && messages[..n] == self.persisted[..] {
            for m in &messages[n..] {
                self.append(json!({"t": "msg", "message": m}))?;
                self.persisted.push(m.clone());
                wrote = true;
            }
        } else {
            self.append(json!({"t": "replace", "messages": messages}))?;
            self.persisted = messages.to_vec();
            wrote = true;
        }
        if self.title.is_none()
            && let Some(title) = default_title(messages)
        {
            self.title = Some(title.clone());
            self.title_auto = true;
            self.append(json!({"t": "title", "text": title, "auto": true}))?;
            wrote = true;
        }
        if wrote {
            self.write_sidecar();
        }
        Ok(wrote)
    }

    /// Sets a title the user chose; creates the file if need be.
    pub fn rename(&mut self, title: &str) -> io::Result<()> {
        if !self.exists() {
            self.create_file()?;
        }
        self.title = Some(title.to_string());
        self.title_auto = false;
        self.append(json!({"t": "title", "text": title, "auto": false}))?;
        self.write_sidecar();
        Ok(())
    }

    /// Adopts a model-generated title unless the user named the chat. True
    /// when it was written.
    pub fn set_auto_title(&mut self, title: &str) -> io::Result<bool> {
        if !self.exists() || !self.title_auto || title.is_empty() {
            return Ok(false);
        }
        self.title = Some(title.to_string());
        self.title_by_model = true;
        self.append(json!({"t": "title", "text": title, "auto": true, "by": "model"}))?;
        self.write_sidecar();
        Ok(true)
    }

    fn write_sidecar(&self) {
        let mut title = self.title.clone().unwrap_or_else(|| "(empty)".into());
        if let Some(redact) = &self.redact {
            title = redact.text(&title);
        }
        let row = Summary {
            id: self.id.clone(),
            title,
            title_auto: self.title_auto,
            project: self.project.clone(),
            created: self.created,
            updated: Some(seconds(self.clock.now())),
            messages: self.persisted.len(),
            exec: self.exec,
            snippet: None,
        };
        // The log is the record; the sidecar is only a cache.
        let _ = write_json_atomic(&sidecar(&self.path), &row);
    }

    /// Ends the log with an `end` event and releases it.
    pub fn close(&mut self, reason: &str) -> io::Result<()> {
        if !self.exists() {
            return Ok(());
        }
        let written = self.append(json!({"t": "end", "reason": reason}));
        if written.is_ok() {
            self.write_sidecar();
        }
        self.file = None;
        written
    }
}

/// Copies the directory `from` to `to`, which must not exist.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;
