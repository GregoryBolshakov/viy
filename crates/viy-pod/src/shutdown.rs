//! `viy shutdown`: after a timeout, close every open chat, run `viy down`,
//! and once that has finished, power off this machine. rpctl's
//! `shutdown.py`.
//!
//! The countdown runs in a detached process (`viy shutdown --run-at`, which
//! survives the terminal closing), so a chat can go on in the same
//! terminal. Every step that cannot happen is flagged twice: once when the
//! shutdown is scheduled ([`schedule`]), and again, with a desktop
//! notification, when it fires ([`fire`]).
//!
//! The machine powers off even if `viy down` fails: a pod left behind is
//! ended by its own idle watchdog, which needs nothing from this machine.
//!
//! One shutdown per machine: `shutdown.json` and `shutdown.log` are rp's
//! (kb d049), so either tool's `--status` and `--cancel` see the other's,
//! and it closes the chats of both.

use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use chrono::DateTime;
use chrono::Days;
use chrono::Local;
use chrono::NaiveDateTime;
use regex::Regex;
use serde_json::Value;
use serde_json::json;
use tokio::time::Instant;
use viy_config::Config;
use viy_config::Paths;
use viy_config::atomic::write_atomically;
use viy_exec::BoxFuture;
use viy_exec::ExecBackend;
use viy_exec::Spawn;
use viy_time::TimeProvider;

use crate::backend::Backend;
use crate::commands;
use crate::commands::CommandError;
use crate::commands::Env;
use crate::compile_regex;
use crate::local;
use crate::notice::Reporter;
use crate::notice::Tone;
use crate::notice::say;
use crate::process;
use crate::regions;

/// Desktop notifications this long before firing, in seconds.
const WARN_AT_S: [f64; 2] = [10.0 * 60.0, 60.0];
/// SIGTERM, then this long to save and exit, then SIGKILL.
pub const CHAT_GRACE: Duration = Duration::from_secs(20);
/// Slept past the deadline: warn, then fire this many seconds later.
const WOKE_LATE_GRACE_S: f64 = 60.0;

/// How long [`fire`] retries `viy down`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownBudget {
    /// No retry starts that could not finish within this.
    pub total: Duration,
    /// The pause between attempts.
    pub retry: Duration,
    /// One attempt at its longest: the lock preempt, the 300 s sweep and
    /// the settle.
    pub worst: Duration,
}

impl Default for DownBudget {
    fn default() -> Self {
        Self {
            total: Duration::from_secs(15 * 60),
            retry: Duration::from_secs(30),
            worst: Duration::from_secs(6 * 60),
        }
    }
}

/// Seconds from `now` (local time) until the shutdown. Accepts a duration
/// (90, 90m, 1h30m, 45s; a bare number is minutes) or a clock time (23:30,
/// the next time it comes round).
pub fn parse_when(text: &str, now: NaiveDateTime) -> Result<f64, String> {
    static CLOCK: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| compile_regex(r"^([0-9]{1,2}):([0-9]{2})$"));
    static NUMBER: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| compile_regex(r"^[0-9]+(\.[0-9]+)?$"));
    static PART: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| compile_regex(r"([0-9]+(?:\.[0-9]+)?)([smhd])"));
    let text = text.trim().to_lowercase();
    if let Some(clock) = CLOCK.captures(&text) {
        let not_clock = || format!("not a clock time: {text}");
        let hour: u32 = clock[1].parse().map_err(|_| not_clock())?;
        let minute: u32 = clock[2].parse().map_err(|_| not_clock())?;
        let mut at = now
            .date()
            .and_hms_opt(hour, minute, 0)
            .ok_or_else(not_clock)?;
        if at <= now {
            at = at.checked_add_days(Days::new(1)).ok_or_else(not_clock)?;
        }
        return Ok((at - now).as_seconds_f64());
    }
    let not_duration = || {
        format!(
            "not a duration: '{}' (try 90m, 1h30m or 23:30)",
            text.escape_debug()
        )
    };
    if NUMBER.is_match(&text) {
        return text
            .parse::<f64>()
            .map(|n| n * 60.0)
            .map_err(|_| not_duration());
    }
    let parts: Vec<(&str, &str)> = PART
        .captures_iter(&text)
        .filter_map(|c| Some((c.get(1)?.as_str(), c.get(2)?.as_str())))
        .collect();
    let joined: String = parts.iter().map(|(n, u)| format!("{n}{u}")).collect();
    if parts.is_empty() || joined != text {
        return Err(not_duration());
    }
    let mut total = 0.0;
    for (n, unit) in parts {
        let n: f64 = n.parse().map_err(|_| not_duration())?;
        total += n * match unit {
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => 86400.0,
        };
    }
    Ok(total)
}

