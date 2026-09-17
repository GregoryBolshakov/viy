//! The pod commands: rpctl pod.py's `up`, `status`, `stop`, `down` and
//! `logs`, with `ensure_volume`, `_create_pod`, `_our_pods` and `_sweep`.
//! Each is a library call that reports as it goes ([`Report`]), so the
//! `viy` subcommand of that name and the chat's slash command run the same
//! code (kb d041).
//!
//! `up`, `stop` and `down` hold `pod.lock` as rp's do (`crate::lock`). A
//! chat that runs `/up` in the background holds it as the chat's own pid,
//! which `down`'s preemption skips; its `/down` therefore cancels its `/up`
//! first (drops the future, which drops the lock) and tells [`down`] so
//! with `cancelled_up`, which then watches as long as it does after
//! killing another process's up.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::sync::watch;
use tokio::time::Instant;
use viy_config::Config;
use viy_config::Paths;
use viy_config::config::require_api_key;
use viy_config::config::tunnel_enabled;
use viy_config::profiles;
use viy_exec::ExecBackend;
use viy_http::HttpClient;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;
use viy_protocol::Answer;
use viy_protocol::Question;
use viy_time::TimeProvider;

use crate::api::RunPod;
use crate::api::RunPodError;
use crate::api::proxy_url;
use crate::api::shown;
use crate::bootreport::Outcome;
use crate::bootreport::Timeline;
use crate::failover::Creation;
use crate::failover::User;
use crate::idle;
use crate::lock::LockError;
use crate::lock::PodLock;
use crate::notice::Alert;
use crate::notice::Asker;
use crate::notice::Report;
use crate::notice::Reporter;
use crate::notice::Tone;
use crate::notice::say;
use crate::regions;
use crate::ssh::Ssh;
use crate::ssh::find_ssh_pubkey;
use crate::state;
use crate::state::key;
use crate::tracked;
use crate::tracked::ResolveError;
use crate::tunnel;
use crate::wait;
use crate::wait::PodWait;
use crate::wait::WaitError;

/// This machine's side of the pod commands.
#[derive(Clone, Copy)]
pub struct Env<'a> {
    pub paths: &'a Paths,
    pub exec: &'a dyn ExecBackend,
    /// RunPod's API and vLLM's /health.
    pub http: &'a Arc<dyn HttpClient>,
    pub time: &'a dyn TimeProvider,
    /// viy itself, which collects a boot's logs ([`wait::CAPTURE_SUBCOMMAND`]).
    pub exe: &'a Path,
}

impl<'a> Env<'a> {
    /// What the ssh and tunnel functions work with.
    pub fn ssh(&self, cfg: &'a Config) -> Ssh<'a> {
        Ssh {
            paths: self.paths,
            cfg,
            exec: self.exec,
        }
    }
}

/// How a pod command ended other than well.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// rpctl's `SystemExit(message)`: the message on stderr, exit status 1.
    #[error("{0}")]
    Exit(String),
    /// rpctl's `SystemExit(1)`: everything was said already.
    #[error("failed")]
    Failed,
    /// Ctrl-C: exit status 130.
    #[error("interrupted")]
    Interrupted,
    /// A RunPod call failed where rpctl lets the error reach `main`: on
    /// stderr, exit status 1.
    #[error(transparent)]
    RunPod(#[from] RunPodError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<ResolveError> for CommandError {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::RunPod(e) => Self::RunPod(e),
            ResolveError::State(e) => Self::Io(e),
        }
    }
}

impl From<WaitError> for CommandError {
    fn from(error: WaitError) -> Self {
        match error {
            WaitError::Startup(message) => Self::Exit(message),
            WaitError::Interrupted => Self::Interrupted,
            WaitError::Io(e) => Self::Io(e),
        }
    }
}

impl From<LockError> for CommandError {
    fn from(error: LockError) -> Self {
        match error {
            busy @ LockError::Busy { .. } => Self::Exit(busy.to_string()),
            LockError::Io(e) => Self::Io(e),
        }
    }
}

pub(crate) fn alert(report: Reporter<'_>, title: &str, body: impl Into<String>, urgent: bool) {
    report(Report::Alert(Alert {
        title: title.into(),
        body: body.into(),
        urgent,
    }));
}

/// Ctrl-C as it comes, counted. Each front end feeds it: the terminal's
/// SIGINT, the chat's own key.
#[derive(Debug, Clone)]
pub struct Interrupt(watch::Receiver<u32>);

impl Interrupt {
    /// The sender to count with, and the interrupt it feeds.
    pub fn channel() -> (watch::Sender<u32>, Self) {
        let (tx, rx) = watch::channel(0);
        (tx, Self(rx))
    }

