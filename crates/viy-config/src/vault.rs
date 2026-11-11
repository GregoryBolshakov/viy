//! Reads an open token from the tok vault (`~/.local/bin/tok`; rpctl's
//! `vault.py`).
//!
//! The vault has two tiers: PIN-gated tokens and open ones, which `tok`
//! reads without a PIN. viy asks for open tokens only, checking `tok list`
//! first so a PIN-gated name does not leave a DENIED line in the vault's
//! audit log on every chat. The one exception is the model's own git
//! repository (`viy_tools::sandboxgit`), which may take a PIN the user types:
//! [`Tok::get`] and [`Tok::add`] hand it to tok in `TOK_PIN`, never in argv,
//! where other users could read it.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// What `tok list` prints after the name of an open token.
pub const OPEN_MARK: &str = "open (no PIN to read)";

const TIMEOUT: Duration = Duration::from_secs(10);

/// `tok` and the environment it runs in.
#[derive(Debug, Clone)]
pub struct Tok {
    env: Vec<(OsString, OsString)>,
    timeout: Duration,
}

impl Tok {
    /// `tok` as this process's PATH finds it.
    pub fn from_env() -> Self {
        Self::with_env(std::env::vars_os())
    }

    /// `tok` as `env`'s PATH finds it, run with `env` minus TOK_PIN.
    pub fn with_env(env: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self {
            env: env
                .into_iter()
                .filter(|(name, _)| name != "TOK_PIN")
                .collect(),
            timeout: TIMEOUT,
        }
    }

    /// The value of the open token `name`, else why there is none.
    pub fn open_token(&self, name: &str) -> Result<String, String> {
        let list = self.run(&["list"])?;
        let Some(line) = list
            .lines()
            .find(|line| line.split_whitespace().next() == Some(name))
        else {
            return Err(format!("the vault has no token named {name}"));
        };
        if !line.contains(OPEN_MARK) {
            return Err(format!(
                "{name} is PIN-gated in the vault; make it readable without the PIN with: tok open {name} --pin=<PIN>"
            ));
        }
        let value = self.run(&["get", name])?.trim().to_string();
        if value.is_empty() {
            return Err(format!("{name} is empty in the vault"));
        }
        Ok(value)
    }

    /// `name`'s tier in `tok list`: `Some(true)` open, `Some(false)`
    /// PIN-gated, `None` when there is no such token (or no tok).
    pub fn state(&self, name: &str) -> Option<bool> {
        let list = self.run(&["list"]).ok()?;
        list.lines()
            .find(|line| line.split_whitespace().next() == Some(name))
            .map(|line| line.contains(OPEN_MARK))
    }

    /// `name`'s value, with `pin` for a PIN-gated token; never printed.
    /// An error is tok's last stderr line, as rpctl's sandboxgit reports it.
    pub fn get(&self, name: &str, pin: Option<&str>) -> Result<String, String> {
        let value = self
            .run_with(&["get", name], pin, None)
            .map_err(TokFailure::last_line)?
            .trim()
            .to_string();
        if value.is_empty() {
            return Err(format!("{name} is empty in the vault"));
        }
        Ok(value)
    }

    /// Adds `name` with `value` on tok's stdin; `open`: readable without
    /// the PIN. Adding always needs the PIN.
    pub fn add(&self, name: &str, value: &str, open: bool, pin: &str) -> Result<(), String> {
        let mut args = vec!["add", name];
        if open {
            args.push("--open");
        }
        self.run_with(&args, Some(pin), Some(value))
            .map(|_| ())
            .map_err(TokFailure::last_line)
    }

    /// The same tok with `timeout` per call (rpctl's sandboxgit gives
    /// `tok get` and `tok add` 60 s, `tok list` 30 s).
    pub fn timed(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `tok <args>`'s stdout, else why it failed.
    fn run(&self, args: &[&str]) -> Result<String, String> {
        self.run_with(args, None, None)
            .map_err(TokFailure::first_line)
    }

    /// `tok <args>` with `pin` in `TOK_PIN` and `input` on stdin.
    fn run_with(
        &self,
        args: &[&str],
        pin: Option<&str>,
        input: Option<&str>,
    ) -> Result<String, TokFailure> {
        let path = self.env.iter().find(|(name, _)| name == "PATH");
        let Some(exe) = find_executable(path.map(|(_, value)| value.as_os_str()), "tok") else {
            return Err(TokFailure::Plain("tok is not installed".to_string()));
        };
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(pin) = pin {
            cmd.env("TOK_PIN", pin);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| TokFailure::Plain(format!("tok failed: {e}")))?;
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            use std::io::Write;
            // tok reads all of it before it answers; a short value fits the pipe
            let _ = stdin.write_all(text.as_bytes());
        }
        let out = wait_with_timeout(child, self.timeout)
            .map_err(|e| TokFailure::Plain(format!("tok failed: {e}")))?;
        if !out.status.success() {
            return Err(TokFailure::Exited {
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                code: out.status.code(),
                status: out.status.to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Why a tok call failed, before it is put into words.
enum TokFailure {
    /// It did not run, or ran over.
    Plain(String),
    Exited {
        stderr: String,
        code: Option<i32>,
        status: String,
    },
}

impl TokFailure {
    /// `tok: <first stderr line>` (the open tokens' reading).
    fn first_line(self) -> String {
        match self {
            Self::Plain(s) => s,
            Self::Exited {
                stderr,
                code,
                status,
            } => match stderr.trim().lines().next() {
                Some(first) => format!("tok: {first}"),
                None => match code {
                    Some(code) => format!("tok: exit {code}"),
                    None => format!("tok: {status}"),
                },
            },
        }
    }

    /// The last non-empty stderr line, else `exit <code>` (rpctl
    /// sandboxgit's `_last`).
    fn last_line(self) -> String {
        match self {
            Self::Plain(s) => s,
            Self::Exited {
                stderr,
                code,
                status,
            } => stderr
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map_or_else(
                    || code.map_or(status, |c| format!("exit {c}")),
                    str::to_string,
                ),
        }
    }
}

/// The first executable file called `name` in the absolute directories of
/// `path` (a `PATH` value), as `shutil.which` finds one. Empty and relative
/// entries name the working directory, which in a project folder may hold
/// a program the model wrote, so they are skipped, as rpctl's main drops
/// them from PATH (7fe286b).
pub fn find_executable(path: Option<&OsStr>, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|file| {
            file.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// Waits for `child` and for its output, both within `timeout`; kills the
/// child when it runs over. Output still held open after the child exits
/// (by a process it left behind) counts against the same deadline.
pub fn wait_with_timeout(mut child: Child, timeout: Duration) -> io::Result<Output> {
    let deadline = Instant::now() + timeout;
    let timed_out = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out after {} seconds", timeout.as_secs()),
        )
    };
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let status = loop {
        let exited = child.try_wait();
        if let Ok(Some(status)) = exited {
            break status;
        }
        if exited.is_err() || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(exited.err().unwrap_or_else(timed_out));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let collect = |rx: mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| timed_out())
    };
    Ok(Output {
        status,
        stdout: collect(stdout)?,
        stderr: collect(stderr)?,
    })
}

#[cfg(test)]
#[path = "vault_tests.rs"]
mod tests;