/// `1h05m`, `4m09s`, `7s`; whole seconds, never below zero.
pub fn fmt(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    let (h, m, s) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// A Unix time on this machine's clock, if it is one.
fn local(at: f64) -> Option<DateTime<Local>> {
    if !at.is_finite() {
        return None;
    }
    DateTime::from_timestamp_micros((at * 1e6) as i64).map(|t| t.with_timezone(&Local))
}

// ----------------------------------------------------------------- the host

/// This machine as the shutdown sees it. `var` reads an environment
/// variable: `PATH` for what is installed, and the desktop session's.
#[derive(Clone, Copy)]
pub struct Host<'a> {
    pub paths: &'a Paths,
    pub exec: &'a dyn ExecBackend,
    pub time: &'a dyn TimeProvider,
    pub chats: &'a dyn Chats,
    pub var: &'a (dyn Fn(&str) -> Option<OsString> + Send + Sync),
}

impl Host<'_> {
    /// Whether `program` is on PATH.
    fn has(&self, program: &str) -> bool {
        viy_config::vault::find_executable((self.var)("PATH").as_deref(), program).is_some()
    }

    fn set(&self, name: &str) -> bool {
        (self.var)(name).is_some_and(|v| !v.is_empty())
    }
}

// ------------------------------------------------- what can and can't happen

/// Calls a no-argument logind Manager method: its data, or `None`.
async fn logind(host: &Host<'_>, method: &str) -> Option<Value> {
    if !host.has("busctl") {
        return None;
    }
    let command = viy_exec::Command::new("busctl")
        .args(["--json=short", "call", "org.freedesktop.login1"])
        .args(["/org/freedesktop/login1", "org.freedesktop.login1.Manager"])
        .arg(method)
        .timeout(Duration::from_secs(10));
    let out = host.exec.run(command).await.ok()?;
    if !out.status.success() {
        return None;
    }
    let mut reply: Value = serde_json::from_slice(&out.stdout).ok()?;
    reply.get_mut("data").map(Value::take)
}

/// Reasons an unattended poweroff would fail; empty if it should work.
pub async fn poweroff_problems(host: &Host<'_>) -> Vec<String> {
    if !host.has("systemctl") {
        return vec!["systemctl is not available, so this machine can't be powered off".into()];
    }
    let Some(can) = logind(host, "CanPowerOff")
        .await
        .and_then(|data| data.get(0).cloned())
    else {
        return vec![
            "could not ask logind whether this user may power off; the poweroff may fail".into(),
        ];
    };
    match can.as_str() {
        Some("yes") => {}
        Some("challenge") => {
            return vec![
                "powering off needs a password here (polkit), which nobody will be there to \
                 type: the machine will stay on"
                    .into(),
            ];
        }
        Some(other) => {
            return vec![format!(
                "this user may not power off the machine (logind: {other})"
            )];
        }
        None => {
            return vec![format!(
                "this user may not power off the machine (logind: {can})"
            )];
        }
    }
    let inhibitors = logind(host, "ListInhibitors").await;
    let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_string();
    inhibitors
        .as_ref()
        .and_then(|data| data.get(0))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .filter(|row| row.len() == 6)
        .filter(|row| {
            text(row.get(3)) == "block" && text(row.first()).split(':').any(|w| w == "shutdown")
        })
        .map(|row| {
            format!(
                "{} blocks shutdown ({}); the poweroff will be refused while it holds that lock",
                text(row.get(1)),
                text(row.get(2))
            )
        })
        .collect()
}

