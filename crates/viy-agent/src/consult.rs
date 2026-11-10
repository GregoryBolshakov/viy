//! ask_claude: the chat hands a task to Claude Code (rpctl consult.py).
//!
//! The model calls ask_claude when a task is beyond it. viy starts
//! `claude -p` on this machine as the full agent (the user's settings,
//! CLAUDE.md files, memory, skills, MCP servers, web tools and unsandboxed
//! Bash all stay on, as answer quality is the only reason for the tool), in
//! a fresh folder of its own with the live workspace attached by
//! `--add-dir`. In act mode (the default) it does the work in the workspace
//! and reports; in advise mode it changes nothing and returns the solution
//! and a patch. The user approves each consult unless CONSULT_APPROVAL is
//! `auto`, which a project's config may not set (`CONSULT_GUARDED`), and
//! the model may ask for advise but never widen advise to act.
//!
//! Each consult gets a private folder:
//!
//! ```text
//! <CONSULT_ROOT>/<chat session>/<YYYYmmddTHHMMSS>-<hex4>/
//!   task.md        what the model asked (task.2.md ... for follow-ups)
//!   request.md     the user's latest message, added here, not by the model
//!   scratch/       Claude's working area
//!   out/           report.md, and patch.diff in advise mode (.2 ...)
//!   claude.jsonl   the stream-json transcript of every run
//!   claude.stderr  the CLI's stderr
//!   meta.json      status, seconds, cost, model, session id, files changed
//! ```
//!
//! What reaches the model is decided here, not by Claude's wording: the
//! workspace is compared before and after, so `files_changed` is what
//! really changed, and a decline comes back as a status and a few words
//! only ([`shape_result`]).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use regex::Regex;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use viy_protocol::Answer;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::QuestionKind;
use viy_tools::untrusted;

use super::Chat;
use super::commands::expand_home;
use super::notice;
use crate::engine::TurnContext;
use crate::trace::Tracer;

/// The tool the model calls.
pub fn consult_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "ask_claude",
            "description": "Hand a task to Claude, a much stronger coding agent, when it is \
                beyond you: hard reasoning, a design decision, a bug you tried and failed to \
                fix. Claude works in the workspace itself: it reads the code, runs commands, \
                changes files and tests them, then reports what it did. It takes minutes. Use \
                it after your own attempt, not for what reading files or explore answers. \
                Afterwards, read a file again before you edit it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "What Claude must achieve and what done looks like. \
                            Self-contained: Claude does not see this chat.",
                    },
                    "context": {
                        "type": "string",
                        "description": "What you tried and how it failed: errors, test \
                            output, constraints the user gave.",
                    },
                    "paths": {
                        "type": "array", "items": {"type": "string"},
                        "description": "Optional workspace files or directories to start from.",
                    },
                    "mode": {
                        "type": "string", "enum": ["act", "advise"],
                        "description": "act (default): Claude makes the changes. advise: \
                            Claude changes nothing and returns the answer and a patch.",
                    },
                    "follow_up_of": {
                        "type": "string",
                        "description": "consult_id of an earlier ask_claude in this chat, to \
                            continue that conversation with Claude.",
                    },
                },
                "required": ["task"],
            },
        },
    })
}

/// The structured report Claude ends with.
pub fn report_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": ["done", "partial", "declined", "failed"]},
            "summary": {"type": "string"},
            "changes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "what": {"type": "string"}},
                    "required": ["path", "what"],
                },
            },
            "verify": {"type": "string"},
            "open": {"type": "string"},
            "patch": {"type": "string"},
            "decline_reason": {"type": "string"},
        },
        "required": ["status", "summary"],
    })
}

/// Claude's system prompt, appended to its own.
pub fn system_prompt(workspace: &Path, folder: &Path) -> String {
    format!(
        "You were started by rpctl's ask_claude tool. A local model (Qwen) working for the \
user in the workspace {ws} handed you a task it could not do well. The user's own latest \
message to it is included. If Qwen's description of the task disagrees with the user's \
message, follow the user and say so in your report.

The workspace was added with --add-dir; its CLAUDE.md files are loaded. Your working \
directory, {folder}, is this consult's own folder: use its scratch/ for experiments and \
throwaway code. Each message states its mode, act or advise; a follow-up can change it.

Nobody can answer questions during this run. Make sensible choices and state them in the \
report.

Finish with the structured output. Qwen reads it and continues the work, so be concrete: \
paths, functions, commands. summary: what you did and why. changes: every workspace file you \
changed, with what changed, paths relative to the workspace. verify: how you tested it, or \
what to run to check it. open: what is left, if anything. patch (advise mode): the proposed \
changes as a unified diff against the workspace root.

If you will not do the task, or part of it: status declined (or partial), and \
decline_reason in a few words. Nothing else about it: no advice to the local model, no \
alternatives, no comments on the rest of its work.",
        ws = workspace.display(),
        folder = folder.display()
    )
}

/// The mode goes in each message, not the system prompt: a resumed session
/// keeps the system prompt of its first run.
pub const ACT_RULES: &str = "Mode: act. Do the task in the workspace: read the code, make \
the changes, run the tests. Then report.";
pub const ADVISE_RULES: &str = "Mode: advise. Do not change anything in the workspace. Work \
out the solution, test ideas in scratch/, and report the answer, with the proposed changes as \
a unified diff in the report's patch field.";

fn note(kind: &str) -> &'static str {
    match kind {
        "act" => {
            "Claude made these changes in the workspace. Read a file again before you edit it. \
             Continue from this report; do not redo Claude's work."
        }
        "advise" => {
            "Claude changed nothing in the workspace. Apply the patch yourself with write_file, \
             reading each file first."
        }
        "declined" => {
            "Claude did not take this task. Continue with the rest of the user's request."
        }
        _ => {
            "Claude did not finish (see error). Files it changed before it stopped are in \
             files_changed."
        }
    }
}

