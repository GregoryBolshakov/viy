//! Replacing a file in one step, as Codex's `write_atomically` does: the
//! config and profile writes (kb d040) and rp's shared `pod.json`.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// Replaces `path` with `contents` in one step: written to a new file beside
/// the target, mode 0600, then renamed over it. A symlink is followed to the
/// file it points to, which is replaced, so the link stays.
pub fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    let target = symlink_target(path);
    let with_path = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", target.display()));
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", target.display()),
        )
    })?;
    fs::create_dir_all(parent).map_err(with_path)?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    // Unique per call, so two threads writing one file never share a
    // temporary; one left behind by an earlier process with the same ID and
    // count is removed first.
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.tmp.{}.{n}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .and_then(|()| fs::rename(&tmp, &target));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.map_err(with_path)
}

/// Where a chain of symlinks starting at `path` ends; `path` itself when it
/// is no symlink or the chain loops.
fn symlink_target(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    let mut seen = HashSet::new();
    while let Ok(link) = fs::read_link(&current) {
        if !seen.insert(current.clone()) {
            return path.to_path_buf();
        }
        current = match current.parent() {
            Some(dir) => dir.join(link),
            None => link,
        };
    }
    current
}

#[cfg(test)]
#[path = "atomic_tests.rs"]
mod tests;