    /// One that never comes.
    pub fn never() -> Self {
        Self::channel().1
    }

    pub(crate) fn count(&self) -> u32 {
        *self.0.borrow()
    }

    /// Resolves once `pred` holds for the count.
    async fn when(&self, pred: impl FnMut(&u32) -> bool) {
        let mut rx = self.0.clone();
        if rx.wait_for(pred).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// The next Ctrl-C from now.
    pub fn next(&self) -> impl Future<Output = ()> + Send + 'static {
        self.after(1)
    }

    /// The `n`th Ctrl-C from now.
    pub fn after(&self, n: u32) -> impl Future<Output = ()> + Send + 'static {
        let this = self.clone();
        let at = this.count() + n;
        async move { this.when(|c| *c >= at).await }
    }
}

/// The RunPod client, with the config's key.
pub fn connect(env: &Env<'_>, cfg: &Config) -> Result<RunPod, CommandError> {
    let key = require_api_key(cfg, env.paths).map_err(CommandError::Exit)?;
    Ok(RunPod::new(key, Arc::clone(env.http)))
}

/// Our pod as RunPod has it, several of that name said as rpctl does.
pub(crate) async fn resolve(
    rp: &RunPod,
    env: &Env<'_>,
    cfg: &Config,
    report: Reporter<'_>,
) -> Result<Option<Value>, ResolveError> {
    tracked::resolve(rp, env.paths, cfg, |many| {
        say(report, Tone::Warn, many.to_string());
    })
    .await
}

fn text<'v>(pod: &'v Value, key: &str) -> Option<&'v str> {
    pod.get(key).and_then(Value::as_str)
}

/// A number RunPod sent as a number or a string, as Python's `float()`
/// reads it; 0 when it is neither.
fn number(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Python's truthiness, for a value RunPod sent.
fn truthy(value: Option<&Value>) -> bool {
    crate::api::truthy(value.unwrap_or(&Value::Null))
}

fn cfg_at<'c>(cfg: &'c Config, key: &str) -> &'c str {
    cfg.get(key).unwrap_or("")
}

// ------------------------------------------------------------------ up

/// `viy up`: resumes or reuses our pod, else creates one, and waits for
/// vLLM within `timeout`. A Ctrl-C ends it, as rpctl's KeyboardInterrupt
/// does: during the wait for vLLM, a booting pod's boot is recorded first,
/// and a second Ctrl-C ends that too.
pub async fn up(
    env: &Env<'_>,
    cfg: &mut Config,
    timeout: Duration,
    interrupt: &Interrupt,
    report: Reporter<'_>,
    ask: Asker<'_>,
) -> Result<(), CommandError> {
    // The Ctrl-C that ends `up` at once: the next one, except during the
    // wait, whose own handling of the first records the boot.
    let abort_at = AtomicU32::new(interrupt.count() + 1);
    let aborted = interrupt.when(|c| *c >= abort_at.load(Ordering::SeqCst));
    tokio::select! {
        biased;
        result = up_until(env, cfg, timeout, interrupt, &abort_at, report, ask) => result,
        () = aborted => Err(CommandError::Interrupted),
    }
}

