//! Drives the user lends the model with /mount (rpctl's `mounts.py`): a USB
//! stick, a second disk, or any folder outside the workspace.
//!
//! A mount is read-only unless the user types rw and confirms. While it
//! lasts (until /unmount or the end of the chat):
//! - the file tools accept an absolute path inside it;
//! - sandboxed commands see it at its own path, read-only or read-write;
//!   the rest of the host stays absent as before;
//! - the workspace's rules hold inside it: env files are masked, and in a rw
//!   mount .git, .vscode, ... are read-only and new ones are quarantined;
//! - the chat counts as holding outside text (the taint): files on a drive
//!   are as untrusted as a web page, so network commands ask first.
//!
//! Refused outright: / and the system directories, a folder that holds
//! $HOME, the workspace, the clients or their config and state, and anything
//! inside ~/.ssh, ~/.config or ~/.local. Refused rw: a volume that boots
//! something (an EFI partition, an installer stick, a system drive), where a
//! changed file runs at the next boot.

use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::guard::PRIVATE_HOME_DIRS;
use crate::resolve::resolve as resolve_path;
use crate::secrets::is_secret_name;

pub const SYSTEM_DIRS: [&str; 18] = [
    "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/boot", "/proc",
    "/sys", "/dev", "/run", "/var", "/opt", "/snap", "/root", "/srv",
];
/// Where desktops and people mount drives; /mount lists what is mounted
/// here. `/run/media/$USER` (udisks on Fedora and Arch) is the one place
/// under /run.
pub const DRIVE_ROOTS: [&str; 3] = ["/media", "/run/media", "/mnt"];
/// Found at a volume's root, these mean the volume boots something.
pub const BOOT_MARKERS: [&str; 10] = [
    "EFI",
    "boot/grub",
    "boot/grub2",
    "boot/loader",
    "casper",
    "isolinux",
    "syslinux",
    "bootmgr",
    "Windows/System32",
    "etc/fstab",
];
/// Directories not worth walking when looking for secret files to mask.
pub const WALK_SKIP: [&str; 9] = [
    "node_modules",
    ".git",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "target",
    "dist",
];
/// The first words of a note from the client to the model (rpctl
/// sessions.py `CLIENT_NOTE_PREFIX`, viy-agent's sessions).
pub const NOTE_PREFIX: &str = "[rp chat: ";
const SSH_NOTE_PREFIX: &str = "[rp chat: ssh: ";

/// One lent folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub path: PathBuf,
    pub rw: bool,
    /// Env files, masked in the sandbox.
    pub secrets: Vec<PathBuf>,
    /// One walk of the folder.
    pub scan: Duration,
}

impl Mount {
    pub fn new(path: impl Into<PathBuf>, rw: bool) -> Self {
        Self {
            path: path.into(),
            rw,
            secrets: Vec::new(),
            scan: Duration::ZERO,
        }
    }

    pub fn mode(&self) -> &'static str {
        if self.rw { "read-write" } else { "read-only" }
    }
}

/// A row of the mount table: where, what, the filesystem, read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub target: PathBuf,
    pub source: String,
    pub fstype: String,
    pub read_only: bool,
}

/// `'/media/me/USB stick rw'` → `('/media/me/USB stick', true)`. Paths may
/// hold spaces; quotes around them are dropped.
pub fn parse_arg(arg: &str) -> (String, bool) {
    let mut arg = arg.trim().to_string();
    let mut rw = false;
    let words: Vec<&str> = arg.rsplitn(2, char::is_whitespace).collect();
    if words.len() == 2 && matches!(words[0], "rw" | "ro") {
        rw = words[0] == "rw";
        arg = words[1].trim().to_string();
    } else if arg == "rw" || arg == "ro" {
        return (String::new(), arg == "rw");
    }
    let b = arg.as_bytes();
    if b.len() >= 2 && b[0] == b[b.len() - 1] && (b[0] == b'\'' || b[0] == b'"') {
        arg = arg[1..arg.len() - 1].to_string();
    }
    (arg, rw)
}

/// /proc/self/mounts writes space, tab, newline and backslash as `\ooo`.
pub fn unescape(field: &str) -> String {
    String::from_utf8_lossy(&unescape_bytes(field.as_bytes())).into_owned()
}

