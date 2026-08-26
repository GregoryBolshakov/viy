//! `pod.lock`, held for the whole of up, stop and down so two of them can't
//! race into a duplicate pod or fight over the tunnel: rpctl's `_exclusive`,
//! `_preempt`, `_read_holder`, `_claim` and reap's `_lock_free`. rp and viy
//! take the same flock on the same file (kb d006), and the holder writes
//! `<verb> <pid>` into it, so either tool can say who holds it. The kernel
//! drops the lock when its holder exits, even on a crash or `kill -9`, so a
//! stale file never blocks anything. Messages name the holder's tool from
//! its command line (kb d050).
//!
//! The holder is a process: a chat that runs `/up` in the background
//! (kb d041) holds the lock as the chat's pid, so its own `/down` needs a way
//! to cancel that up in-process rather than through [`PodLock::preempt`]
//! (WP 2.7).

use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use thiserror::Error;
use viy_config::Paths;

use crate::process;
use crate::process::Tool;

/// The pod lock, held until dropped.
#[derive(Debug)]
pub struct PodLock {
    _file: File,
}

/// Whoever holds the lock, as the file names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// rp or viy, from the holder's command line; `None` when it is
    /// neither, which messages call rp, as rp itself would.
    pub tool: Option<Tool>,
    pub verb: String,
    pub pid: u32,
}

#[derive(Debug, Error)]
pub enum LockError {
    /// Another up, stop or down holds the lock.
    #[error(
        "Another {} is already running. Let it finish, or Ctrl-C it, then re-run viy {verb}.",
        who(.holder.as_ref())
    )]
    Busy {
        holder: Option<Holder>,
        verb: String,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl Holder {
    fn tool_name(&self) -> &'static str {
        self.tool.unwrap_or(Tool::Rp).name()
    }
}

fn who(holder: Option<&Holder>) -> String {
    match holder {
        Some(h) => format!("{} {} (pid {})", h.tool_name(), h.verb, h.pid),
        None => "rp or viy command".to_string(),
    }
}

/// What [`PodLock::preempt`] tells the user as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreemptNotice {
    /// About to end the holder.
    Killing(Holder),
    /// The deadline passed with the lock still held.
    GaveUp,
}

impl std::fmt::Display for PreemptNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreemptNotice::Killing(h) => write!(
                f,
                "Killing the running {} {} (pid {}) so it can't start or resume anything.",
                h.tool_name(),
                h.verb,
                h.pid
            ),
            PreemptNotice::GaveUp => {
                f.write_str("Couldn't take the pod lock; tearing down without it.")
            }
        }
    }
}

/// What [`PodLock::preempt`] did.
#[derive(Debug)]
pub struct Preempted {
    /// `None` when the lock could not be had: down goes on without it.
    pub lock: Option<PodLock>,
    /// The verbs of the holders it killed, in order.
    pub killed: Vec<String>,
}

/// How long `down` tries for the lock, how often, and the SIGTERM grace each
/// holder gets: rpctl's 15 s, 0.2 s and 3 s.
const PREEMPT_DEADLINE: Duration = Duration::from_secs(15);
const PREEMPT_POLL: Duration = Duration::from_millis(200);
const KILL_GRACE: Duration = Duration::from_secs(3);

impl PodLock {
    /// The lock for `viy <verb>` (up or stop), refused while anyone holds it.
    pub fn exclusive(paths: &Paths, verb: &str) -> Result<Self, LockError> {
        let path = paths.pod_lock_file();
        let mut file = open_lock_file(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(LockError::Busy {
                    holder: read_holder(&mut file),
                    verb: verb.to_string(),
                });
            }
            Err(TryLockError::Error(e)) => return Err(with_path(&path, e).into()),
        }
        claim(&mut file, verb).map_err(|e| with_path(&path, e))?;
        Ok(Self { _file: file })
    }

    /// The lock for `down`, taken by force: whatever up or stop of rp's or
    /// viy's holds it is killed first, so nothing can create or resume a pod
    /// behind its back. Past the deadline, down carries on without the lock:
    /// tearing the pod down matters more.
    pub fn preempt(paths: &Paths, notice: impl FnMut(PreemptNotice)) -> io::Result<Preempted> {
        preempt_within(paths, PREEMPT_DEADLINE, PREEMPT_POLL, KILL_GRACE, notice)
    }

    /// False while an up, stop or down of either tool holds the lock: reap
    /// never interferes with a boot in progress.
    pub fn is_free(paths: &Paths) -> io::Result<bool> {
        let file = open_lock_file(&paths.pod_lock_file())?;
        Ok(file.try_lock().is_ok())
    }
}

fn preempt_within(
    paths: &Paths,
    deadline: Duration,
    poll: Duration,
    grace: Duration,
    mut notice: impl FnMut(PreemptNotice),
) -> io::Result<Preempted> {
    let path = paths.pod_lock_file();
    let mut file = open_lock_file(&path)?;
    let mut killed = Vec::new();
    let end = Instant::now() + deadline;
    loop {
        match file.try_lock() {
            Ok(()) => {
                claim(&mut file, "down").map_err(|e| with_path(&path, e))?;
                return Ok(Preempted {
                    lock: Some(PodLock { _file: file }),
                    killed,
                });
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(with_path(&path, e)),
        }
        if let Some(holder) = read_holder(&mut file)
            && holder.pid != std::process::id()
            && holder.tool.is_some()
        {
            notice(PreemptNotice::Killing(holder.clone()));
            process::kill(holder.pid, grace);
            killed.push(holder.verb);
        }
        if Instant::now() > end {
            notice(PreemptNotice::GaveUp);
            return Ok(Preempted { lock: None, killed });
        }
        thread::sleep(poll);
    }
}

/// One of rp's lock files (`pod.lock`, `pod.json.lock`), opened for reading
/// and appending, created with its directory if missing, as rp's
/// `open(path, "a+")`. Errors name the file.
pub(crate) fn open_lock_file(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| with_path(dir, e))?;
    }
    OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
        .map_err(|e| with_path(path, e))
}

/// `e`, prefixed with the file it is about.
pub(crate) fn with_path(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

/// `<verb> <pid>` from the lock file; `None` unless it is exactly that, with
/// a pid other than 0 (rp's `if hpid`).
fn read_holder(file: &mut File) -> Option<Holder> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut text).ok()?;
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [verb, pid] = parts[..] else {
        return None;
    };
    if !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pid: u32 = pid.parse().ok().filter(|&p| p != 0)?;
    Some(Holder {
        tool: process::tool_of(&process::cmdline(pid)),
        verb: verb.to_string(),
        pid,
    })
}

/// Writes `<verb> <pid>` over the file's contents.
fn claim(file: &mut File, verb: &str) -> io::Result<()> {
    file.set_len(0)?;
    write!(file, "{verb} {}", std::process::id())?;
    file.flush()
}

#[cfg(test)]
#[path = "lock_tests.rs"]
pub(crate) mod tests;