/// [`up`]'s work; `abort_at` is raised around the wait, and while
/// failover has new volumes to delete.
async fn up_until(
    env: &Env<'_>,
    cfg: &mut Config,
    timeout: Duration,
    interrupt: &Interrupt,
    abort_at: &AtomicU32,
    report: Reporter<'_>,
    ask: Asker<'_>,
) -> Result<(), CommandError> {
    let _lock = PodLock::exclusive(env.paths, "up")?;
    let mut started = Instant::now();
    let mut started_wall = env.time.now();
    let mut timeline = Timeline::start(env.time);
    let rp = connect(env, cfg)?;
    let vllm_key = profiles::ensure_vllm_api_key(cfg, env.paths)?;

    // Never create a duplicate: adopt any existing pod with our name first.
    let existing = match resolve(&rp, env, cfg, report).await {
        Ok(existing) => existing,
        Err(ResolveError::RunPod(error)) => {
            say(
                report,
                Tone::Error,
                "Could not check RunPod for an existing pod, so not creating one (that would \
                 risk a duplicate). Try again in a moment.",
            );
            say(report, Tone::Dim, error.to_string());
            return Err(CommandError::Failed);
        }
        Err(error) => return Err(error.into()),
    };
    if let Some(existing) = existing {
        let cfg = &*cfg;
        let pod_id = text(&existing, "id").unwrap_or_default().to_string();
        regions::note_pod(env.paths, &existing, regions::now(env.time))?;
        let status = existing.get("desiredStatus");
        let booting = text(&existing, "desiredStatus") == Some("EXITED");
        let name = cfg_at(cfg, "POD_NAME");
        if booting {
            say(
                report,
                Tone::Dim,
                format!("Resuming stopped pod {pod_id} (named '{name}')..."),
            );
            rp.start_pod(&pod_id).await?;
            timeline.mark("resume requested", [("pod_id", json!(pod_id))]);
        } else {
            say(
                report,
                Tone::Dim,
                format!(
                    "Pod {pod_id} named '{name}' already exists ({}); reusing it.",
                    shown(status)
                ),
            );
        }
        if state::load(env.paths)?.get(key::TUNNEL_ONLY).is_none() && tunnel_enabled(cfg) {
            say(
                report,
                Tone::Warn,
                "This pod predates tunnel mode. The tunnel works on it, but the pod also still \
                 exposes an http port through RunPod's proxy; only recreating it (viy down && \
                 viy up) closes that route.",
            );
        }
        let w = wait_for(env, cfg, &rp, &pod_id);
        abort_at.store(interrupt.count() + 2, Ordering::SeqCst);
        let ready = wait::await_recorded(
            &w,
            timeout,
            started_wall,
            booting,
            &mut timeline,
            interrupt.next(),
            report,
        )
        .await?;
        abort_at.store(interrupt.count() + 1, Ordering::SeqCst);
        if ready.is_some() {
            let ready_wall = env.time.now();
            let took = started.elapsed().as_secs_f64();
            say(
                report,
                Tone::Ok,
                format!("Ready in {took:.0}s. Run viy chat"),
            );
            idle::touch(env.paths, env.time, true);
            alert(
                report,
                "viy up: vLLM is serving",
                format!("ready in {took:.0}s"),
                false,
            );
            if booting {
                wait::report_boot(
                    &w,
                    started_wall,
                    ready_wall,
                    Outcome::Ready,
                    &timeline,
                    report,
                )
                .await;
            }
            let volume_id = tracked::pod_volume_id(&existing).unwrap_or_default();
            retire_replaced(env, cfg, &rp, volume_id, report).await;
            return Ok(());
        }
        if booting {
            let now = env.time.now();
            wait::report_boot(&w, started_wall, now, Outcome::Failed, &timeline, report).await;
        }
        say(
            report,
            Tone::Error,
            "Startup did not complete. Run viy logs. The pod remains allocated.",
        );
        alert(
            report,
            "viy up FAILED",
            "the pod is still billing; run viy logs",
            true,
        );
        return Err(CommandError::Failed);
    }

    let pubkey = find_ssh_pubkey(env.paths)?;
    if tunnel_enabled(cfg) && pubkey.is_none() {
        // sshd is the only route into a tunnel-only pod, and the key can
        // only be injected at creation, so making one now would strand it.
        return Err(CommandError::Exit(
            "Tunnel mode needs an SSH key, and there is none in ~/.ssh/.\n\
             The public key can only be injected when the pod is created, so a\n\
             pod made now would be reachable only through RunPod's web console.\n\
             \n  ssh-keygen -t ed25519      then re-run viy up\n\
             \n\
             Or accept RunPod's TLS-terminating proxy instead:  TUNNEL=0 viy up"
                .to_string(),
        ));
    }
    let (volume_id, volume_is_new) = ensure_volume(env, cfg, &rp, report).await?;
    timeline.mark("pod requested", []);
    let user = User {
        report,
        ask,
        interrupt,
        abort_at,
    };
    let created = Creation::new(env, cfg, &rp, &vllm_key, pubkey.as_deref(), user)
        .run(&volume_id, volume_is_new)
        .await?;
    // Time spent answering failover questions, or waiting between retries
    // for a free GPU, is the user's or RunPod's, not the boot's.
    started += created.waited;
    started_wall += chrono::Duration::from_std(created.waited).unwrap_or_default();
    // This boot's copy only: the boot record shows where it ran, and the
    // next `viy up` (the chat's too, on the same config) starts from the
    // primary again.
    let mut booted = cfg.clone();
    booted.set("DATA_CENTER_ID", &created.region);
    let cfg = &booted;
    let (pod, volume_id, volume_is_new) = (created.pod, created.volume_id, created.volume_is_new);
    let pod_id = text(&pod, "id").unwrap_or_default().to_string();
    let cost = pod.get("costPerHr");
    timeline.mark("pod created", [("pod_id", json!(pod_id))]);
    // The transport is recorded now: the exposed ports and vLLM's bind
    // address are fixed for this pod's life, so a later TUNNEL flip must
    // not change how it is addressed.
    let created_at = env.time.now().timestamp_micros() as f64 / 1e6;
    state::save(
        env.paths,
        [
            (key::POD_ID, json!(pod_id)),
            (key::CREATED_AT, json!(created_at)),
            (key::COST_PER_HR, cost.cloned().unwrap_or(Value::Null)),
            (key::TUNNEL_ONLY, json!(tunnel_enabled(cfg))),
            (key::VOLUME_ID, json!(volume_id)),
        ],
    )?;
    say(report, Tone::Ok, format!("  pod id:   {pod_id}"));
    if truthy(cost) {
        say(
            report,
            Tone::Warn,
            format!(
                "  cost:     ${:.2}/hr  <-- billing has started",
                number(cost)
            ),
        );
    }
    say(
        report,
        Tone::Plain,
        format!("  transport: {}", tracked::transport_label(env.paths, cfg)?),
    );
    say(
        report,
        Tone::Dim,
        if volume_is_new {
            "\nFresh volume: this boot must download ~56GB of weights.\nWeights and successful \
             compilation caches are retained for later boots."
        } else {
            "\nReusing the network volume; checking model and compilation caches.\nImage pulls, \
             missing files, and new hardware/configuration can add startup time."
        },
    );
    let w = wait_for(env, cfg, &rp, &pod_id);
    abort_at.store(interrupt.count() + 2, Ordering::SeqCst);
    let ready = wait::await_recorded(
        &w,
        timeout,
        started_wall,
        true,
        &mut timeline,
        interrupt.next(),
        report,
    )
    .await?;
    abort_at.store(interrupt.count() + 1, Ordering::SeqCst);
    if ready.is_some() {
        let ready_wall = env.time.now();
        let took = started.elapsed().as_secs_f64();
        state::save(
            env.paths,
            [(
                key::LAST_STARTUP_SECONDS,
                json!((took * 10.0).round() / 10.0),
            )],
        )?;
        idle::touch(env.paths, env.time, true);
        say(
            report,
            Tone::Ok,
            format!("vLLM is serving. Startup took {took:.0}s. Run viy chat"),
        );
        alert(
            report,
            "viy up: vLLM is serving",
            format!("startup took {took:.0}s"),
            false,
        );
        wait::report_boot(
            &w,
            started_wall,
            ready_wall,
            Outcome::Ready,
            &timeline,
            report,
        )
        .await;
        retire_replaced(env, cfg, &rp, &volume_id, report).await;
        return Ok(());
    }
    let now = env.time.now();
    wait::report_boot(&w, started_wall, now, Outcome::Failed, &timeline, report).await;
    say(
        report,
        Tone::Error,
        "Startup did not complete. Run viy logs. The pod remains allocated; viy down releases \
         it.",
    );
    alert(
        report,
        "viy up FAILED",
        "the pod is still billing; run viy logs or viy down",
        true,
    );
    Err(CommandError::Failed)
}

