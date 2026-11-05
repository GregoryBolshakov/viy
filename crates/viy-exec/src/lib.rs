//! Processes viy starts, behind a seam (kb d037): the [`ExecBackend`] trait
//! after Codex's (exec-server/src/process.rs), object-safe with boxed
//! futures, its system implementation [`SystemExec`] on tokio, and in
//! viy-testkit a scripted fake. Below viy-pod and viy-agent, which both
//! start processes.
//!
//! Codex's backend streams the output of long-lived processes to a remote
//! client. viy needs less: a command run to its end with its output
//! captured ([`ExecBackend::run`]: `ssh-keygen`, `ssh-keyscan`, a preflight
//! `ssh`), and one that outlives the call ([`ExecBackend::spawn`]: the
//! tunnel's `ssh`), either detached in a session of its own, as Codex
//! starts its daemon (app-server-daemon/src/backend/pid_start.rs), or in
//! the foreground on the terminal.
//!
//! A captured or detached child reads its stdin from `/dev/null` unless the
//! command gives it bytes: rpctl lets these inherit the terminal, where an
//! `ssh` would read what the user types.

pub mod sandboxed;

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

/// A boxed future, as Codex's traits return.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A program and its arguments, run without a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// Bytes for the child's stdin, which is `/dev/null` without them.
    /// [`ExecBackend::run`] only.
    pub stdin: Option<Vec<u8>>,
    /// How long [`ExecBackend::run`] waits before it kills the child and
    /// fails with [`io::ErrorKind::TimedOut`]; no limit without one.
    pub timeout: Option<Duration>,
    /// Set in the child's environment, over this process's.
    pub env: Vec<(OsString, OsString)>,
}

impl Command {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            stdin: None,
            timeout: None,
            env: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I: Into<OsString>>(mut self, args: impl IntoIterator<Item = I>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// The program and its arguments as text, for tests and messages.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }
}

/// What a finished [`ExecBackend::run`] left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Where a spawned child's standard streams go.
#[derive(Debug)]
pub enum Spawn {
    /// In a new session, so the terminal's signals and its closing don't
    /// reach it: stdin and stdout `/dev/null`, stderr to this file.
    Detached { stderr: File },
    /// Detached the same way, with stdout and stderr both to this file:
    /// a process that writes its own log.
    Logged { log: File },
    /// On the terminal, all three streams inherited; Ctrl-C reaches it.
    Terminal,
}

/// A process [`ExecBackend::spawn`] started. Dropping it leaves the process
/// running.
pub trait Child: Send {
    fn id(&self) -> u32;

    /// Its exit status if it has exited, reaping it.
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;

    /// Waits for it to exit.
    fn wait(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>>;

    /// SIGTERM, unless it has already exited.
    fn terminate(&mut self) -> io::Result<()>;
}

/// Starts processes: this machine's, or a test's script.
pub trait ExecBackend: Send + Sync {
    /// Runs `command` to its end, stdout and stderr captured.
    fn run(&self, command: Command) -> BoxFuture<'_, io::Result<Output>>;

    /// Starts `command` and returns at once. A command with stdin bytes or
    /// a timeout is refused: those are [`ExecBackend::run`]'s.
    fn spawn(&self, command: Command, how: Spawn) -> io::Result<Box<dyn Child>>;
}

/// Exit text as messages show it: the code, or the signal that ended it.
pub fn exit_text(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => code.to_string(),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => status.to_string(),
    }
}

/// The processes of this machine, through tokio.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemExec;

impl ExecBackend for SystemExec {
    fn run(&self, command: Command) -> BoxFuture<'_, io::Result<Output>> {
        Box::pin(async move {
            let mut child = tokio::process::Command::new(&command.program)
                .args(&command.args)
                .envs(command.env.iter().map(|(k, v)| (k, v)))
                .stdin(if command.stdin.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // A run that times out is dropped, and its child with it.
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| named(&command.program, e))?;
            let pipe = child.stdin.take();
            let feed = async {
                if let (Some(mut pipe), Some(bytes)) = (pipe, &command.stdin) {
                    // A child that exits without reading all of it is no
                    // error of ours.
                    match pipe.write_all(bytes).await {
                        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(e),
                        _ => {}
                    }
                }
                Ok(())
            };
            let finish = async {
                let (fed, output) = tokio::join!(feed, child.wait_with_output());
                fed?;
                output
            };
            let output = match command.timeout {
                Some(limit) => tokio::time::timeout(limit, finish).await.map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "{} did not finish within {limit:?}",
                            command.program.to_string_lossy()
                        ),
                    )
                })?,
                None => finish.await,
            }
            .map_err(|e| named(&command.program, e))?;
            Ok(Output {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            })
        })
    }

    fn spawn(&self, command: Command, how: Spawn) -> io::Result<Box<dyn Child>> {
        refuse_run_only(&command)?;
        let mut process = tokio::process::Command::new(&command.program);
        process
            .args(&command.args)
            .envs(command.env.iter().map(|(k, v)| (k, v)));
        let detached = match how {
            Spawn::Detached { stderr } => Some((Stdio::null(), Stdio::from(stderr))),
            Spawn::Logged { log } => Some((Stdio::from(log.try_clone()?), Stdio::from(log))),
            Spawn::Terminal => None,
        };
        match detached {
            Some((stdout, stderr)) => {
                process.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
                // SAFETY: setsid is async-signal-safe, and the closure
                // touches nothing else between fork and exec.
                unsafe {
                    process.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            None => {
                process
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            }
        }
        let child = process.spawn().map_err(|e| named(&command.program, e))?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("the child exited before it had a pid"))?;
        Ok(Box::new(SystemChild { child, pid }))
    }
}

struct SystemChild {
    child: tokio::process::Child,
    pid: u32,
}

impl Child for SystemChild {
    fn id(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn wait(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>> {
        Box::pin(self.child.wait())
    }

    fn terminate(&mut self) -> io::Result<()> {
        // Once reaped, the pid may be someone else's.
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        let pid = libc::pid_t::try_from(self.pid).map_err(io::Error::other)?;
        // SAFETY: a plain signal to our own unreaped child.
        if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl fmt::Debug for dyn Child {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Child")
            .field("pid", &self.id())
            .finish_non_exhaustive()
    }
}

/// Stdin bytes and a timeout belong to a run; a spawn that silently
/// dropped them would leave its child waiting on nothing.
pub fn refuse_run_only(command: &Command) -> io::Result<()> {
    if command.stdin.is_some() || command.timeout.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: stdin bytes and a timeout are for run, not spawn",
                command.program.to_string_lossy()
            ),
        ));
    }
    Ok(())
}

/// `error` with the program's name in front, as a missing `ssh-keygen`
/// should say.
fn named(program: &OsStr, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{}: {error}", program.to_string_lossy()),
    )
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
