//! The toolbox (rpctl `toolbox.py`, rpctl kb d021): command-line tools
//! kept between chats.
//!
//! Qwen rewrites the same parsers and fetchers as inline Python in every
//! chat (46.7 % of run_shell calls in 49 saved chats, rpctl kb f114), yet
//! reuses a script whenever a loaded instruction file names it. The
//! toolbox keeps such scripts and names them in the prompt: a one-line
//! index in the system prompt, frozen for the chat so the prefix cache
//! keeps hitting, and find_tool for the full usage.
//!
//! A tool is a directory:
//!
//! ```text
//! <toolbox>/<name>/
//!   TOOL.md      frontmatter (name, summary, use_when, usage, examples, tags,
//!                selftest, deps, author, status, created) and the full usage
//!   <name>       the executable, run by name through run_shell
//! ```
//!
//! Two scopes: global (`<data>/viy/toolbox`) and project (`.viy/toolbox`);
//! a project tool overrides a global one of the same name. Both go on PATH
//! after the system directories, so a tool can never shadow `git` or `rm`,
//! and running one is an ordinary run_shell command with the same approval
//! and sandbox as any other.
//!
//! Codex's nearest counterpart, its skills (`skills/src/parser.rs`), reads
//! SKILL.md with serde_yaml; TOOL.md keeps rpctl's YAML subset, which
//! accepts lines YAML refuses (`summary: Build: then test`) and is the
//! format rp's tools are written in (kb d126).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_protocol::NoticeLevel;
use viy_tools::netguard::url::py_repr;

pub const REQUIRED: [&str; 3] = ["name", "summary", "usage"];
pub const LIST_FIELDS: [&str; 3] = ["examples", "tags", "deps"];
pub const AUTHORS: [&str; 3] = ["claude", "qwen", "user"];
pub const STATUSES: [&str; 3] = ["tested", "untested", "broken"];
pub const SUMMARY_MAX: usize = 100;
pub const USE_WHEN_MAX: usize = 160;
pub const USAGE_MAX: usize = 160;
pub const INDEX_MAX_DEFAULT: usize = 40;
pub const FIND_LIMIT: usize = 5;

/// A line of the tool guide (SYSTEM_GUIDE=1) while the toolbox is offered.
pub use crate::prompt::TOOLBOX_GUIDE;

/// The find_tool tool (rp's `FIND_TOOL`).
pub fn find_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "find_tool",
            "description": "Look up command-line tools in the toolbox (kept from earlier work, \
                on PATH for run_shell). name: one tool's full usage and docs. query: the tools \
                that fit a job, best first. Check before writing a script for a common job \
                (parsing a format, fetching, converting).",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string",
                              "description": "What the tool should do, e.g. 'extract tables from pdf'."},
                    "name": {"type": "string",
                             "description": "Exact tool name, for its full usage."},
                },
            },
        },
    })
}

pub const INDEX_HEAD: &str = "# Toolbox\n\nCommand-line tools kept from earlier work. They are on \
    PATH: run one with run_shell by its name. find_tool(name=...) shows a tool's full usage; \
    find_tool(query=...) searches them. When a tool below fits the task, run it instead of \
    writing new code for the same job.";

/// Built from a fixed pattern below; they are tested, so a failure here is
/// a bug in this file.
fn re(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => panic!("toolbox.rs pattern {pattern:?}: {e}"),
    }
}

static NAME: LazyLock<Regex> = LazyLock::new(|| re(r"\A[a-z0-9][a-z0-9-]{1,39}\z"));
static ITEM: LazyLock<Regex> = LazyLock::new(|| re(r"\A\s+-\s*(.*)\z"));
static KEY: LazyLock<Regex> = LazyLock::new(|| re(r"\A([a-z_]+):\s*(.*)\z"));
/// What Python's `str.isprintable` refuses, once whitespace runs are one
/// space: controls, format characters, private use, unassigned, and the
/// line and paragraph separators.
static UNPRINTABLE: LazyLock<Regex> =
    LazyLock::new(|| re(r"[\p{Cc}\p{Cf}\p{Co}\p{Cn}\p{Zl}\p{Zp}]"));
static WORD: LazyLock<Regex> = LazyLock::new(|| re(r"[a-z0-9]+"));
static SEP: LazyLock<Regex> = LazyLock::new(|| re(r"&&|\|\||[|;&()\n]|\$\(|`"));
static ASSIGN: LazyLock<Regex> = LazyLock::new(|| re(r"\A[A-Za-z_][A-Za-z0-9_]*="));

