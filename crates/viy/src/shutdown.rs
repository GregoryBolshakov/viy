//! `viy shutdown`, rpctl's `cmd_shutdown`: schedules, shows or cancels the
//! shutdown (`viy_pod::shutdown`), and runs its countdown, detached with
//! `--run-at` or in this terminal with `--foreground`. SIGTERM ends the
//! countdown with status 143 (that is how `--cancel` stops it), Ctrl-C
//! with 130; either way `shutdown.json` goes if it is this process's.

use std::io::Write;
use std::sync::Arc;

use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;
use viy_config::Config;
use viy_config::Paths;
use viy_exec::SystemExec;
use viy_http::HttpClient;
use viy_http::ReqwestHttp;
use viy_pod::commands::Env;
use viy_pod::notice::Report;
use viy_pod::notice::Tone;
use viy_pod::shutdown;
use viy_pod::shutdown::DownBudget;
use viy_pod::shutdown::Host;
use viy_pod::shutdown::Log;
use viy_pod::shutdown::Scheduled;
use viy_pod::shutdown::SystemChats;
use viy_time::SystemTimeProvider;

use crate::Failure;
use crate::Terminal;
use crate::cli::ShutdownArgs;
use crate::pods::Printer;

/// Runs `viy shutdown` with `args`. `reload` reads the config again when
/// the countdown ends.
pub fn run(
    args: &ShutdownArgs,
    paths: &Paths,
    cfg: Config,
    reload: impl FnOnce() -> Result<Config, String>,
    out: &mut (dyn Write + Send),
    terminal: Terminal,
) -> Result<(), Failure> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::from(e.to_string()))?;
    // tokio's process spawn needs the runtime's reactor, block_on or not.
    let _context = runtime.enter();
    let exec = SystemExec;
    let time = SystemTimeProvider;
    let var = |name: &str| std::env::var_os(name);
    let host = Host {
        paths,
        exec: &exec,
        time: &time,
        chats: &SystemChats,
        var: &var,
    };
    let poweroff = !args.no_poweroff;
    if let Some(at) = &args.run_at {
        let deadline: f64 = at
            .parse()
            .map_err(|_| Failure::from(format!("--run-at: not a time: {at}")))?;
        return runtime.block_on(countdown(
            &host, cfg, deadline, poweroff, false, reload, out,
        ));
    }
    let exe = std::env::current_exe().map_err(|e| Failure::from(format!("viy itself: {e}")))?;
    let deadline = {
        let printer = Printer::new(&mut *out, terminal, &exec);
        let report = |r| printer.report(r);
        match runtime.block_on(arrange(args, &host, &cfg, &exe, &report))? {
            Some(deadline) => deadline,
            None => return Ok(()),
        }
    };
    Scheduled {
        pid: std::process::id(),
        deadline,
        poweroff,
    }
    .save(paths)
    .map_err(|e| Failure::from(e.to_string()))?;
    runtime.block_on(countdown(&host, cfg, deadline, poweroff, true, reload, out))
}

/// What `viy shutdown` does short of a countdown in this process, the
/// same in the terminal and the chat (kb d068): cancels, shows, or
/// schedules the shutdown and starts its detached runner. With
/// `--foreground`, the deadline, for the countdown in this terminal.
pub(crate) async fn arrange(
    args: &ShutdownArgs,
    host: &Host<'_>,
    cfg: &Config,
    exe: &std::path::Path,
    report: viy_pod::notice::Reporter<'_>,
) -> Result<Option<f64>, Failure> {
    let say = |text: &str| {
        report(Report::Notice(viy_pod::notice::Notice::new(
            Tone::Plain,
            text,
        )))
    };
    if args.cancel {
        shutdown::cancel(host.paths, false, report)
            .await
            .map_err(|e| Failure::from(e.to_string()))?;
        return Ok(None);
    }
    let Some(when) = args.when.as_deref().filter(|_| !args.status) else {
        shutdown::status(host, report);
        if args.when.is_none() && !args.status {
            say("schedule one with: viy shutdown 90m   (or 1h30m, 23:30)");
        }
        return Ok(None);
    };
    let deadline = shutdown::schedule(host, cfg, when, args.poweroff(), args.foreground, report)
        .await
        .map_err(|e| Failure::from(e.to_string()))?;
    if args.foreground {
        return Ok(Some(deadline));
    }
    shutdown::start_runner(host, exe, deadline, args.poweroff(), report)
        .map_err(|e| Failure::from(e.to_string()))?;
    Ok(None)
}

/// Waits for `deadline` and fires, writing to `out`: the log, for the
/// detached runner. `foreground` redraws the time left.
async fn countdown(
    host: &Host<'_>,
    cfg: Config,
    deadline: f64,
    poweroff: bool,
    foreground: bool,
    reload: impl FnOnce() -> Result<Config, String>,
    out: &mut (dyn Write + Send),
) -> Result<(), Failure> {
    let http: Arc<dyn HttpClient> =
        Arc::new(ReqwestHttp::new().map_err(|e| Failure::from(e.to_string()))?);
    let exe = std::env::current_exe().map_err(|e| Failure::from(format!("viy itself: {e}")))?;
    let env = Env {
        paths: host.paths,
        exec: host.exec,
        http: &http,
        time: host.time,
        exe: &exe,
    };
    let log = Log::new(out, host.time);
    // What `viy down` says goes to the log as it is.
    let report = |r: Report| {
        if let Report::Notice(notice) = r {
            log.raw(&format!("{notice}\n"));
        }
    };
    let down = async |cfg: &Config| shutdown::take_down(&env, cfg, &report).await;
    let signals = |kind| signal(kind).map_err(|e| Failure::from(e.to_string()));
    let mut terminated = signals(SignalKind::terminate())?;
    let mut interrupted = signals(SignalKind::interrupt())?;
    // Detached, nobody's terminal can hang it up; held, the handler keeps
    // SIGHUP from ending it, as rp's SIG_IGN does.
    let _hangup = if foreground {
        None
    } else {
        Some(signals(SignalKind::hangup())?)
    };
    let work = shutdown::run(
        host,
        &log,
        cfg,
        deadline,
        poweroff,
        foreground,
        DownBudget::default(),
        reload,
        down,
    );
    let result = tokio::select! {
        code = work => match code {
            0 => Ok(()),
            code => Err(Failure::silent(code)),
        },
        _ = terminated.recv() => Err(Failure::silent(143)),
        _ = interrupted.recv() => {
            log.raw("\ncancelled\n");
            Err(Failure::silent(130))
        }
    };
    shutdown::clear_state(host.paths);
    result
}
