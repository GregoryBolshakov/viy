//! Waiting for a booting pod, and the report of its boot: rpctl pod.py's
//! wait section (`StartupError`, `_redact_log`, `_startup_progress`,
//! `_wait_with_clock`, `wait_ssh`, `wait_ready`, `_await_endpoint`,
//! `_report_boot`, `_report_watchdog`, `_await_recorded`, `_spawn_capture`,
//! `capture_boot`).
//!
//! A wait is a future that reports as it goes ([`Report`]): the terminal
//! prints it, the chat turns it into events while it runs in the
//! background (kb d041). rpctl runs each probe on a thread so the clock
//! keeps ticking; here the clock and the probe are one `select!`. Probes
//! start on whole seconds of the wait, every `poll`, and their answer
//! counts the moment it comes (flags.md F16, F46).
//!
//! This is the boot path (kb d004): nothing here may make a boot slower.
//! The report runs after the ready message, and the boot's logs are
//! collected afterwards by a detached `viy` ([`CAPTURE_SUBCOMMAND`]).

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use regex::Regex;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use tokio::time::Instant;
use viy_config::Config;
use viy_exec::Command;
use viy_exec::Spawn;
use viy_exec::exit_text;
use viy_http::HttpClient;
use viy_http::HttpError;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;
use viy_time::TimeProvider;

use crate::api::LogEntry;
use crate::api::RunPod;
use crate::api::proxy_url;
use crate::bootreport;
use crate::bootreport::Boot;
use crate::bootreport::Outcome;
use crate::bootreport::Records;
use crate::bootreport::Timeline;
use crate::compile_regex;
use crate::idle;
use crate::notice::Alert;
use crate::notice::Clock;
use crate::notice::Notice;
use crate::notice::Report;
use crate::notice::Reporter;
use crate::notice::Tone;
use crate::ssh::Ssh;
use crate::ssh::SshWatch;
use crate::ssh::ssh_endpoint;
use crate::tracked;
use crate::tunnel;
use crate::tunnel::Guard;
use crate::tunnel::GuardError;

/// How long vLLM may report ready inside the pod while the local endpoint
/// still fails before the wait says so out loud, rather than silently
/// running down the clock on a pod that is billing.
pub const UNREACHABLE_WARN: Duration = Duration::from_secs(90);

/// vLLM's /health is asked this often, sshd half as often.
pub const HEALTH_POLL: Duration = Duration::from_secs(1);
pub const SSH_POLL: Duration = Duration::from_secs(2);

/// The pod's log is summarised at most this often.
const PROGRESS_EVERY: Duration = Duration::from_secs(20);

/// The hidden `viy` subcommand that collects a boot's logs:
/// `viy capture-boot <boot_id> <pod_id>`, run by [`spawn_capture`].
pub const CAPTURE_SUBCOMMAND: &str = "capture-boot";

/// Why a wait ended without vLLM.
#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    /// rpctl's `StartupError`: the boot cannot succeed, and this says why.
    #[error("{0}")]
    Startup(String),
    /// Ctrl-C.
    #[error("interrupted")]
    Interrupted,
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn say(report: Reporter<'_>, tone: Tone, text: impl Into<String>) {
    report(Report::Notice(Notice::new(tone, text)));
}

/// `line` with the config's secrets and anything shaped like a key masked,
/// for log files written to disk.
pub fn redact_log(line: &str, cfg: &Config) -> String {
    static KEY_SHAPED: LazyLock<Regex> =
        LazyLock::new(|| compile_regex(r"\b(?:sk-|hf_|rpa_)[A-Za-z0-9_-]+"));
    let mut line = line.to_string();
    for (key, value) in cfg.iter() {
        let secret = ["API_KEY", "TOKEN", "PASSWORD", "SECRET"]
            .iter()
            .any(|part| key.contains(part));
        if secret && value.chars().count() >= 8 {
            line = line.replace(value, "[redacted]");
        }
    }
    KEY_SHAPED.replace_all(&line, "[redacted]").into_owned()
}

// ------------------------------------------------------------ progress

