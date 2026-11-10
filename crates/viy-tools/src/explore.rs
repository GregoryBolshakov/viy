//! The schemas of the explore workflow (rpctl workflows.py `EXPLORE_TOOL`,
//! `HELPER_READ_FILE`, `HELPER_LIST_DIR`, `HELPER_SHELL`) and its helpers'
//! system prompt and notices. Key order is kept: the chat template renders
//! the schemas with `tojson`. The workflow itself runs in viy-agent's chat.

use serde_json::Value;
use serde_json::json;

/// `explore`, which the main model calls.
pub fn explore_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "explore",
            "description": "Investigate the workspace with read-only helpers that run in \
                parallel. Give 1-4 independent questions; each helper reads files and runs \
                read-only commands on its own, then returns a short report citing file:line. \
                Use it when answering needs reading many files or several areas of the \
                project (e.g. one question per module). It is faster than reading them \
                yourself and keeps your context small. Helpers cannot change anything; do \
                edits yourself afterwards.",
            "parameters": {
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": "1-4 independent questions, one per helper.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {
                                    "type": "string",
                                    "description": "What this helper must find out. Self-contained: it does not see this chat."},
                                "paths": {
                                    "type": "array", "items": {"type": "string"},
                                    "description": "Optional starting files or directories."},
                            },
                            "required": ["question"],
                        },
                    },
                },
                "required": ["tasks"],
            },
        },
    })
}

/// A helper's system prompt.
pub const HELPER_SYSTEM: &str = "You are a read-only research helper inside a coding agent. \
You get one question about the project in the workspace. Find the answer with your tools: \
read_file, list_dir and run_shell (read-only: grep, find, sed -n, wc, git log; the workspace \
is mounted read-only and there is no network). Read only what you need. Then reply with a \
short report: the answer first, then the evidence as file:line references or command output, \
then anything you could not determine. Do not suggest edits unless the question asks. Do not \
repeat a tool call you already made. Your report is all the asker will see; keep it under \
400 words.";

/// The helpers' tools: read_file and list_dir, and run_shell when the
/// sandbox works.
pub fn helper_tools(shell: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a text file from the workspace. Optional 1-based line range.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Path relative to the workspace."},
                        "start_line": {"type": "integer"},
                        "end_line": {"type": "integer"},
                    },
                    "required": ["path"],
                },
            },
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List a workspace directory.",
                "parameters": {"type": "object",
                               "properties": {"path": {"type": "string"}}},
            },
        }),
    ];
    if shell {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "run_shell",
                "description": "Run a read-only shell command in the workspace (grep -rn, find, \
                    sed -n, wc, git log). Nothing can be written; no network.",
                "parameters": {"type": "object",
                               "properties": {"command": {"type": "string"}},
                               "required": ["command"]},
            },
        }));
    }
    tools
}

pub const BUDGET_NOTICE: &str = "[rpctl: your tool budget is used up. Do not call any more \
tools. Write your report now from what you have, and say what you could not verify.]";
pub const EMPTY_NOTICE: &str = "[rpctl: your last reply had no report. Write the report now \
from what you already found. No more tools.]";

/// The note on explore's result.
pub const RESULT_NOTE: &str = "Helpers only read files; nothing was changed. Each report cites \
file:line evidence. Answer from these reports; do not re-read the same files. Read a file \
yourself only to check one specific citation you doubt, or to cover what a report says it \
could not determine.";

/// `(first, last)` when read_file asked for a line range (1-based, within
/// `n_lines`), else `None` (rp's `line_span`): start_line/end_line, or the
/// offset/limit spelling models also use.
pub fn line_span(args: &serde_json::Map<String, Value>, n_lines: usize) -> Option<(usize, usize)> {
    let num = |k: &str| -> Option<i64> {
        match args.get(k)? {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
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
        b = Some(a.unwrap_or(1) + lim - 1);
    }
    if a.is_none() && b.is_none() {
        return None;
    }
    let n = n_lines as i64;
    let mut a = a.unwrap_or(1).max(1);
    let mut b = b.unwrap_or(n).min(n);
    if a > b {
        a = a.min(n).max(1);
        b = a;
    }
    Some((a as usize, b as usize))
}

#[cfg(test)]
#[path = "explore_tests.rs"]
mod tests;
