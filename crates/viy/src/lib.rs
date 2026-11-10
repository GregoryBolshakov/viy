//! The `viy` command: argument parsing and dispatch, rpctl's `__main__.py`.
//!
//! The parser has all of rpctl's 19 subcommands (WP 1.6); those whose work
//! package has not come yet say so and exit 1. The pod commands are in
//! [`pods`], `viy shutdown` in [`shutdown`].

pub mod chat;
pub mod cli;
pub mod dev;
pub mod exec;
pub mod hardening;
pub mod init;
pub mod migrate;
pub mod model;
pub mod pod_server;
pub mod pods;
pub mod sessions_cli;
pub mod shutdown;
pub mod toolbox_cli;

use std::ffi::OsString;
use std::io;
use std::io::Write;
use std::path::PathBuf;

use viy_config::Paths;
use viy_config::Project;

use crate::cli::Command;

/// Where `viy` writes: whether stdout and stderr are terminals (colour,
/// the clock redrawn in place, the bell), whether stdin is one (someone to
/// ask), and whether there is a desktop to notify.
#[derive(Debug, Clone, Copy, Default)]
pub struct Terminal {
    pub tty: bool,
    pub stdin_tty: bool,
    /// Whether stderr is one, for the colour of an error message.
    pub err_tty: bool,
    pub desktop: bool,
}

impl Terminal {
    /// This process's.
    pub fn detect() -> Self {
        use std::io::IsTerminal;
        let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        Self {
            tty: io::stdout().is_terminal(),
            stdin_tty: io::stdin().is_terminal(),
            err_tty: io::stderr().is_terminal(),
            desktop: set("DISPLAY") || set("WAYLAND_DISPLAY"),
        }
    }
}

/// How a command failed: rpctl's `SystemExit`, a message for stderr or
/// none, and the exit status.
#[derive(Debug)]
pub struct Failure {
    message: Option<String>,
    code: i32,
}

impl Failure {
    /// Everything was said already.
    pub fn silent(code: i32) -> Self {
        Self {
            message: None,
            code,
        }
    }
}

impl Failure {
    /// Its message, if it has one.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message: Some(message),
            code: 1,
        }
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

/// Runs `viy` with `args` (without the program name) and returns the exit
/// status: 0, 1 when the command fails (its message on `err`, as rpctl's
/// `SystemExit`), 2 for a usage error, 130 after Ctrl-C. Output goes to
/// `out` and `err`, so tests can capture it.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    out: &mut (dyn Write + Send),
    err: &mut (dyn Write + Send),
    terminal: Terminal,
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
    match dispatch(&cli.command, out, err, terminal) {
        Ok(()) => 0,
        Err(failure) => {
            if let Some(message) = failure.message {
                let _ = writeln!(err, "{message}");
            }
            failure.code
        }
    }
}

fn dispatch(
    command: &Command,
    out: &mut (dyn Write + Send),
    err: &mut (dyn Write + Send),
    terminal: Terminal,
) -> Result<(), Failure> {
    let not_ported = |phase: u8| {
        Err(Failure::from(format!(
            "viy {}: not ported yet (ROADMAP phase {phase})",
            command.name()
        )))
    };
    let paths = || Paths::from_env().map_err(|e| e.to_string());
    let cwd = || -> Result<PathBuf, String> {
        std::env::current_dir().map_err(|e| format!("the working directory: {e}"))
    };
    let config = |paths: &Paths| -> Result<viy_config::Config, String> {
        let project = Project::find(&cwd()?).map_err(|e| e.to_string())?;
        viy_config::config::load(paths, &project).map_err(|e| e.to_string())
    };
    match command {
        Command::Init(args) => {
            let paths = paths()?;
            init::cmd_init(&paths, &cwd()?, out)?;
            if args.kb {
                let cfg = config(&paths)?;
                init::cmd_init_kb(args, &cfg, &cwd()?, out)?;
            }
            Ok(())
        }
        Command::Migrate => Ok(migrate::cmd_migrate(&paths()?, out)?),
        Command::Model(args) => {
            let paths = paths()?;
            let cfg = config(&paths)?;
            Ok(model::cmd_model(&cfg, &paths, args, out)?)
        }
        Command::Up { .. }
        | Command::Status
        | Command::Boots(_)
        | Command::Logs { .. }
        | Command::Stop
        | Command::Down
        | Command::Tunnel(_)
        | Command::Backup { .. }
        | Command::Volume(_)
        | Command::Reap { .. }
        | Command::CaptureBoot { .. } => {
            let paths = paths()?;
            let cfg = config(&paths)?;
            pods::run(command, &paths, cfg, out, terminal)
        }
        Command::Shutdown(args) => {
            let paths = paths()?;
            let cfg = config(&paths)?;
            let reload = || config(&paths);
            shutdown::run(args, &paths, cfg, reload, out, terminal)
        }
        Command::Dev(args) => {
            let paths = paths()?;
            let base = if args.live() {
                config(&paths)?
            } else {
                dev::scripted_base()
            };
            dev::run(args, &paths, base, out, terminal)
        }
        Command::Exec(args) => {
            let paths = paths()?;
            let cfg = config(&paths)?;
            exec::cmd_exec(args, &paths, cfg, &cwd()?, out, err)
        }
        Command::Archive(target) => {
            sessions_cli::cmd_archive(sessions_cli::Action::Archive, target, &paths()?, out)
        }
        Command::Unarchive(target) => {
            sessions_cli::cmd_archive(sessions_cli::Action::Unarchive, target, &paths()?, out)
        }
        Command::Delete(args) => {
            use std::io::IsTerminal;
            let asking = sessions_cli::Asking {
                can_ask: terminal.stdin_tty && std::io::stderr().is_terminal(),
                input: &mut std::io::stdin().lock(),
                err,
            };
            sessions_cli::cmd_delete(args, &paths()?, asking, out)
        }
        Command::Chat(_) | Command::Resume(_) | Command::Fork(_) => {
            let paths = paths()?;
            let cfg = config(&paths)?;
            let opening = chat_opening(command);
            chat::cmd_chat(&opening, &paths, cfg, &cwd()?, out, terminal)
        }
        Command::Toolbox(args) => {
            let asking = toolbox_cli::Asking {
                input: &mut std::io::stdin().lock(),
                err,
            };
            toolbox_cli::cmd_toolbox(args, &paths()?, &cwd()?, terminal.tty, asking, out)
        }
        Command::Trace(_) => not_ported(7),
    }
}

/// What `viy chat`, `viy resume` or `viy fork` opens.
fn chat_opening(command: &Command) -> chat::Opening {
    let wanted = |pick: &cli::Pick, include_exec: bool, fork: bool| chat::Wanted {
        id: pick.session_id.clone(),
        last: pick.last,
        all: pick.all,
        include_exec,
        fork,
    };
    match command {
        Command::Chat(args) => chat::Opening {
            url: args.url.clone(),
            system: args.system.clone(),
            session: None,
        },
        Command::Resume(args) => chat::Opening {
            url: args.url.clone(),
            system: None,
            session: Some(wanted(&args.pick, args.include_non_interactive, false)),
        },
        Command::Fork(args) => chat::Opening {
            url: args.url.clone(),
            system: None,
            session: Some(wanted(&args.pick, false, true)),
        },
        _ => chat::Opening::default(),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
