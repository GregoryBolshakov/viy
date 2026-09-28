//! An [`ExecBackend`] for tests (kb d037): it records every command and
//! plays scripted replies in order. A captured run gets an exit status and
//! output, or fails; a spawn starts a stand-in command on this machine
//! instead, so the code under test has a real pid to watch and signal.

use std::collections::VecDeque;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::Mutex;
use std::sync::PoisonError;

use viy_exec::BoxFuture;
use viy_exec::Child;
use viy_exec::Command;
use viy_exec::ExecBackend;
use viy_exec::Output;
use viy_exec::Spawn;
use viy_exec::SystemExec;

/// What one command gets.
#[derive(Debug, Clone)]
pub enum Reply {
    /// [`ExecBackend::run`] returns this.
    Exit {
        code: i32,
        stdout: String,
        stderr: String,
    },
    /// [`ExecBackend::run`] fails: a missing program, a timeout.
    Fail(io::ErrorKind),
    /// [`ExecBackend::spawn`] starts this instead, with the same streams.
    StandIn(Command),
    /// [`ExecBackend::spawn`] starts nothing and returns a child that has
    /// exited with this code: no fork, so no other test's locked file is
    /// held open by a child between fork and exec.
    Exited(i32),
}

impl Reply {
    /// Exit 0 with this on stdout.
    pub fn stdout(text: impl Into<String>) -> Self {
        Self::Exit {
            code: 0,
            stdout: text.into(),
            stderr: String::new(),
        }
    }

    pub fn exit(code: i32, stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self::Exit {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }
}

/// An exit status with this code.
pub fn exit_status(code: i32) -> ExitStatus {
    ExitStatus::from_raw((code & 0xff) << 8)
}

#[derive(Debug, Default)]
pub struct FakeExec {
    replies: Mutex<VecDeque<Reply>>,
    sent: Mutex<Vec<Command>>,
}

impl FakeExec {
    pub fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            sent: Mutex::default(),
        }
    }

    /// Every command so far, in order.
    pub fn sent(&self) -> Vec<Command> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The replies not yet used.
    pub fn left(&self) -> usize {
        self.replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Panics when the script has no reply left: the code under test ran a
    /// command its test did not expect.
    fn next(&self, command: Command) -> Reply {
        let reply = self
            .replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let Some(reply) = reply else {
            panic!("FakeExec: no reply scripted for {:?}", command.argv());
        };
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(command);
        reply
    }
}

impl ExecBackend for FakeExec {
    fn run(&self, command: Command) -> BoxFuture<'_, io::Result<Output>> {
        let argv = command.argv();
        let reply = self.next(command);
        Box::pin(async move {
            match reply {
                Reply::Exit {
                    code,
                    stdout,
                    stderr,
                } => Ok(Output {
                    status: exit_status(code),
                    stdout: stdout.into_bytes(),
                    stderr: stderr.into_bytes(),
                }),
                Reply::Fail(kind) => Err(io::Error::new(kind, format!("{}: scripted", argv[0]))),
                Reply::StandIn(_) | Reply::Exited(_) => {
                    panic!("FakeExec: a spawn reply scripted for run {argv:?}")
                }
            }
        })
    }

    fn spawn(&self, command: Command, how: Spawn) -> io::Result<Box<dyn Child>> {
        viy_exec::refuse_run_only(&command)?;
        let argv = command.argv();
        match self.next(command) {
            Reply::StandIn(stand_in) => SystemExec.spawn(stand_in, how),
            Reply::Exited(code) => Ok(Box::new(Exited(exit_status(code)))),
            Reply::Fail(kind) => Err(io::Error::new(kind, format!("{}: scripted", argv[0]))),
            Reply::Exit { .. } => panic!("FakeExec: an exit scripted for spawn {argv:?}"),
        }
    }
}

/// A child that never ran, already exited.
struct Exited(ExitStatus);

impl Child for Exited {
    fn id(&self) -> u32 {
        0
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(Some(self.0))
    }

    fn wait(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>> {
        let status = self.0;
        Box::pin(async move { Ok(status) })
    }

    fn terminate(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A live process whose command line starts with `name` and then `args`,
/// as `/proc/<pid>/cmdline` shows it (bash's `exec -a`): a stand-in for
/// someone else's `ssh`. It sleeps until dropped, which ends its whole
/// process group, the `sleep` included.
pub fn named_process(name: &str, args: &[&str]) -> io::Result<NamedProcess> {
    use std::os::unix::process::CommandExt;
    // `; :` keeps sh from replacing itself with sleep.
    let child = std::process::Command::new("bash")
        .arg("-c")
        .arg(r#"exec -a "$0" sh -c 'sleep 600; :' "$@""#)
        .arg(name)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()?;
    let process = NamedProcess { child };
    // Until bash has exec'd, the command line is bash's.
    let want = name.as_bytes();
    for _ in 0..200 {
        let cmdline = std::fs::read(format!("/proc/{}/cmdline", process.id()))?;
        if cmdline.split(|&b| b == 0).next() == Some(want) {
            return Ok(process);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Err(io::Error::other(format!("{name}: never exec'd")))
}

/// See [`named_process`].
#[derive(Debug)]
pub struct NamedProcess {
    child: std::process::Child,
}

impl NamedProcess {
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Waits for it to end; its status.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

impl Drop for NamedProcess {
    fn drop(&mut self) {
        if let Ok(group) = i32::try_from(self.child.id()) {
            // SAFETY: a plain signal to the group this child leads.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        let _ = self.child.wait();
    }
}
