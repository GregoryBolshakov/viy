//! The file tools: `read_file`, `write_file`, `list_dir` (rpctl chat.py
//! `_tool_read_file`, `_tool_write_file`, `_tool_list_dir`, with
//! `_resolve_in_workspace`, `_guard_path` and `_write_conflict`).
//!
//! - Confinement: a path is relative to the workspace, or absolute inside a
//!   folder the user mounted; nothing else is reachable, `..` included.
//! - Protected paths (the clients' code, `.git`, `.venv`, env files...) need
//!   the user's yes to write, secret files to read (`viy_tools::guard`); a
//!   read-only mount refuses every write.
//! - The write guard: an existing file must have been read (or written) by
//!   this chat, and unchanged since, or the write would erase what another
//!   chat, a helper or a command put there; checked and written under a lock
//!   shared with every rp and viy process on the workspace. A new
//!   knowledge-base fact may not reuse a taken number.
//! - A bare git repository assembled by writes is moved out (quarantine).
//!
//! The tool card lines rp prints (`[read] ...`) are the UI's, from the tool
//! loop's events (WP 4.10); the warnings, questions and quarantine lines are
//! said here.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use sha1::Digest;
use sha1::Sha1;
use viy_protocol::Answer;
use viy_protocol::Asking;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::QuestionKind;
use viy_tools::guard;
use viy_tools::mounts;
use viy_tools::mounts::Mount;
use viy_tools::quarantine;
use viy_tools::resolve::resolve;
use viy_tools::resolve::resolve_checked;

use crate::engine::TurnContext;
use crate::filehistory::Planned;

/// Where the file tools say and ask things: the engine, whose chat mode may
/// answer a question, or a test.
pub trait Ui: Sync {
    fn notice(&self, notice: NoticeEvent);
    fn ask(&self, question: &Question) -> Asking;
}

impl Ui for TurnContext {
    fn notice(&self, notice: NoticeEvent) {
        self.emit(viy_protocol::EventMsg::Notice(notice));
    }

    fn ask(&self, question: &Question) -> Asking {
        TurnContext::ask(self, question)
    }
}

/// A tool's result for the model, whether the user refused it (the tool
/// loop counts refusals in a row), and what write_file overwrote.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub result: Value,
    pub refused: bool,
    /// For `/rewind`'s record ([`crate::filehistory`]): the file write_file
    /// wrote, unless what it held before could not be read.
    pub wrote: Option<Box<Wrote>>,
}

/// A file write_file wrote: what it held before (`None`: it did not
/// exist) and the SHA-1 of what it holds now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrote {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: String,
}

