//! Whether to capture the mouse: not when tmux has `mouse off`, where
//! capturing would take the wheel from tmux's own scrolling for nothing
//! (Codex's `tui/tmux.rs`). Outside tmux, or when the probe says nothing
//! clear, the mouse is captured.

use std::io::Read;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

/// How long tmux may take to answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// tmux's `mouse` option for this pane, when running inside tmux and tmux
/// answers within [`PROBE_TIMEOUT`].
pub fn probe() -> Option<String> {
    std::env::var_os("TMUX")?;
    let mut command = Command::new("tmux");
    command.args(["display-message", "-p"]);
    if let Some(pane) = std::env::var_os("TMUX_PANE") {
        command.arg("-t").arg(pane);
    }
    let mut child = command
        .arg("#{mouse}")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(Some(_)) | Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = String::new();
    child.stdout?.take(4096).read_to_string(&mut output).ok()?;
    Some(output)
}

/// Captures the mouse unless tmux's answer says `mouse` is off.
pub fn mouse_capture(tmux_mouse: Option<&str>) -> bool {
    !matches!(tmux_mouse.map(str::trim), Some("0" | "off"))
}

#[cfg(test)]
#[path = "tmux_tests.rs"]
mod tests;