/// What the pod's log says the boot is doing, for the wait's clock. The
/// log is fetched in the background at most every 20 s, so a slow fetch
/// (up to 8 s) never delays the next probe.
#[derive(Debug, Clone)]
pub struct Progress {
    rp: RunPod,
    pod_id: Arc<str>,
    /// An `RPCTL_ERROR` line ends the wait: sshd failed on the pod.
    require_ssh: bool,
    summary: Arc<Mutex<Summary>>,
}

#[derive(Debug, Default)]
struct Summary {
    fetched: Option<Instant>,
    fetching: bool,
    text: String,
    /// The log last showed vLLM serving inside the pod.
    ready: bool,
    /// A layer of the image failed and restarted.
    retried: bool,
    failure: Option<String>,
}

impl Progress {
    pub fn new(rp: &RunPod, pod_id: &str, require_ssh: bool) -> Self {
        Self {
            rp: rp.clone(),
            pod_id: pod_id.into(),
            require_ssh,
            summary: Arc::default(),
        }
    }

    fn summary(&self) -> std::sync::MutexGuard<'_, Summary> {
        self.summary.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The summary so far, empty before the first fetch; fetches again in
    /// the background when it is due. Fails once the log showed the boot
    /// cannot succeed.
    pub fn poll(&self) -> Result<String, WaitError> {
        let mut summary = self.summary();
        if summary.failure.is_none()
            && !summary.fetching
            && summary
                .fetched
                .is_none_or(|at| at.elapsed() >= PROGRESS_EVERY)
        {
            summary.fetched = Some(Instant::now());
            summary.fetching = true;
            let this = self.clone();
            tokio::spawn(async move { this.refresh().await });
        }
        summary.result()
    }

    /// Whether the log last showed vLLM serving inside the pod.
    pub fn pod_ready(&self) -> bool {
        self.summary().ready
    }

    async fn refresh(&self) {
        let entries = self
            .rp
            .pod_logs(&self.pod_id, 30, Duration::from_secs(8))
            .await
            .unwrap_or_default();
        let mut summary = self.summary();
        summary.summarise(&entries, self.require_ssh);
        summary.fetching = false;
    }
}

/// Lines of an image pull. A pull writes a "<layer> Downloading" line
/// several times a second, so the 30-line window rarely holds anything else.
const PULL_WORDS: [&str; 8] = [
    "Downloading",
    "Extracting",
    "Pulling",
    "fetching image",
    "Verifying Checksum",
    "Download complete",
    "Pull complete",
    "Waiting",
];

impl Summary {
    fn result(&self) -> Result<String, WaitError> {
        match &self.failure {
            Some(failure) => Err(WaitError::Startup(failure.clone())),
            None => Ok(self.text.clone()),
        }
    }

    fn summarise(&mut self, entries: &[LogEntry], require_ssh: bool) {
        for entry in entries {
            let line = entry.line.as_str();
            let lower = line.to_lowercase();
            let source = entry.source.as_deref();
            if require_ssh && line.contains("RPCTL_ERROR") {
                self.failure = Some(
                    "SSH setup failed; run viy logs for the installation/daemon error.".into(),
                );
                return;
            }
            // Lines come oldest first, so a later boot clears an earlier ready.
            if line.contains("=== boot ") || line.contains("RPCTL_PHASE starting vLLM") {
                self.ready = false;
            } else if line.contains("RPCTL_PHASE model ready")
                || line.contains("HTTP server started")
            {
                self.ready = true;
            }
            if let Some((_, phase)) = line.split_once("RPCTL_PHASE ") {
                self.text = phase.chars().take(160).collect();
            } else if line.contains("Application startup complete")
                || line.contains("HTTP server started")
            {
                self.text = "model ready inside pod".into();
            } else if line.contains("Capturing CUDA graphs") || lower.contains("warmup") {
                self.text = "warming model / capturing graphs".into();
            } else if lower.contains("compil") && source == Some("container") {
                self.text = "loading compilation cache / compiling".into();
            } else if line.contains("Loading safetensors")
                || line.contains("Starting to load model")
            {
                self.text = "loading model weights".into();
            } else if source == Some("system") && line.contains("Retrying in") {
                self.retried = true;
                self.text = "pulling container image (a layer failed and restarted)".into();
            } else if source == Some("system") && PULL_WORDS.iter().any(|w| line.contains(w)) {
                self.text = if self.retried {
                    "pulling container image (a layer failed and restarted)".into()
                } else {
                    "pulling container image".into()
                };
            }
        }
    }
}