fn wait_for<'a>(env: &Env<'a>, cfg: &'a Config, rp: &'a RunPod, pod_id: &'a str) -> PodWait<'a> {
    PodWait {
        ssh: env.ssh(cfg),
        rp,
        http: &**env.http,
        time: env.time,
        pod_id,
        exe: env.exe,
    }
}

/// The network volume to boot from, and whether this call made it (the
/// one case where the weights are surely absent): NETWORK_VOLUME_ID if it
/// exists, else the volume named NETWORK_VOLUME_NAME, else a new one in
/// DATA_CENTER_ID. A volume found by name is remembered, and a pod lives
/// where its volume lives.
pub async fn ensure_volume(
    env: &Env<'_>,
    cfg: &mut Config,
    rp: &RunPod,
    report: Reporter<'_>,
) -> Result<(String, bool), CommandError> {
    if let Some(id) = cfg.get("NETWORK_VOLUME_ID").filter(|v| !v.is_empty()) {
        match rp.get_volume(id).await {
            Ok(Some(volume)) => {
                return Ok((text(&volume, "id").unwrap_or(id).to_string(), false));
            }
            Ok(None) => {}
            Err(_) => say(
                report,
                Tone::Warn,
                "Configured NETWORK_VOLUME_ID no longer exists; making a new one.",
            ),
        }
    }
    let name = cfg_at(cfg, "NETWORK_VOLUME_NAME").to_string();
    for volume in rp.list_volumes().await? {
        if text(&volume, "name") != Some(name.as_str()) {
            continue;
        }
        let region = cfg_at(cfg, "DATA_CENTER_ID").to_string();
        let Some(theirs) = text(&volume, "dataCenterId") else {
            // A pod must live where its volume lives; rpctl stops here too.
            return Err(CommandError::Exit(format!(
                "RunPod lists volume '{name}' without its data center, so there is no telling \
                 where the pod must run."
            )));
        };
        if theirs != region {
            say(
                report,
                Tone::Warn,
                format!(
                    "Volume '{name}' is in {} but DATA_CENTER_ID={region}. Using the volume's \
                     region, since a pod must live where its volume lives.",
                    shown(volume.get("dataCenterId"))
                ),
            );
            cfg.set("DATA_CENTER_ID", theirs);
        }
        let id = text(&volume, "id").unwrap_or_default().to_string();
        profiles::persist(cfg, env.paths, "NETWORK_VOLUME_ID", &id)?;
        return Ok((id, false));
    }
    let size = cfg_at(cfg, "NETWORK_VOLUME_SIZE_GB");
    let region = cfg_at(cfg, "DATA_CENTER_ID").to_string();
    say(
        report,
        Tone::Dim,
        format!("Creating {size}GB network volume '{name}' in {region}..."),
    );
    let size_gb: u64 = size.trim().parse().map_err(|_| {
        CommandError::Exit(format!(
            "NETWORK_VOLUME_SIZE_GB must be a whole number, not {size:?}"
        ))
    })?;
    let volume = rp
        .create_volume(&name, size_gb, &region)
        .await?
        .unwrap_or(Value::Null);
    let id = text(&volume, "id")
        .ok_or_else(|| RunPodError::new("RunPod created no volume", None))?
        .to_string();
    profiles::persist(cfg, env.paths, "NETWORK_VOLUME_ID", &id)?;
    say(report, Tone::Ok, format!("  volume id: {id}"));
    Ok((id, true))
}

