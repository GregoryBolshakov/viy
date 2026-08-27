//! Other processes by pid, through `/proc` and signals (rpctl's
//! `pod._cmdline`, `_pid_alive`, `_is_rp_process` and `_kill`): whether a pid
//! is alive, whether it is rp or viy, and ending it.

use std::path::Path;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// The tool a process is, from its command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Rp,
    Viy,
}

impl Tool {
    /// The command's name, as messages give it.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Rp => "rp",
            Tool::Viy => "viy",
        }
    }
}

/// `/proc/<pid>/cmdline` split at its NULs; empty when it can't be read.
pub fn cmdline(pid: u32) -> Vec<Vec<u8>> {
    let Ok(raw) = std::fs::read(Path::new("/proc").join(pid.to_string()).join("cmdline")) else {
        return Vec::new();
    };
    let raw = raw.strip_suffix(b"\0").unwrap_or(&raw);
    raw.split(|&b| b == 0).map(<[u8]>::to_vec).collect()
}

/// Which tool a command line is, by rpctl's own test (`_is_rp_process`):
/// rp or viy when an argument ends in `/rp` or `/viy`, is `rp` or `viy`, or
/// names `rpctl`; neither otherwise. Of the two, viy when the program itself
/// is named viy or nothing names rp.
pub fn tool_of(argv: &[Vec<u8>]) -> Option<Tool> {
    let named = |a: &[u8], name: &[u8]| {
        a == name || (a.ends_with(name) && a[..a.len() - name.len()].ends_with(b"/"))
    };
    let rp = |a: &Vec<u8>| named(a, b"rp") || a.windows(5).any(|w| w == b"rpctl");
    let viy = |a: &Vec<u8>| named(a, b"viy");
    if argv.first().is_some_and(viy) || (argv.iter().any(viy) && !argv.iter().any(rp)) {
        Some(Tool::Viy)
    } else {
        argv.iter().any(rp).then_some(Tool::Rp)
    }
}

/// True if `pid` is a live process this user may signal. `kill(pid, 0)`
/// alone also succeeds on a zombie, which is exactly what a dead ssh child
/// of a running `up` is, so the state in `/proc/<pid>/stat` decides.
pub fn pid_alive(pid: u32) -> bool {
    let Some(raw) = raw_pid(pid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only checks the pid.
    if unsafe { libc::kill(raw, 0) } != 0 {
        return false;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    let after_name = stat.rfind(')').map_or("", |i| &stat[i + 1..]);
    !matches!(after_name.split_whitespace().next(), Some("Z" | "X"))
}

/// SIGTERM, then SIGKILL if it hasn't gone after `grace`; gives up on a pid
/// that is gone or not this user's.
pub fn kill(pid: u32, grace: Duration) {
    let Some(raw) = raw_pid(pid) else {
        return;
    };
    for (signal, wait) in [
        (libc::SIGTERM, grace),
        (libc::SIGKILL, Duration::from_secs(2)),
    ] {
        // SAFETY: a plain signal to one positive pid.
        if unsafe { libc::kill(raw, signal) } != 0 {
            return;
        }
        let end = Instant::now() + wait;
        while Instant::now() < end {
            if !pid_alive(pid) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// `pid` as `kill(2)` takes it, if it names one process: 0 and the negative
/// values signal groups.
fn raw_pid(pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid).ok().filter(|&p| p > 0)
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