// ------------------------------------------------------------ the clock

/// What one probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step<T> {
    /// The wait is over.
    Done(T),
    /// Not yet; the clock shows this.
    Waiting(String),
}

/// One look at what a wait waits for. Its future is `Send`, so a wait can
/// run as a task of its own (kb d041).
pub trait Probe: Send {
    type Done;

    fn probe(&mut self) -> impl Future<Output = Result<Step<Self::Done>, WaitError>> + Send;
}

/// A closure that makes each probe's future.
impl<F, Fut, T> Probe for F
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = Result<Step<T>, WaitError>> + Send,
{
    type Done = T;

    fn probe(&mut self) -> impl Future<Output = Result<Step<T>, WaitError>> + Send {
        self()
    }
}

/// Runs `probe` every `poll` while the clock ticks each second, until a
/// probe is [`Step::Done`] or `timeout` has passed (`None`). Probes start
/// on whole seconds of the wait; one still running at the timeout is
/// dropped.
pub async fn wait_with_clock<P: Probe>(
    label: &'static str,
    timeout: Duration,
    poll: Duration,
    mut probe: P,
    report: Reporter<'_>,
) -> Result<Option<P::Done>, WaitError> {
    if timeout.is_zero() {
        return Ok(None);
    }
    let started = Instant::now();
    let clock = |elapsed: u64, state: &str| {
        report(Report::Clock(Clock {
            elapsed,
            label,
            state: state.to_string(),
        }));
    };
    let mut state = "starting".to_string();
    let mut second = 0;
    let mut next_probe = Duration::ZERO;
    clock(second, &state);
    loop {
        while Duration::from_secs(second) < next_probe {
            second = tick(started, second).await;
            if Duration::from_secs(second) >= timeout {
                return Ok(None);
            }
            clock(second, &state);
        }
        let step = {
            let probing = probe.probe();
            tokio::pin!(probing);
            loop {
                tokio::select! {
                    biased;
                    step = &mut probing => break step?,
                    now = tick(started, second) => {
                        second = now;
                        if Duration::from_secs(second) >= timeout {
                            return Ok(None);
                        }
                        clock(second, &state);
                    }
                }
            }
        };
        match step {
            Step::Done(value) => return Ok(Some(value)),
            Step::Waiting(now) => state = now,
        }
        next_probe = (next_probe + poll).max(started.elapsed());
    }
}

/// Sleeps to the whole second after `second` of the wait; the second it
/// woke in.
async fn tick(started: Instant, second: u64) -> u64 {
    tokio::time::sleep_until(started + Duration::from_secs(second + 1)).await;
    started.elapsed().as_secs()
}

// ------------------------------------------------------------ the waits

/// `wait_ssh`'s probe: the endpoint, or the boot's progress meanwhile.
struct SshProbe<'a> {
    watch: SshWatch<'a>,
    progress: Option<&'a Progress>,
}

impl Probe for SshProbe<'_> {
    type Done = (String, u16);

    async fn probe(&mut self) -> Result<Step<(String, u16)>, WaitError> {
        match self.watch.probe().await {
            Ok(endpoint) => Ok(Step::Done(endpoint)),
            Err(state) => {
                let shown = match self.progress {
                    Some(progress) => progress.poll()?,
                    None => String::new(),
                };
                Ok(Step::Waiting(if shown.is_empty() { state } else { shown }))
            }
        }
    }
}

