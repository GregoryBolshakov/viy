//! The pod subcommands, rpctl's `cmd_up`, `cmd_status`, `cmd_stop`,
//! `cmd_down`, `cmd_logs`, `cmd_tunnel`, `cmd_boots`, `cmd_backup`,
//! `cmd_volume` and `cmd_reap`: each runs the library call of its name
//! (`viy_pod::commands`, `viy_pod::tunnel`, `viy_pod::regions`,
//! `viy_pod::idle`) on a tokio runtime and prints what it reports, as rp
//! prints it, with colour and the clock redrawn in place on a terminal,
//! and asks its questions there. Ctrl-C ends a command with status 130,
//! after `up` has recorded an interrupted boot and deleted the volumes its
//! failover made.

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
use viy_pod::backend;
use viy_pod::backend::Backend;
use viy_pod::bootreport;
use viy_pod::bootreport::Records;
use viy_pod::commands;
use viy_pod::commands::CommandError;
use viy_pod::commands::Env;
use viy_pod::commands::Interrupt;
use viy_pod::commands::VolumeAction;
use viy_pod::idle;
use viy_pod::local;
use viy_pod::notice::Alert;
use viy_pod::notice::Asking;
use viy_pod::notice::ClockPrinter;
use viy_pod::notice::Notice;
use viy_pod::notice::Report;
use viy_pod::notice::Reporter;
use viy_pod::notice::Tone;
use viy_pod::notice::desktop_notify;
use viy_pod::regions;
use viy_pod::tunnel;
use viy_pod::tunnel::Action;
use viy_pod::tunnel::TunnelError;
use viy_pod::wait;
use viy_protocol::Answer;
use viy_protocol::Question;
use viy_time::SystemTimeProvider;

use crate::Failure;
use crate::Terminal;
use crate::cli::BootsAction;
use crate::cli::BootsArgs;
use crate::cli::Command;
use crate::cli::TunnelArgs;
use crate::cli::VolumeAction as VolumeArg;

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
pub(crate) struct Printer<'w> {
    out: Mutex<(&'w mut (dyn Write + Send), ClockPrinter)>,
    terminal: Terminal,
    exec: &'w dyn ExecBackend,
}

impl<'w> Printer<'w> {
    pub(crate) fn new(
        out: &'w mut (dyn Write + Send),
        terminal: Terminal,
        exec: &'w dyn ExecBackend,
    ) -> Self {
        Self {
            out: Mutex::new((out, ClockPrinter::new(terminal.tty))),
            terminal,
            exec,
        }
    }

    pub(crate) fn report(&self, report: Report) {
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

    /// rpctl's `_ask` on a terminal: the question in its colour with the
    /// default shown, and a line read from stdin; Enter alone, or the end
    /// of input, is the default. `None` unless stdin and stdout are both
    /// terminals.
    fn ask(&self, question: &Question) -> Option<Asking> {
        if !(self.terminal.stdin_tty && self.terminal.tty) {
            return None;
        }
        self.close();
        {
            let mut guard = self.out.lock().unwrap_or_else(PoisonError::into_inner);
            for notice in &question.reason {
                let _ = writeln!(guard.0, "{}", notice.message);
            }
            let prompt = format!("\x1b[33m{} {} \x1b[0m", question.prompt, question.hint());
            let _ = guard.0.write_all(prompt.as_bytes());
            let _ = guard.0.flush();
        }
        // A thread of its own, not tokio's blocking pool: a Ctrl-C ends viy
        // without waiting for the line.
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let read = io::stdin().read_line(&mut line);
            let _ = tx.send(matches!(read, Ok(n) if n > 0).then_some(line));
        });
        let default = question.default;
        Some(Box::pin(async move {
            let Ok(Some(line)) = rx.await else {
                return default;
            };
            match line.trim().to_lowercase().as_str() {
                "" => default,
                "y" | "yes" => Answer::Yes,
                _ => Answer::No,
            }
        }))
    }

    /// Ends a redrawn line, before something else is written.
    pub(crate) fn close(&self) {
        let mut guard = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let (out, clock) = &mut *guard;
        if let Some(end) = clock.close() {
            let _ = out.write_all(end.as_bytes());
        }
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
    let backend = match command {
        Command::Boots(args) => return boots(paths, args, out),
        // RunPod's own, whatever BACKEND says; reap looks itself.
        Command::Backup { .. }
        | Command::Volume(_)
        | Command::Reap { .. }
        | Command::CaptureBoot { .. } => Backend::Runpod,
        _ => Backend::resolve(&cfg)?,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::from(e.to_string()))?;
    let exec = SystemExec;
    let http: Arc<dyn HttpClient> =
        Arc::new(ReqwestHttp::new().map_err(|e| Failure::from(e.to_string()))?);
    let exe = std::env::current_exe().map_err(|e| Failure::from(format!("viy itself: {e}")))?;
    let printer = Printer::new(out, terminal, &exec);
    let result = runtime.block_on(pod_command(
        command, backend, paths, cfg, &http, &exe, &printer,
    ));
    printer.close();
    result
}