fn unescape_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = u32::from(b[i + 1] - b'0') * 64
                + u32::from(b[i + 2] - b'0') * 8
                + u32::from(b[i + 3] - b'0');
            out.push(u8::try_from(v).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Everything mounted. Read as bytes, so a mount point that is not UTF-8
/// (a stick's Latin-1 label) keeps its real path and the rest stay listed.
pub fn mount_table() -> Vec<Volume> {
    fs::read("/proc/self/mounts")
        .map(|raw| parse_mount_table(&raw))
        .unwrap_or_default()
}

/// The rows of a mount table's text.
pub fn parse_mount_table(raw: &[u8]) -> Vec<Volume> {
    raw.split(|b| *b == b'\n')
        .filter_map(|line| {
            let f: Vec<&[u8]> = line
                .split(u8::is_ascii_whitespace)
                .filter(|f| !f.is_empty())
                .collect();
            (f.len() >= 4).then(|| Volume {
                target: PathBuf::from(std::ffi::OsString::from_vec(unescape_bytes(f[1]))),
                source: String::from_utf8_lossy(&unescape_bytes(f[0])).into_owned(),
                fstype: String::from_utf8_lossy(f[2]).into_owned(),
                read_only: f[3].split(|b| *b == b',').any(|o| o == b"ro"),
            })
        })
        .collect()
}

/// The mounted volumes under [`DRIVE_ROOTS`].
pub fn drives(table: &[Volume]) -> Vec<&Volume> {
    table
        .iter()
        .filter(|v| {
            DRIVE_ROOTS
                .iter()
                .any(|r| v.target != Path::new(r) && v.target.starts_with(r))
        })
        .collect()
}

/// The folder `arg` names: a path, or the name of a mounted drive (case
/// does not matter, a unique prefix will do).
pub fn resolve(arg: &str, table: &[Volume], home: &Path) -> Result<PathBuf, String> {
    if arg.is_empty() {
        return Err("no drive or folder given".into());
    }
    if arg.starts_with(['/', '~', '.']) {
        let expanded = match arg.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                home.join(rest.trim_start_matches('/'))
            }
            _ => PathBuf::from(arg),
        };
        return Ok(resolve_path(&expanded));
    }
    let rows = drives(table);
    let low = arg.to_lowercase();
    let name = |v: &Volume| {
        v.target
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let exact: Vec<&&Volume> = rows.iter().filter(|v| name(v) == low).collect();
    let hits: Vec<&&Volume> = if exact.is_empty() {
        rows.iter().filter(|v| name(v).starts_with(&low)).collect()
    } else {
        exact
    };
    match hits.as_slice() {
        [one] => Ok(resolve_path(&one.target)),
        [] => Err(format!(
            "no mounted drive is called {} (give a path, or /mount alone to list the drives)",
            py_quote(arg)
        )),
        _ => Err(format!(
            "{} matches several drives: {}",
            py_quote(arg),
            hits.iter()
                .map(|v| v.target.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// A name in quotes, as rp writes it with `{arg!r}` for plain names.
fn py_quote(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// The shortest easy name /mount resolves to the drive at `target`: its
/// first word in lower case ("ubuntu" for "UBUNTU 22_0"), else its whole
/// name, else `None` (two drives share it; only the path will do).
pub fn short_name(target: &Path, table: &[Volume], home: &Path) -> Option<String> {
    let name = target.file_name()?.to_string_lossy().into_owned();
    let first = name
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
        .next()
        .unwrap_or("")
        .to_string();
    let mut tried: Vec<String> = Vec::new();
    for cand in [first, name] {
        let cand = cand.to_lowercase();
        if cand.is_empty() || tried.contains(&cand) {
            continue;
        }
        tried.push(cand.clone());
        if resolve(&cand, table, home).is_ok_and(|p| p == resolve_path(target)) {
            return Some(cand);
        }
    }
    None
}

/// The row of the filesystem `path` is on.
pub fn volume_of<'a>(path: &Path, table: &'a [Volume]) -> Option<&'a Volume> {
    // The deepest; the first of equals, as rp's.
    table
        .iter()
        .filter(|v| path.starts_with(&v.target))
        .fold(None, |best: Option<&Volume>, v| match best {
            Some(b) if b.target.components().count() >= v.target.components().count() => Some(b),
            _ => Some(v),
        })
}

/// `root/rel` if it exists, matching each part without regard to case (vfat
/// and NTFS volumes are written by systems that ignore case).
fn find_ci(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    for part in rel.split('/') {
        let names = fs::read_dir(&cur).ok()?;
        let hit = names
            .flatten()
            .map(|e| e.file_name())
            .find(|n| n.to_string_lossy().to_lowercase() == part.to_lowercase())?;
        cur = cur.join(hit);
    }
    Some(cur)
}

/// Why the volume at `volume_root` boots something, or `None`.
pub fn boot_reason(volume_root: &Path) -> Option<String> {
    BOOT_MARKERS.iter().find_map(|m| {
        find_ci(volume_root, m)
            .map(|hit| format!("it is a boot or system volume ({} is there)", hit.display()))
    })
}

/// Why `path` must not be mounted (rw: read-write), or `None`. `guarded`
/// are paths the mount must neither hold nor sit inside (the workspace, the
/// clients, their config and state).
pub fn problem(
    path: &Path,
    rw: bool,
    guarded: &[PathBuf],
    table: &[Volume],
    home: &Path,
) -> Option<String> {
    let home = resolve_path(home);
    if !path.exists() {
        return Some(format!("{} does not exist", path.display()));
    }
    if !path.is_dir() {
        return Some(format!("{} is not a folder", path.display()));
    }
    if path == Path::new("/") {
        return Some("that is the whole filesystem".into());
    }
    if home.starts_with(path) {
        return Some(format!("it holds your home directory {}", home.display()));
    }
    for name in PRIVATE_HOME_DIRS {
        let private = home.join(name);
        if path.starts_with(&private) {
            return Some(format!("it is inside {}", private.display()));
        }
    }
    let in_run_media = path.starts_with("/run/media");
    for d in SYSTEM_DIRS {
        if path.starts_with(d) && !in_run_media {
            return Some(format!("{d} is a system directory"));
        }
    }
    for g in guarded {
        let g = resolve_path(g);
        if g.starts_with(path) {
            return Some(format!(
                "it holds {}, which the model must not reach this way",
                g.display()
            ));
        }
        if path.starts_with(&g) {
            return Some(format!(
                "it is inside {}, which the model must not reach this way",
                g.display()
            ));
        }
    }
    if rw && let Some(vol) = volume_of(path, table) {
        if vol.read_only {
            return Some(format!(
                "the drive {} itself is mounted read-only",
                vol.target.display()
            ));
        }
        if vol.target != Path::new("/")
            && let Some(why) = boot_reason(&vol.target)
        {
            return Some(format!("{why}; files changed there run at its next boot"));
        }
    }
    None
}

/// Secret files anywhere under `root` (at most `limit`), to be masked with
/// empty files in the sandbox. Symlinked folders are not walked.
pub fn secret_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    secret_files_until(root, limit, &AtomicBool::new(false)).unwrap_or_default()
}

/// [`secret_files`], given up (`None`) once `stop` is set.
pub fn secret_files_until(root: &Path, limit: usize, stop: &AtomicBool) -> Option<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut dirs = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // A link to a folder counts as a folder, as os.walk sorts it,
            // and is not walked into.
            if e.path().is_dir() {
                let link = e.file_type().is_ok_and(|t| t.is_symlink());
                if !link && !WALK_SKIP.contains(&name.as_str()) {
                    dirs.push(e.path());
                }
            } else if is_secret_name(&name) {
                found.push(e.path());
                if found.len() >= limit {
                    return Some(found);
                }
            }
        }
        // Top-down, in the directory's own order, as os.walk.
        stack.extend(dirs.into_iter().rev());
    }
    Some(found)
}