/// Waits for the pod to publish a TCP mapping for port 22 and accept on
/// it: its address, or `None` at the timeout.
pub async fn wait_ssh(
    rp: &RunPod,
    pod_id: &str,
    timeout: Duration,
    progress: Option<&Progress>,
    report: Reporter<'_>,
) -> Result<Option<(String, u16)>, WaitError> {
    let probe = SshProbe {
        watch: SshWatch::new(rp, pod_id),
        progress,
    };
    wait_with_clock("waiting for sshd", timeout, SSH_POLL, probe, report).await
}

/// The tunnel watched while vLLM is awaited through it ([`Guard`]).
pub struct TunnelGuard<'a> {
    guard: Guard,
    ssh: Ssh<'a>,
    rp: &'a RunPod,
}

impl<'a> TunnelGuard<'a> {
    pub fn new(guard: Guard, ssh: Ssh<'a>, rp: &'a RunPod) -> Self {
        Self { guard, ssh, rp }
    }

    async fn check(&mut self, report: Reporter<'_>) -> Result<(), WaitError> {
        let mut said = |notice| report(Report::Notice(notice));
        match self.guard.check(&self.ssh, self.rp, &mut said).await {
            Ok(()) => Ok(()),
            Err(GuardError::GaveUp { message, alert }) => {
                report(Report::Alert(alert));
                Err(WaitError::Startup(message))
            }
            Err(GuardError::Io(error)) => Err(error.into()),
        }
    }
}

/// What a failed /health request shows on the clock: the kind of failure,
/// by `requests`' names.
fn failure_name(error: &HttpError) -> &'static str {
    match error {
        HttpError::Timeout => "Timeout",
        HttpError::Connect(_) | HttpError::Network(_) => "ConnectionError",
        HttpError::Build(_) => "InvalidURL",
    }
}

/// `wait_ready`'s probe: the tunnel looked at, then one /health request.
struct HealthProbe<'a, 'g> {
    http: &'a dyn HttpClient,
    url: &'a str,
    api_key: Option<&'a str>,
    progress: Option<&'a Progress>,
    guard: Option<&'a mut TunnelGuard<'g>>,
    report: Reporter<'a>,
    /// Since when vLLM has been ready in the pod while /health failed.
    stuck: Option<Instant>,
    warned: bool,
}

impl Probe for HealthProbe<'_, '_> {
    type Done = ();

    async fn probe(&mut self) -> Result<Step<()>, WaitError> {
        let (url, report) = (self.url, self.report);
        if let Some(guard) = self.guard.as_deref_mut() {
            guard.check(report).await?;
        }
        let mut request = Request::new(Method::GET, format!("{url}/health"))
            .header("User-Agent", user_agent::RP_REQUESTS)
            .header("Accept", "*/*")
            .timeouts(Duration::from_secs(5), Duration::from_secs(5));
        if let Some(key) = self.api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        let state = match viy_http::send(self.http, request).await {
            Ok(response) if response.status == 200 => return Ok(Step::Done(())),
            Ok(response) => format!("HTTP {}", response.status),
            Err(error) => failure_name(&error).to_string(),
        };
        let Some(progress) = self.progress else {
            return Ok(Step::Waiting(state));
        };
        let mut shown = progress.poll()?;
        if shown.is_empty() {
            shown.clone_from(&state);
        }
        if !progress.pod_ready() {
            self.stuck = None;
            return Ok(Step::Waiting(shown));
        }
        let since = *self.stuck.get_or_insert_with(Instant::now);
        if !self.warned && since.elapsed() >= UNREACHABLE_WARN {
            self.warned = true;
            say(
                report,
                Tone::Error,
                format!(
                    "vLLM reports ready inside the pod, but {url} has failed for {}s \
                     ({state}). The pod is billing meanwhile.",
                    UNREACHABLE_WARN.as_secs()
                ),
            );
            say(
                report,
                Tone::Warn,
                "Check: viy tunnel --status, viy logs. Ctrl-C and re-run viy up to reattach \
                 to the same pod.",
            );
            report(Report::Alert(Alert {
                title: "viy up: pod ready but unreachable".into(),
                body: format!("{url} -> {state}"),
                urgent: true,
            }));
        }
        Ok(Step::Waiting(format!(
            "ready in pod, local endpoint: {state}"
        )))
    }
}

