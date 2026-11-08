//! `viy init --kb` and `/kb` (kb d112, WP 4.15): the self-checking
//! knowledge base `kb_init_qwen.py` writes, with viy's names for rp's.
//!
//! No model is asked: the files are templates under `assets/kb/`, as
//! Codex keeps its `/init` prompt (`tui/assets/prompt_for_init_command.md`),
//! filled in one pass. The agent guide names no model; only its file name
//! follows the model, from `KB_GUIDE_FILE` (a profile sets it; QWEN.md
//! when none does), which must be one of the `INSTRUCTION_FILES` viy loads.
//! The KB keeps the Python checkers, so it checks itself whoever works in
//! it; they are this repository's `tools/kb_lint.py` and `tools/kb_facts.py`
//! byte for byte (the pre-commit hook compares them).
//!
//! Nothing that exists is overwritten. A folder the model can write into
//! may hold anything, so no write goes through a link, git runs without the
//! caller's `GIT_*` variables and with no fsmonitor, and the checkers run
//! with `python3 -I`, which keeps a planted module in `tools/` out of
//! `sys.path`.

use std::io;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::process::Output;

use chrono::Days;
use chrono::NaiveDate;
use viy_config::Config;
use viy_config::Project;

use crate::instructions;

/// The guide's file name when no profile sets `KB_GUIDE_FILE`: the name
/// the default model, Qwen, looks for.
pub const DEFAULT_GUIDE_FILE: &str = "QWEN.md";

const GUIDE: &str = include_str!("../assets/kb/agent-guide.md");
const README: &str = include_str!("../assets/kb/README.md");
const INDEX: &str = include_str!("../assets/kb/00-index.md");
const EXAMPLE_FACT: &str = include_str!("../assets/kb/f001-example.md");
const SCHEMA: &str = include_str!("../assets/kb/schema.md");
const CALENDAR: &str = include_str!("../assets/kb/s001-pinned-calendar.md");
const KB_LINT: &str = include_str!("../assets/kb/kb_lint.py");
const KB_FACTS: &str = include_str!("../assets/kb/kb_facts.py");
const PRE_COMMIT: &str = include_str!("../assets/kb/pre-commit");
const GITIGNORE: &str = include_str!("../assets/kb/gitignore");

const NO_CONTEXT: &str = "_(One line on what this project is betting on. Replace me.)_";

/// The folders the KB writes into; one that is a link stops it.
const DIRS: [&str; 6] = [
    "kb/facts",
    "kb/strategy",
    "kb/decisions",
    "kb-system",
    "tools",
    ".githooks",
];

/// What `--project`, `--context` and `--day0` said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub project: Option<String>,
    pub context: Option<String>,
    pub day0: Option<NaiveDate>,
}

impl Options {
    /// `/kb`'s arguments, split as a shell would.
    pub fn parse(arg: &str) -> Result<Self, String> {
        const USAGE: &str = "usage: /kb [--project NAME] [--context TEXT] [--day0 YYYY-MM-DD]";
        let words = shlex::split(arg).ok_or_else(|| format!("unbalanced quotes; {USAGE}"))?;
        let mut opts = Self::default();
        let mut words = words.into_iter();
        while let Some(word) = words.next() {
            let (flag, inline) = match word.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
                _ => (word, None),
            };
            let mut value = || {
                inline
                    .clone()
                    .or_else(|| words.next())
                    .ok_or_else(|| format!("{flag} needs a value; {USAGE}"))
            };
            match flag.as_str() {
                "--project" => opts.project = Some(value()?),
                "--context" => opts.context = Some(value()?),
                "--day0" => opts.day0 = Some(parse_day0(&value()?)?),
                _ => return Err(format!("unknown argument {flag:?}; {USAGE}")),
            }
        }
        Ok(opts)
    }
}

/// `--day0`'s value, an ISO date.
pub fn parse_day0(text: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| format!("--day0 must be ISO YYYY-MM-DD, got {text:?}"))
}

