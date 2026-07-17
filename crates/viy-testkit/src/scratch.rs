//! Scratch directories for tests that need a real file system: unique,
//! canonical (no symlinks, so a path in it is what `getcwd()` reports), and
//! removed on drop.

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A directory under the system temp dir, removed with its contents on drop.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub fn new(label: &str) -> io::Result<Self> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("viy-test-{label}-{}-{n}", std::process::id()));
        std::fs::create_dir(&dir)?;
        Ok(Self {
            path: std::fs::canonicalize(&dir)?,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
