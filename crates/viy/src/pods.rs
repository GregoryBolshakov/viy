//! The pod subcommands, rpctl's `cmd_up`, `cmd_status`, `cmd_stop`,
//! `cmd_down`, `cmd_logs`, `cmd_tunnel` and `cmd_boots`: each runs the
//! library call of its name (`viy_pod::commands`, `viy_pod::tunnel`) on a
//! tokio runtime and prints what it reports, as rp prints it, with colour
//! and the clock redrawn in place on a terminal. Ctrl-C ends a command
//! with status 130, after `up` has recorded an interrupted boot.

use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;
use viy_config::Config;
use viy_config::Paths;
use viy_exec::ExecBackend;
use viy_exec::SystemExec;
use viy_http::HttpClient;
use viy_http::ReqwestHttp;
use viy_pod::bootreport;
use viy_pod::bootreport::Records;
use viy_pod::commands;
use viy_pod::commands::CommandError;
use viy_pod::commands::Env;
use viy_pod::commands::Interrupt;
use viy_pod::notice::Alert;
use viy_pod::notice::ClockPrinter;
use viy_pod::notice::Notice;
use viy_pod::notice::Report;
use viy_pod::notice::Tone;
use viy_pod::notice::desktop_notify;
use viy_pod::tunnel;
use viy_pod::tunnel::Action;
use viy_pod::tunnel::TunnelError;
use viy_pod::wait;
use viy_time::SystemTimeProvider;

use crate::Failure;
use crate::Terminal;
use crate::cli::BootsAction;
use crate::cli::BootsArgs;
use crate::cli::Command;
use crate::cli::TunnelArgs;

/// rpctl's colours, `C_DIM` and the rest, then `C_RST`.
fn colour(tone: Tone) -> &'static str {
    match tone {
        Tone::Plain => "",
        Tone::Dim => "\x1b[2m",
        Tone::Ok => "\x1b[32m",
        Tone::Warn => "\x1b[33m",
        Tone::Error => "\x1b[31m",
    }
}

/// Prints reports to the terminal or wherever stdout goes.
struct Printer<'w> {
    out: Mutex<(&'w mut (dyn Write + Send), ClockPrinter)>,
    terminal: Terminal,
    exec: &'w dyn ExecBackend,
}

impl<'w> Printer<'w> {
    fn new(out: &'w mut (dyn Write + Send), terminal: Terminal, exec: &'w dyn ExecBackend) -> Self {
        Self {
            out: Mutex::new((out, ClockPrinter::new(terminal.tty))),
            terminal,
            exec,
        }
    }

    fn report(&self, report: Report) {
        let mut guard = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let (out, clock) = &mut *guard;
        let text = match &report {
            Report::Clock(c) => clock.clock(c),
            Report::Passing(p) => clock.passing(p),
            Report::Notice(n) => Some(format!(
                "{}{}\n",
                clock.close().unwrap_or(""),
                self.notice(n)
            )),
            Report::Alert(alert) => {
                self.alert(alert);
                self.terminal.tty.then(|| "\x07".to_string())
            }
        };
        if let Some(text) = text {
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
    }

    fn notice(&self, notice: &Notice) -> String {
        if self.terminal.tty {
            format!("{}{}\x1b[0m", colour(notice.tone), notice.text)
        } else {
            notice.text.clone()
        }
    }

    fn alert(&self, alert: &Alert) {
        if self.terminal.desktop {
            desktop_notify(self.exec, alert);
        }
    }

    /// Ends the line after Ctrl-C's `^C`, and any redrawn line first.
    fn newline(&self) {
        self.close();
        let mut guard = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = guard.0.write_all(b"\n");
        let _ = guard.0.flush();
    }

    /// Ends a redrawn line, before something else is written.
    fn close(&self) {
        let mut guard = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let (out, clock) = &mut *guard;
        if let Some(end) = clock.close() {
            let _ = out.write_all(end.as_bytes());
        }
    }
}

/// rpctl's `resolve_backend`: only the RunPod one so far.
fn runpod_backend(cfg: &Config, verb: &str) -> Result<(), Failure> {
    let kind = cfg.get("BACKEND").unwrap_or("").trim().to_lowercase();
    match kind.as_str() {
        "" | "runpod" => Ok(()),
        "openai" | "local" => Err(Failure::from(format!(
            "viy {verb}: the {kind} backend is not ported yet (ROADMAP phase 2)"
        ))),
        _ => Err(Failure::from(format!(
            "unknown BACKEND '{kind}'; supported: local, openai, runpod"
        ))),
    }
}

/// The pod commands and `viy tunnel`, run to their end.
pub fn run(
    command: &Command,
    paths: &Paths,
    cfg: Config,
    out: &mut (dyn Write + Send),
    terminal: Terminal,
) -> Result<(), Failure> {
    if let Command::Boots(args) = command {
        return boots(paths, args, out);
    }
    runpod_backend(&cfg, command.name())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::from(e.to_string()))?;
    let exec = SystemExec;
    let http: Arc<dyn HttpClient> =
        Arc::new(ReqwestHttp::new().map_err(|e| Failure::from(e.to_string()))?);
    let exe = std::env::current_exe().map_err(|e| Failure::from(format!("viy itself: {e}")))?;
    let printer = Printer::new(out, terminal, &exec);
    let result = runtime.block_on(pod_command(command, paths, cfg, &http, &exe, &printer));
    printer.close();
    result
}