/// Wrappers `used_in` looks past.
const PREFIX_WORDS: [&str; 8] = [
    "env", "time", "nice", "nohup", "command", "exec", "xargs", "timeout",
];

/// Field weights: repeating a field's words is BM25's simplest boost.
const WEIGHTS: [(&str, usize); 6] = [
    ("name", 3),
    ("tags", 2),
    ("summary", 2),
    ("use_when", 1),
    ("usage", 1),
    ("examples", 1),
];

// ---- tools ----------------------------------------------------------------------

/// A frontmatter value: one line, or a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    Text(String),
    List(Vec<String>),
}

impl Field {
    /// Python's truth: an empty text or list is false.
    fn is_empty(&self) -> bool {
        match self {
            Self::Text(t) => t.is_empty(),
            Self::List(l) => l.is_empty(),
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Self::Text(t) => json!(t),
            Self::List(l) => json!(l),
        }
    }
}

pub type Meta = BTreeMap<String, Field>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub scope: Scope,
    pub dir: PathBuf,
    pub meta: Meta,
    pub body: String,
    pub problems: Vec<String>,
}

impl Tool {
    pub fn exe(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    /// The field, or `None` when missing or empty (rp's `get`).
    fn field(&self, key: &str) -> Option<&Field> {
        self.meta.get(key).filter(|f| !f.is_empty())
    }

    /// A one-line field's text; "" when missing, empty or a list.
    pub fn text(&self, key: &str) -> &str {
        match self.field(key) {
            Some(Field::Text(t)) => t,
            _ => "",
        }
    }

    /// A list field's items.
    pub fn list(&self, key: &str) -> Vec<String> {
        match self.field(key) {
            Some(Field::List(l)) => l.clone(),
            Some(Field::Text(t)) => vec![t.clone()],
            None => Vec::new(),
        }
    }

    pub fn status(&self) -> &str {
        Some(self.text("status"))
            .filter(|s| !s.is_empty())
            .unwrap_or("untested")
    }

    pub fn author(&self) -> &str {
        Some(self.text("author"))
            .filter(|s| !s.is_empty())
            .unwrap_or("user")
    }
}

// ---- manifest -------------------------------------------------------------------

fn scalar(v: &str) -> String {
    let v = v.trim();
    let mut ends = v.chars();
    match (ends.next(), ends.next_back()) {
        (Some(a), Some(b)) if a == b && (a == '"' || a == '\'') => v[1..v.len() - 1].to_string(),
        _ => v.to_string(),
    }
}

fn inline_list(v: &str) -> Vec<String> {
    let v = v.trim();
    let inner = &v[1..v.len() - 1];
    inner
        .split(',')
        .filter(|x| !x.trim().is_empty())
        .map(scalar)
        .collect()
}

/// `(meta, body)` from a TOOL.md (rp's `parse_manifest`). The frontmatter
/// is a small YAML subset: `key: value`, `key: [a, b]`, and `key:`
/// followed by `  - item` lines. An error when there is no frontmatter.
pub fn parse_manifest(text: &str) -> Result<(Meta, String), String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.first().is_none_or(|l| l.trim() != "---") {
        return Err("TOOL.md does not start with a --- frontmatter block".into());
    }
    let Some(end) = (1..lines.len()).find(|&i| lines[i].trim() == "---") else {
        return Err("the frontmatter block is not closed with ---".into());
    };
    let mut meta = Meta::new();
    let mut key: Option<String> = None;
    for ln in &lines[1..end] {
        if ln.trim().is_empty() {
            continue;
        }
        if let Some(m) = ITEM.captures(ln)
            && let Some(Field::List(items)) = key.as_ref().and_then(|k| meta.get_mut(k))
        {
            items.push(scalar(&m[1]));
            continue;
        }
        let Some(m) = KEY.captures(ln) else {
            let head: String = ln.trim().chars().take(60).collect();
            return Err(format!("cannot read frontmatter line: {head}"));
        };
        let (k, v) = (m[1].to_string(), m[2].trim());
        let field = if v.is_empty() {
            Field::List(Vec::new())
        } else if v.starts_with('[') && v.ends_with(']') {
            Field::List(inline_list(v))
        } else {
            Field::Text(scalar(v))
        };
        meta.insert(k.clone(), field);
        key = Some(k);
    }
    for k in LIST_FIELDS {
        if let Some(Field::Text(t)) = meta.get(k) {
            let one = Field::List(vec![t.clone()]);
            meta.insert(k.to_string(), one);
        }
    }
    Ok((meta, lines[end + 1..].join("\n").trim().to_string()))
}