/// Reasons `viy down` would not take the pod down.
pub fn down_problems(cfg: &Config) -> Vec<String> {
    match Backend::resolve(cfg) {
        Err(message) => vec![message],
        Ok(backend) if !backend.manages_server() => vec![format!(
            "viy down does nothing for the '{}' backend; that step is skipped",
            backend.name()
        )],
        Ok(Backend::Runpod) if cfg.get("RUNPOD_API_KEY").unwrap_or("").is_empty() => vec![
            "RUNPOD_API_KEY is not set, so viy down can't reach RunPod; the pod would be left \
             to its own idle watchdog"
                .into(),
        ],
        Ok(_) => Vec::new(),
    }
}

/// Reasons the desktop warnings before the shutdown would not show.
pub fn notify_problems(host: &Host<'_>) -> Vec<String> {
    if !host.has("notify-send") {
        return vec!["notify-send is missing: no desktop warnings before the shutdown".into()];
    }
    if !["DBUS_SESSION_BUS_ADDRESS", "DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|name| host.set(name))
    {
        return vec!["no desktop session in this environment: no desktop warnings".into()];
    }
    Vec::new()
}

// --------------------------------------------------------------- open chats

/// Whether a command line is an `rp chat` or a viy chat: the first
/// argument naming rp or viy is followed, past its options, by `chat`; for
/// viy also by `resume` or `fork`, or by nothing, as bare `viy` opens the
/// chat (kb d067, d083), where rp's `_is_chat` knows only `viy chat`.
pub fn is_chat(argv: &[Vec<u8>]) -> bool {
    let is_viy = |a: &[u8]| a.ends_with(b"/viy") || a == b"viy";
    let names_tool =
        |a: &[u8]| a.ends_with(b"/rp") || is_viy(a) || [&b"rp"[..], b"rpctl"].contains(&a);
    let Some(i) = argv.iter().position(|a| names_tool(a)) else {
        return false;
    };
    let viy = is_viy(&argv[i]);
    match argv[i + 1..]
        .iter()
        .find(|a| !a.is_empty() && !a.starts_with(b"-"))
    {
        Some(sub) => sub == b"chat" || (viy && (sub == b"resume" || sub == b"fork")),
        None => viy,
    }
}

/// What [`Chats::close`] did with each pid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Closed {
    /// Exited on SIGTERM, having saved.
    pub closed: Vec<u32>,
    /// Still there after the grace: killed.
    pub killed: Vec<u32>,
    /// Not ours to signal.
    pub failed: Vec<u32>,
}

/// The open chats on this machine: the system's, or a test's.
pub trait Chats: Send + Sync {
    /// This user's running chats of either tool, by pid.
    fn find(&self) -> Vec<u32>;

    /// SIGTERM to each (a chat saves and exits on it), SIGKILL to
    /// whatever is left after `grace`.
    fn close<'a>(&'a self, pids: &'a [u32], grace: Duration) -> BoxFuture<'a, Closed>;
}

/// This machine's chats, through `/proc` and signals.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemChats;

impl Chats for SystemChats {
    fn find(&self) -> Vec<u32> {
        find_chats()
    }

    fn close<'a>(&'a self, pids: &'a [u32], grace: Duration) -> BoxFuture<'a, Closed> {
        Box::pin(close_chats(pids, grace))
    }
}

/// The pids of this user's running chats, rp's and viy's, but this
/// process.
pub fn find_chats() -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;
    let me = std::process::id();
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != me)
        .filter(|pid| fs::metadata(format!("/proc/{pid}")).is_ok_and(|m| m.uid() == uid))
        .filter(|&pid| is_chat(&process::cmdline(pid)))
        .collect();
    pids.sort_unstable();
    pids
}

/// How a signal to one pid went.
enum Sent {
    Delivered,
    Gone,
    Refused,
}

fn signal(pid: u32, signal: libc::c_int) -> Sent {
    let Some(raw) = libc::pid_t::try_from(pid).ok().filter(|&p| p > 0) else {
        return Sent::Refused;
    };
    // SAFETY: a plain signal to one positive pid.
    if unsafe { libc::kill(raw, signal) } == 0 {
        return Sent::Delivered;
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Sent::Gone,
        _ => Sent::Refused,
    }
}