pub const DECLINE_REASON_CHARS: usize = 80;
const WALK_SKIP: [&str; 9] = [
    "node_modules",
    ".git",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "target",
    "dist",
];
/// Byproducts of running the code, not changes worth telling the model about.
const JUNK: [&str; 4] = ["__pycache__", ".mypy_cache", ".pytest_cache", ".ruff_cache"];
const SNAPSHOT_MAX_FILES: usize = 50000;
static SLUG: LazyLock<Regex> = LazyLock::new(|| re(r"[^A-Za-z0-9]"));
static CONSULT_ID: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{8}T\d{6}-[0-9a-f]{4}$"));

fn re(p: &str) -> Regex {
    match Regex::new(p) {
        Ok(r) => r,
        Err(e) => panic!("consult.rs pattern {p:?}: {e}"),
    }
}

/// `text` with every character that is not a letter or digit as `-`.
fn slug(text: &str) -> String {
    SLUG.replace_all(text, "-").into_owned()
}

/// Where consults go without CONSULT_ROOT: viy's own folder under /tmp.
pub fn default_root() -> PathBuf {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/viy-consult-{uid}"))
}

/// Where Claude Code keeps its per-folder sessions and memory.
pub fn claude_projects_dir() -> PathBuf {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".claude")
        });
    base.join("projects")
}

/// `path` as a private folder: made 0700 when missing; refused when it is a
/// link, not a folder, or someone else's (/tmp is shared); opened to
/// nobody else.
fn private_dir(path: &Path) -> Result<PathBuf, String> {
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
    {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let st = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if st.file_type().is_symlink() || !st.is_dir() {
        return Err(format!("{} is not a plain directory", path.display()));
    }
    // SAFETY: getuid cannot fail.
    if st.uid() != unsafe { libc::getuid() } {
        return Err(format!("{} belongs to another user", path.display()));
    }
    if st.mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(path.to_path_buf())
}

// ---- workspace changes -------------------------------------------------------

/// The workspace state compared after the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    git: bool,
    top: PathBuf,
    head: String,
    /// Path (relative to `top`) to its size and mtime, `None` when gone.
    files: BTreeMap<String, Option<(u64, i64, i64)>>,
}

fn stat_key(p: &Path) -> Option<(u64, i64, i64)> {
    let st = std::fs::symlink_metadata(p).ok()?;
    Some((st.len(), st.mtime(), st.mtime_nsec()))
}