// ---- scopes ---------------------------------------------------------------------

/// The two toolbox folders: global (`<data>/viy/toolbox`) and the
/// project's (`.viy/toolbox`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dirs {
    pub global: PathBuf,
    pub project: PathBuf,
}

impl Dirs {
    pub fn new(paths: &viy_config::Paths, project: &viy_config::Project) -> Self {
        Self {
            global: paths.data_dir().join("toolbox"),
            project: project.dir().join("toolbox"),
        }
    }

    /// `(scope, directory)`, lowest precedence first.
    pub fn scopes(&self) -> [(Scope, &Path); 2] {
        [
            (Scope::Global, self.global.as_path()),
            (Scope::Project, self.project.as_path()),
        ]
    }

    pub fn dir(&self, scope: Scope) -> &Path {
        match scope {
            Scope::Global => &self.global,
            Scope::Project => &self.project,
        }
    }

    /// PATH without any toolbox directory, for the shadowing check.
    fn system_path(&self) -> OsString {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let keep: Vec<PathBuf> = std::env::split_paths(&path)
            .filter(|p| {
                !p.as_os_str().is_empty()
                    && !self.scopes().iter().any(|(_, r)| {
                        if r.as_os_str().is_empty() {
                            return false;
                        }
                        let (p, r) = (p.as_os_str().as_encoded_bytes(), r.as_os_str());
                        let r = r.as_encoded_bytes();
                        p == r || p.starts_with(r) && p.get(r.len()) == Some(&b'/')
                    })
            })
            .collect();
        std::env::join_paths(keep).unwrap_or_default()
    }

    /// The tool in directory `d`, problems included; `None` without a
    /// TOOL.md.
    pub fn load(&self, d: &Path, scope: Scope) -> Option<Tool> {
        load(d, scope, &self.system_path())
    }

    /// Every tool found, valid or not, in scope order (global first).
    pub fn scan_all(&self) -> Vec<Tool> {
        let system = self.system_path();
        let mut out = Vec::new();
        for (scope, root) in self.scopes() {
            let Ok(rd) = std::fs::read_dir(root) else {
                continue;
            };
            let mut dirs: Vec<PathBuf> = rd
                .filter_map(Result::ok)
                .filter(|e| !e.file_name().as_encoded_bytes().starts_with(b"."))
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            out.extend(dirs.iter().filter_map(|d| load(d, scope, &system)));
        }
        out
    }

    /// The valid tools in effect: a project tool overrides a global one of
    /// the same name. Project first, then newest, then by name.
    pub fn scan(&self) -> Vec<Tool> {
        let mut eff: BTreeMap<String, Tool> = BTreeMap::new();
        for t in self.scan_all().into_iter().filter(Tool::ok) {
            eff.insert(t.name.clone(), t);
        }
        let mut tools: Vec<Tool> = eff.into_values().collect();
        tools.sort_by(|a, b| {
            (a.scope != Scope::Project, neg_date(a), &a.name).cmp(&(
                b.scope != Scope::Project,
                neg_date(b),
                &b.name,
            ))
        });
        tools
    }

    pub fn get(&self, name: &str) -> Option<Tool> {
        self.scan().into_iter().find(|t| t.name == name)
    }

    /// The toolbox roots that exist, resolved, for the sandbox to bind
    /// read-only.
    pub fn mount_dirs(&self) -> Vec<PathBuf> {
        self.scopes()
            .iter()
            .filter(|(_, r)| r.is_dir())
            .filter_map(|(_, r)| std::fs::canonicalize(r).ok())
            .collect()
    }
}

/// Newest first within a scope; tools without a date sort last.
fn neg_date(t: &Tool) -> i128 {
    let d = t.text("created").replace('-', "");
    if !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()) {
        d.parse::<i128>().map_or(0, |n| -n)
    } else {
        0
    }
}

