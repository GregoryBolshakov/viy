//! Moving auto-running paths out of the workspace (rpctl approve.py
//! `is_bare_repo`, `bare_repo_ancestors`, `quarantine`). So far the part
//! the file tools need: a bare git repository assembled one file at a time
//! (HEAD, objects/, refs/), which git treats as a repository, running its
//! config, when someone cd's into it. The walk after every sandboxed
//! command (`autorun_paths`) comes with the sandbox (WP 4.7).

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use crate::resolve::resolve;

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

/// Moves `paths` out of workspace `ws` into `dest_root` (the quarantine
/// folder and a timestamp), deepest first so a parent's move cannot strand
/// a child. Returns `(workspace-relative, destination)` of each moved.
pub fn quarantine(
    paths: &[PathBuf],
    ws: &Path,
    dest_root: &Path,
) -> io::Result<Vec<(String, PathBuf)>> {
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
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        move_path(p, &dest)?;
        moved.push((rel.display().to_string(), dest));
    }
    Ok(moved)
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