impl Outcome {
    fn ok(result: Value) -> Self {
        Self {
            result,
            refused: false,
            wrote: None,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self::ok(json!({"error": text.into()}))
    }
}

/// The file tools' state for one chat.
#[derive(Debug, Clone)]
pub struct FileTools {
    pub workspace: PathBuf,
    /// The project folder (`.viy`/`.rpctl` marker), for root-file rules.
    pub project_dir: PathBuf,
    pub mounts: Vec<Mount>,
    /// The clients' own code (`guard::client_paths`).
    pub clients: Vec<PathBuf>,
    /// WRITE_REQUIRES_READ.
    pub write_requires_read: bool,
    /// TOOLS_MAX_OUTPUT, in characters.
    pub max_output: usize,
    /// Where the workspace locks live: rpctl's state folder, so rp and viy
    /// lock each other out.
    pub locks_dir: PathBuf,
    /// Where quarantined paths go, under a timestamp.
    pub quarantine_dir: PathBuf,
    /// Path to the SHA-1 of what this chat last read or wrote there.
    seen: HashMap<PathBuf, String>,
}

pub(crate) fn sha1_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(40);
    for b in Sha1::digest(bytes) {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn line(level: NoticeLevel, text: String) -> NoticeEvent {
    NoticeEvent {
        level,
        code: "files".into(),
        message: text,
    }
}

/// `start_line`/`end_line`, or the `offset`/`limit` spelling models also
/// use: the first and last line to show (rpctl workflows.py `line_span`).
pub fn line_span(args: &Map<String, Value>, lines: usize) -> Option<(usize, usize)> {
    let num = |k: &str| -> Option<i64> {
        match args.get(k)? {
            // Python's int(): any size; here clamped, which picks the same
            // lines (a range is cut to the file).
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
            Value::String(s) => s.trim().parse().ok(),
            Value::Bool(b) => Some(i64::from(*b)),
            _ => None,
        }
    };
    let (mut a, mut b) = (num("start_line"), num("end_line"));
    let (off, lim) = (num("offset"), num("limit"));
    if a.is_none() && off.is_some() {
        a = off;
    }
    if b.is_none()
        && let Some(lim) = lim
    {
        b = Some(
            a.filter(|a| *a != 0)
                .unwrap_or(1)
                .saturating_add(lim)
                .saturating_sub(1),
        );
    }
    if a.is_none() && b.is_none() {
        return None;
    }
    let n = i64::try_from(lines).unwrap_or(i64::MAX);
    let mut a = a.filter(|a| *a != 0).unwrap_or(1).max(1);
    let mut b = b.unwrap_or(n).min(n);
    if a > b {
        a = a.min(n).max(1);
        b = a;
    }
    Some((
        usize::try_from(a).unwrap_or(1),
        usize::try_from(b).unwrap_or(1),
    ))
}

/// Long text cut to `limit` characters, the head and the tail kept (errors
/// usually surface at the end).
pub fn clip(text: &str, limit: usize) -> String {
    let n = text.chars().count();
    if n <= limit {
        return text.to_string();
    }
    let head = limit / 2;
    let tail = limit - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(n - tail).collect();
    format!("{start}\n...[{} chars omitted]...\n{end}", n - limit)
}

/// `^([fsd])(\d+)-.+\.md$`: a knowledge-base file's letter and number.
/// As rpctl's Python pattern: `\d` is any digit (every Unicode numeric
/// character here), and `$` also matches before a final newline.
fn kb_id(name: &str) -> Option<(char, &str)> {
    let name = name.strip_suffix('\n').unwrap_or(name);
    let letter = name
        .chars()
        .next()
        .filter(|c| matches!(c, 'f' | 's' | 'd'))?;
    let rest = &name[1..];
    let digits = rest.len() - rest.trim_start_matches(char::is_numeric).len();
    let after = &rest[digits..];
    (digits > 0
        && after.starts_with('-')
        && after.chars().count() > 4
        && after.ends_with(".md")
        && !after.contains('\n'))
    .then_some((letter, &rest[..digits]))
}

/// A run of digits as rpctl's `int()` compares them: leading zeros dropped,
/// any length. A non-ASCII digit keeps its character, so two spellings of
/// one number may not compare equal; the check then refuses no less than
/// it must only when the spellings agree (see [`FileTools::write_conflict`]).
fn digits_value(digits: &str) -> String {
    let v = digits.trim_start_matches('0');
    if v.is_empty() {
        "0".into()
    } else {
        v.to_string()
    }
}

const KB_DIRS: [&str; 3] = ["facts", "strategy", "decisions"];

/// A decimal number written in ASCII digits, plus one, of any length.
fn plus_one(n: &str) -> String {
    let mut digits: Vec<u8> = n.bytes().map(|b| b - b'0').collect();
    let mut i = digits.len();
    loop {
        if i == 0 {
            digits.insert(0, 1);
            break;
        }
        i -= 1;
        if digits[i] == 9 {
            digits[i] = 0;
        } else {
            digits[i] += 1;
            break;
        }
    }
    digits.iter().map(|d| char::from(b'0' + d)).collect()
}

/// The path the model gave. `Ok(None)` when it gave none or a false-like
/// one (rpctl's `if not rel`: null, false, 0, "", [], {}); an error for any
/// other value that is not a string, which rpctl's tool cannot take either.
fn path_arg(args: &Map<String, Value>) -> Result<Option<String>, Outcome> {
    let falsy = |v: &Value| match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    };
    match args.get("path") {
        None => Ok(None),
        Some(v) if falsy(v) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(Outcome::error("path must be a string")),
    }
}

impl FileTools {
    pub fn new(
        workspace: PathBuf,
        project_dir: PathBuf,
        locks_dir: PathBuf,
        quarantine_dir: PathBuf,
    ) -> Self {
        Self {
            workspace,
            project_dir,
            mounts: Vec::new(),
            clients: guard::client_paths(),
            write_requires_read: true,
            max_output: 20000,
            locks_dir,
            quarantine_dir,
            seen: HashMap::new(),
        }
    }

    /// Forgets every read (a new chat, a resumed one, a compaction).
    pub fn forget_reads(&mut self) {
        self.seen.clear();
    }