fn load(d: &Path, scope: Scope, system_path: &OsStr) -> Option<Tool> {
    let man = d.join("TOOL.md");
    if !man.is_file() {
        return None;
    }
    let name = d
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parsed = std::fs::read(&man)
        .map_err(|e| e.to_string())
        .and_then(|bytes| parse_manifest(&String::from_utf8_lossy(&bytes)));
    let mut t = Tool {
        name,
        scope,
        dir: d.to_path_buf(),
        meta: Meta::new(),
        body: String::new(),
        problems: Vec::new(),
    };
    match parsed {
        Ok((meta, body)) => {
            t.meta = meta;
            t.body = body;
            t.problems = validate(&t, system_path);
        }
        Err(e) => t.problems.push(e),
    }
    Some(t)
}

/// Problems that keep a tool out of the index and off PATH.
pub fn validate(tool: &Tool, system_path: &OsStr) -> Vec<String> {
    let mut out = Vec::new();
    let meta = &tool.meta;
    for k in REQUIRED {
        if meta.get(k).is_none_or(Field::is_empty) {
            out.push(format!("missing {k}"));
        }
    }
    match meta.get("name") {
        Some(Field::Text(n)) if !n.is_empty() && *n != tool.name => out.push(format!(
            "name {} does not match its directory {}",
            py_repr(n),
            py_repr(&tool.name)
        )),
        Some(Field::List(l)) if !l.is_empty() => out.push(format!(
            "name [{}] does not match its directory {}",
            l.iter().map(|x| py_repr(x)).collect::<Vec<_>>().join(", "),
            py_repr(&tool.name)
        )),
        _ => {}
    }
    let named = NAME.is_match(&tool.name);
    if !named {
        out.push("the name must be 2-40 characters of a-z, 0-9 and -".into());
    }
    for k in ["summary", "use_when", "usage"] {
        if matches!(meta.get(k), Some(Field::List(_))) {
            out.push(format!("{k} must be one line of text"));
        }
    }
    let one_of = |key: &str, allowed: &[&str], what: &str| match meta.get(key) {
        Some(Field::Text(v)) if !v.is_empty() && !allowed.contains(&v.as_str()) => {
            Some(format!("{what} must be one of {}", allowed.join(", ")))
        }
        Some(Field::List(l)) if !l.is_empty() => {
            Some(format!("{what} must be one of {}", allowed.join(", ")))
        }
        _ => None,
    };
    out.extend(one_of("author", &AUTHORS, "author"));
    out.extend(one_of("status", &STATUSES, "status"));
    let exe = tool.exe();
    if !exe.is_file() {
        out.push(format!(
            "no executable named {} in its directory",
            tool.name
        ));
    } else if !executable(&exe) {
        out.push(format!("{} is not executable (chmod +x)", tool.name));
    }
    if named && viy_config::vault::find_executable(Some(system_path), &tool.name).is_some() {
        out.push(format!(
            "{} is already a command on this machine; pick another name",
            tool.name
        ));
    }
    out
}

/// `os.access(path, X_OK)`.
fn executable(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a NUL-terminated path that lives across the call.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Directories to append to PATH, one per tool in effect.
pub fn path_dirs(tools: &[Tool]) -> Vec<PathBuf> {
    tools
        .iter()
        .map(|t| std::fs::canonicalize(&t.dir).unwrap_or_else(|_| t.dir.clone()))
        .collect()
}

/// `base` with the tools' directories after it.
pub fn with_path(base: &OsStr, tools: &[Tool]) -> OsString {
    let mut out = OsString::new();
    let dirs = path_dirs(tools);
    let parts = std::iter::once(base).chain(dirs.iter().map(|d| d.as_os_str()));
    for p in parts.filter(|p| !p.is_empty()) {
        if !out.is_empty() {
            out.push(":");
        }
        out.push(p);
    }
    out
}

// ---- prompt ---------------------------------------------------------------------

/// One printable line of at most `limit` characters.
pub fn oneline(text: &str, limit: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = UNPRINTABLE.replace_all(&joined, "");
    if text.chars().count() <= limit {
        return text.into_owned();
    }
    let head: String = text.chars().take(limit.saturating_sub(3)).collect();
    format!("{}...", head.trim_end())
}

fn sentence(text: &str, limit: usize) -> String {
    let text = oneline(text, limit);
    if text.ends_with(['.', '!', '?']) {
        text
    } else {
        text + "."
    }
}

pub fn index_line(t: &Tool) -> String {
    let mut line = format!("- {}: {}", t.name, sentence(t.text("summary"), SUMMARY_MAX));
    if !t.text("use_when").is_empty() {
        line += &format!(" Use when: {}", sentence(t.text("use_when"), USE_WHEN_MAX));
    }
    line += &format!(" Usage: `{}`", oneline(t.text("usage"), USAGE_MAX));
    if t.status() != "tested" {
        line += &format!(" ({})", t.status());
    }
    line
}

/// The system prompt's toolbox block, `None` without tools.
pub fn render_index(tools: &[Tool], limit: usize) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let shown = &tools[..limit.min(tools.len())];
    let mut lines = vec![INDEX_HEAD.to_string(), String::new()];
    lines.extend(shown.iter().map(index_line));
    let more = tools.len() - shown.len();
    if more > 0 {
        lines.push(format!(
            "- ... {more} more: find_tool(query=...) searches all of them."
        ));
    }
    Some(lines.join("\n"))
}

