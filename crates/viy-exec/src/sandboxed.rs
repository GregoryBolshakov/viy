//! The one way viy starts a sandboxed command (flags F14): the child keeps
//! no descriptor but its standard streams and the ones passed on purpose,
//! runs in a session and process group of its own, and the whole group is
//! killed on timeout, when the command ends (`reap_group`) and when the
//! caller goes away (rpctl sandbox.py `run_process`).
//!
//! Python's `subprocess` closes every descriptor but `pass_fds`
//! (`close_fds=True`); Rust's `Command` closes only those marked CLOEXEC,
//! so one this process inherited without the flag (from the shell that
//! started viy, or a library) would be open inside bwrap: a way out of the
//! sandbox. The child marks every descriptor from 3 up CLOEXEC
//! (`close_range`) and then clears the flag on exactly the passed ones,
//! which keep their numbers.

use std::ffi::OsString;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;

/// How long the output of a killed group is waited for.
const DRAIN: Duration = Duration::from_secs(5);

/// What to start.
#[derive(Debug)]
pub struct Spec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    /// The child's whole environment: nothing else is inherited.
    pub env: Vec<(OsString, OsString)>,
    /// Descriptors the child keeps, at the same numbers.
    pub pass_fds: Vec<OwnedFd>,
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// The exit code, or minus the signal that killed it (Python's
    /// `returncode`); `None` when it timed out.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The output, invalid UTF-8 replaced.
    pub stdout: String,
    pub stderr: String,
}

/// A started command. Dropped before [`Running::finish`] or
/// [`Running::abandon`] (the caller cancelled), its group is killed.
#[derive(Debug)]
pub struct Running {
    child: tokio::process::Child,
    pid: i32,
    stdout: Reader,
    stderr: Reader,
    /// The exit status, once waited for.
    status: Option<ExitStatus>,
    done: bool,
}

/// A stream's reader, then what it read: a finished task is never polled
/// again, even when a timeout cancelled the wait halfway.
#[derive(Debug)]
enum Reader {
    Reading(JoinHandle<Vec<u8>>),
    Read(Vec<u8>),
}

impl Reader {
    async fn get(&mut self) -> Vec<u8> {
        if let Self::Reading(h) = self {
            let got = h.await.unwrap_or_default();
            *self = Self::Read(got);
        }
        match self {
            Self::Read(got) => got.clone(),
            Self::Reading(_) => Vec::new(),
        }
    }
}

/// Marks every descriptor from 3 up close-on-exec, then clears the mark on
/// `keep`. Runs in the child between fork and exec: async-signal-safe
/// calls only, no allocation.
fn hygiene(keep: &[i32]) -> io::Result<()> {
    // SAFETY: plain syscalls on descriptor numbers; nothing here allocates
    // or takes a lock.
    unsafe {
        if libc::setsid() < 0 {
            return Err(io::Error::last_os_error());
        }
        let ranged = libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        );
        if ranged != 0 {
            // Kernels before 5.11: every descriptor the limit allows.
            let max = libc::sysconf(libc::_SC_OPEN_MAX);
            let max = if max <= 0 {
                65536
            } else {
                max.min(1 << 20) as i32
            };
            for fd in 3..max {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        for &fd in keep {
            if libc::fcntl(fd, libc::F_SETFD, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

fn read_all<R: tokio::io::AsyncRead + Unpin + Send + 'static>(r: Option<R>) -> Reader {
    match r {
        Some(mut r) => Reader::Reading(tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf).await;
            buf
        })),
        None => Reader::Read(Vec::new()),
    }
}

/// Starts `spec`: stdin `/dev/null`, stdout and stderr captured.
pub fn spawn(spec: Spec) -> io::Result<Running> {
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let keep: Vec<i32> = spec.pass_fds.iter().map(AsRawFd::as_raw_fd).collect();
    // SAFETY: `hygiene` is async-signal-safe and only reads `keep`, which
    // was allocated before the fork.
    unsafe {
        cmd.pre_exec(move || hygiene(&keep));
    }
    let mut child = cmd.spawn()?;
    // The child has its copies; this process's go now.
    drop(spec.pass_fds);
    let pid = child
        .id()
        .and_then(|p| i32::try_from(p).ok())
        .ok_or_else(|| io::Error::other("the child has no pid"))?;
    let stdout = read_all(child.stdout.take());
    let stderr = read_all(child.stderr.take());
    Ok(Running {
        child,
        pid,
        stdout,
        stderr,
        status: None,
        done: false,
    })
}

fn exit_code(status: ExitStatus) -> Option<i32> {
    status.code().or_else(|| status.signal().map(|s| -s))
}

impl Running {
    pub fn id(&self) -> u32 {
        self.pid.unsigned_abs()
    }

    /// SIGKILL to the command's whole group.
    pub fn kill_group(&self) {
        // SAFETY: a signal to a process group this process started.
        unsafe {
            libc::killpg(self.pid, libc::SIGKILL);
        }
    }

    /// The exit status and all the output, however long they take.
    async fn collect(&mut self) -> (io::Result<ExitStatus>, Vec<u8>, Vec<u8>) {
        let status = match self.status {
            Some(s) => Ok(s),
            None => self.child.wait().await,
        };
        if let Ok(s) = status {
            self.status = Some(s);
        }
        let out = self.stdout.get().await;
        let err = self.stderr.get().await;
        (status, out, err)
    }

    /// Waits for the command and its output, at most `timeout`; past it the
    /// group is killed and its output waited for a little longer. With
    /// `reap_group`, whatever it left running in the background is killed
    /// too (the sandbox wants that).
    pub async fn finish(mut self, timeout: Duration, reap_group: bool) -> Finished {
        let first = tokio::time::timeout(timeout, self.collect()).await;
        let (timed_out, (status, out, err)) = match first {
            Ok(done) => (false, done),
            Err(_) => {
                self.kill_group();
                let late = tokio::time::timeout(DRAIN, self.collect()).await;
                (
                    true,
                    late.unwrap_or_else(|_| (Err(io::ErrorKind::TimedOut.into()), vec![], vec![])),
                )
            }
        };
        if reap_group {
            self.kill_group();
        }
        self.done = true;
        Finished {
            exit_code: if timed_out {
                None
            } else {
                status.ok().and_then(exit_code)
            },
            timed_out,
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        }
    }

    /// Kills the group of a command that must not go on (its setup failed)
    /// and returns what it wrote, waited for a little.
    pub async fn abandon(mut self) -> (String, String) {
        self.kill_group();
        let late = tokio::time::timeout(DRAIN, self.collect()).await;
        self.done = true;
        let (out, err) = late.map(|(_, o, e)| (o, e)).unwrap_or_default();
        (
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }
}

impl Drop for Running {
    /// The caller went away (Ctrl-C): the group goes, unless its leader was
    /// already reaped, when the number may belong to someone else by now.
    fn drop(&mut self) {
        let reaped = self.status.is_some() || matches!(self.child.try_wait(), Ok(Some(_)));
        if !self.done && !reaped {
            self.kill_group();
        }
    }
}

#[cfg(test)]
#[path = "sandboxed_tests.rs"]
mod tests;