    /// Keeps the reads of `keep` only.
    pub fn keep_reads(&mut self, keep: &[PathBuf]) {
        self.seen.retain(|p, _| keep.contains(p));
    }

    /// The write guard's key for a read_file or write_file call; `None`
    /// for other tools and for paths that do not resolve (rp's
    /// `_call_path`).
    fn call_path(&self, call: &Value) -> Option<PathBuf> {
        let function = &call["function"];
        if !matches!(function["name"].as_str(), Some("read_file" | "write_file")) {
            return None;
        }
        let args: Value = match function["arguments"].as_str() {
            None | Some("") => return None,
            Some(text) => serde_json::from_str(text).ok()?,
        };
        let rel = args.get("path")?.as_str().filter(|r| !r.is_empty())?;
        self.resolve_in_workspace(rel).ok()
    }

    /// The write guard's keys of the files `messages` read or wrote (rp's
    /// `_paths_touched`).
    pub fn paths_touched(&self, messages: &[Value]) -> HashSet<PathBuf> {
        messages
            .iter()
            .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
            .filter_map(|call| self.call_path(call))
            .collect()
    }

    /// After `/rewind` left `kept` of the history: a file only the dropped
    /// turns touched counts as unread again, and one they wrote counts as
    /// read as it was before that write, so writing over a file left
    /// changed needs a read first.
    pub fn rewound(&mut self, kept: &[Value], files: &[Planned]) {
        let keep = self.paths_touched(kept);
        self.seen.retain(|p, _| keep.contains(p));
        for f in files {
            match &f.before {
                Some(before) if keep.contains(&f.path) => {
                    self.seen.insert(f.path.clone(), before.clone());
                }
                _ => {
                    self.seen.remove(&f.path);
                }
            }
        }
    }

    fn mounts_hint(&self) -> String {
        if self.mounts.is_empty() {
            return String::new();
        }
        let listed: Vec<String> = self
            .mounts
            .iter()
            .map(|m| m.path.display().to_string())
            .collect();
        format!(
            ", or absolute inside a mounted folder: {}",
            listed.join(", ")
        )
    }

    /// The path `rel` names, resolved and confined to the workspace or a
    /// mount.
    pub fn resolve_in_workspace(&self, rel: &str) -> Result<PathBuf, String> {
        let _ = fs::create_dir_all(&self.workspace);
        let ws = resolve(&self.workspace);
        let rel = rel.trim();
        let rel = if rel.is_empty() { "." } else { rel };
        let loops = || format!("too many levels of symbolic links: {rel}");
        if Path::new(rel).is_absolute() {
            let target = resolve_checked(Path::new(rel)).map_err(|_| loops())?;
            if mounts::containing(&self.mounts, &target).is_some() {
                return Ok(target);
            }
            return Err(format!(
                "path must be relative to the workspace{}",
                self.mounts_hint()
            ));
        }
        let target = resolve_checked(&ws.join(rel)).map_err(|_| loops())?;
        if !target.starts_with(&ws) {
            if mounts::containing(&self.mounts, &target).is_some() {
                return Ok(target);
            }
            return Err(format!("path escapes the workspace{}", self.mounts_hint()));
        }
        Ok(target)
    }

    /// A path the file tools resolved (a [`crate::filehistory`] record) as
    /// the workspace sees it, not resolved again: a link put there since
    /// shows as the path written, not where it leads.
    pub fn recorded_rel(&self, path: &Path) -> String {
        match path.strip_prefix(resolve(&self.workspace)) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
            _ => self.ws_rel(path),
        }
    }

    /// `path` as the workspace sees it.
    pub fn ws_rel(&self, path: &Path) -> String {
        let ws = resolve(&self.workspace);
        match resolve(path).strip_prefix(&ws) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }

