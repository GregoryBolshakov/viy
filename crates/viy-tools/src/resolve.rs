//! A path made absolute with its symlinks resolved, as Python's non-strict
//! `Path.resolve()`: the part that exists is resolved, a missing rest is
//! kept as written (flags F08, revised: over `Path::components`, `..` pops,
//! each existing component is checked with `symlink_metadata`, a link is
//! followed, at most 40 links).

use std::env;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Links followed before giving up and keeping the rest as written.
const MAX_LINKS: usize = 40;

pub fn resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut links = 0;
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
        if !is_link || links >= MAX_LINKS {
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
    out
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