/// After readiness only: a new backup that just served deletes the one it
/// replaced. Never fails `up`; a miss is retried on the next ready.
async fn retire_replaced(
    env: &Env<'_>,
    cfg: &Config,
    rp: &RunPod,
    volume_id: &str,
    report: Reporter<'_>,
) {
    let mut tell = |text: String| say(report, Tone::Dim, text);
    if let Err(error) = regions::retire_replaced(rp, env.paths, cfg, volume_id, &mut tell).await {
        say(
            report,
            Tone::Dim,
            format!("(old backup not deleted yet: {:?})", error.kind()),
        );
    }
}

// -------------------------------------------------------------- volume

/// What `viy volume` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeAction {
    List,
    Create,
    Delete,
}

/// `viy volume list|create|delete`: the weights network volume. Delete
/// asks first, and with nobody to ask deletes nothing.
pub async fn volume(
    env: &Env<'_>,
    cfg: &mut Config,
    action: VolumeAction,
    volume_id: Option<&str>,
    report: Reporter<'_>,
    ask: Asker<'_>,
) -> Result<(), CommandError> {
    let rp = connect(env, cfg)?;
    let print = |line: String| say(report, Tone::Plain, line);
    match action {
        VolumeAction::List => {
            let volumes = rp.list_volumes().await?;
            if volumes.is_empty() {
                print("no network volumes".to_string());
            }
            for v in volumes {
                print(format!(
                    "  {}  {:<24} {}GB  {}",
                    shown(v.get("id")),
                    shown(v.get("name")),
                    shown(v.get("size")),
                    shown(v.get("dataCenterId"))
                ));
            }
        }
        VolumeAction::Create => {
            let (id, created) = ensure_volume(env, cfg, &rp, report).await?;
            let existed = if created { "" } else { " (already existed)" };
            print(format!("volume: {id}{existed}"));
        }
        VolumeAction::Delete => {
            let configured = cfg.get("NETWORK_VOLUME_ID").filter(|v| !v.is_empty());
            let Some(id) = volume_id.filter(|v| !v.is_empty()).or(configured) else {
                return Err(CommandError::Exit(
                    "nothing to delete: pass an id or set NETWORK_VOLUME_ID".to_string(),
                ));
            };
            let id = id.to_string();
            let question = Question::yes_no(
                None,
                format!("Permanently delete volume {id} and its weights?"),
            );
            let answer = match ask(&question) {
                Some(asking) => asking.await,
                None => question.unattended,
            };
            if answer == Answer::No {
                print("aborted".to_string());
                return Ok(());
            }
            rp.delete_volume(&id).await?;
            regions::forget(env.paths, &id)?;
            if cfg.get("NETWORK_VOLUME_ID") == Some(id.as_str()) {
                profiles::persist(cfg, env.paths, "NETWORK_VOLUME_ID", "")?;
            }
            print(format!("deleted {id}"));
        }
    }
    Ok(())
}

// -------------------------------------------------------------- status