// ---- search ---------------------------------------------------------------------

fn words(text: &str) -> Vec<String> {
    let lower: String = text.chars().flat_map(char::to_lowercase).collect();
    WORD.find_iter(&lower)
        .map(|m| {
            let w = m.as_str();
            if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
                w[..w.len() - 1].to_string() // tables -> table, pdfs -> pdf
            } else {
                w.to_string()
            }
        })
        .collect()
}

/// Equal, or one word form of the other: parse / parser / parsing.
fn same(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let n = a.len().min(b.len());
    if n < 4 {
        return false;
    }
    let need = (n - 2).max(4);
    a.as_bytes()[..need] == b.as_bytes()[..need]
}

fn doc(t: &Tool) -> Vec<String> {
    let mut out = Vec::new();
    for (key, w) in WEIGHTS {
        let text = match t.field(key) {
            Some(Field::List(l)) => l.join(" "),
            Some(Field::Text(s)) => s.clone(),
            None => String::new(),
        };
        let ws = words(&text);
        for _ in 0..w {
            out.extend(ws.iter().cloned());
        }
    }
    out
}

/// `[(score, tool)]` best first: BM25 over the manifest fields, words
/// matching across simple word forms ([`same`]), so "parsing tables"
/// finds a tool whose manifest says "parse a table".
pub fn search<'a>(query: &str, tools: &'a [Tool], limit: usize) -> Vec<(f64, &'a Tool)> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let mut q: Vec<String> = Vec::new();
    for w in words(query).into_iter().filter(|w| w.len() > 1) {
        if !q.contains(&w) {
            q.push(w);
        }
    }
    if q.is_empty() || tools.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Vec<String>> = tools.iter().map(doc).collect();
    let total: usize = docs.iter().map(Vec::len).sum();
    // `sum / len or 1.0`: no words at all averages to one
    let avg = if total == 0 {
        1.0
    } else {
        total as f64 / docs.len() as f64
    };
    let n = docs.len() as f64;
    let hits = |word: &str, doc: &[String]| doc.iter().filter(|w| same(word, w)).count();
    let mut scored: Vec<(f64, &Tool)> = Vec::new();
    for (t, d) in tools.iter().zip(&docs) {
        let mut s = 0.0;
        for word in &q {
            let tf = hits(word, d) as f64;
            if tf == 0.0 {
                continue;
            }
            let df = docs.iter().filter(|d| hits(word, d) > 0).count() as f64;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            s += idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * d.len() as f64 / avg));
        }
        if s > 0.0 {
            scored.push((s, t));
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.truncate(limit);
    scored
}

/// A tool as find_tool returns it.
pub fn describe(t: &Tool, full: bool) -> Value {
    let mut d = Map::new();
    d.insert("name".into(), json!(t.name));
    for k in ["summary", "use_when", "usage"] {
        d.insert(k.into(), t.field(k).map_or(json!(""), Field::to_value));
    }
    d.insert("examples".into(), json!(t.list("examples")));
    d.insert("status".into(), json!(t.status()));
    d.insert("author".into(), json!(t.author()));
    d.insert("scope".into(), json!(t.scope.as_str()));
    if let Some(deps) = t.field("deps") {
        d.insert("deps".into(), deps.to_value());
    }
    if full {
        d.insert("path".into(), json!(t.exe().display().to_string()));
        if !t.body.is_empty() {
            d.insert("doc".into(), json!(t.body));
        }
    }
    d.retain(|_, v| !matches!(v, Value::String(s) if s.is_empty()) && *v != json!([]));
    Value::Object(d)
}

