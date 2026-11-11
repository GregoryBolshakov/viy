//! The schemas of the four build tools, `write_file`, `read_file`,
//! `list_dir` and `run_shell`, as rpctl's `BUILD_TOOLS_SANDBOXED` and
//! `BUILD_TOOLS_SANDBOXED_HOST_NET` offer them (chat.py 126-238). Key order
//! is kept: the chat template renders them with `tojson`.
//!
//! When `.git` is the model's own repository ([`crate::sandboxgit`],
//! SANDBOX_GIT), run_shell says so ([`tools_for`]), as rpctl's
//! `BUILD_TOOLS_SANDBOXED_GIT` and `_HOST_NET_GIT` do.

use serde_json::Value;
use serde_json::json;

/// The build tools' names, as rpctl's `BUILD_TOOL_NAMES`.
pub const NAMES: [&str; 4] = ["write_file", "read_file", "list_dir", "run_shell"];

/// The tools a round may run side by side: they only read (rpctl's
/// `PARALLEL_SAFE`).
pub const PARALLEL_SAFE: [&str; 6] = [
    "read_file",
    "list_dir",
    "web_search",
    "fetch_url",
    "explore",
    "find_tool",
];

const SHELL_HEAD: &str = "Run a shell command in the workspace and return its exit code, \
    stdout and stderr. Use this to install dependencies, build, run tests, or start the \
    setup. Commands run in a sandbox: the workspace is writable, the home directory is \
    empty, secret files (.env*, private keys, credentials) are hidden, .git and the tool's \
    own code are read-only, and there is NO network unless you set needs_network. ";

/// What the proxy lets through; not said with SANDBOX_NET=host.
const SHELL_PROXY: &str = "With it, the command reaches the internet only by HTTP(S) \
    through a proxy that is already set in HTTPS_PROXY/HTTP_PROXY, and only package \
    registries (PyPI, npm, crates.io, Go, GitHub, Hugging Face), hosts written in the \
    command and hosts the user allowed; other hosts are blocked and listed in the result. \
    No ssh, ping or UDP. ";

const SHELL_TAIL: &str = "Background processes end when the command ends; /tmp is kept for \
    the rest of this chat. To read a web page use fetch_url if you have it, not curl. \
    Destructive or risky commands are shown to the user, who may decline.";

fn write_file() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "Create or overwrite a file in the workspace with the given text. \
                Use this to lay down code, configs, Dockerfiles, scripts -- the setup you are \
                building. Parent directories are created as needed.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string",
                             "description": "Path relative to the workspace, e.g. app/main.py."},
                    "content": {"type": "string",
                                "description": "The full file contents to write."},
                },
                "required": ["path", "content"],
            },
        },
    })
}

fn read_file() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a text file from the workspace and return its contents. Use \
                this to inspect what already exists before changing it. Large files are cut in \
                the middle; pass start_line/end_line (1-based) to read a part, which comes back \
                with line numbers.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string",
                             "description": "Path relative to the workspace."},
                    "start_line": {"type": "integer",
                                   "description": "Optional first line to return (1-based)."},
                    "end_line": {"type": "integer",
                                 "description": "Optional last line to return (inclusive)."},
                },
                "required": ["path"],
            },
        },
    })
}

fn list_dir() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "list_dir",
            "description": "List the files and folders in a workspace directory.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string",
                             "description": "Directory relative to the workspace; defaults to the root."},
                },
            },
        },
    })
}

fn run_shell(host_net: bool) -> Value {
    let proxy = if host_net { "" } else { SHELL_PROXY };
    json!({
        "type": "function",
        "function": {
            "name": "run_shell",
            "description": format!("{SHELL_HEAD}{proxy}{SHELL_TAIL}"),
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string",
                                "description": "The command line to run, e.g. 'python -m pytest'."},
                    "needs_network": {
                        "type": "boolean",
                        "description": "Set true only when the command must reach the internet \
                            (pip/npm install, git clone, curl). It is then reviewed before it \
                            runs. Leave false for everything else.",
                    },
                },
                "required": ["command"],
            },
        },
    })
}

/// The build tools in rpctl's order; `host_net` for SANDBOX_NET=host,
/// whose run_shell does not describe the proxy.
pub fn tools(host_net: bool) -> Vec<Value> {
    vec![write_file(), read_file(), list_dir(), run_shell(host_net)]
}

/// What run_shell says about .git when it is read-only, and what it says
/// instead when it is the model's own repository (rp's `_with_private_git`).
const GIT_READ_ONLY: &str = ".git and the tool's own code are read-only, ";
const GIT_PRIVATE: &str = "the tool's own code is read-only, .git is your own private \
    repository on this machine (commit and branch freely: it is not the user's repository, \
    which is the read-only remote 'host', and the user imports your branches from it; \
    read_file and write_file see the user's .git, not yours), ";

/// [`tools`], with run_shell telling the model `.git` is its own
/// repository when `private_git`.
pub fn tools_for(host_net: bool, private_git: bool) -> Vec<Value> {
    let mut out = tools(host_net);
    if private_git
        && let Some(desc) = out
            .last_mut()
            .and_then(|t| t.pointer_mut("/function/description"))
        && let Some(text) = desc.as_str()
    {
        // rp asserts it: a wording without it would hide the repository
        debug_assert!(
            text.contains(GIT_READ_ONLY),
            "run_shell's .git wording moved"
        );
        *desc = Value::String(text.replacen(GIT_READ_ONLY, GIT_PRIVATE, 1));
    }
    out
}

#[cfg(test)]
#[path = "build_tests.rs"]
mod tests;
