//! `viy migrate`: copies rp's global config and model profiles to viy's
//! (`~/.config/rpctl/` to `~/.config/viy/`), never moving them, since rp
//! keeps using its own until cutover (kb d006). A file viy already has is
//! kept. Sessions, the toolbox and the per-project input history are copied
//! at cutover (`viy migrate --finish`, ROADMAP phase 8), so viy holds no
//! stale copies of chats rp is still writing (Gregory, 2026-07-14).
//!
//! Each copy is written to a new file beside the target, mode 0600 (kb
//! d040), and linked into place, which fails if the target appeared in the
//! meantime: a copy never replaces anything and is never seen half-written.

use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use crate::Paths;

/// What one run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// (rp's file, viy's copy), in the order copied.
    pub copied: Vec<(PathBuf, PathBuf)>,
    /// (rp's file, viy's file that was already there).
    pub kept: Vec<(PathBuf, PathBuf)>,
}

/// Copies rp's `config` and `profiles/*.env` that viy lacks.
pub fn migrate(paths: &Paths) -> io::Result<Report> {
    let mut report = Report::default();
    let from = paths.rpctl_config_dir();
    let mut pairs = vec![(from.join("config"), paths.config_file())];
    for name in rp_profiles(&from.join("profiles"))? {
        pairs.push((
            from.join("profiles").join(&name),
            paths.profiles_dir().join(&name),
        ));
    }
    for (src, dst) in pairs {
        if !src.is_file() {
            continue;
        }
        if dst.symlink_metadata().is_ok() || !copy_new(&src, &dst)? {
            report.kept.push((src, dst));
        } else {
            report.copied.push((src, dst));
        }
    }
    Ok(report)
}

/// The `*.env` file names in rp's profiles directory, sorted.
fn rp_profiles(dir: &Path) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", dir.display()))),
    };
    let mut names = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "env")
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Copies `src` to `dst` unless `dst` exists; false when it did.
fn copy_new(src: &Path, dst: &Path) -> io::Result<bool> {
    let with_path =
        |path: &Path, e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    let contents = fs::read(src).map_err(|e| with_path(src, e))?;
    let parent = dst.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", dst.display()),
        )
    })?;
    fs::create_dir_all(parent).map_err(|e| with_path(parent, e))?;
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{name}.migrate.{}", std::process::id()));
    // Left behind by an earlier run with the same process ID.
    let _ = fs::remove_file(&tmp);
    let linked = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(&contents))
        .and_then(|()| fs::hard_link(&tmp, dst));
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(with_path(dst, e)),
    }
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