/// Polls vLLM's `/health` at `url` until it answers 200 (true) or the
/// timeout passes (false). `guard` looks at the tunnel before every poll,
/// and may restart it or end the wait.
pub async fn wait_ready(
    http: &dyn HttpClient,
    url: &str,
    api_key: Option<&str>,
    timeout: Duration,
    progress: Option<&Progress>,
    guard: Option<&mut TunnelGuard<'_>>,
    report: Reporter<'_>,
) -> Result<bool, WaitError> {
    let probe = HealthProbe {
        http,
        url,
        api_key,
        progress,
        guard,
        report,
        stuck: None,
        warned: false,
    };
    let ready = wait_with_clock("waiting for vLLM", timeout, HEALTH_POLL, probe, report).await?;
    Ok(ready.is_some())
}

/// What a wait for a booting pod works with.
#[derive(Clone, Copy)]
pub struct PodWait<'a> {
    /// Paths, config and the processes.
    pub ssh: Ssh<'a>,
    pub rp: &'a RunPod,
    /// What reaches vLLM's /health.
    pub http: &'a dyn HttpClient,
    pub time: &'a dyn TimeProvider,
    pub pod_id: &'a str,
    /// The program that collects a boot's logs: viy itself.
    pub exe: &'a Path,
}

/// A wait's result, or `None` after a startup error, said and marked.
fn settle<T>(
    result: Result<T, WaitError>,
    timeline: &mut Timeline<'_>,
    report: Reporter<'_>,
) -> Result<Option<T>, WaitError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(WaitError::Startup(message)) => {
            timeline.mark("startup error", [("message", json!(message))]);
            say(report, Tone::Error, message);
            Ok(None)
        }
        Err(other) => Err(other),
    }
}

fn healthy(timeline: &mut Timeline<'_>, ready: bool) -> bool {
    timeline.mark(
        if ready {
            "vLLM healthy"
        } else {
            "timed out waiting for vLLM"
        },
        [],
    );
    ready
}

/// Waits for vLLM and returns the base URL it answered on, or `None`.
///
/// A tunnel-only pod publishes no http port, so the only way to probe it
/// is through the forward: wait for sshd, raise the tunnel, then poll the
/// local end, within the one `timeout`. That leaves the tunnel up, which
/// is what the next command needs.
pub async fn await_endpoint(
    w: &PodWait<'_>,
    timeout: Duration,
    timeline: &mut Timeline<'_>,
    report: Reporter<'_>,
) -> Result<Option<String>, WaitError> {
    let started = Instant::now();
    let cfg = w.ssh.cfg;
    let tunnel_only = tracked::tunnel_only(w.ssh.paths, cfg)?;
    let progress = Progress::new(w.rp, w.pod_id, tunnel_only);
    let api_key = cfg.get("VLLM_API_KEY");
    if !tunnel_only {
        let url = proxy_url(w.pod_id, cfg.get("VLLM_PORT").unwrap_or_default());
        say(report, Tone::Plain, format!("Endpoint: {url}"));
        say(
            report,
            Tone::Warn,
            "Traffic to this URL is decrypted by RunPod's proxy. Set TUNNEL=1 and recreate the \
             pod to avoid that.",
        );
        let waited = wait_ready(
            w.http,
            &url,
            api_key,
            timeout,
            Some(&progress),
            None,
            report,
        )
        .await;
        let Some(ready) = settle(waited, timeline, report)? else {
            return Ok(None);
        };
        return Ok(healthy(timeline, ready).then_some(url));
    }

    say(
        report,
        Tone::Dim,
        "Tunnel mode: no http port is exposed, so the endpoint is reached over SSH.",
    );
    let waited = wait_ssh(w.rp, w.pod_id, timeout, Some(&progress), report).await;
    let Some(endpoint) = settle(waited, timeline, report)? else {
        return Ok(None);
    };
    let Some((ip, port)) = endpoint else {
        timeline.mark("sshd never came up", []);
        say(
            report,
            Tone::Error,
            "sshd never came up, and a tunnel-only pod exposes nothing else.",
        );
        say(
            report,
            Tone::Warn,
            format!(
                "Run viy logs for platform and SSH errors, or use the web console at \
                 https://console.runpod.io/pods/{}.",
                w.pod_id
            ),
        );
        return Ok(None);
    };
    timeline.mark(
        "ssh reachable",
        [("host", json!(ip)), ("port", json!(port))],
    );
    say(
        report,
        Tone::Dim,
        format!(
            "SSH reachable after {:.0}s.",
            started.elapsed().as_secs_f64()
        ),
    );
    let mut said = |notice| report(Report::Notice(notice));
    if !tunnel::start(&w.ssh, w.rp, &ip, port, false, &mut said).await? {
        timeline.mark("tunnel failed to start", []);
        return Ok(None);
    }
    timeline.mark("tunnel up", []);
    let url = tracked::local_url(cfg);
    say(report, Tone::Plain, format!("Endpoint: {url}"));
    let remaining = timeout.saturating_sub(started.elapsed());
    let mut guard = TunnelGuard::new(Guard::new(w.pod_id, ip, port), w.ssh, w.rp);
    let waited = if remaining.is_zero() {
        Ok(false)
    } else {
        wait_ready(
            w.http,
            &url,
            api_key,
            remaining,
            Some(&progress),
            Some(&mut guard),
            report,
        )
        .await
    };
    let Some(ready) = settle(waited, timeline, report)? else {
        return Ok(None);
    };
    Ok(healthy(timeline, ready).then_some(url))
}