/// [`Chats::close`] on this machine.
pub async fn close_chats(pids: &[u32], grace: Duration) -> Closed {
    let mut failed: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|&pid| matches!(signal(pid, libc::SIGTERM), Sent::Refused))
        .collect();
    let waiting: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|p| !failed.contains(p))
        .collect();
    let end = Instant::now() + grace;
    while Instant::now() < end && waiting.iter().any(|&p| process::pid_alive(p)) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut killed = Vec::new();
    for &pid in &waiting {
        if process::pid_alive(pid) {
            match signal(pid, libc::SIGKILL) {
                Sent::Delivered => killed.push(pid),
                Sent::Gone => {}
                Sent::Refused => failed.push(pid),
            }
        }
    }
    let closed = waiting
        .into_iter()
        .filter(|p| !killed.contains(p) && !failed.contains(p))
        .collect();
    Closed {
        closed,
        killed,
        failed,
    }
}

fn joined(pids: &[u32]) -> String {
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

// --------------------------------------------------------------- the runner

/// What the runner writes: lines stamped with the local time, and the
/// foreground countdown.
pub struct Log<'a> {
    out: Mutex<&'a mut (dyn Write + Send)>,
    time: &'a dyn TimeProvider,
}

impl<'a> Log<'a> {
    pub fn new(out: &'a mut (dyn Write + Send), time: &'a dyn TimeProvider) -> Self {
        Self {
            out: Mutex::new(out),
            time,
        }
    }

    /// `2026-07-17 23:30:00 <msg>`.
    pub fn line(&self, msg: &str) {
        let at = self.time.now().with_timezone(&Local);
        self.raw(&format!("{} {msg}\n", at.format("%Y-%m-%d %H:%M:%S")));
    }

    /// `text` as it is.
    pub fn raw(&self, text: &str) {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }
}

/// A desktop notification, waited on; nothing without notify-send.
async fn notify(host: &Host<'_>, msg: &str, urgent: bool) {
    if !host.has("notify-send") {
        return;
    }
    let urgency = if urgent { "critical" } else { "normal" };
    let command = viy_exec::Command::new("notify-send")
        .args(["-a", "viy", "-u", urgency, "viy shutdown", msg])
        .timeout(Duration::from_secs(10));
    let _ = host.exec.run(command).await;
}

async fn flag(host: &Host<'_>, log: &Log<'_>, msg: &str) {
    log.line(&format!("FLAG: {msg}"));
    notify(host, msg, true).await;
}

/// How one `viy down` attempt went well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Down {
    /// It confirmed no pod is running.
    Done,
    /// This backend has nothing to take down.
    Nothing(&'static str),
}

/// `viy down` for the backend `cfg` names, with what it says written to
/// the log; why it failed otherwise.
pub async fn take_down(env: &Env<'_>, cfg: &Config, report: Reporter<'_>) -> Result<Down, String> {
    match Backend::resolve(cfg)? {
        backend if !backend.manages_server() => Ok(Down::Nothing(backend.name())),
        backend => match backend_down(env, cfg, backend, report).await {
            Ok(()) => Ok(Down::Done),
            // rp's SystemExit(1), everything said already.
            Err(CommandError::Failed) => Err("1".into()),
            Err(error) => Err(error.to_string()),
        },
    }
}

async fn backend_down(
    env: &Env<'_>,
    cfg: &Config,
    backend: Backend,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    match backend {
        Backend::Local => local::down(env, report).await,
        _ => commands::down(env, cfg, false, report).await,
    }
}