async fn pod_command(
    command: &Command,
    paths: &Paths,
    mut cfg: Config,
    http: &Arc<dyn HttpClient>,
    exe: &PathBuf,
    printer: &Printer<'_>,
) -> Result<(), Failure> {
    // SIGINT is counted from the start, so no Ctrl-C ends viy without its
    // status 130; with no handler to be had, the command runs on.
    let (presses, interrupt) = Interrupt::channel();
    if let Ok(mut sigint) = signal(SignalKind::interrupt()) {
        tokio::spawn(async move {
            while sigint.recv().await.is_some() {
                presses.send_modify(|count| *count += 1);
            }
        });
    }
    let report = |r| printer.report(r);
    let time = SystemTimeProvider;
    let env = Env {
        paths,
        exec: printer.exec,
        http,
        time: &time,
        exe,
    };
    let result = match command {
        Command::Up { timeout } => {
            let timeout = Duration::from_secs(*timeout);
            commands::up(&env, &mut cfg, timeout, &interrupt, &report).await
        }
        Command::Logs { follow } => {
            commands::logs(&env, &cfg, *follow, interrupt.next(), &report).await
        }
        Command::Tunnel(args) => {
            return tunnel_command(&env, &cfg, args, &interrupt, &report)
                .await
                .map_err(|failure| ended(failure, printer));
        }
        Command::CaptureBoot { boot_id, pod_id } => {
            let rp = commands::connect(&env, &cfg).map_err(|e| failure(e, printer))?;
            return wait::capture_boot(&env.ssh(&cfg), &rp, boot_id, pod_id)
                .await
                .map_err(|e| Failure::from(e.to_string()));
        }
        other => {
            let work = async {
                match other {
                    Command::Status => commands::status(&env, &cfg, &report).await,
                    Command::Stop => commands::stop(&env, &cfg, &report).await,
                    Command::Down => commands::down(&env, &cfg, false, &report).await,
                    _ => Err(CommandError::Exit(format!(
                        "viy {}: not a pod command",
                        other.name()
                    ))),
                }
            };
            tokio::select! {
                biased;
                () = interrupt.next() => Err(CommandError::Interrupted),
                result = work => result,
            }
        }
    };
    result.map_err(|error| failure(error, printer))
}

/// After Ctrl-C, rpctl's `main` prints a newline before it exits 130.
fn ended(failure: Failure, printer: &Printer<'_>) -> Failure {
    if failure.code == 130 {
        printer.newline();
    }
    failure
}

/// How a command's error ends viy, as rpctl's `main` ends it.
fn failure(error: CommandError, printer: &Printer<'_>) -> Failure {
    match error {
        CommandError::Exit(message) => Failure::from(message),
        CommandError::Failed => Failure::silent(1),
        CommandError::Interrupted => ended(Failure::silent(130), printer),
        CommandError::RunPod(error) => {
            let message = error.to_string();
            Failure::from(if printer.terminal.err_tty {
                format!("\x1b[31m{message}\x1b[0m")
            } else {
                message
            })
        }
        CommandError::Io(error) => Failure::from(error.to_string()),
    }
}

/// `viy tunnel`, the first of its flags in rpctl's order winning.
async fn tunnel_command(
    env: &Env<'_>,
    cfg: &Config,
    args: &TunnelArgs,
    interrupt: &Interrupt,
    report: &(dyn Fn(Report) + Send + Sync),
) -> Result<(), Failure> {
    let action = if args.forget_key {
        Action::ForgetKey
    } else if args.stop {
        Action::Stop
    } else if args.status {
        Action::Status
    } else if args.background {
        Action::Background
    } else {
        Action::Foreground
    };
    let ssh = env.ssh(cfg);
    let connect = || commands::connect(env, cfg).map_err(|e| e.to_string());
    let mut say = |notice| report(Report::Notice(notice));
    // The foreground tunnel ends on the first Ctrl-C itself, closing its
    // ssh; anything else, and a second one, ends at once.
    let presses = if action == Action::Foreground { 2 } else { 1 };
    let work = tunnel::command(&ssh, action, connect, interrupt.next(), &mut say);
    let result = tokio::select! {
        biased;
        () = interrupt.after(presses) => return Err(Failure::silent(130)),
        result = work => result,
    };
    match result {
        Ok(()) => Ok(()),
        Err(TunnelError::Failed) => Err(Failure::silent(1)),
        Err(TunnelError::Exit(message)) => Err(Failure::from(message)),
        Err(TunnelError::Io(error)) => Err(Failure::from(error.to_string())),
    }
}

/// `viy boots [list|show] [BOOT] [-n N]`.
fn boots(paths: &Paths, args: &BootsArgs, out: &mut (dyn Write + Send)) -> Result<(), Failure> {
    let records = Records::new(paths);
    let text = match args.action {
        BootsAction::List => bootreport::history_table(&records, args.n),
        BootsAction::Show => bootreport::show(&records, args.boot.as_deref())?,
    };
    writeln!(out, "{text}").map_err(|e: io::Error| Failure::from(e.to_string()))
}

#[cfg(test)]
#[path = "pods_tests.rs"]
mod tests;
