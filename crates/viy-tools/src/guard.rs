//! The paths the model must not write, or must not read without a yes
//! (rpctl sandbox.py `protected_reason`, `secret_reason`, `git_hooks_dir`,
//! `workspace_problem`). The file tools and the shell sandbox share this
//! policy.
//!
//! Where rpctl names itself, viy protects both tools' names and code (kb
//! d014): `.rpctl` and `.viy`, `rp` and `viy` at the project root, rp's
//! installed package and viy's own binary and source tree
//! ([`client_paths`]).

use std::io::Read as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// How long `git config` may take, as rpctl's.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

use crate::resolve::resolve;
use crate::secrets::secret_name_reason;

/// Top-level names in the workspace that must never be writable from
/// inside: code that runs outside the sandbox later (the client, its venv,
/// git hooks and config, editor task runners) or config the approval layer
/// trusts. Matched as path components anywhere below the workspace root.
pub const PROTECTED_DIRS: [&str; 7] = [
    ".git", ".venv", "venv", ".rpctl", ".viy", ".vscode", ".idea",
];

/// Files at the workspace or project root that run outside the sandbox.
pub const PROTECTED_ROOT_FILES: [&str; 8] = [
    "rp",
    "viy",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "setup.sh",
    ".envrc",
    "Makefile",
];

/// Home folders that hold keys and the clients' config: never a workspace
/// and never mounted.
pub const PRIVATE_HOME_DIRS: [&str; 3] = [".ssh", ".config", ".local"];