/// Up to three names close to `name`, best first (Python's
/// `difflib.get_close_matches(name, names, 3, 0.5)`).
pub fn suggest(name: &str, names: &[&str]) -> Vec<String> {
    let mut scored: Vec<(f64, &str)> = names
        .iter()
        .map(|x| (ratio(x, name), *x))
        .filter(|(r, _)| *r >= 0.5)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(a.1)));
    scored
        .into_iter()
        .take(3)
        .map(|(_, x)| x.to_string())
        .collect()
}

/// `SequenceMatcher(None, a, b).ratio()`: twice the matched characters
/// over both lengths, the matches found as difflib finds them (longest
/// block first, earliest in `a`, then in `b`; `b`'s popular characters
/// left out of the index past 200 characters).
fn ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    let mut b2j: BTreeMap<char, Vec<usize>> = BTreeMap::new();
    for (j, c) in b.iter().enumerate() {
        b2j.entry(*c).or_default().push(j);
    }
    if b.len() >= 200 {
        let ntest = b.len() / 100 + 1;
        b2j.retain(|_, js| js.len() <= ntest);
    }
    let longest = |alo: usize, ahi: usize, blo: usize, bhi: usize| {
        let (mut bi, mut bj, mut size) = (alo, blo, 0);
        let mut j2len: BTreeMap<usize, usize> = BTreeMap::new();
        for (i, c) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut next = BTreeMap::new();
            for &j in b2j.get(c).map_or(&[][..], Vec::as_slice) {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j.checked_sub(1).and_then(|p| j2len.get(&p)).unwrap_or(&0) + 1;
                next.insert(j, k);
                if k > size {
                    (bi, bj, size) = (i + 1 - k, j + 1 - k, k);
                }
            }
            j2len = next;
        }
        while bi > alo && bj > blo && a[bi - 1] == b[bj - 1] {
            (bi, bj, size) = (bi - 1, bj - 1, size + 1);
        }
        while bi + size < ahi && bj + size < bhi && a[bi + size] == b[bj + size] {
            size += 1;
        }
        (bi, bj, size)
    };
    let mut matched = 0;
    let mut queue = vec![(0, a.len(), 0, b.len())];
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest(alo, ahi, blo, bhi);
        if k > 0 {
            matched += k;
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    2.0 * matched as f64 / total as f64
}

/// Python's `str(x)` of a JSON argument, falsy ones as "".
fn arg_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null | Value::Bool(false)) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => String::new(),
        Some(Value::Array(a)) if a.is_empty() => String::new(),
        Some(Value::Object(o)) if o.is_empty() => String::new(),
        Some(other) => other.to_string(),
    }
}

/// find_tool's result. Reads `tools` as given: the chat passes the live
/// toolbox, so a tool kept after the chat started (and missing from its
/// frozen index) is found.
pub fn run_find(args: &Map<String, Value>, tools: &[Tool]) -> Value {
    let name = arg_text(args.get("name")).trim().to_string();
    let query = arg_text(args.get("query")).trim().to_string();
    if !name.is_empty() {
        if let Some(t) = tools.iter().find(|t| t.name == name) {
            return json!({"tool": describe(t, true)});
        }
        let mut out = Map::new();
        out.insert("error".into(), json!(format!("no tool named {name}")));
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        let close = suggest(&name, &names);
        if !close.is_empty() {
            out.insert("did_you_mean".into(), json!(close));
        }
        if !query.is_empty() {
            let matches: Vec<Value> = search(&query, tools, FIND_LIMIT)
                .into_iter()
                .map(|(_, t)| describe(t, false))
                .collect();
            out.insert("matches".into(), json!(matches));
        }
        return Value::Object(out);
    }
    if !query.is_empty() {
        let res = search(&query, tools, FIND_LIMIT);
        if res.is_empty() {
            return json!({"matches": [], "note": format!(
                "no tool fits {}; {} tool(s) in the toolbox",
                py_repr(&query),
                tools.len()
            )});
        }
        let matches: Vec<Value> = res.into_iter().map(|(_, t)| describe(t, false)).collect();
        return json!({ "matches": matches });
    }
    let all: Vec<Value> = tools
        .iter()
        .map(|t| json!({"name": t.name, "summary": t.field("summary").map_or(json!(""), Field::to_value)}))
        .collect();
    json!({ "tools": all })
}

/// What find_tool's progress line names: `name`, else `query`, else "all".
pub fn find_what(args: &Map<String, Value>) -> String {
    [arg_text(args.get("name")), arg_text(args.get("query"))]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "all".into())
}

// ---- commands -------------------------------------------------------------------