/// `viy down`, retried while another full attempt fits in the budget.
/// True once it reports success, or the backend has nothing to take down.
async fn run_down<D>(log: &Log<'_>, cfg: &Config, budget: DownBudget, down: &mut D) -> bool
where
    D: AsyncFnMut(&Config) -> Result<Down, String>,
{
    let start = Instant::now();
    for attempt in 1.. {
        log.line(&format!("viy down (attempt {attempt})"));
        match down(cfg).await {
            Ok(Down::Done) => return true,
            Ok(Down::Nothing(name)) => {
                log.line(&format!("the '{name}' backend has nothing to take down"));
                return true;
            }
            Err(why) => log.line(&format!("viy down failed: {why}")),
        }
        if start.elapsed() + budget.retry + budget.worst > budget.total {
            return false;
        }
        tokio::time::sleep(budget.retry).await;
    }
    false
}

/// Closes the chats, takes the pod down with `down`, powers off. The exit
/// status.
pub async fn fire<D>(
    host: &Host<'_>,
    log: &Log<'_>,
    cfg: &Config,
    poweroff: bool,
    budget: DownBudget,
    mut down: D,
) -> i32
where
    D: AsyncFnMut(&Config) -> Result<Down, String>,
{
    log.line("time is up");
    let chats = host.chats.find();
    if chats.is_empty() {
        log.line("no open chats");
    } else {
        log.line(&format!(
            "closing {} chat(s): {}",
            chats.len(),
            joined(&chats)
        ));
        let done = host.chats.close(&chats, CHAT_GRACE).await;
        if !done.killed.is_empty() {
            let msg = format!(
                "chat(s) {} did not exit within {}s and were killed; their last turn may be \
                 unsaved",
                joined(&done.killed),
                CHAT_GRACE.as_secs()
            );
            flag(host, log, &msg).await;
        }
        if !done.failed.is_empty() {
            let msg = format!(
                "could not close chat(s) {} (permission denied)",
                joined(&done.failed)
            );
            flag(host, log, &msg).await;
        }
        if !done.closed.is_empty() {
            log.line(&format!("closed and saved: {}", joined(&done.closed)));
        }
    }

    let down_ok = run_down(log, cfg, budget, &mut down).await;
    if down_ok {
        log.line("viy down finished");
    } else if poweroff {
        let msg = "viy down could not confirm that no pod is running; powering off anyway. The \
                   pod's idle watchdog ends a leftover pod; check https://console.runpod.io/pods \
                   when you are back";
        flag(host, log, msg).await;
    } else {
        let msg = "viy down could not confirm that no pod is running. Run viy down yourself, or \
                   check https://console.runpod.io/pods";
        flag(host, log, msg).await;
    }

    if !poweroff {
        log.line("--no-poweroff: leaving the machine on");
        return if down_ok { 0 } else { 1 };
    }
    for problem in poweroff_problems(host).await {
        log.line(&format!("warning: {problem}"));
    }
    log.line("powering off");
    let command = viy_exec::Command::new("systemctl")
        .args(["poweroff", "--no-ask-password"])
        .timeout(Duration::from_secs(60));
    match host.exec.run(command).await {
        Err(error) => {
            let msg = format!("the pod is down, but the poweroff failed: {error}");
            flag(host, log, &msg).await;
            1
        }
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let why = match stderr.trim() {
                "" => format!("systemctl exited {}", viy_exec::exit_text(out.status)),
                text => text.to_string(),
            };
            let msg = format!("the pod is down, but the poweroff was refused: {why}");
            flag(host, log, &msg).await;
            1
        }
        Ok(_) => 0,
    }
}

/// Waits for the wall-clock `deadline` (suspend doesn't stop it), then
/// fires with the config `reload` reads then: a profile switched since
/// scheduling means another POD_NAME, and that pod is the one running.
/// `countdown`: redraws the time left in the foreground every second.
#[allow(clippy::too_many_arguments)]
pub async fn run<R, D>(
    host: &Host<'_>,
    log: &Log<'_>,
    cfg: Config,
    mut deadline: f64,
    poweroff: bool,
    countdown: bool,
    budget: DownBudget,
    reload: R,
    down: D,
) -> i32
where
    R: FnOnce() -> Result<Config, String>,
    D: AsyncFnMut(&Config) -> Result<Down, String>,
{
    let mut warned: Vec<f64> = Vec::new();
    let mut last_check = regions::now(host.time);
    loop {
        let now = regions::now(host.time);
        let left = deadline - now;
        if left <= 0.0 {
            // Slept through the deadline: don't power off the instant the
            // lid opens without a word.
            if now - last_check > WOKE_LATE_GRACE_S + 30.0 && now - deadline > 30.0 {
                log.line("woke up past the deadline");
                let msg = format!(
                    "Woke up past the deadline: chats close, the pod goes down and this PC \
                     powers off in {WOKE_LATE_GRACE_S:.0}s. Cancel: viy shutdown --cancel"
                );
                notify(host, &msg, true).await;
                deadline = now + WOKE_LATE_GRACE_S;
                last_check = now;
                continue;
            }
            break;
        }
        for w in WARN_AT_S {
            if left <= w && !warned.contains(&w) {
                warned.extend(WARN_AT_S.iter().filter(|&&x| x >= w));
                let action = if poweroff {
                    "this PC powers off"
                } else {
                    "the pod goes down"
                };
                let msg = format!(
                    "In {}: open chats close and {action}. Cancel: viy shutdown --cancel",
                    fmt(left)
                );
                notify(host, &msg, w <= 60.0).await;
            }
        }
        if countdown {
            log.raw(&format!(
                "\r\x1b[Kshutdown in {}  (Ctrl-C cancels)",
                fmt(left)
            ));
        }
        last_check = now;
        let step = if countdown { 1.0 } else { 5.0 };
        tokio::time::sleep(Duration::from_secs_f64(f64::min(step, left.max(0.1)))).await;
    }
    if countdown {
        log.raw("\n");
    }
    let cfg = match reload() {
        Ok(fresh) => fresh,
        // The old config still knows how to reach RunPod.
        Err(error) => {
            log.line(&format!(
                "could not reread the config, using the one from scheduling: {error}"
            ));
            cfg
        }
    };
    fire(host, log, &cfg, poweroff, budget, down).await
}