/// A new [`Mount`] for `path`: its secret files and the time one walk
/// takes; `None` when `stop` was set during the walk (rp's Ctrl-C).
pub fn scan(path: &Path, rw: bool, stop: &AtomicBool) -> Option<Mount> {
    let t0 = Instant::now();
    let secrets = secret_files_until(path, 200, stop)?;
    Some(Mount {
        path: path.to_path_buf(),
        rw,
        secrets,
        scan: t0.elapsed(),
    })
}

/// The mount `path` is in.
pub fn containing<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts.iter().find(|m| path.starts_with(&m.path))
}

/// A mount that holds `path` or sits inside it (not the same folder).
pub fn overlap<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts
        .iter()
        .find(|m| m.path != path && (path.starts_with(&m.path) || m.path.starts_with(path)))
}

/// What the model is told, as the first part of the next user message: the
/// mounts are not in the system prompt, whose every change would re-prefill
/// the whole history.
pub fn note(mounts: &[Mount], gone: &[PathBuf]) -> String {
    let paths = |ps: &[PathBuf]| {
        ps.iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let text = if mounts.is_empty() {
        let lead = if gone.is_empty() {
            "no folders are mounted now; ".to_string()
        } else {
            format!("the user unmounted {}; ", paths(gone))
        };
        format!("{lead}absolute paths outside the workspace no longer work.")
    } else {
        let listed = mounts
            .iter()
            .map(|m| format!("{} ({})", m.path.display(), m.mode()))
            .collect::<Vec<_>>()
            .join("; ");
        let write = if mounts.iter().any(|m| m.rw) {
            ", write_file"
        } else {
            ""
        };
        let mut text = format!(
            "the user mounted folders for you to use: {listed}. Reach them by absolute path \
             with read_file, list_dir{write} and run_shell; relative paths still mean the \
             workspace."
        );
        if !gone.is_empty() {
            text.push_str(&format!(" No longer mounted: {}.", paths(gone)));
        }
        text
    };
    format!("{NOTE_PREFIX}{text}]")
}

pub fn is_note(text: &str) -> bool {
    text.starts_with(NOTE_PREFIX) && !text.starts_with(SSH_NOTE_PREFIX)
}

#[cfg(test)]
#[path = "mounts_tests.rs"]
mod tests;