/// Toolbox tools that `cmd` runs as a command, in order of appearance. A
/// speed bump for labelling, not a parser: it looks at the first word of
/// each pipeline segment, past `VAR=value` and wrappers like env or time.
pub fn used_in<'a>(cmd: &str, tools: &'a [Tool]) -> Vec<&'a Tool> {
    let mut found: Vec<&Tool> = Vec::new();
    if tools.is_empty() {
        return found;
    }
    for seg in SEP.split(cmd) {
        for w in seg.split_whitespace() {
            let w = w.trim_matches(['\'', '"']);
            if ASSIGN.is_match(w) || w.starts_with('-') {
                continue;
            }
            if PREFIX_WORDS.contains(&w) || !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let base = w.rsplit('/').next().unwrap_or(w);
            // the last tool of a name is the one in effect, as a dict keeps it
            if let Some(t) = tools.iter().rev().find(|t| t.name == base)
                && !found.iter().any(|f| std::ptr::eq(*f, t))
            {
                found.push(t);
            }
            break;
        }
    }
    found
}

pub fn label(t: &Tool) -> String {
    format!(
        "{} (by {}, {}, {})",
        t.name,
        t.author(),
        t.status(),
        t.scope.as_str()
    )
}

/// One line the toolbox commands say, with its level (rp's colours: dim
/// and cyan are Info, yellow Warn, red Error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub level: NoticeLevel,
    pub text: String,
}

fn info(text: impl Into<String>) -> Line {
    Line {
        level: NoticeLevel::Info,
        text: text.into(),
    }
}

fn warn(text: impl Into<String>) -> Line {
    Line {
        level: NoticeLevel::Warn,
        text: text.into(),
    }
}

fn error(text: impl Into<String>) -> Line {
    Line {
        level: NoticeLevel::Error,
        text: text.into(),
    }
}

fn tool_row(t: &Tool) -> Vec<Line> {
    let mark = if t.ok() { "" } else { "! " };
    let row = format!(
        "  {mark}{}  [{}, {}, {}]",
        t.name,
        t.scope.as_str(),
        t.author(),
        t.status()
    );
    let mut out = vec![if t.ok() { info(row) } else { error(row) }];
    if !t.text("summary").is_empty() {
        out.push(info(format!("      {}", oneline(t.text("summary"), 100))));
    }
    out.extend(t.problems.iter().map(|p| warn(format!("      {p}"))));
    out
}

/// `list`: every tool, the overridden ones marked.
pub fn cmd_list(dirs: &Dirs) -> Vec<Line> {
    let every = dirs.scan_all();
    if every.is_empty() {
        return vec![info(format!(
            "the toolbox is empty ({}, {})",
            dirs.global.display(),
            dirs.project.display()
        ))];
    }
    let eff: BTreeMap<String, PathBuf> = dirs
        .scan()
        .into_iter()
        .map(|t| (t.name.clone(), t.dir))
        .collect();
    let mut out = Vec::new();
    for t in &every {
        if t.ok() && eff.get(&t.name) != Some(&t.dir) {
            out.push(info(format!(
                "  {}  [{}] overridden by the project's {}",
                t.name,
                t.scope.as_str(),
                t.name
            )));
            continue;
        }
        out.extend(tool_row(t));
    }
    out
}

/// `show <name>`: one tool's manifest and usage; 1 when there is none.
pub fn cmd_show(dirs: &Dirs, name: &str) -> (Vec<Line>, i32) {
    let tools = dirs.scan();
    let Some(t) = tools.iter().find(|t| t.name == name) else {
        if let Some(broken) = dirs.scan_all().into_iter().rev().find(|x| x.name == name) {
            return (tool_row(&broken), 1);
        }
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        let close = suggest(name, &names);
        let hint = if close.is_empty() {
            String::new()
        } else {
            format!("; did you mean {}?", close.join(", "))
        };
        return (vec![warn(format!("no tool named {name}{hint}"))], 1);
    };
    let mut out = tool_row(t);
    out.push(info(format!("  {}", t.exe().display())));
    for k in ["use_when", "usage"] {
        if !t.text(k).is_empty() {
            out.push(info(format!("  {k}: {}", t.text(k))));
        }
    }
    out.extend(
        t.list("examples")
            .iter()
            .map(|ex| info(format!("  example: {ex}"))),
    );
    if !t.body.is_empty() {
        out.push(info(""));
        out.extend(t.body.lines().map(info));
    }
    (out, 0)
}