/// `git -C dir args` within `secs` (rpctl's timeouts): its stdout and
/// whether it succeeded; `None` when it could not run or ran over.
fn git(dir: &Path, args: &[&str], secs: u64) -> Option<(bool, String)> {
    let child = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let out = viy_config::vault::wait_with_timeout(child, Duration::from_secs(secs)).ok()?;
    Some((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

/// The workspace's state (rp's `snapshot`): in a git repository HEAD and
/// the stat of every dirty or untracked file, else the stat of every file
/// (capped). A repository with no commits yet stays in git mode, its
/// HEAD git's unresolved `HEAD`, as rpctl's; git failing to run or
/// running over falls back to the walk. Blocking: run it off the async
/// runtime.
pub fn snapshot(ws: &Path) -> Snapshot {
    let ws = std::fs::canonicalize(ws).unwrap_or_else(|_| ws.to_path_buf());
    let top = git(&ws, &["rev-parse", "--show-toplevel"], 10)
        .filter(|(ok, _)| *ok)
        .map(|(_, t)| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .map(PathBuf::from);
    if let Some(top) = top {
        let head = git(&top, &["rev-parse", "HEAD"], 10);
        let status = git(
            &top,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            60,
        );
        if let (Some((_, head)), Some((_, status))) = (head, status) {
            let mut files = BTreeMap::new();
            let entries: Vec<&str> = status.split('\0').collect();
            let mut i = 0;
            while i < entries.len() {
                let e = entries[i];
                i += 1;
                if e.chars().count() < 4 {
                    continue;
                }
                if e.starts_with(['R', 'C']) {
                    i += 1; // the rename's source path follows
                }
                let rel = &e[3..];
                files.insert(rel.to_string(), stat_key(&top.join(rel)));
            }
            return Snapshot {
                git: true,
                top,
                head: head.trim().to_string(),
                files,
            };
        }
    }
    let mut files = BTreeMap::new();
    let mut stack = vec![ws.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                if !WALK_SKIP.contains(&name.as_str()) {
                    stack.push(p);
                }
                continue;
            }
            let rel = p.strip_prefix(&ws).unwrap_or(&p).display().to_string();
            files.insert(rel, stat_key(&p));
            if files.len() >= SNAPSHOT_MAX_FILES {
                return Snapshot {
                    git: false,
                    top: ws,
                    head: String::new(),
                    files,
                };
            }
        }
    }
    Snapshot {
        git: false,
        top: ws,
        head: String::new(),
        files,
    }
}

/// The paths, relative to the workspace, that differ between two
/// snapshots (rp's `changed_files`). Blocking: run it off the async
/// runtime.
pub fn changed_files(before: &Snapshot, after: &Snapshot, ws: &Path) -> Vec<String> {
    let keys: BTreeSet<&String> = before.files.keys().chain(after.files.keys()).collect();
    let mut changed: BTreeSet<String> = keys
        .into_iter()
        .filter(|p| before.files.get(*p) != after.files.get(*p))
        .cloned()
        .collect();
    if before.git
        && before.head != after.head
        && let Some((_, out)) = git(
            &before.top,
            &["diff", "--name-only", &before.head, &after.head],
            30,
        )
    {
        changed.extend(out.lines().filter(|l| !l.is_empty()).map(str::to_string));
    }
    let ws = std::fs::canonicalize(ws).unwrap_or_else(|_| ws.to_path_buf());
    let mut out: Vec<String> = changed
        .into_iter()
        .filter(|p| {
            !Path::new(p)
                .components()
                .any(|c| JUNK.contains(&c.as_os_str().to_string_lossy().as_ref()))
        })
        .map(|p| relpath(&before.top.join(&p), &ws))
        .collect();
    out.sort();
    out
}

/// `path` relative to `base`, with `..` where it lies outside (Python's
/// `os.path.relpath` on absolute paths).
fn relpath(path: &Path, base: &Path) -> String {
    let p: Vec<_> = path.components().collect();
    let b: Vec<_> = base.components().collect();
    let common = p.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), b.len() - common).collect();
    parts.extend(
        p[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

// ---- the result ----------------------------------------------------------------

fn clip(text: &str, limit: usize) -> String {
    let text = text.trim();
    let n = text.chars().count();
    if n <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}\n...[{} chars omitted]", n - limit)
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The decline reason in at most [`DECLINE_REASON_CHARS`] characters.
fn reason(report: &Map<String, Value>) -> String {
    let r = report
        .get("decline_reason")
        .map(text_of)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let r = if r.chars().count() > DECLINE_REASON_CHARS {
        let mut cut: String = r.chars().take(DECLINE_REASON_CHARS - 1).collect();
        cut.push('…');
        cut
    } else {
        r
    };
    if r.is_empty() {
        "no reason given".into()
    } else {
        r
    }
}

/// `path` relative to the workspace when it lies inside it.
fn ws_path(path: &str, ws: Option<&Path>) -> String {
    if let Some(ws) = ws
        && Path::new(path).is_absolute()
    {
        let rel = relpath(Path::new(path), ws);
        if !rel.starts_with("..") {
            return rel;
        }
    }
    path.to_string()
}

/// One run's facts for the result.
#[derive(Debug, Clone, Default)]
pub struct RunFacts {
    pub error: Option<String>,
    pub model: Option<String>,
    pub secs: f64,
    pub cost_usd: Option<Value>,
}

/// The tool result the model sees (rp's `shape_result`). A decline is a
/// status and a few words: everything else Claude wrote with it is dropped,
/// so a decline costs one task and never reaches the model as advice that
/// pulls it off other work.
#[allow(clippy::too_many_arguments)]
pub fn shape_result(
    cid: &str,
    mode: &str,
    report: Option<&Map<String, Value>>,
    files: &[String],
    run: &RunFacts,
    report_chars: usize,
    patch: Option<&str>,
    ws: Option<&Path>,
) -> Value {
    let status = report
        .and_then(|r| r.get("status"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status == "declined" {
        let mut out = json!({"consult_id": cid, "status": "declined",
                             "reason": reason(report.unwrap_or(&Map::new())),
                             "note": note("declined")});
        if !files.is_empty() {
            out["files_changed"] = json!(files);
        }
        return out;
    }
    let Some(report) = report else {
        return json!({"consult_id": cid, "status": "failed",
                      "error": run.error.clone().unwrap_or_else(|| "no report".into()),
                      "files_changed": files, "note": note("failed")});
    };
    let shown_status = if matches!(status, "done" | "partial" | "failed") {
        status
    } else {
        "done"
    };
    let mut out = json!({"consult_id": cid, "status": shown_status, "mode": mode,
                         "model": run.model});
    let summary = clip(
        &report.get("summary").map(text_of).unwrap_or_default(),
        report_chars,
    );
    let budget = report_chars.saturating_sub(summary.chars().count());
    out["summary"] = json!(summary);
    let changes: Vec<Value> = report
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let c = c.as_object()?;
            let path = c.get("path").map(text_of).filter(|p| !p.is_empty())?;
            Some(json!({"path": ws_path(&path, ws),
                        "what": clip(&c.get("what").map(text_of).unwrap_or_default(), 500)}))
        })
        .take(100)
        .collect();
    if !changes.is_empty() {
        out["changes"] = json!(changes);
    }
    out["files_changed"] = json!(files);
    for k in ["verify", "open"] {
        if let Some(v) = report.get(k).map(text_of).filter(|v| !v.is_empty()) {
            out[k] = json!(clip(&v, 2000));
        }
    }
    if status == "partial"
        && report
            .get("decline_reason")
            .is_some_and(|r| !text_of(r).is_empty())
    {
        out["declined_part"] = json!(reason(report));
    }
    if let Some(patch) = patch.filter(|_| mode == "advise") {
        out["patch"] = json!(clip(patch, budget.max(2000)));
    }
    if status == "failed"
        && let Some(e) = &run.error
    {
        out["error"] = json!(e);
    }
    out["secs"] = json!(run.secs);
    if let Some(cost) = run.cost_usd.as_ref().filter(|c| !c.is_null()) {
        out["cost_usd"] = cost.clone();
    }
    out["note"] = json!(if status == "failed" {
        note("failed")
    } else {
        note(mode)
    });
    out
}

/// One progress line for a tool Claude used.
fn brief(name: &str, input: &Value) -> String {
    let v = [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ]
    .iter()
    .find_map(|k| {
        input
            .get(*k)
            .filter(|v| !matches!(v, Value::Null | Value::Bool(false)) && !text_of(v).is_empty())
    })
    .map(text_of)
    .unwrap_or_default();
    untrusted::terminal_safe(format!("{name} {v}").trim(), Some(110))
}

// ---- the run ---------------------------------------------------------------------

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum How {
    Exited,
    Timeout,
}

impl How {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Timeout => "timeout",
        }
    }
}

/// What one `claude -p` process said.
#[derive(Debug, Default)]
struct Streamed {
    result: Option<Map<String, Value>>,
    session_id: Option<String>,
    model: Option<String>,
    tools_used: u64,
}

/// A run the turn dropped, recorded as rpctl records a cancelled one: the
/// run's row in meta.json (`how` "cancelled", "cancelled by the user",
/// the files changed) and the trace's consult_end. Written on a thread of
/// its own, as a drop cannot wait; Claude's group is killed by
/// [`Group`] already.
struct Cancelled {
    armed: Option<CancelRecord>,
    st: Streamed,
}

struct CancelRecord {
    folder: PathBuf,
    meta: Value,
    cid: String,
    n: usize,
    task: String,
    mode: String,
    ws: PathBuf,
    before: Option<Snapshot>,
    t0: Instant,
    tracer: Tracer,
    turn: Value,
}

impl Drop for Cancelled {
    fn drop(&mut self) {
        let Some(mut r) = self.armed.take() else {
            return;
        };
        let st = std::mem::take(&mut self.st);
        let secs = (r.t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        std::thread::spawn(move || {
            let files = r
                .before
                .as_ref()
                .map(|b| changed_files(b, &snapshot(&r.ws), &r.ws))
                .unwrap_or_default();
            let res = st.result.unwrap_or_default();
            let cost = res.get("total_cost_usd").cloned().filter(|c| !c.is_null());
            let row = json!({
                "run": r.n, "how": "cancelled", "rc": null, "secs": secs,
                "error": "cancelled by the user", "model": st.model,
                "session_id": st.session_id, "cost_usd": cost,
                "turns": res.get("num_turns"), "tools_used": st.tools_used,
                "files_changed": files, "status": "failed", "task": r.task, "mode": r.mode,
            });
            if let Some(sid) = &st.session_id {
                r.meta["session_id"] = json!(sid);
            }
            if let Some(runs) = r.meta["runs"].as_array_mut() {
                runs.push(row);
            }
            let _ = std::fs::write(
                r.folder.join("meta.json"),
                serde_json::to_string_pretty(&r.meta).unwrap_or_default(),
            );
            let files_head: Vec<&String> = files.iter().take(50).collect();
            r.tracer.event(
                "consult_end",
                json!({"turn": r.turn, "consult": r.cid, "run": r.n, "how": "cancelled",
                       "status": "failed", "secs": secs, "cost_usd": cost,
                       "files_changed": files_head, "tools_used": st.tools_used}),
            );
        });
    }
}

/// Kills the process group when dropped: a turn interrupted drops the
/// consult, and Claude must not run on after it.
struct Group(Option<i32>);

impl Group {
    fn kill(&mut self, sig: i32) {
        if let Some(pid) = self.0 {
            // SAFETY: a negative pid names the process group made at spawn.
            unsafe {
                libc::kill(-pid, sig);
            }
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill(libc::SIGKILL);
    }
}

/// Runs `cmd` in `folder` with `prompt` on stdin: the stream appended to
/// claude.jsonl, progress said, what it said kept in `st` (so a dropped
/// turn's [`Cancelled`] still has it).
async fn run_claude(
    ctx: &TurnContext,
    cmd: &[String],
    folder: &Path,
    prompt: &str,
    timeout: Duration,
    st: &mut Streamed,
) -> Result<(How, Option<i32>), String> {
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join("claude.stderr"))
        .map_err(|e| e.to_string())?;
    let mut child = tokio::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(folder)
        .env("CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut group = Group(child.id().and_then(|p| i32::try_from(p).ok()));
    // Fed alongside the reading, as rpctl starts its reader first: a claude
    // that writes before it has read the whole prompt never blocks on us.
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = prompt.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
        });
    }
    let stdout = child.stdout.take();
    let log_path = folder.join("claude.jsonl");
    // A signal's exit is -N, as Python's returncode.
    let code = |s: std::io::Result<std::process::ExitStatus>| {
        s.ok()
            .and_then(|s| s.code().or_else(|| s.signal().map(|n| -n)))
    };
    let (how, rc) = {
        let mut reading = std::pin::pin!(read_stream(ctx, stdout, &log_path, st));
        let mut done = false;
        let waited = tokio::time::timeout(timeout, async {
            loop {
                tokio::select! {
                    () = &mut reading, if !done => done = true,
                    status = child.wait() => break status,
                }
            }
        })
        .await;
        match waited {
            Ok(status) => {
                group.0 = None; // exited: nothing left to kill
                // rpctl gives its reader 10 s more: a process Claude left
                // behind may hold the stream open.
                if !done {
                    let _ = tokio::time::timeout(Duration::from_secs(10), &mut reading).await;
                }
                (How::Exited, code(status))
            }
            Err(_) => {
                group.kill(libc::SIGTERM);
                let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await
                {
                    Ok(status) => status,
                    Err(_) => {
                        group.kill(libc::SIGKILL);
                        child.wait().await
                    }
                };
                group.0 = None;
                if !done {
                    let _ = tokio::time::timeout(Duration::from_secs(5), &mut reading).await;
                }
                (How::Timeout, code(status))
            }
        }
    };
    // What the stream said is kept on a timeout too (the session id a
    // follow-up resumes), as rpctl's reader keeps it.
    Ok((how, rc))
}

