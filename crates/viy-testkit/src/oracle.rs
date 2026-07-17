//! Client for the oracle (`tools/oracle.py`): rpctl's own answer for a call,
//! from the snapshot pinned in `tests/rpctl-ref.txt`. One long-running
//! `python3 -I` process per [`Oracle`], spoken to in JSON lines.
//!
//! Values cross the pipe as JSON, with tuples, bytes and paths tagged
//! (`{"$tuple": [...]}`, `{"$bytes": "<base64>"}`, `{"$path": "..."}`) and
//! anything else returned as `{"$repr": "..."}`; see the script's docstring.

use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;

use serde_json::Value;
use serde_json::json;

/// What the Python function did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// It returned this value.
    Returned(Value),
    /// It raised; `kind` is the exception's class name (`SystemExit`
    /// included, which rpctl uses for error messages).
    Raised { kind: String, message: String },
}

/// One call's result, with whatever the function printed.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub outcome: Outcome,
    pub stdout: String,
    pub stderr: String,
}

/// A running oracle process.
pub struct Oracle {
    child: Child,
    /// `None` only while dropping: closing it ends the script's loop.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Oracle {
    /// Starts `python3 -I tools/oracle.py`. The script extracts the pinned
    /// snapshot on first use (`tools/refsnap.sh`), which needs the rpctl
    /// checkout next to this repository.
    pub fn spawn() -> io::Result<Self> {
        let root = repo_root();
        let mut child = Command::new("python3")
            .arg("-I")
            .arg(root.join("tools").join("oracle.py"))
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("oracle: no stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("oracle: no stdout pipe"))?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
        })
    }

    /// The snapshot's short commit and the Python version, as
    /// `{"rpctl_commit": "6bfec96", "python": "3.10.12"}`.
    pub fn hello(&mut self) -> io::Result<Value> {
        let mut reply = self.request(&json!({"fn": "oracle.hello"}))?;
        reply
            .get_mut("ok")
            .map(Value::take)
            .ok_or_else(|| io::Error::other(format!("oracle: unexpected hello reply {reply}")))
    }

    /// Calls `func` (a name on the script's whitelist) with positional `args`
    /// (a JSON array) and keyword `kwargs` (a JSON object).
    pub fn call(&mut self, func: &str, args: Value, kwargs: Value) -> io::Result<Answer> {
        let reply = self.request(&json!({"fn": func, "args": args, "kwargs": kwargs}))?;
        let text = |key: &str| {
            reply
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let outcome = if let Some(value) = reply.get("ok") {
            Outcome::Returned(value.clone())
        } else if let Some(raised) = reply.get("raised") {
            let field = |key: &str| {
                raised
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            Outcome::Raised {
                kind: field("type"),
                message: field("message"),
            }
        } else if let Some(why) = reply.get("refused").and_then(Value::as_str) {
            return Err(io::Error::other(format!("oracle refused {func}: {why}")));
        } else {
            return Err(io::Error::other(format!(
                "oracle: unexpected reply {reply}"
            )));
        };
        Ok(Answer {
            outcome,
            stdout: text("stdout"),
            stderr: text("stderr"),
        })
    }

    fn request(&mut self, req: &Value) -> io::Result<Value> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::other("oracle: closed"))?;
        writeln!(stdin, "{req}")?;
        stdin.flush()?;
        let mut line = String::new();
        if self.stdout.read_line(&mut line)? == 0 {
            return Err(io::Error::other("oracle exited (its stderr is above)"));
        }
        serde_json::from_str(&line).map_err(io::Error::other)
    }
}

impl Drop for Oracle {
    fn drop(&mut self) {
        // EOF, not a kill: the script then removes its temporary HOME.
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// A Python tuple, as the oracle tags it.
pub fn tuple(items: Vec<Value>) -> Value {
    json!({ "$tuple": items })
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

#[cfg(test)]
#[path = "oracle_tests.rs"]
mod tests;
