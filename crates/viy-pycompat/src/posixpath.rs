//! CPython 3.10's `posixpath` and `pathlib.Path.resolve()`, on bytes
//! (docs/flags.md F08). Workspace confinement, protected paths, mounts and
//! `files_for_review` stand on `Path(p).resolve()`: non-strict, it resolves
//! the part that exists and keeps the rest, where Rust's `canonicalize`
//! needs the whole path to exist and `Path::components` normalizes
//! differently.
//!
//! Paths are bytes, as the kernel has them; Python's str paths are the same
//! bytes through surrogateescape. Functions that would read the process's
//! working directory take it as `cwd`, which must be what `getcwd()` reports
//! (no symlinks in it); `resolve` passes the real one.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use crate::pyrepr::repr_bytes_path;

const SEP: u8 = b'/';
/// Linux's ELOOP.
const ELOOP: i32 = 40;

fn bytes(p: &Path) -> &[u8] {
    p.as_os_str().as_bytes()
}

fn path(b: Vec<u8>) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(&b))
}

/// `posixpath.isabs(p)`.
pub fn isabs(p: &Path) -> bool {
    bytes(p).first() == Some(&SEP)
}

fn join_bytes(a: &[u8], b: &[u8]) -> Vec<u8> {
    if b.first() == Some(&SEP) {
        b.to_vec()
    } else if a.is_empty() || a.last() == Some(&SEP) {
        [a, b].concat()
    } else {
        [a, b"/", b].concat()
    }
}

/// `posixpath.join(a, b)`.
pub fn join(a: &Path, b: &Path) -> PathBuf {
    path(join_bytes(bytes(a), bytes(b)))
}

fn split_bytes(p: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let i = p.iter().rposition(|&c| c == SEP).map_or(0, |i| i + 1);
    let (head, tail) = p.split_at(i);
    let mut head = head.to_vec();
    if !head.is_empty() && head.iter().any(|&c| c != SEP) {
        while head.last() == Some(&SEP) {
            head.pop();
        }
    }
    (head, tail.to_vec())
}

/// `posixpath.split(p)`: (head, tail).
pub fn split(p: &Path) -> (PathBuf, PathBuf) {
    let (head, tail) = split_bytes(bytes(p));
    (path(head), path(tail))
}

/// `posixpath.dirname(p)`.
pub fn dirname(p: &Path) -> PathBuf {
    split(p).0
}

/// `posixpath.basename(p)`.
pub fn basename(p: &Path) -> PathBuf {
    split(p).1
}

fn normpath_bytes(p: &[u8]) -> Vec<u8> {
    if p.is_empty() {
        return b".".to_vec();
    }
    // POSIX keeps two leading slashes; three or more are one.
    let initial = match p {
        [SEP, SEP, SEP, ..] => 1,
        [SEP, SEP, ..] => 2,
        [SEP, ..] => 1,
        _ => 0,
    };
    let mut comps: Vec<&[u8]> = Vec::new();
    for comp in p.split(|&c| c == SEP) {
        if comp.is_empty() || comp == b"." {
            continue;
        }
        if comp != b".." || (initial == 0 && comps.is_empty()) || comps.last() == Some(&&b".."[..])
        {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let mut out = vec![SEP; initial];
    out.extend(comps.join(&SEP));
    if out.is_empty() {
        out.push(b'.');
    }
    out
}

/// `posixpath.normpath(p)`.
pub fn normpath(p: &Path) -> PathBuf {
    path(normpath_bytes(bytes(p)))
}

fn abspath_bytes(p: &[u8], cwd: &[u8]) -> Vec<u8> {
    if p.first() == Some(&SEP) {
        normpath_bytes(p)
    } else {
        normpath_bytes(&join_bytes(cwd, p))
    }
}

/// `posixpath.abspath(p)` with `cwd` as the working directory.
pub fn abspath_from(p: &Path, cwd: &Path) -> PathBuf {
    path(abspath_bytes(bytes(p), bytes(cwd)))
}

/// `str(PurePosixPath(p))`: repeated slashes and `.` components dropped,
/// `..` kept, exactly two leading slashes kept, `.` for nothing.
pub fn pure_path(p: &Path) -> PathBuf {
    let b = bytes(p);
    let leading = b.iter().take_while(|&&c| c == SEP).count();
    let root: &[u8] = match leading {
        0 => b"",
        2 => b"//",
        _ => b"/",
    };
    let parts: Vec<&[u8]> = b[leading..]
        .split(|&c| c == SEP)
        .filter(|c| !c.is_empty() && *c != b".")
        .collect();
    let mut out = root.to_vec();
    out.extend(parts.join(&SEP));
    if out.is_empty() {
        out.push(b'.');
    }
    path(out)
}

/// The file system as `_joinrealpath` sees it: relative paths against `cwd`.
struct Fs<'a> {
    cwd: &'a [u8],
}

impl Fs<'_> {
    fn at(&self, p: &[u8]) -> PathBuf {
        if p.first() == Some(&SEP) {
            path(p.to_vec())
        } else {
            path(join_bytes(self.cwd, p))
        }
    }

    fn is_link(&self, p: &[u8]) -> bool {
        std::fs::symlink_metadata(self.at(p)).is_ok_and(|m| m.file_type().is_symlink())
    }

    fn readlink(&self, p: &[u8]) -> io::Result<Vec<u8>> {
        Ok(bytes(&std::fs::read_link(self.at(p))?).to_vec())
    }
}