/// Reads Claude's stream into `st`: each line appended to claude.jsonl
/// (undecodable bytes replaced, so one bad line never stops the reading),
/// progress said, the result event kept.
async fn read_stream(
    ctx: &TurnContext,
    stdout: Option<tokio::process::ChildStdout>,
    log_path: &Path,
    st: &mut Streamed,
) {
    let Some(stdout) = stdout else {
        return;
    };
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .ok();
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf);
        let line = text.strip_suffix('\n').unwrap_or(&text);
        if let Some(f) = log.as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
        let Ok(Value::Object(ev)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match ev.get("type").and_then(Value::as_str) {
            Some("system") if ev.get("subtype").and_then(Value::as_str) == Some("init") => {
                if let Some(s) = ev.get("session_id").and_then(Value::as_str) {
                    st.session_id = Some(s.to_string());
                }
                if let Some(m) = ev.get("model").and_then(Value::as_str) {
                    st.model = Some(m.to_string());
                }
            }
            Some("assistant") => {
                for c in ev["message"]["content"].as_array().into_iter().flatten() {
                    if c["type"] == "tool_use" && c["name"] != "StructuredOutput" {
                        st.tools_used += 1;
                        let name = c["name"].as_str().unwrap_or_default();
                        notice(
                            ctx,
                            NoticeLevel::Info,
                            "ask_claude",
                            format!("  [claude] {}", brief(name, &c["input"])),
                        );
                    }
                }
            }
            Some("result") => {
                if let Some(s) = ev.get("session_id").and_then(Value::as_str) {
                    st.session_id = Some(s.to_string());
                }
                st.result = Some(ev);
            }
            _ => {}
        }
    }
}