/// The guide's file name: `KB_GUIDE_FILE`, or [`DEFAULT_GUIDE_FILE`]. It
/// must be a plain name in `INSTRUCTION_FILES`, or viy would write a guide
/// it never loads.
pub fn guide_file(cfg: &Config) -> Result<String, String> {
    let name = cfg
        .get("KB_GUIDE_FILE")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_GUIDE_FILE);
    let plain = Path::new(name).file_name().is_some_and(|f| f == name);
    if !plain {
        return Err(format!(
            "KB_GUIDE_FILE={name}: a file name, without a folder, is needed"
        ));
    }
    let names = instructions::names(cfg);
    if !names.iter().any(|n| n == name) {
        return Err(format!(
            "KB_GUIDE_FILE={name} is not in INSTRUCTION_FILES ({}), so viy would never load the guide; add it there or pick one of those",
            names.join(",")
        ));
    }
    Ok(name.to_string())
}

/// One file the KB is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// Relative to the KB's root.
    pub path: String,
    pub text: String,
    pub executable: bool,
}

/// The KB's files for a root named `root_name`, the guide as `guide`,
/// dated `today` (pure: the tests and the writer share it). README.md and
/// `.gitignore` come last; [`init`] treats them apart.
pub fn files(root_name: &str, guide: &str, opts: &Options, today: NaiveDate) -> Vec<File> {
    let day = |from: NaiveDate, n: u64| {
        from.checked_add_days(Days::new(n))
            .unwrap_or(from)
            .to_string()
    };
    let day0 = opts.day0.unwrap_or(today);
    let pad = 19usize.saturating_sub(guide.chars().count()).max(1);
    let guide_line = format!(
        "{guide}{}standing instructions for the agent (auto-loaded by viy chat)",
        " ".repeat(pad)
    );
    let values = [
        ("today", today.to_string()),
        ("ttl_90", day(today, 90)),
        ("ttl_120", day(today, 120)),
        ("ttl_180", day(today, 180)),
        ("day0", day0.to_string()),
        ("day30", day(day0, 30)),
        ("day90", day(day0, 90)),
        ("stop", day(day0, 182)),
        (
            "project",
            opts.project
                .clone()
                .unwrap_or_else(|| root_name.to_string()),
        ),
        (
            "context",
            opts.context
                .clone()
                .unwrap_or_else(|| NO_CONTEXT.to_string()),
        ),
        ("guide_file", guide.to_string()),
        ("guide_line", guide_line),
    ];
    let file = |path: &str, template: &str, executable: bool| File {
        path: path.to_string(),
        text: render(template, &values),
        executable,
    };
    // The checkers, the hook and .gitignore as they are: kb_lint.py's own
    // f-strings hold `{today}`.
    let verbatim = |path: &str, text: &str, executable: bool| File {
        path: path.to_string(),
        text: text.to_string(),
        executable,
    };
    let mut out = vec![
        file(guide, GUIDE, false),
        file("kb/00-index.md", INDEX, false),
        file("kb/facts/f001-example.md", EXAMPLE_FACT, false),
        file("kb-system/schema.md", SCHEMA, false),
        verbatim("tools/kb_lint.py", KB_LINT, true),
        verbatim("tools/kb_facts.py", KB_FACTS, true),
        verbatim(".githooks/pre-commit", PRE_COMMIT, true),
        // git runs the hook outside the sandbox: it runs these copies, in
        // the hooks folder the sandbox and write_file keep the model out of,
        // not the ones in tools/ that the model can change.
        verbatim(".githooks/kb_lint.py", KB_LINT, false),
        verbatim(".githooks/kb_facts.py", KB_FACTS, false),
    ];
    if opts.day0.is_some() {
        out.push(file("kb/strategy/s001-pinned-calendar.md", CALENDAR, false));
    }
    out.push(file("README.md", README, false));
    out.push(verbatim(".gitignore", GITIGNORE, false));
    out
}