async fn pod_command(
    command: &Command,
    backend: Backend,
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
    let ask = |q: &Question| printer.ask(q);
    let time = SystemTimeProvider;
    let env = Env {
        paths,
        exec: printer.exec,
        http,
        time: &time,
        exe,
    };
    if backend != Backend::Runpod {
        return other_backend(command, backend, &env, &mut cfg, &interrupt, &report)
            .await
            .map_err(|error| failure(error, printer));
    }
    let result = match command {
        Command::Up { timeout } => {
            let timeout = Duration::from_secs(*timeout);
            commands::up(&env, &mut cfg, timeout, &interrupt, &report, &ask).await
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
                    Command::Reap { install: true, .. } => {
                        let path = std::env::var_os("PATH");
                        let viy = viy_config::vault::find_executable(path.as_deref(), "viy");
                        idle::install(paths, env.exec, viy.as_deref(), &report).await
                    }
                    Command::Reap { quiet, .. } => idle::reap(&env, &cfg, *quiet, &report).await,
                    Command::Backup { samples } => {
                        regions::backup(&env, &cfg, *samples, &report).await
                    }
                    Command::Volume(args) => {
                        let action = match args.action {
                            VolumeArg::List => VolumeAction::List,
                            VolumeArg::Create => VolumeAction::Create,
                            VolumeArg::Delete => VolumeAction::Delete,
                        };
                        let id = args.volume_id.as_deref();
                        commands::volume(&env, &mut cfg, action, id, &report, &ask).await
                    }
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

/// The commands for a backend other than RunPod, or rpctl's
/// NotSupported for one it lacks.
async fn other_backend(
    command: &Command,
    backend: Backend,
    env: &Env<'_>,
    cfg: &mut Config,
    interrupt: &Interrupt,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    if let (Backend::Local, Command::Logs { follow }) = (backend, command) {
        // Following ends on Ctrl-C by itself, quietly.
        return local::logs(env, *follow, interrupt.next(), report).await;
    }
    let work = async {
        match (backend, command) {
            (Backend::Openai, Command::Up { .. }) => {
                backend::openai_up(report);
                Ok(())
            }
            (Backend::Openai, Command::Status) => backend::openai_status(env, cfg, report).await,
            (Backend::Local, Command::Up { timeout }) => {
                local::up(env, cfg, Duration::from_secs(*timeout), report).await
            }
            (Backend::Local, Command::Status) => local::status(env, cfg, report).await,
            (Backend::Local, Command::Stop | Command::Down) => local::down(env, report).await,
            (_, other) => Err(backend::not_supported(backend, other.name())),
        }
    };
    tokio::select! {
        biased;
        () = interrupt.next() => Err(CommandError::Interrupted),
        result = work => result,
    }
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