fn stderr_tail(folder: &Path) -> String {
    let Ok(bytes) = std::fs::read(folder.join("claude.stderr")) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let n = text.chars().count();
    let tail: String = text.chars().skip(n.saturating_sub(300)).collect();
    untrusted::terminal_safe(&tail, None)
}

// ---- the chat's side --------------------------------------------------------------

/// The chat's ask_claude settings (rp's `consult_*`), which `/consult`
/// changes per chat.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsultSettings {
    pub on: bool,
    /// `act` or `advise`.
    pub mode: String,
    pub model: String,
    pub effort: String,
    /// `ask` or `auto`.
    pub approval: String,
    /// The claude command (CONSULT_CLAUDE_BIN), as the shell finds it.
    bin: Option<PathBuf>,
    pub timeout: Duration,
    pub budget: String,
    pub permission_mode: String,
    pub report_chars: usize,
    pub keep_days: f64,
    pub root: PathBuf,
    /// Where Claude Code keeps per-folder sessions ([`claude_projects_dir`]),
    /// read when the chat starts.
    pub projects_dir: PathBuf,
}

impl ConsultSettings {
    /// `home` expands `~` in CONSULT_ROOT and CONSULT_CLAUDE_BIN.
    pub fn from_config(cfg: &viy_config::config::Config, home: &Path) -> Self {
        let get = |k: &str| cfg.get(k).unwrap_or_default().trim().to_string();
        let or = |k: &str, d: &str| Some(get(k)).filter(|v| !v.is_empty()).unwrap_or(d.into());
        let root = get("CONSULT_ROOT");
        Self {
            on: cfg.get("CONSULT_ENABLED").unwrap_or("1") == "1",
            mode: if cfg.get("CONSULT_MODE") == Some("advise") {
                "advise".into()
            } else {
                "act".into()
            },
            model: get("CONSULT_MODEL"),
            effort: get("CONSULT_EFFORT"),
            approval: if cfg.get("CONSULT_APPROVAL") == Some("auto") {
                "auto".into()
            } else {
                "ask".into()
            },
            bin: which(&or("CONSULT_CLAUDE_BIN", "claude"), home),
            timeout: or("CONSULT_TIMEOUT", "1800")
                .parse::<f64>()
                .ok()
                .and_then(|s| Duration::try_from_secs_f64(s).ok())
                .unwrap_or(Duration::from_secs(1800)),
            budget: get("CONSULT_MAX_BUDGET_USD"),
            permission_mode: or("CONSULT_PERMISSION_MODE", "bypassPermissions"),
            report_chars: or("CONSULT_REPORT_CHARS", "20000").parse().unwrap_or(20000),
            keep_days: or("CONSULT_KEEP_DAYS", "7").parse().unwrap_or(7.0),
            root: if root.is_empty() {
                default_root()
            } else {
                expand_home(&root, home)
            },
            projects_dir: claude_projects_dir(),
        }
    }

    /// The claude command (rp's `consult_bin`).
    pub fn bin(&self) -> Option<PathBuf> {
        self.bin.clone()
    }
}