/// [`await_endpoint`] until `interrupted` resolves (Ctrl-C). An
/// interrupted boot still leaves a record: the slow ones are the most
/// worth investigating.
pub async fn await_recorded(
    w: &PodWait<'_>,
    timeout: Duration,
    started: DateTime<Utc>,
    booting: bool,
    timeline: &mut Timeline<'_>,
    interrupted: impl Future<Output = ()>,
    report: Reporter<'_>,
) -> Result<Option<String>, WaitError> {
    let ended = tokio::select! {
        biased;
        () = interrupted => None,
        result = await_endpoint(w, timeout, timeline, report) => Some(result),
    };
    if let Some(result) = ended {
        return result;
    }
    if booting {
        timeline.mark("interrupted (Ctrl-C)", []);
        report_boot(
            w,
            started,
            w.time.now(),
            Outcome::Interrupted,
            timeline,
            report,
        )
        .await;
    }
    Err(WaitError::Interrupted)
}

// ------------------------------------------------------------ the report

/// Says the stage breakdown of the boot that just ended and records it,
/// then starts the collection of its logs. Runs after the ready message,
/// so it never delays the endpoint; a report that fails says so and
/// nothing else. The boot's length in seconds.
pub async fn report_boot(
    w: &PodWait<'_>,
    started: DateTime<Utc>,
    ready: DateTime<Utc>,
    outcome: Outcome,
    timeline: &Timeline<'_>,
    report: Reporter<'_>,
) -> f64 {
    let total = (ready - started).as_seconds_f64();
    let over = match record_boot(w, started, ready, total, outcome, timeline, report).await {
        Ok(over) => over,
        Err(error) => {
            say(
                report,
                Tone::Dim,
                format!("(boot report unavailable: {error})"),
            );
            return total;
        }
    };
    if over && outcome == Outcome::Ready {
        report(Report::Alert(Alert {
            title: "viy up: over the boot budget".into(),
            body: format!(
                "{total:.0}s against {}s; see the breakdown",
                bootreport::BUDGET_S
            ),
            urgent: true,
        }));
    }
    total
}

