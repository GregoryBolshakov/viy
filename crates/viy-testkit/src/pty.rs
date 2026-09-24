//! A program under a pseudo-terminal of a fixed size, for UI scenarios
//! (ui-plan 13): keys in through the master, everything the program writes
//! recorded by a thread and read back through a `vt100` emulator with a
//! [`Recorder`]. Codex's `utils/pty` does this with portable-pty; libc's
//! `openpty` is enough for Linux and adds no dependency.

use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::ops::Deref;
use std::ops::DerefMut;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use crate::screen::Feed;
use crate::screen::Recorder;
use crate::screen::dump;
use crate::screen::parser;

/// How long [`Pty::wait_for`] waits by default.
pub const WAIT: Duration = Duration::from_secs(20);

pub struct Pty {
    master: File,
    slave: File,
    out: Arc<Mutex<Vec<u8>>>,
    /// The screen so far, fed only the bytes that arrived since.
    emulator: Mutex<Emulator>,
}

struct Emulator {
    parser: vt100::Parser<Recorder>,
    feed: Feed,
    /// Bytes of `out` already fed.
    fed: usize,
}

/// A child on the terminal, killed and reaped when dropped while it still
/// runs, so a failed test leaves no program behind in raw mode.
pub struct Spawned(Child);

impl Deref for Spawned {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for Spawned {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

impl Pty {
    /// A pty of `rows` x `cols`, its output recorded from now on.
    pub fn open(rows: u16, cols: u16) -> io::Result<Self> {
        let (mut master, mut slave) = (-1, -1);
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty writes two descriptors and reads `size`; no name
        // buffer and no termios are passed.
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both descriptors are new and owned by nobody else.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut reader = master.try_clone()?;
        let sink = Arc::clone(&out);
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            // EIO once no slave is open: after the `Pty` is dropped and
            // every child has exited.
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(&buf[..n]);
            }
        });
        Ok(Self {
            master,
            slave,
            out,
            emulator: Mutex::new(Emulator {
                parser: parser(rows, cols),
                feed: Feed::default(),
                fed: 0,
            }),
        })
    }

    /// The terminal side, as a shell would hold it.
    pub fn slave(&self) -> &File {
        &self.slave
    }

    /// Runs `command` on the terminal: stdin, stdout and stderr, in a
    /// process group of its own, without tmux's environment.
    pub fn spawn(&self, command: &mut Command) -> io::Result<Spawned> {
        command
            .env_remove("TMUX")
            .stdin(Stdio::from(self.slave.try_clone()?))
            .stdout(Stdio::from(self.slave.try_clone()?))
            .stderr(Stdio::from(self.slave.try_clone()?))
            .process_group(0)
            .spawn()
            .map(Spawned)
    }

    /// Types `bytes`, as keys or a paste.
    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let mut master = &self.master;
        master.write_all(bytes)?;
        master.flush()
    }

    /// Everything written to the terminal so far.
    pub fn bytes(&self) -> Vec<u8> {
        self.out
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Runs `f` on the screen those bytes make.
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Parser<Recorder>) -> R) -> R {
        let mut emulator = self.emulator.lock().unwrap_or_else(PoisonError::into_inner);
        let new = {
            let out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
            out[emulator.fed..].to_vec()
        };
        emulator.fed += new.len();
        let Emulator { parser, feed, .. } = &mut *emulator;
        feed.process(parser, &new);
        f(&emulator.parser)
    }

    /// Waits until `done` holds for the screen, for at most [`WAIT`];
    /// panics with `what` and the screen otherwise.
    pub fn wait_for(&self, what: &str, done: impl Fn(&vt100::Screen) -> bool) {
        let end = Instant::now() + WAIT;
        loop {
            let shown = self.with_screen(|p| (!done(p.screen())).then(|| dump(p.screen())));
            let Some(shown) = shown else {
                return;
            };
            assert!(
                Instant::now() < end,
                "timed out waiting for {what}:\n{shown}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether the terminal is in the shell's mode: line editing and echo
    /// on.
    pub fn cooked(&self) -> bool {
        // SAFETY: an all-zero termios is valid storage for tcgetattr to fill.
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a live descriptor and a termios to write.
        if unsafe { libc::tcgetattr(self.slave.as_raw_fd(), &mut termios) } != 0 {
            return false;
        }
        termios.c_lflag & (libc::ICANON | libc::ECHO) == libc::ICANON | libc::ECHO
    }
}

/// Waits for `child` to exit, for at most [`WAIT`]; kills it otherwise.
pub fn wait_exit(child: &mut Spawned) -> io::Result<std::process::ExitStatus> {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= end {
            child.kill()?;
            return Err(io::Error::other("the child did not exit; killed"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