/// `template` with each `{key}` of `values` filled, in one pass, so a
/// value that holds a `{key}` stays as it is.
fn render(template: &str, values: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let hit = values
            .iter()
            .find(|(key, _)| rest[1..].starts_with(key) && rest[1 + key.len()..].starts_with('}'));
        match hit {
            Some((key, value)) => {
                out.push_str(value);
                rest = &rest[key.len() + 2..];
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// What [`init`] did, line by line, and whether it failed after writing.
#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<String>,
    /// The guide's file name.
    pub guide: String,
    /// The pre-commit hook is in use.
    pub hooked: bool,
    /// Set when the files were written but the checkers did not pass.
    pub failed: Option<String>,
}

/// Writes the KB into `root` (made a project too, as `viy init` does).
/// An `Err` from the checks before writing means nothing was written; one
/// while writing names the file it stopped at.
pub fn init(root: &Path, cfg: &Config, opts: &Options, today: NaiveDate) -> Result<Report, String> {
    let guide = guide_file(cfg)?;
    let root = &std::fs::canonicalize(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let root_name = root.file_name().map_or_else(
        || root.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let files = files(&root_name, &guide, opts, today);

    // A KB already here, its guide under another model's name.
    if root.join("kb/00-index.md").exists() && !exists(&root.join(&guide)) {
        let other = instructions::names(cfg)
            .into_iter()
            .find(|n| *n != guide && exists(&root.join(n)));
        if let Some(other) = other {
            return Err(format!(
                "{} has a KB whose guide is {other}; this model's profile names {guide} (KB_GUIDE_FILE). Nothing written. The guide's text fits any model: rename it, e.g. git mv {other} {guide}",
                root.display()
            ));
        }
    }
    let links: Vec<&str> = DIRS
        .iter()
        .flat_map(|d| ancestors(d))
        .chain([".gitignore"])
        .filter(|p| is_link(&root.join(p)))
        .collect();
    if !links.is_empty() {
        return Err(format!(
            "{} is a link; viy writes the KB through no link. Nothing written",
            dedup(links).join(", ")
        ));
    }
    let mut owned: Vec<&str> = files
        .iter()
        .map(|f| f.path.as_str())
        .filter(|p| !matches!(*p, "README.md" | ".gitignore"))
        .filter(|p| !p.starts_with("kb/"))
        .filter(|p| exists(&root.join(p)))
        .collect();
    if exists(&root.join("kb")) {
        owned.insert(0, "kb/");
    }
    if !owned.is_empty() {
        return Err(format!(
            "{} already has {}; viy overwrites nothing. Nothing written",
            root.display(),
            owned.join(", ")
        ));
    }

    let (_, created) = Project::init(root).map_err(|e| e.to_string())?;
    let mut report = Report {
        guide,
        ..Report::default()
    };
    let lines = &mut report.lines;
    let fail = |path: &str, e: io::Error| format!("{path}: {e}");
    for dir in DIRS {
        std::fs::create_dir_all(root.join(dir)).map_err(|e| fail(dir, e))?;
    }
    for file in &files {
        let path = root.join(&file.path);
        match file.path.as_str() {
            "README.md" if exists(&path) => {
                lines.push("  kept README.md (there is one)".into());
                continue;
            }
            ".gitignore" if exists(&path) => {
                let have = std::fs::read_to_string(&path).map_err(|e| fail(".gitignore", e))?;
                let missing: Vec<&str> = file
                    .text
                    .lines()
                    .filter(|l| !have.lines().any(|h| h.trim() == *l))
                    .collect();
                if !missing.is_empty() {
                    let mut add = String::new();
                    if !have.is_empty() && !have.ends_with('\n') {
                        add.push('\n');
                    }
                    for line in &missing {
                        add.push_str(line);
                        add.push('\n');
                    }
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .and_then(|mut f| f.write_all(add.as_bytes()))
                        .map_err(|e| fail(".gitignore", e))?;
                    lines.push(format!("  .gitignore: added {}", missing.join(", ")));
                }
                continue;
            }
            _ => {}
        }
        // create_new: a file that appeared meanwhile, or a link, is not
        // written through.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut f| f.write_all(file.text.as_bytes()))
            .map_err(|e| fail(&file.path, e))?;
        if file.executable {
            let mut perms = std::fs::metadata(&path)
                .map_err(|e| fail(&file.path, e))?
                .permissions();
            perms.set_mode(perms.mode() | 0o111);
            std::fs::set_permissions(&path, perms).map_err(|e| fail(&file.path, e))?;
        }
        lines.push(format!("  wrote {}", file.path));
    }
    if created {
        lines.push("  wrote .viy/".into());
    }
    report.hooked = install_hook(root, lines);
    lines.push(String::new());
    lines.push(format!("KB created at {}", root.display()));
    report.failed = verify(root, lines);
    Ok(report)
}

/// Points git at `.githooks/` (the script's `install_hook`): `git init` in
/// a folder outside any repository, `core.hooksPath` in a repository rooted
/// here with no hooks of its own, nothing otherwise. True when the hook is
/// in use.
fn install_hook(root: &Path, lines: &mut Vec<String>) -> bool {
    let top = match git(root, &["rev-parse", "--show-toplevel"]) {
        Ok(out) => out,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            lines.push("  git not found - pre-commit hook not installed".into());
            return false;
        }
        Err(e) => {
            lines.push(format!("  git: {e} - pre-commit hook not installed"));
            return false;
        }
    };
    if !top.status.success() {
        if let Err(e) = ok(git(root, &["init", "-q"])) {
            lines.push(format!("  git init: {e} - pre-commit hook not installed"));
            return false;
        }
    } else {
        let top = String::from_utf8_lossy(&top.stdout).trim().to_string();
        if std::fs::canonicalize(&top).ok().as_deref() != Some(root) {
            lines.push(format!(
                "  inside the git repo at {top} - pre-commit hook NOT installed"
            ));
            lines.push(
                "  (run its checks from that repo's own hooks, or make this KB its own repo)"
                    .into(),
            );
            return false;
        }
        let hooks_path = git(root, &["config", "core.hooksPath"])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let own_hook = git(root, &["rev-parse", "--git-path", "hooks/pre-commit"])
            .ok()
            .filter(|o| o.status.success())
            .map(|o| root.join(String::from_utf8_lossy(&o.stdout).trim()))
            .is_some_and(|p| exists(&p));
        if !hooks_path.is_empty() || own_hook {
            lines.push("  repo already has its own hooks - pre-commit hook NOT installed".into());
            lines.push("  (to use it: git config core.hooksPath .githooks)".into());
            return false;
        }
    }
    if let Err(e) = ok(git(root, &["config", "core.hooksPath", ".githooks"])) {
        lines.push(format!("  git config: {e} - pre-commit hook not installed"));
        return false;
    }
    lines.push("  git hooks -> .githooks/ (pre-commit runs kb_lint + kb_facts)".into());
    true
}

/// git in `root`, without the caller's `GIT_*` variables (a hook's
/// `GIT_DIR` would point it at another repository) and with no fsmonitor.
fn git(root: &Path, args: &[&str]) -> io::Result<Output> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(root);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.output()
}

/// A command's failure as text.
fn ok(out: io::Result<Output>) -> Result<(), String> {
    let out = out.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Runs the KB's two checkers; the reason when they did not pass.
fn verify(root: &Path, lines: &mut Vec<String>) -> Option<String> {
    lines.push(String::new());
    lines.push("verifying:".into());
    let checks: [&[&str]; 2] = [
        &["tools/kb_lint.py"],
        &["tools/kb_facts.py", "check", "--strict", "--quiet"],
    ];
    for args in checks {
        let out = Command::new("python3")
            .arg("-I")
            .args(args)
            .current_dir(root)
            .output();
        let out = match out {
            Ok(out) => out,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Some(
                    "python3 not found: the KB's checks (tools/kb_lint.py, tools/kb_facts.py) need it"
                        .into(),
                );
            }
            Err(e) => return Some(format!("python3 {}: {e}", args[0])),
        };
        for stream in [&out.stdout, &out.stderr] {
            lines.extend(
                String::from_utf8_lossy(stream)
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_string),
            );
        }
        if !out.status.success() {
            return Some(format!(
                "the KB just written fails {}: a bug in viy",
                args[0]
            ));
        }
    }
    None
}

/// `dir` and the folders above it, as relative paths: "kb/facts", "kb".
fn ancestors(dir: &str) -> Vec<&str> {
    let mut out = vec![dir];
    let mut rest = dir;
    while let Some((up, _)) = rest.rsplit_once('/') {
        out.push(up);
        rest = up;
    }
    out
}

fn dedup(mut v: Vec<&str>) -> Vec<&str> {
    v.sort_unstable();
    v.dedup();
    v
}

/// Exists, a dangling link included.
fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn is_link(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
}

#[cfg(test)]
#[path = "kb_tests.rs"]
mod tests;