    /// `None` when the tool may go on, else its refusal.
    async fn guard_path(&self, path: &Path, write: bool, ui: &dyn Ui) -> Option<Outcome> {
        let m = mounts::containing(&self.mounts, path);
        if let Some(m) = m.filter(|m| write && !m.rw) {
            ui.notice(line(
                NoticeLevel::Warn,
                format!(
                    "  [write refused] {}: {} is mounted read-only",
                    path.display(),
                    m.path.display()
                ),
            ));
            return Some(Outcome::error(format!(
                "{} is mounted read-only. Only the user can mount it read-write (/mount ... rw).",
                m.path.display()
            )));
        }
        let (root, proj) = match m {
            Some(m) => (m.path.clone(), m.path.clone()),
            None => (self.workspace.clone(), self.project_dir.clone()),
        };
        let reason = if write {
            guard::protected_reason(path, &root, &proj, &self.clients)
        } else {
            guard::secret_reason(path).map(str::to_string)
        }?;
        let verb = if write { "write" } else { "read" };
        let kind = if write {
            QuestionKind::ProtectedWrite
        } else {
            QuestionKind::SecretRead
        };
        let mut q = Question::yes_no(Some(kind), format!("  let the model {verb} it?"));
        q.reason.push(line(
            NoticeLevel::Warn,
            format!("  [{verb}] {} is protected: {reason}", self.ws_rel(path)),
        ));
        if ui.ask(&q).await == Answer::Yes {
            return None;
        }
        ui.notice(line(NoticeLevel::Info, "  [declined]".into()));
        Some(Outcome {
            result: json!({
                "declined": true,
                "note": format!("the user declined to let you {verb} this file: {reason}"),
            }),
            refused: true,
            wrote: None,
        })
    }

    /// `None` when `write_file` may write `path`, else `(reason, error)`.
    fn write_conflict(&self, path: &Path) -> Option<(String, String)> {
        if path.is_file() {
            let now = fs::read(path).ok().map(|b| sha1_hex(&b));
            let seen = self.seen.get(path);
            if seen.is_none() && self.write_requires_read {
                return Some((
                    "not read in this chat".into(),
                    format!(
                        "{} already exists and you have not read it in this chat. read_file it \
                         first, then write the full new content.",
                        self.ws_rel(path)
                    ),
                ));
            }
            if seen.is_some() && now.as_ref() != seen {
                return Some((
                    "changed since this chat last read it".into(),
                    format!(
                        "{} changed on disk since you last read it (another chat, a helper or a \
                         command wrote it). read_file it again and merge your change into the \
                         current content.",
                        self.ws_rel(path)
                    ),
                ));
            }
            return None;
        }
        let name = path.file_name()?.to_string_lossy().into_owned();
        let (letter, num) = kb_id(&name)?;
        let parent = path.parent()?;
        let dir = parent.file_name()?.to_string_lossy().into_owned();
        if !KB_DIRS.contains(&dir.as_str())
            || parent
                .parent()
                .and_then(Path::file_name)
                .is_none_or(|n| n != "kb")
            || !parent.is_dir()
        {
            return None;
        }
        // A number in digits other than ASCII: rpctl reads its value, which
        // viy cannot compare; refuse it rather than risk a taken id.
        if !num.chars().all(|c| c.is_ascii_digit()) {
            return Some((
                format!("id {letter}{num} is not in ASCII digits"),
                format!(
                    "write the knowledge-base id {letter}{num} with the digits 0-9 (and use \
                     that id in its frontmatter)."
                ),
            ));
        }
        let want = digits_value(num);
        let mut taken: Vec<String> = Vec::new();
        let mut nums: Vec<String> = Vec::new();
        for entry in fs::read_dir(parent).ok()?.flatten() {
            let other = entry.file_name().to_string_lossy().into_owned();
            if let Some((_, n)) = kb_id(&other).filter(|(l, _)| *l == letter) {
                let n = digits_value(n);
                if n == want {
                    taken.push(other.clone());
                }
                nums.push(n);
            }
        }
        let first = taken.first()?;
        // The largest by value: longer is larger once zeros are dropped.
        let max = nums
            .iter()
            .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
            .max_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
            .cloned()
            .unwrap_or_default();
        let next = format!("{letter}{:0>width$}", plus_one(&max), width = num.len());
        Some((
            format!("id {letter}{num} is taken by {first}"),
            format!(
                "the id {letter}{num} is already used by {dir}/{first}. The next free id is \
                 {next}: write the fact as {next}-<slug>.md (and use that id in its frontmatter)."
            ),
        ))
    }

