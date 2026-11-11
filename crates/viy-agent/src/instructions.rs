//! Standing instruction files (AGENTS.md, QWEN.md, CLAUDE.md) pasted into
//! the system prompt on every request: rpctl's `instructions.py`. Files come
//! from viy's config directory, then from every directory between the
//! filesystem root and the workspace, outermost first, so the most specific
//! file sits last, closest to the conversation. They are re-read for each
//! request, so an edit takes effect on the next message.
//!
//! The character budget (INSTRUCTIONS_MAX_CHARS) is spent innermost first:
//! if something has to give, it is the broad, far-away files.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use viy_config::Config;

const DEFAULT_NAMES: &str = "AGENTS.md,QWEN.md,CLAUDE.md";
const DEFAULT_MAX_CHARS: usize = 32000;

/// What ends a file cut to the budget; rpctl's words with viy's name
/// (flags F21, kb d131).
const TRUNCATED: &str = "\n\n[... truncated by viy ...]";

const HEADER: &str = "# Project instructions\n\nThe files below were loaded automatically \
    from this project and its parent directories. They are standing instructions from the \
    user: follow them, and let them override your default behavior. When two of them \
    conflict, the one listed later (more specific to the project) wins. You do not need to \
    read these files again with your tools.";

/// One file as it goes into the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub path: PathBuf,
    pub text: String,
    pub truncated: bool,
}

/// The file names looked for (INSTRUCTION_FILES).
pub fn names(cfg: &Config) -> Vec<String> {
    let raw = cfg
        .get("INSTRUCTION_FILES")
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_NAMES);
    raw.split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

fn max_chars(cfg: &Config) -> usize {
    match cfg.get("INSTRUCTIONS_MAX_CHARS") {
        None => DEFAULT_MAX_CHARS,
        Some(v) => v
            .trim()
            .parse::<i64>()
            .map_or(DEFAULT_MAX_CHARS, |n| usize::try_from(n).unwrap_or(0)),
    }
}

/// The instruction files that apply to `workspace`, outermost first: those
/// in `global_dir`, then each ancestor's from `/` down to the workspace.
/// Links to a file already found (CLAUDE.md -> AGENTS.md) count once.
pub fn find(names: &[String], global_dir: &Path, workspace: &Path) -> Vec<PathBuf> {
    let ws = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut dirs = vec![global_dir.to_path_buf()];
    dirs.extend(
        ws.ancestors()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(Path::to_path_buf),
    );
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        for name in names {
            let path = dir.join(name);
            if !path.is_file() {
                continue;
            }
            let Ok(real) = std::fs::canonicalize(&path) else {
                continue;
            };
            if seen.insert(real) {
                out.push(path);
            }
        }
    }
    out
}

/// The files that apply, within the budget, outermost first. Unreadable or
/// empty files are skipped; bytes that are not UTF-8 become U+FFFD.
pub fn load(cfg: &Config, global_dir: &Path, workspace: &Path) -> Vec<Loaded> {
    let mut budget = max_chars(cfg);
    let mut loaded = Vec::new();
    if budget == 0 {
        return loaded;
    }
    for path in find(&names(cfg), global_dir, workspace).into_iter().rev() {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let mut text = String::from_utf8_lossy(&bytes).trim().to_string();
        if text.is_empty() {
            continue;
        }
        let truncated = text.chars().count() > budget;
        if truncated {
            let kept: String = text.chars().take(budget).collect();
            text = format!("{}{TRUNCATED}", kept.trim_end());
        }
        budget -= budget.min(text.chars().count());
        loaded.push(Loaded {
            path,
            text,
            truncated,
        });
        if budget == 0 {
            break;
        }
    }
    loaded.reverse();
    loaded
}

/// The system prompt's block for `loaded`, or `None` when there is nothing.
pub fn render(loaded: &[Loaded]) -> Option<String> {
    if loaded.is_empty() {
        return None;
    }
    let mut parts = vec![HEADER.to_string()];
    parts.extend(loaded.iter().map(|l| {
        format!(
            "<instructions path=\"{}\">\n{}\n</instructions>",
            l.path.display(),
            l.text
        )
    }));
    Some(parts.join("\n\n"))
}

/// What `/instructions` and a chat's start say about them: off, none found
/// (with the names looked for), or each file with its size.
pub fn report(on: bool, names: &[String], loaded: &[Loaded]) -> Vec<String> {
    if !on {
        return vec!["instruction files: off (/instructions on)".into()];
    }
    if loaded.is_empty() {
        return vec![format!(
            "instruction files: none found ({})",
            names.join(", ")
        )];
    }
    let mut lines = vec!["instruction files (in the system prompt every turn):".to_string()];
    lines.extend(loaded.iter().map(|l| {
        let note = if l.truncated { ", truncated" } else { "" };
        format!(
            "  {}  ({} chars{note})",
            l.path.display(),
            l.text.chars().count()
        )
    }));
    lines
}

#[cfg(test)]
#[path = "instructions_tests.rs"]
mod tests;
