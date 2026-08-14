//! The `viy` command: argument parsing and dispatch, rpctl's `__main__.py`.
//!
//! Until the clap CLI with rpctl's 19 subcommands lands (WP 1.6), only
//! `--version` is understood; everything else is a usage error. The
//! subcommands are written as their work packages come (`model`, WP 1.3;
//! `init` and `migrate`, WP 1.5) and wired to the parser in WP 1.6.

pub mod init;
pub mod migrate;
pub mod model;

use std::ffi::OsString;
use std::io::Write;

/// Exit status of a usage error, as argparse gives it.
const EXIT_USAGE: i32 = 2;

/// Runs `viy` with `args` (without the program name) and returns the exit
/// status. Output goes to `out` and `err`, so tests can capture it.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let args: Vec<OsString> = args.into_iter().collect();
    match args.as_slice() {
        [flag] if flag == "--version" || flag == "-V" => {
            let version = env!("CARGO_PKG_VERSION");
            if writeln!(out, "viy {version}").is_err() {
                return 1;
            }
            0
        }
        _ => {
            let _ = writeln!(
                err,
                "usage: viy --version\nviy: error: no subcommands yet (ROADMAP phase 1)"
            );
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