type Seen = std::collections::HashMap<Vec<u8>, Option<Vec<u8>>>;

/// `posixpath._joinrealpath(path, rest, strict=False, seen)`.
fn join_real(
    fs: &Fs<'_>,
    mut path: Vec<u8>,
    rest: &[u8],
    seen: &mut Seen,
) -> io::Result<(Vec<u8>, bool)> {
    let mut rest = rest;
    if rest.first() == Some(&SEP) {
        rest = &rest[1..];
        path = vec![SEP];
    }
    while !rest.is_empty() {
        let (name, after) = match rest.iter().position(|&c| c == SEP) {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        rest = after;
        if name.is_empty() || name == b"." {
            continue;
        }
        if name == b".." {
            if path.is_empty() {
                path = b"..".to_vec();
            } else {
                let (head, tail) = split_bytes(&path);
                path = head;
                if tail == b".." {
                    path = join_bytes(&join_bytes(&path, b".."), b"..");
                }
            }
            continue;
        }
        let newpath = join_bytes(&path, name);
        if !fs.is_link(&newpath) {
            path = newpath;
            continue;
        }
        if let Some(known) = seen.get(&newpath) {
            match known {
                Some(resolved) => {
                    path = resolved.clone();
                    continue;
                }
                // A link met again before it resolved: a loop. Non-strict
                // realpath gives up here and keeps the rest unresolved.
                None => return Ok((join_bytes(&newpath, rest), false)),
            }
        }
        seen.insert(newpath.clone(), None);
        let target = fs.readlink(&newpath)?;
        let (resolved, ok) = join_real(fs, path, &target, seen)?;
        if !ok {
            return Ok((join_bytes(&resolved, rest), false));
        }
        seen.insert(newpath, Some(resolved.clone()));
        path = resolved;
    }
    Ok((path, true))
}

/// `os.path.realpath(p)` (non-strict) with `cwd` as the working directory.
/// Fails only where Python does: reading a link that `lstat` saw.
pub fn realpath_from(p: &Path, cwd: &Path) -> io::Result<PathBuf> {
    let fs = Fs { cwd: bytes(cwd) };
    let (resolved, _) = join_real(&fs, Vec::new(), bytes(p), &mut Seen::new())?;
    Ok(path(abspath_bytes(&resolved, bytes(cwd))))
}

/// Why `Path.resolve()` failed.
#[derive(Debug)]
pub enum ResolveError {
    /// `RuntimeError("Symlink loop from %r")`, naming the path whose `stat`
    /// met ELOOP.
    SymlinkLoop(PathBuf),
    /// An `OSError` from reading a link.
    Os(io::Error),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::SymlinkLoop(p) => {
                write!(f, "Symlink loop from {}", repr_bytes_path(bytes(p)))
            }
            ResolveError::Os(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// `Path(p).resolve()` (strict=False) with `cwd` as the working directory.
pub fn resolve_from(p: &Path, cwd: &Path) -> Result<PathBuf, ResolveError> {
    let resolved = realpath_from(&pure_path(p), cwd).map_err(ResolveError::Os)?;
    // Non-strict realpath does not raise on a loop; pathlib's stat() does.
    match std::fs::metadata(&resolved) {
        Err(e) if e.raw_os_error() == Some(ELOOP) => Err(ResolveError::SymlinkLoop(resolved)),
        _ => Ok(resolved),
    }
}

/// `Path(p).resolve()` against the process's working directory.
pub fn resolve(p: &Path) -> Result<PathBuf, ResolveError> {
    let cwd = std::env::current_dir().map_err(ResolveError::Os)?;
    resolve_from(p, &cwd)
}

#[cfg(test)]
#[path = "posixpath_tests.rs"]
mod tests;