// -------------------------------------------------------------- the command

/// The scheduled shutdown, as `shutdown.json` has it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scheduled {
    /// The runner's: the detached `--run-at`, or a foreground countdown.
    pub pid: u32,
    /// Unix seconds.
    pub deadline: f64,
    pub poweroff: bool,
}

impl Scheduled {
    pub fn save(&self, paths: &Paths) -> io::Result<()> {
        fs::create_dir_all(paths.pod_state_dir())?;
        let state = json!({"pid": self.pid, "deadline": self.deadline, "poweroff": self.poweroff});
        write_atomically(&paths.shutdown_state_file(), &state.to_string())
    }

    /// The live scheduled shutdown: its runner alive and a `shutdown`
    /// command. A file left by a dead one is removed.
    pub fn live(paths: &Paths) -> Option<Self> {
        let live = Self::read(paths)?;
        if live.is_none() {
            let _ = fs::remove_file(paths.shutdown_state_file());
        }
        live
    }

    /// [`Scheduled::live`] without removing a dead one's file: for `viy
    /// migrate --finish --dry-run`, which changes nothing.
    pub fn peek(paths: &Paths) -> Option<Self> {
        Self::read(paths).flatten()
    }

    /// `None` without a readable `shutdown.json`; `Some(None)` when the
    /// one there is a dead runner's.
    fn read(paths: &Paths) -> Option<Option<Self>> {
        let state: Value =
            serde_json::from_str(&fs::read_to_string(paths.shutdown_state_file()).ok()?).ok()?;
        let pid = state
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok())
            .filter(|&p| p != 0);
        if let Some(pid) = pid
            && process::pid_alive(pid)
            && process::cmdline(pid).iter().any(|a| a == b"shutdown")
        {
            return Some(Some(Self {
                pid,
                deadline: state.get("deadline").and_then(Value::as_f64).unwrap_or(0.0),
                poweroff: state
                    .get("poweroff")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            }));
        }
        Some(None)
    }
}

/// Removes `shutdown.json` if it is this process's.
pub fn clear_state(paths: &Paths) {
    let ours = fs::read_to_string(paths.shutdown_state_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|state| state.get("pid").and_then(Value::as_u64))
        == Some(u64::from(std::process::id()));
    if ours {
        let _ = fs::remove_file(paths.shutdown_state_file());
    }
}

/// `viy shutdown --cancel`: ends the scheduled shutdown's runner. False
/// when none was scheduled. `quiet` says nothing.
pub async fn cancel(paths: &Paths, quiet: bool, report: Reporter<'_>) -> io::Result<bool> {
    let Some(scheduled) = Scheduled::live(paths) else {
        if !quiet {
            say(report, Tone::Plain, "no shutdown is scheduled");
        }
        return Ok(false);
    };
    let pid = scheduled.pid;
    tokio::task::spawn_blocking(move || process::kill(pid, Duration::from_secs(3)))
        .await
        .map_err(io::Error::other)?;
    let _ = fs::remove_file(paths.shutdown_state_file());
    if !quiet {
        let at = local(scheduled.deadline)
            .map_or_else(String::new, |at| at.format("%H:%M:%S").to_string());
        say(
            report,
            Tone::Plain,
            format!("cancelled the shutdown scheduled for {at}"),
        );
    }
    Ok(true)
}

