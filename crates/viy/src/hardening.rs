//! Process hardening, the first thing `main` does (kb d025): Codex's
//! `process-hardening` (codex-rs/process-hardening/src/lib.rs) for Linux,
//! and rpctl 7fe286b's `drop_relative_path_entries`.
//!
//! - Not dumpable (`PR_SET_DUMPABLE` 0): no core dump, and a process of the
//!   same user can neither ptrace viy nor read its `/proc/<pid>/environ` or
//!   memory, which hold the RunPod and vLLM keys during a chat. Children get
//!   their own flag back at exec.
//! - Core file size limit 0, Codex's defence in depth.
//! - `LD_*` variables removed, so no library is preloaded into anything viy
//!   starts.
//! - Empty and relative `PATH` entries removed, for viy and everything it
//!   starts. They name the working directory, which in a project folder may
//!   hold files the model wrote: a `tok`, `bwrap` or `ssh` planted there
//!   would run in place of the real one, with viy's environment. With
//!   nothing left, `/bin:/usr/bin` (Python's `os.defpath`, as rpctl).
//!
//! Called as `main`'s first statement, as Codex's voice-host does, rather
//! than from a `ctor` as its responses-api-proxy: viy runs no code of its
//! own before `main`, and that saves a dependency.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;

/// Codex's exit statuses when a step fails.
const PRCTL_FAILED_EXIT_CODE: i32 = 5;
const SET_RLIMIT_CORE_FAILED_EXIT_CODE: i32 = 7;

/// `PATH` when no absolute entry is left.
pub const DEFAULT_PATH: &str = "/bin:/usr/bin";

/// Hardens this process. Exits if the kernel refuses a step. It changes
/// the environment, so it must run before any other thread starts.
pub fn pre_main() {
    // SAFETY: plain integer arguments; it changes only this process's flag.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        eprintln!(
            "viy: prctl(PR_SET_DUMPABLE, 0) failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(PRCTL_FAILED_EXIT_CODE);
    }
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `no_core` is a valid rlimit that outlives the call.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) } != 0 {
        eprintln!(
            "viy: setrlimit(RLIMIT_CORE) failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(SET_RLIMIT_CORE_FAILED_EXIT_CODE);
    }
    for key in env_keys_with_prefix(std::env::vars_os(), b"LD_") {
        // SAFETY: no other thread exists yet (see above), so nothing reads
        // the environment while it changes.
        unsafe { std::env::remove_var(key) };
    }
    if let Some(path) = std::env::var_os("PATH") {
        // SAFETY: as above.
        unsafe { std::env::set_var("PATH", absolute_path_entries(&path)) };
    }
}

/// The names in `vars` that start with `prefix`, non-UTF-8 ones included.
fn env_keys_with_prefix(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    prefix: &[u8],
) -> Vec<OsString> {
    vars.into_iter()
        .filter_map(|(key, _)| key.as_bytes().starts_with(prefix).then_some(key))
        .collect()
}

/// `path` with only its absolute entries, or [`DEFAULT_PATH`] when it has
/// none.
pub fn absolute_path_entries(path: &OsStr) -> OsString {
    let kept: Vec<&[u8]> = path
        .as_bytes()
        .split(|&b| b == b':')
        .filter(|dir| dir.starts_with(b"/"))
        .collect();
    if kept.is_empty() {
        return OsString::from(DEFAULT_PATH);
    }
    OsString::from_vec(kept.join(&b':'))
}

#[cfg(test)]
#[path = "hardening_tests.rs"]
mod tests;