/// `name` as an executable: a path when it has a `/`, else the first in
/// PATH's absolute folders.
fn which(name: &str, home: &Path) -> Option<PathBuf> {
    if name.contains('/') {
        let p = expand_home(name, home);
        let ok =
            std::fs::metadata(&p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
        return ok.then_some(p);
    }
    viy_config::vault::find_executable(std::env::var_os("PATH").as_deref(), name)
}

/// Rows for `/consult`: this chat's consults, newest first.
fn recent(root: &Path, sid: &str, limit: usize) -> Vec<Value> {
    let d = root.join(slug(sid));
    let Ok(rd) = std::fs::read_dir(&d) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    dirs.sort();
    dirs.reverse();
    let mut rows = Vec::new();
    for f in dirs.into_iter().take(limit) {
        let Some(meta) = std::fs::read_to_string(f.join("meta.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        let runs = meta["runs"].as_array().cloned().unwrap_or_default();
        let last = runs.last().cloned().unwrap_or(json!({}));
        rows.push(json!({
            "id": f.file_name().map(|n| n.to_string_lossy().into_owned()),
            "folder": f.display().to_string(), "status": last["status"], "secs": last["secs"],
            "cost_usd": last["cost_usd"], "runs": runs.len(), "task": last["task"],
            "files": last["files_changed"].as_array().map_or(0, Vec::len),
        }));
    }
    rows
}

/// The text of a user message whose content is a list of parts.
fn message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .map(|p| p["text"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn prompt_for(
    n: usize,
    mode: &str,
    task: &str,
    context: &str,
    paths: &[String],
    request: &str,
) -> String {
    let mut parts = vec![
        if mode == "act" {
            ACT_RULES
        } else {
            ADVISE_RULES
        }
        .to_string(),
        String::new(),
        format!(
            "# Task from Qwen{}",
            if n > 1 { " (follow-up)" } else { "" }
        ),
        String::new(),
        task.to_string(),
    ];
    if !context.is_empty() {
        parts.extend([
            "".into(),
            "## Context from Qwen".into(),
            "".into(),
            context.to_string(),
        ]);
    }
    if !paths.is_empty() {
        parts.extend(["".into(), "## Paths Qwen pointed at".into(), "".into()]);
        parts.extend(paths.iter().map(|p| format!("- {p}")));
    }
    if !request.is_empty() {
        parts.extend([
            "".into(),
            "## The user's latest message to Qwen".into(),
            "".into(),
            request.to_string(),
        ]);
    }
    parts.join("\n") + "\n"
}

fn mtime_secs(p: &Path) -> Option<f64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(m.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs_f64())
}

impl Chat {
    /// Whether ask_claude is offered: build tools on, consults on, and the
    /// claude command found.
    pub(crate) fn consult_available(&self) -> bool {
        self.tools_on && self.consult.on && self.consult.bin().is_some()
    }

    fn session_dir(&self) -> Result<PathBuf, String> {
        private_dir(&self.consult.root)?;
        private_dir(&self.consult.root.join(slug(&self.tracer.sid())))
    }

    /// The folder of consult `cid` in this chat, if there is one.
    fn find_consult(&self, cid: &str) -> Option<PathBuf> {
        if !CONSULT_ID.is_match(cid) {
            return None;
        }
        let d = self.consult.root.join(slug(&self.tracer.sid())).join(cid);
        d.join("meta.json").is_file().then_some(d)
    }

    /// Drops consult folders older than CONSULT_KEEP_DAYS, with the session
    /// entry Claude Code keeps for each (rp's `prune`).
    pub(crate) fn prune_consults(&self) -> usize {
        let keep = self.consult.keep_days;
        let root = &self.consult.root;
        if keep <= 0.0 || !root.is_dir() {
            return 0;
        }
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        let cutoff = now - keep * 86400.0;
        let projects = &self.consult.projects_dir;
        let mut n = 0;
        let Ok(sessions) = std::fs::read_dir(root) else {
            return 0;
        };
        for sdir in sessions.filter_map(Result::ok).map(|e| e.path()) {
            let plain_dir = |p: &Path| {
                std::fs::symlink_metadata(p)
                    .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
            };
            if !plain_dir(&sdir) {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&sdir) else {
                continue;
            };
            for d in rd.filter_map(Result::ok).map(|e| e.path()) {
                if !plain_dir(&d) || mtime_secs(&d).is_none_or(|m| m > cutoff) {
                    continue;
                }
                let _ = std::fs::remove_dir_all(&d);
                let entry = projects.join(slug(&d.display().to_string()));
                if plain_dir(&entry) {
                    let _ = std::fs::remove_dir_all(&entry);
                }
                n += 1;
            }
            let _ = std::fs::remove_dir(&sdir); // only when empty
        }
        n
    }

    /// The user's yes to a consult, unless approval is `auto` (rp's
    /// `_approve`); "always" makes it `auto` for the rest of the chat.
    async fn approve_consult(
        &mut self,
        ctx: &TurnContext,
        mode: &str,
        task: &str,
        paths: &[String],
        follow: Option<&str>,
    ) -> bool {
        if self.consult.approval == "auto" {
            return true;
        }
        let what = follow.map_or("new consult".to_string(), |f| format!("follow-up to {f}"));
        let line = |text: String| NoticeEvent {
            level: NoticeLevel::Info,
            code: "ask_claude".into(),
            message: text,
        };
        let mut q = Question::yes_no_always(Some(QuestionKind::Consult), "Hand this to Claude?");
        let model = if self.consult.model.is_empty() {
            "default model".to_string()
        } else {
            self.consult.model.clone()
        };
        q.reason.push(line(format!(
            "  [ask_claude] {what}, {mode} mode, {model} {}, up to {:.0} min",
            self.consult.effort,
            self.consult.timeout.as_secs_f64() / 60.0
        )));
        q.reason.push(line(format!(
            "    task: {}",
            untrusted::terminal_safe(task, Some(400))
        )));
        if !paths.is_empty() {
            q.reason.push(line(format!(
                "    paths: {}",
                untrusted::terminal_safe(&paths.join(", "), Some(200))
            )));
        }
        match ctx.ask(&q).await {
            Answer::Yes => true,
            Answer::Always => {
                self.consult.approval = "auto".into();
                notice(
                    ctx,
                    NoticeLevel::Info,
                    "ask_claude",
                    "  [ask_claude will run without asking for the rest of this chat]",
                );
                true
            }
            _ => false,
        }
    }

    /// One ask_claude call (rp's `ConsultRunner.run`).
    pub(crate) async fn run_consult(&mut self, ctx: &TurnContext, raw: &str) -> Value {
        let args = match super::tools::parse_args(raw) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let task = args
            .get("task")
            .map(text_of)
            .unwrap_or_default()
            .trim()
            .to_string();
        if task.is_empty() {
            return json!({"error": "give task: what Claude must achieve"});
        }
        let context = args
            .get("context")
            .map(text_of)
            .unwrap_or_default()
            .trim()
            .to_string();
        let paths: Vec<String> = match args.get("paths") {
            Some(Value::Array(a)) => a
                .iter()
                .map(text_of)
                .filter(|p| !p.trim().is_empty())
                .take(20)
                .collect(),
            _ => Vec::new(),
        };
        let mut mode = self.consult.mode.clone();
        if args.get("mode").and_then(Value::as_str) == Some("advise") {
            mode = "advise".into(); // the model may ask for less, never for more
        }
        let follow = args
            .get("follow_up_of")
            .map(text_of)
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty());
        let Some(bin) = self.consult.bin() else {
            return json!({"error": "the claude command is not installed here"});
        };

        let mut meta: Option<Value> = None;
        let mut folder: Option<PathBuf> = None;
        if let Some(f) = &follow {
            let Some(d) = self.find_consult(f) else {
                return json!({"error": format!("no consult {f} in this chat")});
            };
            let m: Value = std::fs::read_to_string(d.join("meta.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(json!({}));
            if m["session_id"].as_str().is_none_or(str::is_empty) {
                return json!({"error": format!("consult {f} has no Claude session to continue")});
            }
            meta = Some(m);
            folder = Some(d);
        }
        if !self
            .approve_consult(ctx, &mode, &task, &paths, follow.as_deref())
            .await
        {
            return json!({"status": "not_run", "error": "the user did not approve this consult"});
        }
        let (folder, mut meta) = match (folder, meta) {
            (Some(f), Some(m)) => (f, m),
            _ => {
                let _ = self.prune_consults();
                let tag: String = untrusted::nonce()
                    .unwrap_or_default()
                    .chars()
                    .take(4)
                    .collect();
                let cid = format!("{}-{tag}", chrono::Local::now().format("%Y%m%dT%H%M%S"));
                let made = self.session_dir().and_then(|s| {
                    let f = s.join(&cid);
                    std::fs::DirBuilder::new()
                        .mode(0o700)
                        .create(&f)
                        .and_then(|()| std::fs::create_dir(f.join("scratch")))
                        .and_then(|()| std::fs::create_dir(f.join("out")))
                        .map_err(|e| e.to_string())?;
                    Ok(f)
                });
                match made {
                    Ok(f) => (
                        f,
                        json!({"consult_id": cid, "chat_session": self.tracer.sid(), "runs": []}),
                    ),
                    Err(e) => return json!({"error": format!("ask_claude failed: OSError: {e}")}),
                }
            }
        };
        let cid = meta["consult_id"].as_str().unwrap_or_default().to_string();
        let n = meta["runs"].as_array().map_or(0, Vec::len) + 1;
        let ws = std::fs::canonicalize(&self.places.workspace)
            .unwrap_or_else(|_| self.places.workspace.clone());
        let request = self
            .turn_user
            .as_ref()
            .map(message_text)
            .unwrap_or_default();
        let prompt = prompt_for(n, &mode, &task, &context, &paths, &request);
        let suffix = if n == 1 {
            String::new()
        } else {
            format!(".{n}")
        };
        let _ = std::fs::write(folder.join(format!("task{suffix}.md")), &prompt);
        if !request.is_empty() {
            let _ = std::fs::write(
                folder.join(format!("request{suffix}.md")),
                format!("{request}\n"),
            );
        }
        let mut cmd: Vec<String> = vec![
            bin.display().to_string(),
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--json-schema".into(),
            report_schema().to_string(),
            "--add-dir".into(),
            ws.display().to_string(),
            "--append-system-prompt".into(),
            system_prompt(&ws, &folder),
            "--permission-mode".into(),
            self.consult.permission_mode.clone(),
        ];
        if !self.consult.model.is_empty() {
            cmd.extend(["--model".into(), self.consult.model.clone()]);
        }
        if !self.consult.effort.is_empty() {
            cmd.extend(["--effort".into(), self.consult.effort.clone()]);
        }
        if !self.consult.budget.is_empty() {
            cmd.extend(["--max-budget-usd".into(), self.consult.budget.clone()]);
        }
        if follow.is_some() {
            cmd.extend(["--resume".into(), text_of(&meta["session_id"])]);
        }
        let turn = self.monitor.as_ref().map(|_| self.turn_n);
        let task_head: String = task.chars().take(500).collect();
        self.tracer.event(
            "consult_start",
            json!({"turn": turn, "consult": cid, "run": n, "mode": mode,
                   "model": self.consult.model, "effort": self.consult.effort,
                   "folder": folder.display().to_string(), "task": task_head}),
        );
        notice(
            ctx,
            NoticeLevel::Info,
            "ask_claude",
            format!(
                "  [ask_claude] {cid}: Claude is working ({mode} mode); folder {}",
                folder.display()
            ),
        );
        let snap_ws = ws.clone();
        let before = tokio::task::spawn_blocking(move || snapshot(&snap_ws))
            .await
            .ok();
        let t0 = Instant::now();
        let task_kept: String = task.chars().take(2000).collect();
        let mut record = Cancelled {
            armed: Some(CancelRecord {
                folder: folder.clone(),
                meta: meta.clone(),
                cid: cid.clone(),
                n,
                task: task_kept.clone(),
                mode: mode.clone(),
                ws: ws.clone(),
                before: before.clone(),
                t0,
                tracer: self.tracer.clone(),
                turn: json!(turn),
            }),
            st: Streamed::default(),
        };
        let ran = run_claude(
            ctx,
            &cmd,
            &folder,
            &prompt,
            self.consult.timeout,
            &mut record.st,
        )
        .await;
        let secs = (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        let snap_ws = ws.clone();
        let files = tokio::task::spawn_blocking(move || {
            before
                .map(|b| changed_files(&b, &snapshot(&snap_ws), &snap_ws))
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default();
        record.armed = None; // ran to the end: recorded below
        let st = std::mem::take(&mut record.st);
        let (how, rc) = match ran {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::write(folder.join("claude.stderr"), &e);
                (How::Exited, Some(127))
            }
        };
        let res = st.result.clone().unwrap_or_default();
        let is_error = res
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut report: Option<Map<String, Value>> = match res.get("structured_output") {
            Some(Value::Object(o)) => Some(o.clone()),
            _ => {
                let text = res.get("result").map(text_of).unwrap_or_default();
                (!res.is_empty() && !is_error && !text.trim().is_empty())
                    .then(|| {
                        json!({"status": "done", "summary": text})
                            .as_object()
                            .cloned()
                    })
                    .flatten()
            }
        };
        let error = if how == How::Timeout {
            Some(format!(
                "timed out after {:.0} s",
                self.consult.timeout.as_secs_f64()
            ))
        } else if is_error || (rc.is_some_and(|c| c != 0) && report.is_none()) {
            let tail = stderr_tail(&folder);
            let head = res
                .get("subtype")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map_or_else(
                    || format!("claude exited with code {}", rc.unwrap_or(-1)),
                    str::to_string,
                );
            Some(if tail.is_empty() {
                head
            } else {
                format!("{head}: {tail}")
            })
        } else if report.is_none() {
            Some("Claude ended without a report".into())
        } else {
            None
        };
        if how != How::Exited {
            report = None; // whatever it said before the stop is not the answer
        }
        let cost = res.get("total_cost_usd").cloned().filter(|c| !c.is_null());
        let status = report
            .as_ref()
            .and_then(|r| r.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("failed")
            .to_string();
        let run_row = json!({
            "run": n, "how": how.as_str(), "rc": rc, "secs": secs, "error": error,
            "model": st.model, "session_id": st.session_id, "cost_usd": cost,
            "turns": res.get("num_turns"), "tools_used": st.tools_used,
            "files_changed": files, "status": status, "task": task_kept, "mode": mode,
        });
        if let Some(sid) = &st.session_id {
            meta["session_id"] = json!(sid);
        }
        if let Some(runs) = meta["runs"].as_array_mut() {
            runs.push(run_row);
        }
        let _ = std::fs::write(
            folder.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap_or_default(),
        );
        if let Some(r) = &report {
            let text = serde_json::to_string_pretty(&Value::Object(r.clone())).unwrap_or_default();
            let _ = std::fs::write(
                folder.join("out").join(format!("report{suffix}.md")),
                text + "\n",
            );
        }
        let patch = report
            .as_ref()
            .filter(|_| mode == "advise")
            .and_then(|r| r.get("patch"))
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        if let Some(p) = &patch {
            let body = if p.ends_with('\n') {
                p.clone()
            } else {
                format!("{p}\n")
            };
            let _ = std::fs::write(folder.join("out").join(format!("patch{suffix}.diff")), body);
        }
        let facts = RunFacts {
            error: error.clone(),
            model: st.model.clone(),
            secs,
            cost_usd: cost.clone(),
        };
        let out = shape_result(
            &cid,
            &mode,
            report.as_ref(),
            &files,
            &facts,
            self.consult.report_chars,
            patch.as_deref(),
            Some(&ws),
        );
        let files_head: Vec<&String> = files.iter().take(50).collect();
        self.tracer.event(
            "consult_end",
            json!({"turn": turn, "consult": cid, "run": n, "how": how.as_str(),
                   "status": out["status"], "secs": secs, "cost_usd": cost,
                   "files_changed": files_head, "tools_used": st.tools_used}),
        );
        if out["status"] == "failed"
            && let Some(m) = self.monitor.as_mut()
        {
            let why = error.as_deref().unwrap_or("no report");
            m.mark("consult_failed", &format!("ask_claude {cid} failed: {why}"));
        }
        let cost_text = cost
            .as_ref()
            .and_then(Value::as_f64)
            .map_or(String::new(), |c| format!(", ${c:.2}"));
        notice(
            ctx,
            NoticeLevel::Info,
            "ask_claude",
            format!(
                "  [ask_claude] {} in {secs:.0}s{cost_text}, {} file(s) changed",
                out["status"].as_str().unwrap_or_default(),
                files.len()
            ),
        );
        out
    }

    /// `/consult`: the settings and this chat's consults, or a change.
    pub(crate) fn consult_command(&mut self, ctx: &TurnContext, arg: &str) {
        let (word, rest) = arg.split_once(' ').unwrap_or((arg, ""));
        let rest = rest.trim();
        const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
        match word {
            "on" | "off" => self.consult.on = word == "on",
            "mode" if matches!(rest, "act" | "advise") => self.consult.mode = rest.into(),
            "model" if !rest.is_empty() => {
                self.consult.model = if rest == "default" {
                    String::new()
                } else {
                    rest.into()
                };
            }
            "effort" if EFFORTS.contains(&rest) || rest == "default" => {
                self.consult.effort = if rest == "default" {
                    String::new()
                } else {
                    rest.into()
                };
            }
            "approval" if matches!(rest, "ask" | "auto") => self.consult.approval = rest.into(),
            "" => {}
            _ => {
                notice(
                    ctx,
                    NoticeLevel::Warn,
                    "consult",
                    format!(
                        "usage: /consult [on|off | mode act|advise | model <name>|default | effort \
                         {}|default | approval ask|auto]",
                        EFFORTS.join("|")
                    ),
                );
                return;
            }
        }
        let state = if self.consult.bin().is_none() {
            "unavailable: the claude command is not on PATH"
        } else if !self.tools_on {
            "off while /tools is off"
        } else if self.consult.on {
            "on"
        } else {
            "off"
        };
        let or_default = |s: &str| {
            if s.is_empty() {
                "default".to_string()
            } else {
                s.to_string()
            }
        };
        notice(
            ctx,
            NoticeLevel::Info,
            "consult",
            format!(
                "ask_claude {state}; {} mode, model {}, effort {}, approval {}",
                self.consult.mode,
                or_default(&self.consult.model),
                or_default(&self.consult.effort),
                self.consult.approval
            ),
        );
        if !word.is_empty() {
            return;
        }
        let rows = recent(&self.consult.root, &self.tracer.sid(), 10);
        if rows.is_empty() {
            notice(
                ctx,
                NoticeLevel::Info,
                "consult",
                "no consults in this chat yet",
            );
        }
        for r in rows {
            let cost = r["cost_usd"]
                .as_f64()
                .map_or(String::new(), |c| format!(", ${c:.2}"));
            let secs = r["secs"]
                .as_f64()
                .map_or(String::new(), |s| format!(" in {s:.0}s"));
            let runs = r["runs"].as_u64().unwrap_or(0);
            let runs = if runs > 1 {
                format!(", {runs} runs")
            } else {
                String::new()
            };
            notice(
                ctx,
                NoticeLevel::Info,
                "consult",
                format!(
                    "  {}  {}{secs}{cost}, {} file(s){runs}\n    {}\n    {}",
                    text_of(&r["id"]),
                    text_of(&r["status"]),
                    r["files"],
                    untrusted::terminal_safe(&text_of(&r["task"]), Some(100)),
                    text_of(&r["folder"])
                ),
            );
        }
    }
}

#[cfg(test)]
#[path = "consult_tests.rs"]
mod tests;