/// `find <words>`: the tools that fit, with their scores.
pub fn cmd_find(dirs: &Dirs, query: &str) -> Vec<Line> {
    let tools = dirs.scan();
    let res = search(query, &tools, FIND_LIMIT);
    if res.is_empty() {
        return vec![info(format!("no tool matches {}", py_repr(query)))];
    }
    res.into_iter()
        .map(|(s, t)| {
            info(format!(
                "  {}  {s:.2}  {}",
                t.name,
                oneline(t.text("summary"), 90)
            ))
        })
        .collect()
}

/// `check`: the tools with problems; 1 when there is any.
pub fn cmd_check(dirs: &Dirs) -> (Vec<Line>, i32) {
    let every = dirs.scan_all();
    let bad: Vec<&Tool> = every.iter().filter(|t| !t.ok()).collect();
    let mut out: Vec<Line> = bad.iter().flat_map(|t| tool_row(t)).collect();
    out.push(info(format!(
        "{} tool(s) ok, {} with problems",
        every.len() - bad.len(),
        bad.len()
    )));
    (out, i32::from(!bad.is_empty()))
}

/// `path`: the two folders.
pub fn cmd_path(dirs: &Dirs) -> Vec<Line> {
    dirs.scopes()
        .iter()
        .map(|(s, d)| info(format!("  {:8} {}", s.as_str(), d.display())))
        .collect()
}

/// `add <dir>`: copies a tool directory into `scope` after validating it.
/// `has_project`: the project has its folder (a `.viy/`).
pub fn cmd_add(dirs: &Dirs, src: &Path, scope: Scope, has_project: bool) -> (Vec<Line>, i32) {
    if scope == Scope::Project && !has_project {
        return (
            vec![error("no .viy/ here: run viy init, or use --global")],
            1,
        );
    }
    let src = std::fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let Some(mut t) = dirs.load(&src, scope) else {
        return (vec![error(format!("{} has no TOOL.md", src.display()))], 1);
    };
    let dir_name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !NAME.is_match(&dir_name) {
        t.problems
            .push("the directory name must be 2-40 characters of a-z, 0-9 and -".into());
    }
    if !t.ok() {
        return (tool_row(&t), 1);
    }
    let root = dirs.dir(scope);
    let dest = root.join(&dir_name);
    if dest.exists() || dest.is_symlink() {
        let flag = if scope == Scope::Global {
            " --global"
        } else {
            ""
        };
        return (
            vec![error(format!(
                "{} already exists; remove it first (viy toolbox rm {dir_name}{flag})",
                dest.display()
            ))],
            1,
        );
    }
    let copied = std::fs::create_dir_all(root).and_then(|()| copy_tree(&src, &dest));
    if let Err(e) = copied {
        return (
            vec![error(format!("could not copy {}: {e}", src.display()))],
            1,
        );
    }
    (
        vec![info(format!(
            "added {dir_name} to the {} toolbox: {}",
            scope.as_str(),
            dest.display()
        ))],
        0,
    )
}

/// `shutil.copytree(src, dest, symlinks=False)` without `__pycache__` and
/// `.stats.json`: links are followed and their targets copied.
fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dest)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        if name == "__pycache__" || name == ".stats.json" {
            continue;
        }
        let (from, to) = (e.path(), dest.join(&name));
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    let perms = std::fs::metadata(src)?.permissions();
    std::fs::set_permissions(dest, perms)
}

/// `rm <name>`: deletes a tool of `scope` once `confirm` says yes to the
/// question it is given.
pub fn cmd_rm(
    dirs: &Dirs,
    name: &str,
    scope: Scope,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> (Vec<Line>, i32) {
    let d = dirs.dir(scope).join(name);
    if !NAME.is_match(name) || !d.join("TOOL.md").is_file() {
        return (
            vec![warn(format!("no {} tool named {name}", scope.as_str()))],
            1,
        );
    }
    if !confirm(&format!("delete {}? [y/N] ", d.display())) {
        return (vec![info("aborted")], 1);
    }
    match std::fs::remove_dir_all(&d) {
        Ok(()) => (vec![info(format!("deleted {}", d.display()))], 0),
        Err(e) => (
            vec![error(format!("could not delete {}: {e}", d.display()))],
            1,
        ),
    }
}

#[cfg(test)]
#[path = "toolbox_tests.rs"]
pub(crate) mod tests;