/// `viy status`: the pod's state, GPU, region, cost, endpoint and
/// transport, the tunnel, and whether vLLM answers.
pub async fn status(env: &Env<'_>, cfg: &Config, report: Reporter<'_>) -> Result<(), CommandError> {
    let rp = connect(env, cfg)?;
    let pod = match resolve(&rp, env, cfg, report).await {
        Ok(pod) => pod,
        Err(ResolveError::RunPod(error)) => {
            say(
                report,
                Tone::Warn,
                "Could not reach RunPod to check status; local state left as-is.",
            );
            say(report, Tone::Dim, error.to_string());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let Some(pod) = pod else {
        say(
            report,
            Tone::Dim,
            format!(
                "No pod named '{}' found. Run viy up",
                cfg_at(cfg, "POD_NAME")
            ),
        );
        return Ok(());
    };
    let pod_id = text(&pod, "id").unwrap_or_default();
    let line = |text: String| say(report, Tone::Plain, text);
    let gpu = pod.get("gpu").cloned().unwrap_or(Value::Null);
    let machine = pod.get("machine").cloned().unwrap_or(Value::Null);
    let tunnelled = tracked::tunnel_only(env.paths, cfg)?;
    let url = if tunnelled {
        tracked::local_url(cfg)
    } else {
        proxy_url(pod_id, cfg_at(cfg, "VLLM_PORT"))
    };
    let gpu_name = [text(&gpu, "displayName"), text(&gpu, "id")]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .unwrap_or("?");
    let count = pod
        .get("gpuCount")
        .map_or("?".to_string(), |c| shown(Some(c)));
    line(format!("  id:        {pod_id}"));
    line(format!("  name:      {}", shown(pod.get("name"))));
    line(format!("  state:     {}", shown(pod.get("desiredStatus"))));
    line(format!("  gpu:       {gpu_name} x{count}"));
    line(format!(
        "  region:    {}",
        text(&machine, "dataCenterId")
            .filter(|s| !s.is_empty())
            .unwrap_or("?")
    ));
    line(format!(
        "  cost:      ${:.2}/hr",
        number(pod.get("costPerHr"))
    ));
    if truthy(pod.get("lastStartedAt")) {
        line(format!("  started:   {}", shown(pod.get("lastStartedAt"))));
    }
    line(format!("  endpoint:  {url}"));
    line(format!(
        "  transport: {}",
        tracked::transport_label(env.paths, cfg)?
    ));
    if tunnelled {
        let mut said = |notice| report(Report::Notice(notice));
        tunnel::status(&env.ssh(cfg), &mut said).await?;
    }
    if text(&pod, "desiredStatus") != Some("RUNNING") {
        return Ok(());
    }
    let request = Request::new(Method::GET, format!("{url}/health"))
        .header("User-Agent", user_agent::RP_REQUESTS)
        .header("Accept", "*/*")
        .timeouts(Duration::from_secs(10), Duration::from_secs(10));
    let served = match viy_http::send(&**env.http, request).await {
        Ok(response) if matches!(response.status, 200 | 401) => "ready".to_string(),
        Ok(response) => format!("not ready (HTTP {})", response.status),
        Err(error) => {
            let mut served = format!("not reachable ({})", wait::failure_name(&error));
            if tunnelled {
                served.push_str(" -- the tunnel carries this, so check it first");
            }
            served
        }
    };
    line(format!("  vllm:      {served}"));
    let limits = idle::limits(cfg);
    if let Some(last) = idle::last_activity(env.paths)
        && limits.ttl != 0.0
    {
        let minutes = (env.time.now() - last).as_seconds_f64() / 60.0;
        line(format!(
            "  idle:      {minutes:.0} min since this machine last streamed; the pod shuts \
             itself down after {:.0} min without tokens",
            limits.ttl / 60.0
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- stop

/// `viy stop`: stops the pod, which ends GPU billing and can be resumed.
pub async fn stop(env: &Env<'_>, cfg: &Config, report: Reporter<'_>) -> Result<(), CommandError> {
    let _lock = PodLock::exclusive(env.paths, "stop")?;
    let rp = connect(env, cfg)?;
    let pod = match resolve(&rp, env, cfg, report).await {
        Ok(pod) => pod,
        Err(ResolveError::RunPod(error)) => {
            say(
                report,
                Tone::Warn,
                "Could not reach RunPod; nothing was stopped.",
            );
            say(report, Tone::Dim, error.to_string());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let Some(pod) = pod else {
        say(report, Tone::Dim, "No pod to stop.");
        return Ok(());
    };
    let pod_id = text(&pod, "id").unwrap_or_default();
    // Nothing left to forward to.
    let mut said = |notice| report(Report::Notice(notice));
    tunnel::stop(&env.ssh(cfg), true, &mut said).await?;
    rp.stop_pod(pod_id).await?;
    say(
        report,
        Tone::Ok,
        format!("Stopped {pod_id}. GPU billing ends; the pod can be resumed with viy up."),
    );
    say(
        report,
        Tone::Dim,
        "Note: a stopped pod still incurs disk charges and may lose its GPU slot.",
    );
    Ok(())
}

// ---------------------------------------------------------------- down

/// How long [`sweep`] tries in all.
const SWEEP_BUDGET: Duration = Duration::from_secs(300);

/// `viy down`: makes sure no pod of ours is running, starting, stopped or
/// about to be created. It comes before everything: an up or stop of
/// either tool that holds the pod lock is killed first, and success is
/// said only once RunPod confirms it. `cancelled_up`: the caller has just
/// cancelled an up of its own (module docs).
pub async fn down(
    env: &Env<'_>,
    cfg: &Config,
    cancelled_up: bool,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let rp = connect(env, cfg)?;
    // Preemption blocks for up to 15 s plus a grace per kill; its notices
    // are said as they happen.
    let paths = env.paths.clone();
    let (tx, mut notices) = tokio::sync::mpsc::unbounded_channel();
    let preempting = tokio::task::spawn_blocking(move || {
        PodLock::preempt(&paths, |n| {
            let _ = tx.send(n);
        })
    });
    while let Some(notice) = notices.recv().await {
        say(report, Tone::Warn, notice.to_string());
    }
    let preempted = preempting.await.map_err(io::Error::other)?;
    let preempted = preempted?;
    let killed_up = preempted
        .killed
        .iter()
        .find(|h| h.verb == "up")
        .map(crate::lock::Holder::tool_name);
    let lock = preempted.lock;
    // The forward goes before the state file, or its pid is lost and the
    // ssh outlives the pod it pointed at.
    let ssh = env.ssh(cfg);
    let mut said = |notice| report(Report::Notice(notice));
    tunnel::stop(&ssh, true, &mut said).await?;
    tunnel::kill_strays(&ssh).await?;

    let mut ids: HashSet<String> = state::load(env.paths)?
        .pod_id()
        .map(str::to_string)
        .into_iter()
        .collect();
    // A killed up may have had a create request in flight, which RunPod
    // can finish after the client is gone: watch long enough to catch it.
    let killed = killed_up.or(cancelled_up.then_some("viy"));
    let settle = Duration::from_secs(if killed.is_some() { 60 } else { 10 });
    if let Some(tool) = killed {
        say(
            report,
            Tone::Dim,
            format!(
                "The killed {tool} up may already have asked RunPod for a pod, so watching {}s \
                 for one to appear.",
                settle.as_secs()
            ),
        );
    }
    let (done, error) = sweep(&rp, cfg, &mut ids, settle, SWEEP_BUDGET, report).await;
    if let Some(error) = error {
        say(
            report,
            Tone::Error,
            "COULD NOT CONFIRM THAT NO POD IS RUNNING. Money may still be spent.",
        );
        say(report, Tone::Dim, error.to_string());
        say(
            report,
            Tone::Warn,
            format!(
                "Re-run viy down, or check https://console.runpod.io/pods for pods named '{}'.",
                cfg_at(cfg, "POD_NAME")
            ),
        );
        return Err(CommandError::Failed);
    }
    state::clear(env.paths)?;
    drop(lock);
    if done.is_empty() {
        say(
            report,
            Tone::Ok,
            "No pod was running or starting. Nothing is billing for a pod.",
        );
    } else {
        say(
            report,
            Tone::Ok,
            format!(
                "Terminated {}. No pod is running or starting; GPU and pod-disk billing has \
                 ended.",
                done.join(", ")
            ),
        );
    }
    say(
        report,
        Tone::Dim,
        "Model and startup caches stay on the network volume for the next viy up.",
    );
    Ok(())
}

/// Every live pod that could be ours: named POD_NAME, or one we tracked.
async fn our_pods(
    rp: &RunPod,
    cfg: &Config,
    ids: &HashSet<String>,
) -> Result<Vec<Value>, RunPodError> {
    let name = cfg_at(cfg, "POD_NAME");
    Ok(rp
        .list_pods()
        .await?
        .into_iter()
        .filter(|p| {
            text(p, "desiredStatus") != Some("TERMINATED")
                && (text(p, "name") == Some(name) || text(p, "id").is_some_and(|i| ids.contains(i)))
        })
        .collect())
}

/// Terminates every pod of ours, then lists again until RunPod has shown
/// none for `settle` straight. The pods terminated, and the error that
/// stopped it, when it could not confirm: a pod may still be billing.
async fn sweep(
    rp: &RunPod,
    cfg: &Config,
    ids: &mut HashSet<String>,
    settle: Duration,
    budget: Duration,
    report: Reporter<'_>,
) -> (Vec<String>, Option<RunPodError>) {
    let started = Instant::now();
    let mut clear_since: Option<Instant> = None;
    let mut done: Vec<String> = Vec::new();
    let mut error: Option<RunPodError> = None;
    let mut delay = Duration::from_secs(2);
    while started.elapsed() < budget {
        let pods = match our_pods(rp, cfg, ids).await {
            Ok(pods) => {
                error = None;
                Some(pods)
            }
            Err(e) if !e.transient() => return (done, Some(e)),
            Err(e) => {
                error = Some(e);
                None
            }
        };
        if pods.as_ref().is_some_and(Vec::is_empty) {
            let since = *clear_since.get_or_insert_with(Instant::now);
            let Some(left) = settle.checked_sub(since.elapsed()).filter(|l| !l.is_zero()) else {
                return (done, None);
            };
            report(Report::Passing(format!(
                "  confirming nothing comes back... {:2}s ",
                left.as_secs_f64().ceil()
            )));
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        clear_since = None;
        for pod in pods.unwrap_or_default() {
            let Some(id) = text(&pod, "id").map(str::to_string) else {
                continue;
            };
            ids.insert(id.clone());
            if let Err(e) = rp.delete_pod(&id).await
                && e.status_code() != Some(404)
            {
                let transient = e.transient();
                error = Some(e);
                if !transient {
                    return (done, error);
                }
                continue;
            }
            if !done.contains(&id) {
                say(
                    report,
                    Tone::Ok,
                    format!(
                        "  terminated {id} (was {})",
                        shown(pod.get("desiredStatus"))
                    ),
                );
                done.push(id);
            }
        }
        if let Some(e) = &error {
            let first = e.to_string().lines().next().unwrap_or_default().to_string();
            report(Report::Passing(format!(
                "  RunPod API error, retrying in {}s: {first}",
                delay.as_secs()
            )));
        }
        tokio::time::sleep(delay).await;
        delay = if error.is_some() {
            (delay * 2).min(Duration::from_secs(20))
        } else {
            Duration::from_secs(2)
        };
    }
    let error = error.unwrap_or_else(|| {
        RunPodError::new(
            format!("pods still listed after {}s", budget.as_secs()),
            None,
        )
    });
    (done, Some(error))
}

// ---------------------------------------------------------------- logs

/// `viy logs`: RunPod's platform and container log of our pod, secrets
/// masked, available before SSH; with `follow`, every 2 s until
/// `interrupted`.
pub async fn logs(
    env: &Env<'_>,
    cfg: &Config,
    follow: bool,
    interrupted: impl Future<Output = ()>,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let rp = connect(env, cfg)?;
    let pod = match resolve(&rp, env, cfg, report).await {
        Ok(pod) => pod,
        Err(ResolveError::RunPod(error)) => {
            say(report, Tone::Warn, "Could not reach RunPod.");
            say(report, Tone::Dim, error.to_string());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let Some(pod) = pod else {
        say(report, Tone::Dim, "No pod tracked.");
        return Ok(());
    };
    let pod_id = text(&pod, "id").unwrap_or_default();
    say(
        report,
        Tone::Dim,
        "RunPod platform + container logs (available before SSH):",
    );
    let print = async {
        let mut seen: HashSet<(Option<String>, Option<String>, String)> = HashSet::new();
        loop {
            let entries = rp.pod_logs(pod_id, 200, Duration::from_secs(8)).await?;
            for entry in &entries {
                let identity = (entry.ts.clone(), entry.source.clone(), entry.line.clone());
                if !seen.insert(identity) {
                    continue;
                }
                say(
                    report,
                    Tone::Plain,
                    format!(
                        "{} [{}] {}",
                        entry.ts.as_deref().unwrap_or(""),
                        entry.source.as_deref().unwrap_or("?"),
                        wait::redact_log(&entry.line, cfg)
                    ),
                );
            }
            if !follow {
                return Ok::<(), RunPodError>(());
            }
            if seen.len() > 5000 {
                seen = entries
                    .into_iter()
                    .map(|e| (e.ts, e.source, e.line))
                    .collect();
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    };
    let result = tokio::select! {
        biased;
        () = interrupted => return Ok(()),
        result = print => result,
    };
    if let Err(error) = result {
        say(report, Tone::Warn, error.to_string());
        say(
            report,
            Tone::Dim,
            format!("Web console logs: https://console.runpod.io/pods/{pod_id}"),
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "commands_tests.rs"]
mod tests;
