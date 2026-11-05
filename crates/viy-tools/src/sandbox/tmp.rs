//! The sandbox's /tmp for one chat (rpctl chat.py `_sandbox_tmp`,
//! `_drop_sandbox_tmp`). It was a fresh tmpfs per command (netaccess kb
//! f006), so a download in one command was gone in the next; now it lasts
//! until the chat ends (netaccess kb f010): a folder under the state
//! folder's `sandbox-tmp/`, bound at /tmp, removed with the chat. Folders
//! a crashed chat left there are removed after a day.

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// How old a left-behind folder must be to go.
const STALE: Duration = Duration::from_secs(86400);

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A new folder `<prefix><unique>` in `parent`, mode 0700 (mkdtemp).
fn make_unique(parent: &Path, prefix: &str) -> io::Result<PathBuf> {
    loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = parent.join(format!("{prefix}{nanos:08x}{n:x}"));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}

/// One chat's /tmp, made on first use.
#[derive(Debug)]
pub struct ChatTmp {
    root: PathBuf,
    dir: Option<PathBuf>,
}

impl ChatTmp {
    /// Folders go under `root` (`<state>/sandbox-tmp`).
    pub fn new(root: PathBuf) -> Self {
        Self { root, dir: None }
    }

    /// The folder, made (and the stale ones removed) the first time.
    pub fn path(&mut self) -> io::Result<&Path> {
        if self.dir.is_none() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.root)?;
            let cutoff = SystemTime::now() - STALE;
            for e in fs::read_dir(&self.root)?.flatten() {
                let stale = e.file_type().is_ok_and(|t| t.is_dir())
                    && e.metadata()
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t < cutoff);
                if stale {
                    let _ = fs::remove_dir_all(e.path());
                }
            }
            let prefix = format!("{}-", std::process::id());
            self.dir = Some(make_unique(&self.root, &prefix)?);
        }
        Ok(self.dir.as_deref().unwrap_or(&self.root))
    }

    /// The folder, no longer this one's to remove: for a caller that
    /// removes it off the async runtime.
    pub fn detach(&mut self) -> Option<PathBuf> {
        self.dir.take()
    }

    /// Removes the folder (the chat ended, or a new one began).
    pub fn remove(&mut self) {
        if let Some(dir) = self.dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

impl Drop for ChatTmp {
    fn drop(&mut self) {
        self.remove();
    }
}

/// A folder in the system temp folder, removed on drop.
#[derive(Debug)]
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A new [`Scratch`], resolved.
pub fn scratch_dir() -> io::Result<Scratch> {
    let dir = make_unique(&std::env::temp_dir(), "viy-sandbox-probe-")?;
    Ok(Scratch(crate::resolve::resolve(&dir)))
}
