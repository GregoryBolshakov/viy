//! Moving auto-running paths out of the workspace (rpctl approve.py
//! `AUTORUN_NAMES`, `autorun_paths`, `is_bare_repo`, `bare_repo_ancestors`,
//! `quarantine`): what git, editors, direnv and the clients run or read
//! when someone opens or cd's into the folder. The sandbox binds the
//! existing ones read-only; one a command creates, or puts in the place of
//! another, is found by comparing [`autorun_snapshot`]s from before and
//! after it ([`autorun_new`]), and moved out. The file tools
//! catch a bare repository assembled one file at a time (HEAD, objects/,
//! refs/) with [`bare_repo_ancestors`].

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use crate::resolve::resolve;

/// Names that run code outside the sandbox when found in a folder: both
/// tools' config folders (kb d014).
pub const AUTORUN_NAMES: [&str; 6] = [".git", ".rpctl", ".viy", ".vscode", ".idea", ".envrc"];

/// How many folders [`autorun_paths`] walks at most, as rpctl.
pub const MAX_DIRS: usize = 200_000;

/// Every auto-running path in workspace `ws`: an entry with one of
/// [`AUTORUN_NAMES`] anywhere (not walked into), a bare repository below
/// the root (not walked into), and git's hooks folder when core.hooksPath
/// names one inside. Links are not followed.
pub fn autorun_paths(ws: &Path) -> BTreeSet<PathBuf> {
    let ws = resolve(ws);
    let mut found = BTreeSet::new();
    let mut stack = vec![ws.clone()];
    let mut n = 0;
    while let Some(dir) = stack.pop() {
        n += 1;
        if n > MAX_DIRS {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut dirs = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name();
            let path = e.path();
            if AUTORUN_NAMES.iter().any(|a| name == *a) {
                found.insert(path);
                continue;
            }
            // A link to a folder is a folder that is not walked into, as
            // os.walk without followlinks.
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(path);
            }
        }
        if dir != ws && is_bare_repo(&dir) {
            found.insert(dir);
            continue;
        }
        stack.extend(dirs.into_iter().rev());
    }
    if let Some(hooks) = crate::guard::git_hooks_dir(&ws)
        && fs::symlink_metadata(&hooks).is_ok()
    {
        found.insert(hooks);
    }
    found
}

/// What is at an auto-running path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A link, its target, and whether that exists: a dangling link
    /// counts as new once its target appears, as the command may have made
    /// it.
    Link {
        target: PathBuf,
        exists: bool,
    },
    Dir,
    File,
    /// A link whose target could not be read.
    Unknown,
}

fn kind(p: &Path) -> Kind {
    match fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => match fs::read_link(p) {
            Ok(target) => Kind::Link {
                target,
                exists: p.exists(),
            },
            Err(_) => Kind::Unknown,
        },
        Ok(m) if m.is_dir() => Kind::Dir,
        _ => Kind::File,
    }
}

/// [`autorun_paths`] with what each one is (rpctl approve.py
/// `autorun_snapshot`), so that a path replaced by something else counts
/// as new: a `.git` that was a link to the user's repository is bound
/// read-only at the link's target, so the link itself can be swapped for a
/// real `.git` with hooks (flags F52).
pub fn autorun_snapshot(ws: &Path) -> BTreeMap<PathBuf, Kind> {
    autorun_paths(ws)
        .into_iter()
        .map(|p| {
            let k = kind(&p);
            (p, k)
        })
        .collect()
}

/// The paths of snapshot `after` that `before` lacks or knew as something
/// else (rpctl approve.py `autorun_new`).
pub fn autorun_new(
    before: &BTreeMap<PathBuf, Kind>,
    after: &BTreeMap<PathBuf, Kind>,
) -> Vec<PathBuf> {
    after
        .iter()
        .filter(|(p, k)| before.get(*p) != Some(*k))
        .map(|(p, _)| p.clone())
        .collect()
}

/// A directory shaped like a bare repository.
pub fn is_bare_repo(d: &Path) -> bool {
    d.join("HEAD").is_file() && d.join("objects").is_dir() && d.join("refs").is_dir()
}

/// Directories between a written file and the workspace root that now form
/// a bare repository.
pub fn bare_repo_ancestors(path: &Path, ws: &Path) -> Vec<PathBuf> {
    let ws = resolve(ws);
    let mut out = Vec::new();
    let path = resolve(path);
    for d in path.ancestors().skip(1) {
        if d == ws || !d.starts_with(&ws) {
            break;
        }
        if is_bare_repo(d) {
            out.push(d.to_path_buf());
        }
    }
    out
}

/// What was moved (workspace-relative, destination), and the first path
/// that could not be, with why.
pub type Moved = (Vec<(String, PathBuf)>, Option<(PathBuf, io::Error)>);

/// Moves `paths` out of workspace `ws` into `dest_root` (the quarantine
/// folder and a timestamp), deepest first so a parent's move cannot strand
/// a child. Stops at the first failure, keeping what was moved before it.
pub fn quarantine(paths: &[PathBuf], ws: &Path, dest_root: &Path) -> Moved {
    let ws = resolve(ws);
    let mut sorted: Vec<&PathBuf> = paths.iter().collect();
    sorted.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    let mut moved = Vec::new();
    for p in sorted {
        if fs::symlink_metadata(p).is_err() {
            continue;
        }
        let rel = p.strip_prefix(&ws).unwrap_or(p).to_path_buf();
        let dest = dest_root.join(&rel);
        let done = match dest.parent() {
            Some(parent) => fs::create_dir_all(parent).and_then(|()| move_path(p, &dest)),
            None => move_path(p, &dest),
        };
        if let Err(e) = done {
            return (moved, Some((p.clone(), e)));
        }
        moved.push((rel.display().to_string(), dest));
    }
    (moved, None)
}

/// A rename, or across filesystems a copy and a removal (shutil.move).
fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(e) if e.raw_os_error() == Some(libc_exdev()) => {
            copy_tree(from, to)?;
            let meta = fs::symlink_metadata(from)?;
            if meta.is_dir() {
                fs::remove_dir_all(from)
            } else {
                fs::remove_file(from)
            }
        }
        other => other,
    }
}

/// EXDEV, "invalid cross-device link".
const fn libc_exdev() -> i32 {
    18
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if meta.is_dir() {
        fs::create_dir_all(to)?;
        for e in fs::read_dir(from)? {
            let e = e?;
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

#[cfg(test)]
#[path = "quarantine_tests.rs"]
mod tests;
