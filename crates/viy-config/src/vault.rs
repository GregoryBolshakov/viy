//! Reads an open token from the tok vault (`~/.local/bin/tok`; rpctl's
//! `vault.py`).
//!
//! The vault has two tiers: PIN-gated tokens and open ones, which `tok`
//! reads without a PIN. viy only ever asks for open tokens, so it never
//! needs or handles the PIN, and it checks `tok list` first so a PIN-gated
//! name does not leave a DENIED line in the vault's audit log on every chat.

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

    /// `tok <args>`'s stdout, else why it failed.
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let path = self.env.iter().find(|(name, _)| name == "PATH");
        let Some(exe) = find_executable(path.map(|(_, value)| value.as_os_str()), "tok") else {
            return Err("tok is not installed".to_string());
        };
        let child = Command::new(exe)
            .args(args)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("tok failed: {e}"))?;
        let out = wait_with_timeout(child, self.timeout).map_err(|e| format!("tok failed: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(match stderr.trim().lines().next() {
                Some(first) => format!("tok: {first}"),
                None => match out.status.code() {
                    Some(code) => format!("tok: exit {code}"),
                    None => format!("tok: {}", out.status),
                },
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
