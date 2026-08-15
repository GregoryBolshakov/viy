//! The `viy` command: argument parsing and dispatch, rpctl's `__main__.py`.
//!
//! The parser has all of rpctl's 19 subcommands (WP 1.6); those whose work
//! package has not come yet say so and exit 1.

pub mod cli;
pub mod hardening;
pub mod init;
pub mod migrate;
pub mod model;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use viy_config::Paths;
use viy_config::Project;

use crate::cli::Command;

/// Runs `viy` with `args` (without the program name) and returns the exit
/// status: 0, 1 when the command fails (its message on `err`, as rpctl's
/// `SystemExit`), 2 for a usage error. Output goes to `out` and `err`, so
/// tests can capture it.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let cli = match cli::parse(args) {
        Ok(cli) => cli,
        // Usage errors, and --help and --version, which clap reports the
        // same way.
        Err(e) => {
            let text = e.render().to_string();
            let written = if e.use_stderr() {
                err.write_all(text.as_bytes())
            } else {
                out.write_all(text.as_bytes())
            };
            return match written {
                Ok(()) => e.exit_code(),
                Err(_) => 1,
            };
        }
    };
    match dispatch(&cli.command, out) {
        Ok(()) => 0,
        Err(msg) => {
            let _ = writeln!(err, "{msg}");
            1
        }
    }
}

fn dispatch(command: &Command, out: &mut dyn Write) -> Result<(), String> {
    let not_ported = |phase: u8| {
        Err(format!(
            "viy {}: not ported yet (ROADMAP phase {phase})",
            command.name()
        ))
    };
    let paths = || Paths::from_env().map_err(|e| e.to_string());
    let cwd = || -> Result<PathBuf, String> {
        std::env::current_dir().map_err(|e| format!("the working directory: {e}"))
    };
    match command {
        Command::Init => init::cmd_init(&paths()?, &cwd()?, out),
        Command::Migrate => migrate::cmd_migrate(&paths()?, out),
        Command::Model(args) => {
            let paths = paths()?;
            let project = Project::find(&cwd()?).map_err(|e| e.to_string())?;
            let cfg = viy_config::config::load(&paths, &project).map_err(|e| e.to_string())?;
            model::cmd_model(&cfg, &paths, args, out)
        }
        Command::Up { .. }
        | Command::Status
        | Command::Boots(_)
        | Command::Logs { .. }
        | Command::Stop
        | Command::Down
        | Command::Reap { .. }
        | Command::Shutdown(_)
        | Command::Tunnel(_)
        | Command::Backup { .. }
        | Command::Volume(_) => not_ported(2),
        Command::Chat(_) | Command::Chats(_) | Command::Ask(_) => not_ported(3),
        Command::Trace(_) | Command::Toolbox(_) => not_ported(7),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