/// `viy shutdown --status`.
pub fn status(host: &Host<'_>, report: Reporter<'_>) {
    let print = |text: String| say(report, Tone::Plain, text);
    let Some(scheduled) = Scheduled::live(host.paths) else {
        print("no shutdown is scheduled".into());
        return;
    };
    let tool = process::tool_of(&process::cmdline(scheduled.pid)).map_or("viy", |t| t.name());
    let what = if scheduled.poweroff {
        format!("chats close, {tool} down, then poweroff")
    } else {
        format!("chats close, {tool} down (no poweroff)")
    };
    let at = local(scheduled.deadline)
        .map_or_else(String::new, |at| at.format("%a %H:%M:%S").to_string());
    let left = scheduled.deadline - regions::now(host.time);
    print(format!("shutdown at {at} (in {}): {what}", fmt(left)));
    print(format!(
        "pid {}, log {}",
        scheduled.pid,
        host.paths.shutdown_log_file().display()
    ));
    let chats = host.chats.find();
    let pids = if chats.is_empty() {
        String::new()
    } else {
        format!(" ({})", joined(&chats))
    };
    print(format!("open chats right now: {}{pids}", chats.len()));
}

/// `viy shutdown WHEN`, up to the runner: the deadline from `when`, what
/// would fail flagged, a previous shutdown replaced, the plan said. The
/// deadline, Unix seconds. `foreground`: the countdown stays in this
/// terminal, so the desktop warnings matter less.
pub async fn schedule(
    host: &Host<'_>,
    cfg: &Config,
    when: &str,
    poweroff: bool,
    foreground: bool,
    report: Reporter<'_>,
) -> Result<f64, CommandError> {
    let now = host.time.now();
    let delay =
        parse_when(when, now.with_timezone(&Local).naive_local()).map_err(CommandError::Exit)?;
    let deadline = regions::now(host.time) + delay;
    let at = local(deadline).ok_or_else(|| {
        CommandError::Exit(format!(
            "not a duration: '{when}' (try 90m, 1h30m or 23:30)"
        ))
    })?;

    let mut problems = down_problems(cfg);
    if poweroff {
        problems.extend(poweroff_problems(host).await);
    }
    if !foreground {
        problems.extend(notify_problems(host));
    }
    for problem in problems {
        say(report, Tone::Warn, format!("FLAG: {problem}"));
    }

    if cancel(host.paths, true, report).await? {
        say(
            report,
            Tone::Dim,
            "replaced the previously scheduled shutdown",
        );
    }
    let steps = if poweroff {
        "close open chats, viy down, power off this PC"
    } else {
        "close open chats, viy down"
    };
    let chats = host.chats.find();
    say(
        report,
        Tone::Ok,
        format!(
            "At {} (in {}): {steps}.",
            at.format("%a %H:%M:%S"),
            fmt(delay)
        ),
    );
    say(
        report,
        Tone::Dim,
        format!(
            "open chats right now: {}. Chats opened later are closed too.",
            chats.len()
        ),
    );
    fs::create_dir_all(host.paths.pod_state_dir())?;
    Ok(deadline)
}

/// The detached runner for `deadline`: `viy` (`exe`) `shutdown --run-at`,
/// in its own session, writing to `shutdown.log`.
pub fn start_runner(
    host: &Host<'_>,
    exe: &Path,
    deadline: f64,
    poweroff: bool,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let mut command = viy_exec::Command::new(exe)
        .args(["shutdown", "--run-at"])
        .arg(deadline.to_string());
    if !poweroff {
        command = command.arg("--no-poweroff");
    }
    let log_path = host.paths.shutdown_log_file();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let child = host.exec.spawn(command, Spawn::Logged { log })?;
    let pid = child.id();
    Scheduled {
        pid,
        deadline,
        poweroff,
    }
    .save(host.paths)?;
    say(
        report,
        Tone::Dim,
        format!(
            "running in the background (pid {pid}); log: {}\nviy shutdown --status shows it, \
             viy shutdown --cancel stops it.",
            log_path.display()
        ),
    );
    Ok(())
}

#[cfg(test)]
#[path = "shutdown_tests.rs"]
mod tests;