    /// An exclusive lock for this workspace, shared by every rp and viy
    /// process (rpctl's `_workspace_lock`: the same file).
    pub(crate) fn lock(&self) -> std::io::Result<fs::File> {
        let ws = resolve(&self.workspace);
        let name = format!(
            "{}.lock",
            &sha1_hex(ws.as_os_str().as_encoded_bytes())[..16]
        );
        fs::create_dir_all(&self.locks_dir)?;
        let file = std::os::unix::fs::OpenOptionsExt::mode(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false),
            0o600,
        )
        .open(self.locks_dir.join(name))?;
        file.lock()?;
        Ok(file)
    }

    pub async fn write_file(
        &mut self,
        args: &Map<String, Value>,
        ui: &dyn Ui,
        stamp: &str,
    ) -> Outcome {
        let rel = match path_arg(args) {
            Ok(Some(rel)) => rel,
            Ok(None) => return Outcome::error("no path given"),
            Err(refused) => return refused,
        };
        let data = match args.get("content") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        };
        let path = match self.resolve_in_workspace(&rel) {
            Ok(p) => p,
            Err(e) => return Outcome::error(e),
        };
        if let Some(refused) = self.guard_path(&path, true, ui).await {
            return refused;
        }
        // Check and write under one lock, so two chats (or a chat and its
        // helpers) cannot both pass the check and overwrite each other.
        let _lock = match self.lock() {
            Ok(l) => l,
            Err(e) => return Outcome::error(format!("could not lock the workspace: {e}")),
        };
        if let Some((reason, error)) = self.write_conflict(&path) {
            ui.notice(line(
                NoticeLevel::Warn,
                format!("  [write refused] {}: {reason}", self.ws_rel(&path)),
            ));
            return Outcome::error(error);
        }
        // What /rewind puts back; unreadable, it leaves this file alone.
        let before = match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => fs::read(&path).ok().map(Some),
            _ => Some(None),
        };
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            return Outcome::error(format!("could not write {rel}: {e}"));
        }
        if let Err(e) = fs::write(&path, data.as_bytes()) {
            return Outcome::error(format!("could not write {rel}: {e}"));
        }
        let after = sha1_hex(data.as_bytes());
        self.seen.insert(path.clone(), after.clone());
        drop(_lock);
        let wrote = before.map(|before| {
            Box::new(Wrote {
                path: path.clone(),
                before,
                after,
            })
        });
        let mut result = json!({
            "ok": true,
            "path": self.ws_rel(&path),
            "bytes": data.chars().count(),
        });
        // A bare git repository can be assembled one file at a time.
        let root = mounts::containing(&self.mounts, &path)
            .map_or_else(|| self.workspace.clone(), |m| m.path.clone());
        let path_written = path.clone();
        let bare = quarantine::bare_repo_ancestors(&path, &root);
        if !bare.is_empty() {
            let dest = self.quarantine_dir.join(stamp);
            let (moved, failed) = quarantine::quarantine(&bare, &root, &dest);
            if !moved.is_empty() {
                for (rel, dest) in &moved {
                    ui.notice(line(
                        NoticeLevel::Error,
                        format!("  [quarantined] {rel} -> {}", dest.display()),
                    ));
                }
                ui.notice(line(
                    NoticeLevel::Info,
                    "  (git repos, editor configs and .rpctl/ run code outside the sandbox, \
                     so new ones are moved out)"
                        .into(),
                ));
                result["quarantined"] = json!(moved.iter().map(|(r, _)| r).collect::<Vec<_>>());
            }
            if let Some((path, e)) = failed {
                // The write happened; the repository may still be there.
                ui.notice(line(
                    NoticeLevel::Error,
                    format!("  [quarantine failed] {}: {e}", path.display()),
                ));
                let mut out = json!({
                    "error": format!(
                        "wrote {}, but could not move the bare repository {} out of the \
                         workspace: {e}",
                        self.ws_rel(&path_written),
                        self.ws_rel(&path)
                    ),
                });
                if !moved.is_empty() {
                    out["quarantined"] = json!(moved.iter().map(|(r, _)| r).collect::<Vec<_>>());
                }
                return Outcome {
                    wrote,
                    ..Outcome::ok(out)
                };
            }
        }
        Outcome {
            wrote,
            ..Outcome::ok(result)
        }
    }

    pub async fn read_file(&mut self, args: &Map<String, Value>, ui: &dyn Ui) -> Outcome {
        let (rel, path) = match self.read_target(args) {
            Ok(target) => target,
            Err(refused) => return refused,
        };
        if let Some(refused) = self.guard_path(&path, false, ui).await {
            return refused;
        }
        let (out, seen) = self.read_resolved(&rel, &path, args);
        if let Some(seen) = seen {
            self.seen.insert(path, seen);
        }
        out
    }

    /// read_file without its question, for a parallel batch: `None` when
    /// the file is secret, and the read must ask (it then runs on its own).
    /// What was read comes back for [`FileTools::remember_read`].
    pub fn read_quietly(
        &self,
        args: &Map<String, Value>,
    ) -> Option<(Outcome, Option<(PathBuf, String)>)> {
        let (rel, path) = match self.read_target(args) {
            Ok(target) => target,
            Err(refused) => return Some((refused, None)),
        };
        if guard::secret_reason(&path).is_some() {
            return None;
        }
        let (out, seen) = self.read_resolved(&rel, &path, args);
        Some((out, seen.map(|s| (path, s))))
    }

    /// Records a read [`FileTools::read_quietly`] made: the write guard's
    /// "read in this chat".
    pub fn remember_read(&mut self, path: PathBuf, sha1: String) {
        self.seen.insert(path, sha1);
    }

    /// The path read_file's arguments name, or its refusal.
    fn read_target(&self, args: &Map<String, Value>) -> Result<(String, PathBuf), Outcome> {
        let rel = match path_arg(args) {
            Ok(Some(rel)) => rel,
            Ok(None) => return Err(Outcome::error("no path given")),
            Err(refused) => return Err(refused),
        };
        let path = self.resolve_in_workspace(&rel).map_err(Outcome::error)?;
        if !path.exists() {
            return Err(Outcome::error(format!("no such file: {rel}")));
        }
        if path.is_dir() {
            return Err(Outcome::error(format!(
                "{rel} is a directory; use list_dir"
            )));
        }
        Ok((rel, path))
    }

    /// The file's text as read_file gives it, and its digest when it was
    /// read.
    fn read_resolved(
        &self,
        rel: &str,
        path: &Path,
        args: &Map<String, Value>,
    ) -> (Outcome, Option<String>) {
        let raw = match fs::read(path) {
            Ok(r) => r,
            Err(e) => return (Outcome::error(format!("could not read {rel}: {e}")), None),
        };
        let Ok(text) = std::str::from_utf8(&raw) else {
            return (Outcome::error(format!("{rel} is not a text file")), None);
        };
        let data = text.replace("\r\n", "\n").replace('\r', "\n");
        let seen = sha1_hex(&raw);
        let shown = self.ws_rel(path);
        let lines: Vec<&str> = data.split('\n').collect();
        if let Some((a, b)) = line_span(args, lines.len()) {
            let part: Vec<String> = (a..=b).map(|n| format!("{n}: {}", lines[n - 1])).collect();
            let part = part.join("\n");
            let clipped = clip(&part, self.max_output);
            let truncated = clipped != part;
            return (
                Outcome::ok(json!({
                    "path": shown,
                    "lines": format!("{a}-{b} of {}", lines.len()),
                    "content": clipped,
                    "truncated": truncated,
                })),
                Some(seen),
            );
        }
        let clipped = clip(&data, self.max_output);
        let mut out = json!({"path": shown, "content": clipped, "truncated": clipped != data});
        if clipped != data {
            out["note"] = json!(format!(
                "cut in the middle: {} lines. Read a part with start_line/end_line, or grep -n \
                 with run_shell.",
                data.matches('\n').count() + 1
            ));
        }
        (Outcome::ok(out), Some(seen))
    }

    pub fn list_dir(&self, args: &Map<String, Value>) -> Outcome {
        let rel = match path_arg(args) {
            Ok(rel) => rel.unwrap_or_else(|| ".".into()),
            Err(refused) => return refused,
        };
        let path = match self.resolve_in_workspace(&rel) {
            Ok(p) => p,
            Err(e) => return Outcome::error(e),
        };
        if !path.exists() {
            return Outcome::error(format!("no such directory: {rel}"));
        }
        if !path.is_dir() {
            return Outcome::error(format!("{rel} is not a directory"));
        }
        let mut children: Vec<PathBuf> = match fs::read_dir(&path) {
            Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
            Err(e) => return Outcome::error(format!("could not list {rel}: {e}")),
        };
        children.sort();
        let entries: Vec<Value> = children
            .iter()
            .map(|child| {
                let is_dir = child.is_dir();
                let name = child
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                json!({
                    "name": if is_dir { format!("{name}/") } else { name },
                    "type": if is_dir { "dir" } else { "file" },
                    "bytes": if is_dir { Value::Null } else {
                        fs::metadata(child).map_or(Value::Null, |m| json!(m.len()))
                    },
                })
            })
            .collect();
        Outcome::ok(json!({"path": self.ws_rel(&path), "entries": entries}))
    }
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;
