//! The project scope: the nearest ancestor of the working directory that
//! holds a `.viy/` or a `.rpctl/` directory, or the working directory itself
//! when none does (rpctl's `config.find_project`, searched as Codex's
//! `discover_project_root`). viy writes only under `.viy/`; of `.rpctl/` it
//! reads the project config and never writes there (kb d039).

use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

/// viy's project marker.
pub const MARKER: &str = ".viy";
/// rp's, still recognised.
pub const RPCTL_MARKER: &str = ".rpctl";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    root: PathBuf,
}

impl Project {
    /// The project `start` is in, with symlinks resolved. At each ancestor
    /// either marker counts, so the nearest marked directory wins.
    pub fn find(start: &Path) -> io::Result<Self> {
        let start = std::fs::canonicalize(start)?;
        let root = start
            .ancestors()
            .find(|dir| has_marker(dir))
            .unwrap_or(&start)
            .to_path_buf();
        Ok(Self { root })
    }

    /// The project a chat opened in `start` works in: [`Project::find`]'s
    /// root, given a `.viy/` as [`Project::init`] makes it when it has
    /// none, so nobody has to run `viy init` first (kb d118). With no
    /// marker from `start` up, that is `start` itself; in an rp project,
    /// its root, `.rpctl/` left alone (kb d039). The root is `find`'s
    /// either way, so the agent's tools reach no further. `home` and the
    /// folders above it are never marked: a marker there would make every
    /// unmarked folder below part of one project. A folder that cannot be
    /// marked (read-only, say) stays as it is and the chat still opens.
    pub fn find_or_init(start: &Path, home: &Path) -> io::Result<Self> {
        let project = Self::find(start)?;
        let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        if project.dir().is_dir() || home.starts_with(project.root()) {
            return Ok(project);
        }
        Ok(match Self::init(project.root()) {
            Ok((marked, _)) => marked,
            Err(_) => project,
        })
    }

    /// Marks `dir` as a project (`viy init`, rpctl's `init_project`):
    /// creates `.viy/` and, when there is none, a `.gitignore` that keeps
    /// the input history out of version control. viy keeps no transcripts
    /// (kb d082), so rp's `transcripts/` is neither made nor ignored.
    /// The project is `dir` itself, not the project it may sit in. Returns
    /// it and whether `.viy/` is new; `.rpctl/` is left alone (kb d039).
    pub fn init(dir: &Path) -> io::Result<(Self, bool)> {
        let project = Self {
            root: std::fs::canonicalize(dir)?,
        };
        let marker = project.dir();
        let created = !marker.exists();
        let with_path = |path: &Path, e: io::Error| {
            io::Error::new(e.kind(), format!("{}: {e}", path.display()))
        };
        if marker.exists() && !marker.is_dir() {
            // The OS's ENOTDIR, as rp got it making its transcripts folder
            // inside the marker.
            let e = io::Error::from_raw_os_error(20);
            return Err(with_path(&marker, e));
        }
        std::fs::create_dir_all(&marker).map_err(|e| with_path(&marker, e))?;
        // create_new: an existing file stays, and a symlink left there (a
        // project folder can come from anywhere) is not written through.
        let gitignore = marker.join(".gitignore");
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&gitignore)
            .and_then(|mut file| file.write_all(b"history\n"));
        match written {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                return Err(with_path(&gitignore, e));
            }
            _ => {}
        }
        Ok((project, created))
    }

    /// Where the agent's tools work, and the base for the paths it shows.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the root holds a marker, read from the disk on each call as
    /// rpctl's `has_project`: a chat that creates `.viy/` makes it true.
    pub fn has_marker(&self) -> bool {
        has_marker(&self.root)
    }

    /// `<root>/.viy`, where viy writes, whether or not it exists yet.
    pub fn dir(&self) -> PathBuf {
        self.root.join(MARKER)
    }

    /// `<root>/.rpctl`, rp's, read only.
    pub fn rpctl_dir(&self) -> PathBuf {
        self.root.join(RPCTL_MARKER)
    }

    /// The input history.
    pub fn history_file(&self) -> PathBuf {
        self.dir().join("history")
    }

    /// Where the chat keeps the input history: none without a `.viy/`, so
    /// sending a message never creates one (kb d118).
    pub fn saved_history(&self) -> Option<PathBuf> {
        self.dir().is_dir().then(|| self.history_file())
    }

    /// The project's config files in layering order: rp's, then viy's on
    /// top. Each may tighten the guarded keys and neither may loosen them,
    /// so a project made stricter for rp is as strict under viy (kb d039).
    pub fn config_files(&self) -> [PathBuf; 2] {
        [self.rpctl_dir().join("config"), self.dir().join("config")]
    }
}

fn has_marker(dir: &Path) -> bool {
    [MARKER, RPCTL_MARKER]
        .iter()
        .any(|marker| dir.join(marker).is_dir())
}

#[cfg(test)]
#[path = "project_tests.rs"]
mod tests;