/// Why `path` (inside the workspace) must not be written by the model, or
/// `None`. `clients` are the clients' own code ([`client_paths`]).
pub fn protected_reason(
    path: &Path,
    workspace: &Path,
    project_dir: &Path,
    clients: &[PathBuf],
) -> Option<String> {
    let path = resolve(path);
    let ws = resolve(workspace);
    let proj = resolve(project_dir);
    if clients.iter().any(|c| path.starts_with(c)) {
        return Some("it is the rp client's own code".into());
    }
    let Ok(rel) = path.strip_prefix(&ws) else {
        return Some("it is outside the workspace".into());
    };
    for part in rel.components() {
        let part = part.as_os_str().to_string_lossy();
        if PROTECTED_DIRS.contains(&part.as_ref()) {
            return Some(format!(
                "{part} runs or configures code outside the sandbox"
            ));
        }
    }
    let roots: Vec<&Path> = if ws == proj {
        vec![&ws]
    } else {
        vec![&ws, &proj]
    };
    for root in &roots {
        if git_hooks_dir(root).is_some_and(|hooks| path.starts_with(hooks)) {
            return Some(
                "it is git's hooks directory (core.hooksPath): git runs it outside the sandbox"
                    .into(),
            );
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for root in &roots {
        if path.parent() == Some(*root) {
            if PROTECTED_ROOT_FILES.contains(&name.as_str()) {
                return Some(format!("{name} runs outside the sandbox"));
            }
            if name.ends_with(".egg-info") {
                return Some("it is the client's install metadata".into());
            }
        }
    }
    if name == ".envrc" {
        return Some("direnv runs it when you cd into the directory".into());
    }
    secret_name_reason(&name).map(str::to_string)
}

/// Why reading `path` must be confirmed, or `None`.
pub fn secret_reason(path: &Path) -> Option<&'static str> {
    secret_name_reason(&path.file_name()?.to_string_lossy())
}

/// The directory core.hooksPath names for the repository at `root/.git`,
/// when it lies inside `root`, else `None`. Hooks there run on the host at
/// the next commit, like `.git/hooks`. Only the real .git's config is read,
/// and `git config` runs nothing.
pub fn git_hooks_dir(root: &Path) -> Option<PathBuf> {
    let root = resolve(root);
    if !root.join(".git").is_dir() {
        return None;
    }
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(root.join(".git"))
        .args(["config", "--get", "core.hooksPath"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // As rpctl's timeout=10: a command can make .git/config a FIFO or a
    // link to /dev/zero, which git would read for ever.
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut stdout = Vec::new();
    child.stdout.take()?.read_to_end(&mut stdout).ok()?;
    let value = String::from_utf8_lossy(&stdout).trim().to_string();
    if !status.success() || value.is_empty() {
        return None;
    }
    let value = match value.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map_or_else(|| PathBuf::from(&value), |h| PathBuf::from(h).join(rest)),
        None => PathBuf::from(&value),
    };
    let hooks = resolve(&root.join(value));
    (hooks != root && hooks.starts_with(&root)).then_some(hooks)
}

/// The folder a `.git` file names (`gitdir: <path>`, as a worktree or a
/// submodule has), resolved against the file's folder; `None` for a
/// `.git` that is not such a file (rpctl sandbox.py `gitdir_target`). git
/// takes its hooks and config from there. A FIFO or a device is not read.
pub fn gitdir_target(git: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(git).ok()?;
    if !meta.is_file() || meta.len() > 4096 {
        return None;
    }
    let text = std::fs::read(git).ok()?;
    let text = String::from_utf8_lossy(&text);
    let value = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    if value.is_empty() {
        return None;
    }
    Some(resolve(&git.parent()?.join(value)))
}

/// Why the sandbox cannot meaningfully confine workspace `ws`, or `None`.
pub fn workspace_problem(ws: &Path, home: &Path, rules_path: &Path) -> Option<String> {
    let ws = resolve(ws);
    let home = resolve(home);
    if ws == Path::new("/") || home.starts_with(&ws) {
        return Some(format!(
            "the workspace {} contains your whole home directory",
            ws.display()
        ));
    }
    for p in PRIVATE_HOME_DIRS.iter().map(|n| home.join(n)) {
        if ws.starts_with(&p) {
            return Some(format!(
                "the workspace {} is inside {}",
                ws.display(),
                p.display()
            ));
        }
    }
    let rules = resolve(rules_path);
    if rules.starts_with(&ws) && rules != ws {
        return Some(format!(
            "the approval rules file {} is inside the workspace",
            rules.display()
        ));
    }
    None
}

/// The clients' own code, which the model must never write (kb d014):
/// viy's binary; when it was built in a source tree
/// (`<repo>/target/<profile>/viy`), that tree's sources and every file
/// cargo reads or runs at the next build there (the manifests and lock,
/// `.cargo/`, the toolchain pin, a build script at the root), as rpctl
/// protects its own `pyproject.toml` and `setup.py`; and rp's installed
/// `rpctl` package, found through the `rp` on PATH (a pipx venv, or the
/// folder its editable install maps `rpctl` to). All resolved, as the
/// paths they are compared with.
pub fn client_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        let exe = resolve(&exe);
        if let Some(repo) = source_tree(&exe) {
            for name in [
                "crates",
                "Cargo.toml",
                "Cargo.lock",
                ".cargo",
                "rust-toolchain.toml",
                "rust-toolchain",
                "build.rs",
                "clippy.toml",
                "rustfmt.toml",
            ] {
                out.push(repo.join(name));
            }
        }
        out.push(exe);
    }
    out.extend(rpctl_package().map(|p| resolve(&p)));
    out
}

/// The source tree a binary at `<repo>/target/<profile>/viy` was built in.
pub fn source_tree(exe: &Path) -> Option<&Path> {
    exe.parent()
        .and_then(Path::parent)
        .filter(|t| t.file_name().is_some_and(|n| n == "target"))
        .and_then(Path::parent)
}

fn rpctl_package() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let rp = path
        .to_string_lossy()
        .split(':')
        .filter(|d| d.starts_with('/'))
        .map(|d| Path::new(d).join("rp"))
        .find(|p| p.is_file())?;
    let venv = resolve(&rp).parent()?.parent()?.to_path_buf();
    for dir in std::fs::read_dir(venv.join("lib")).ok()?.flatten() {
        let site = dir.path().join("site-packages");
        if site.join("rpctl").is_dir() {
            return Some(site.join("rpctl"));
        }
        for f in std::fs::read_dir(&site).into_iter().flatten().flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if !(name.starts_with("__editable___rpctl") && name.ends_with("_finder.py")) {
                continue;
            }
            // One unreadable or odd finder does not end the search.
            let Ok(text) = std::fs::read_to_string(f.path()) else {
                continue;
            };
            let Some(at) = text.find("'rpctl': '").map(|i| i + "'rpctl': '".len()) else {
                continue;
            };
            if let Some(end) = text[at..].find('\'') {
                return Some(PathBuf::from(&text[at..at + end]));
            }
        }
    }
    None
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
