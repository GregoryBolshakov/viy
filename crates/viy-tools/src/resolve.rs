//! A path made absolute with its symlinks resolved, as Python's non-strict
//! `Path.resolve()`: the part that exists is resolved, a missing rest is
//! kept as written (flags F08, revised: over `Path::components`, `..` pops,
//! each existing component is checked with `symlink_metadata`, a link is
//! followed, at most 40 links; [`resolve_checked`] refuses a path that
//! needs more).

use std::env;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Links followed before giving up.
const MAX_LINKS: usize = 40;

/// More than [`MAX_LINKS`] symlinks on the way: a loop, or a chain made to
/// outrun the check. What comes back then may still be a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooManyLinks;

/// `path` resolved; after [`MAX_LINKS`] links the rest is kept as written.
/// For comparisons that only need a best effort (is this workspace under
/// `/tmp`?). A guard that then opens the path uses [`resolve_checked`].
pub fn resolve(path: &Path) -> PathBuf {
    walk(path).0
}

/// `path` resolved, or [`TooManyLinks`]. Python 3.10's `Path.resolve()`
/// raises on a loop and follows any chain; this refuses both past 40 links,
/// so a caller never checks a name that is still a link.
pub fn resolve_checked(path: &Path) -> Result<PathBuf, TooManyLinks> {
    match walk(path) {
        (out, false) => Ok(out),
        (_, true) => Err(TooManyLinks),
    }
}

/// The path, and whether the link limit was hit.
fn walk(path: &Path) -> (PathBuf, bool) {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut links = 0;
    let mut capped = false;
    let mut out = PathBuf::from("/");
    // Components still to walk, last first.
    let mut todo: Vec<PathBuf> = components(&absolute);
    todo.reverse();
    while let Some(part) = todo.pop() {
        if part.as_os_str() == ".." {
            out.pop();
            continue;
        }
        let next = out.join(&part);
        let is_link = fs::symlink_metadata(&next).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            out = next;
            continue;
        }
        if links >= MAX_LINKS {
            capped = true;
            out = next;
            continue;
        }
        links += 1;
        let Ok(target) = fs::read_link(&next) else {
            out = next;
            continue;
        };
        if target.is_absolute() {
            out = PathBuf::from("/");
        }
        let mut more = components(&target);
        more.reverse();
        todo.extend(more);
    }
    (out, capped)
}

/// The normal and `..` components; `.` and the root are dropped.
fn components(path: &Path) -> Vec<PathBuf> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(PathBuf::from(n)),
            Component::ParentDir => Some(PathBuf::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