/// [`report_boot`]'s work; whether the boot was over budget.
async fn record_boot(
    w: &PodWait<'_>,
    started: DateTime<Utc>,
    ready: DateTime<Utc>,
    total: f64,
    outcome: Outcome,
    timeline: &Timeline<'_>,
    report: Reporter<'_>,
) -> io::Result<bool> {
    let entries =
        w.rp.pod_logs(w.pod_id, 600, Duration::from_secs(8))
            .await
            .unwrap_or_default();
    let (stages, details) = bootreport::measure(bootreport::latest_boot(&entries), started, ready);
    let (text, over) = bootreport::render(&stages, &details, total, bootreport::BUDGET_S, outcome);
    let tone = if over || outcome != Outcome::Ready {
        Tone::Error
    } else {
        Tone::Dim
    };
    say(report, tone, text);
    let facts = bootreport::facts(&entries);
    let records = Records::new(w.ssh.paths);
    let boot = Boot {
        pod_id: w.pod_id,
        stages: &stages,
        details: &details,
        total,
        outcome,
        facts: &facts,
    };
    let row = records.record(w.time, w.ssh.cfg, boot, timeline)?;
    report_watchdog(w.ssh.cfg, facts.watchdog.as_deref(), report);
    let boot_id = row
        .get("boot_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    spawn_capture(w, boot_id)?;
    say(
        report,
        Tone::Dim,
        format!(
            "Boot record: viy boots show   ({})",
            records.record_path(boot_id).display()
        ),
    );
    Ok(over)
}

/// What the pod's idle watchdog reported: it learns within seconds of the
/// container starting whether RunPod accepts its key, so this is known by
/// the time vLLM serves. A watchdog that cannot stop the pod must be loud.
pub fn report_watchdog(cfg: &Config, state: Option<&str>, report: Reporter<'_>) {
    let limits = idle::limits(cfg);
    match state {
        Some("armed") => say(
            report,
            Tone::Dim,
            format!(
                "Idle watchdog armed: the pod terminates itself after {:.0} min without tokens, \
                 or after {:.0} h.",
                limits.ttl.as_secs_f64() / 60.0,
                limits.max.as_secs_f64() / 3600.0
            ),
        ),
        Some(state) if state.starts_with("CANNOT") => say(
            report,
            Tone::Warn,
            format!(
                "Idle watchdog {}. viy reap on this machine is the only guard against a \
                 forgotten pod; viy down when you are done.",
                state.to_lowercase()
            ),
        ),
        Some("off") => {}
        _ => say(
            report,
            Tone::Warn,
            "Idle watchdog: no report from the pod yet (see viy logs).",
        ),
    }
}

/// Starts `viy capture-boot` in a session of its own, so `up` returns at
/// once; its errors go to `<boot_id>.capture.err`.
pub fn spawn_capture(w: &PodWait<'_>, boot_id: &str) -> io::Result<()> {
    let records = Records::new(w.ssh.paths);
    fs::create_dir_all(w.ssh.paths.boots_dir())?;
    let stderr = fs::File::create(records.file(boot_id, "capture.err"))?;
    let command = Command::new(w.exe).args([CAPTURE_SUBCOMMAND, boot_id, w.pod_id]);
    // Dropped, it runs on; tokio reaps it if viy outlives it.
    w.ssh.exec.spawn(command, Spawn::Detached { stderr })?;
    Ok(())
}

/// The shell script that prints this boot's part of the pod's vllm.log,
/// then the tail of its sshd.log.
const VLLM_LOG_SCRIPT: &str = "awk 'index($0,\"=== boot \")==1{n++} {l[n]=l[n] $0 \"\\n\"} \
     END{printf \"%s\", l[n]}' /workspace/vllm.log | head -n 4000; \
     echo '=== sshd.log (tail) ==='; tail -n 100 /workspace/sshd.log";

/// `viy capture-boot`: writes the boot's platform log, the pod's vllm.log
/// for this boot and the sshd.log tail next to its record, secrets
/// masked, and adds the host's facts to the record.
pub async fn capture_boot(
    ssh: &Ssh<'_>,
    rp: &RunPod,
    boot_id: &str,
    pod_id: &str,
) -> io::Result<()> {
    let records = Records::new(ssh.paths);
    let mut data = match records.read(boot_id) {
        Some(Value::Object(data)) => data,
        _ => Map::new(),
    };
    let mut files = match data.remove("files") {
        Some(Value::Object(files)) => files,
        _ => Map::new(),
    };
    match rp.pod_logs(pod_id, 5000, Duration::from_secs(40)).await {
        Ok(entries) => {
            let path = records.file(boot_id, "platform.log");
            let text: String = entries
                .iter()
                .map(|e| {
                    format!(
                        "{} [{}] {}\n",
                        e.ts.as_deref().unwrap_or(""),
                        e.source.as_deref().unwrap_or("?"),
                        redact_log(&e.line, ssh.cfg)
                    )
                })
                .collect();
            fs::write(&path, text)?;
            files.insert("platform_log".into(), json!(path.display().to_string()));
            let facts = data.entry("facts").or_insert_with(|| json!({}));
            if let Value::Object(facts) = facts {
                facts.extend(bootreport::facts(&entries).to_json());
            }
        }
        Err(error) => {
            files.insert(
                "platform_log".into(),
                json!(format!("unavailable: {error}")),
            );
        }
    }
    if let Err(error) =
        capture_vllm_log(ssh, rp, &records, boot_id, pod_id, &mut data, &mut files).await
    {
        files
            .entry("vllm_log")
            .or_insert_with(|| json!(format!("unavailable: {error}")));
    }
    data.insert("files".into(), Value::Object(files));
    records.write(boot_id, &Value::Object(data))?;
    let err = records.file(boot_id, "capture.err");
    if fs::metadata(&err).is_ok_and(|m| m.len() == 0) {
        fs::remove_file(err)?;
    }
    Ok(())
}

/// The host's facts into `data`, and the pod's vllm.log over ssh.
async fn capture_vllm_log(
    ssh: &Ssh<'_>,
    rp: &RunPod,
    records: &Records<'_>,
    boot_id: &str,
    pod_id: &str,
    data: &mut Map<String, Value>,
    files: &mut Map<String, Value>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let info = rp.get_pod(pod_id).await?.unwrap_or(Value::Null);
    let endpoint = ssh_endpoint(&info);
    let machine = info.get("machineId").cloned().unwrap_or(Value::Null);
    let mut host = Map::new();
    host.insert("machine_id".into(), machine.clone());
    host.insert(
        "public_ip".into(),
        json!(endpoint.as_ref().map(|(ip, _)| ip)),
    );
    host.insert(
        "cost_per_hr".into(),
        info.get("costPerHr").cloned().unwrap_or(Value::Null),
    );
    let previous = records
        .history(20)
        .into_iter()
        .rev()
        .filter(|row| row.get("boot_id").and_then(Value::as_str) != Some(boot_id))
        .filter_map(|row| records.read(row.get("boot_id")?.as_str()?))
        .find_map(|other| other.get("host").filter(|h| truthy_host(h)).cloned());
    if let Some(other) = previous {
        let same = other.get("machine_id").unwrap_or(&Value::Null) == &machine;
        host.insert("same_machine_as_previous_boot".into(), json!(same));
    }
    data.insert("host".into(), Value::Object(host));
    let Some((ip, port)) = endpoint else {
        return Ok(());
    };
    let command = Command::new("ssh")
        .args(ssh.opts("yes")?)
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-p"])
        .arg(port.to_string())
        .arg(format!("root@{ip}"))
        .arg(VLLM_LOG_SCRIPT)
        .timeout(Duration::from_secs(60));
    let output = ssh.exec.run(command).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.is_empty() {
        files.insert(
            "vllm_log".into(),
            json!(format!(
                "unavailable: ssh exit {}",
                exit_text(output.status)
            )),
        );
        return Ok(());
    }
    let path = records.file(boot_id, "vllm.log");
    let text: String = stdout
        .lines()
        .map(|line| redact_log(line, ssh.cfg) + "\n")
        .collect();
    fs::write(&path, text)?;
    files.insert("vllm_log".into(), json!(path.display().to_string()));
    Ok(())
}

/// A host entry worth comparing with: a non-empty one.
fn truthy_host(host: &Value) -> bool {
    match host {
        Value::Object(map) => !map.is_empty(),
        Value::Null => false,
        _ => true,
    }
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;
