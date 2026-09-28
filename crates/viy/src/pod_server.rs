//! The chat's server on the pod (kb d041, d068): the `viy_agent` chat asks
//! it before each turn where to send the turn, and it runs the pod's slash
//! commands. The state it shows comes from the pod state shared with rp,
//! read again before each send and every few seconds by a watcher, which
//! sends it as [`EventMsg::PodStatus`] for the footer (kb d079): none,
//! booting, ready, stopping; the GPU, price and this chat's spend, the idle
//! time left before the reaper, the tunnel.
//!
//! A pod command is parsed by `viy`'s own parser, as the subcommand of its
//! name, and runs the same function as that subcommand
//! ([`pods::work`], [`shutdown::arrange`]) in the background: what it says
//! becomes notices, its clock the footer's progress, its questions question
//! boxes (rpctl's failover questions unchanged). Ctrl-C with no turn
//! running stops the commands as Ctrl-C stops them in a terminal (`up`
//! records the boot first); `/down` during this chat's `/up` stops that up
//! in-process first, since the lock is this process's (WP 2.7). What needs
//! a terminal of its own stays a subcommand: `/reap --install`, `/shutdown
//! --foreground`; `/tunnel` without a flag runs in the background.
//!
//! A ready pod is one whose vLLM answers `/health` (200, or 401 as `viy
//! status` takes it), at the address `viy chat` would use: pod.json's pod
//! through the tunnel or RunPod's proxy. A given `--url` (or
//! VLLM_BASE_URL) is always taken to answer, as rp's chat takes it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use viy_agent::BoxFuture;
use viy_agent::TurnContext;
use viy_agent::server::Ready;
use viy_agent::server::Server;
use viy_config::Config;
use viy_config::Paths;
use viy_exec::ExecBackend;
use viy_http::HttpClient;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;
use viy_pod::backend::Backend;
use viy_pod::bootreport::Records;
use viy_pod::commands::CommandError;
use viy_pod::commands::Env;
use viy_pod::commands::Interrupt;
use viy_pod::idle;
use viy_pod::lock::Holder;
use viy_pod::lock::PodLock;
use viy_pod::notice::Report;
use viy_pod::notice::Tone;
use viy_pod::notice::desktop_notify;
use viy_pod::shutdown::Host;
use viy_pod::shutdown::SystemChats;
use viy_pod::state;
use viy_pod::tracked;
use viy_pod::tunnel;
use viy_protocol::BackgroundEvent;
use viy_protocol::BackgroundState;
use viy_protocol::BackgroundTask;
use viy_protocol::EventMsg;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::PodState;
use viy_protocol::PodStatusEvent;
use viy_time::TimeProvider;

use crate::cli;
use crate::cli::Command;
use crate::pods;
use crate::shutdown;

/// How often the watcher looks at the pod.
const WATCH_EVERY: Duration = Duration::from_secs(5);
/// A status unchanged but for the countdown and the spend is sent again
/// this often.
const RESEND_EVERY: Duration = Duration::from_secs(60);
/// How long vLLM's `/health` may take to answer before the pod counts as
/// not answering.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a command stopped by Ctrl-C gets to wind down (an `up`
/// records its boot) before it is pressed again, and again before it is
/// dropped.
const WIND_DOWN: Duration = Duration::from_secs(30);

/// What the server is made of.
pub struct Parts {
    pub cfg: Config,
    pub paths: Paths,
    pub http: Arc<dyn HttpClient>,
    pub exec: Arc<dyn ExecBackend>,
    pub time: Arc<dyn TimeProvider>,
    /// viy itself, for the boot's log collector and the shutdown runner.
    pub exe: PathBuf,
    /// `--url` or VLLM_BASE_URL: the model's address, taken as it is.
    pub url: Option<String>,
    /// Whether there is a desktop for `up`'s alerts.
    pub desktop: bool,
}

/// A pod command running in the background.
struct Job {
    id: u64,
    /// The command's name, without the `/`.
    name: String,
    /// Where its clock is: what the footer shows of a boot.
    stage: Option<String>,
    presses: watch::Sender<u32>,
    task: Option<JoinHandle<()>>,
}

/// Ends a job however its task ends, an abort included: takes it off the
/// list, says it failed if its run did not get to say how it ended, and
/// wakes the watcher.
struct JobEnd {
    server: Arc<PodServer>,
    ctx: TurnContext,
    id: u64,
    task: BackgroundTask,
    finished: bool,
}

impl Drop for JobEnd {
    fn drop(&mut self) {
        lock(&self.server.jobs).retain(|j| j.id != self.id);
        if !self.finished {
            self.ctx.emit(EventMsg::Background(BackgroundEvent {
                task: self.task.clone(),
                state: BackgroundState::Failed,
                progress: None,
            }));
        }
        self.server.wake.notify_one();
    }
}

/// What the watcher last sent, and the spend so far.
#[derive(Default)]
struct Shown {
    status: Option<PodStatusEvent>,
    sent: Option<Instant>,
    /// When the spend was last counted.
    counted: Option<Instant>,
    spent: f64,
}

pub struct PodServer {
    parts: Parts,
    backend: Result<Backend, String>,
    /// The config, as `up` leaves it (a volume's region).
    cfg: Mutex<Config>,
    jobs: Mutex<Vec<Job>>,
    next_job: AtomicU64,
    shown: Mutex<Shown>,
    /// Wakes the watcher: a command ended, a boot moved on.
    wake: Notify,
    /// The GPU of a pod, once its boot record names it.
    gpu: Mutex<Option<(String, String)>>,
    /// Whether the preflight warnings were said.
    preflighted: std::sync::atomic::AtomicBool,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PodServer {
    pub fn new(parts: Parts) -> Arc<Self> {
        Arc::new(Self {
            backend: Backend::resolve(&parts.cfg),
            cfg: Mutex::new(parts.cfg.clone()),
            parts,
            jobs: Mutex::default(),
            next_job: AtomicU64::new(1),
            shown: Mutex::default(),
            wake: Notify::new(),
            gpu: Mutex::default(),
            preflighted: std::sync::atomic::AtomicBool::new(false),
            watcher: Mutex::default(),
        })
    }

    fn cfg(&self) -> Config {
        lock(&self.cfg).clone()
    }

    fn env(&self) -> Env<'_> {
        Env {
            paths: &self.parts.paths,
            exec: self.parts.exec.as_ref(),
            http: &self.parts.http,
            time: self.parts.time.as_ref(),
            exe: &self.parts.exe,
        }
    }

    /// The state this chat's own commands put the pod in, if any.
    fn own_state(&self) -> Option<PodState> {
        let jobs = lock(&self.jobs);
        if let Some(up) = jobs.iter().find(|j| j.name == "up") {
            return Some(PodState::Booting {
                stage: up.stage.clone().unwrap_or_else(|| "starting".into()),
            });
        }
        jobs.iter()
            .any(|j| j.name == "stop" || j.name == "down")
            .then_some(PodState::Stopping)
    }

    /// What another up, stop or down of rp's or viy's is doing to the pod.
    fn others_state(&self) -> Option<PodState> {
        holder_state(PodLock::holder(&self.parts.paths)?, std::process::id())
    }

    /// Where the model answers now, or why not; `None` for a reason that
    /// is a state (booting, stopping).
    async fn probe(&self, cfg: &Config, backend: Backend) -> Result<String, String> {
        let report = |_: Report| {};
        let base = backend
            .base_url(&self.env(), cfg, false, &report)
            .await
            .map_err(|e| e.to_string())?;
        match backend {
            Backend::Openai => {
                base.ok_or_else(|| "ENDPOINT_URL is not set (BACKEND=openai).".into())
            }
            Backend::Local => {
                base.ok_or_else(|| "no local vLLM is running: /up starts one.".into())
            }
            Backend::Runpod => {
                let Some(base) = base else {
                    return Err("no pod is running: /up starts one.".into());
                };
                if self.healthy(&base).await {
                    Ok(base)
                } else {
                    Err("the pod is not answering: /status says why, /up brings it back.".into())
                }
            }
        }
    }

    /// Whether vLLM at `base` answers `/health`, as `viy status` asks it.
    async fn healthy(&self, base: &str) -> bool {
        let request = || {
            Request::new(Method::GET, format!("{base}/health"))
                .header("User-Agent", user_agent::RP_REQUESTS)
                .header("Accept", "*/*")
                .timeouts(HEALTH_TIMEOUT, HEALTH_TIMEOUT)
        };
        // Asked twice when the first never got an answer: vLLM's server
        // closes an idle kept-alive connection after 5 s, and through the
        // tunnel a probe can go out on one it has just closed (seen live
        // 2026-07-18, every other probe 5 s apart); the second goes on a
        // new connection.
        for _ in 0..2 {
            match viy_http::send(self.parts.http.as_ref(), request()).await {
                Ok(response) => return matches!(response.status, 200 | 401),
                Err(viy_http::HttpError::Timeout) => return false,
                Err(_) => {}
            }
        }
        false
    }

    /// The pod's status as the footer shows it.
    async fn status(&self) -> PodStatusEvent {
        let cfg = self.cfg();
        let mut status = PodStatusEvent {
            backend: "runpod".into(),
            model: None,
            state: PodState::Absent,
            gpu: None,
            price_usd_per_hour: None,
            spent_usd: None,
            idle_seconds_left: None,
            tunnel_up: None,
        };
        let Ok(backend) = self.backend.clone() else {
            return status;
        };
        status.backend = backend.name().into();
        if backend != Backend::Runpod {
            status.model = Some(backend.model_name(&cfg));
        }
        let others = || {
            (backend == Backend::Runpod)
                .then(|| self.others_state())
                .flatten()
        };
        if let Some(state) = self.own_state().or_else(others) {
            status.state = state;
        } else if self.parts.url.is_some() || self.probe(&cfg, backend).await.is_ok() {
            status.state = PodState::Ready;
        }
        if backend != Backend::Runpod {
            return status;
        }
        let pod = state::load(&self.parts.paths).unwrap_or_default();
        let Some(pod_id) = pod.pod_id().map(str::to_string) else {
            return status;
        };
        status.price_usd_per_hour = pod
            .get(state::key::COST_PER_HR)
            .and_then(number)
            .filter(|p| *p > 0.0);
        status.gpu = self.gpu_of(&pod_id);
        status.idle_seconds_left = self.idle_left(&cfg, &pod_id);
        if tracked::tunnel_only(&self.parts.paths, &cfg).unwrap_or(false) {
            let ssh = self.env().ssh(&cfg);
            status.tunnel_up = Some(tunnel::running(&ssh).is_ok_and(|t| t.alive));
        }
        status
    }

    /// The GPU of pod `pod_id`, from its most recent boot record; kept once
    /// found.
    fn gpu_of(&self, pod_id: &str) -> Option<String> {
        if let Some((id, gpu)) = lock(&self.gpu).as_ref()
            && id == pod_id
        {
            return Some(gpu.clone());
        }
        let gpu = Records::new(&self.parts.paths)
            .history(20)
            .iter()
            .rev()
            .find(|row| row["pod_id"].as_str() == Some(pod_id))
            .and_then(|row| row["gpu"].as_str().map(short_gpu))?;
        *lock(&self.gpu) = Some((pod_id.to_string(), gpu.clone()));
        Some(gpu)
    }

    /// Seconds before the reaper takes the pod down for want of tokens, as
    /// `viy reap` counts them; `None` with IDLE_TTL_MIN off.
    fn idle_left(&self, cfg: &Config, pod_id: &str) -> Option<u64> {
        let limits = idle::limits(cfg);
        if limits.ttl == 0.0 {
            return None;
        }
        let now = self.parts.time.now().timestamp_micros() as f64 / 1e6;
        let seen = idle::Seen::load(&self.parts.paths, pod_id, now);
        let activity =
            idle::last_activity(&self.parts.paths).map(|at| at.timestamp_micros() as f64 / 1e6);
        let last = idle::last_active(activity, &seen);
        Some((limits.ttl + limits.grace - (now - last)).max(0.0) as u64)
    }

    /// Looks at the pod and sends its status if it changed, or if it was
    /// last sent a while ago; counts the spend of a pod that bills. The
    /// first time the pod is ready, says rp chat's warnings before it
    /// opens on a server (resolve_base_url's preflight), which a given
    /// address skips there too.
    async fn look(&self, ctx: &TurnContext) {
        let mut status = self.status().await;
        if status.state == PodState::Ready
            && self.parts.url.is_none()
            && !self.preflighted.swap(true, Ordering::SeqCst)
            && let Ok(backend) = self.backend.clone()
        {
            let cfg = self.cfg();
            for line in backend
                .preflight(&self.env(), &cfg)
                .await
                .unwrap_or_default()
            {
                ctx.emit(EventMsg::Notice(NoticeEvent {
                    level: NoticeLevel::Warn,
                    code: "preflight".into(),
                    message: line,
                }));
            }
        }
        let now = Instant::now();
        let mut shown = lock(&self.shown);
        let billing = matches!(status.state, PodState::Ready | PodState::Booting { .. });
        if let (Some(price), Some(at), true) = (status.price_usd_per_hour, shown.counted, billing) {
            shown.spent += price * now.duration_since(at).as_secs_f64() / 3600.0;
        }
        shown.counted = Some(now);
        if shown.spent > 0.0 {
            status.spent_usd = Some((shown.spent * 100.0).round() / 100.0);
        }
        let same = shown.status.as_ref().is_some_and(|old| {
            PodStatusEvent {
                idle_seconds_left: status.idle_seconds_left,
                spent_usd: status.spent_usd,
                ..old.clone()
            } == status
        });
        let fresh = shown
            .sent
            .is_some_and(|at| now.duration_since(at) < RESEND_EVERY);
        if same && fresh {
            return;
        }
        shown.status = Some(status.clone());
        shown.sent = Some(now);
        drop(shown);
        ctx.emit(EventMsg::PodStatus(status));
    }

    /// Runs `/name args`.
    fn start(self: Arc<Self>, ctx: &TurnContext, name: &str, args: &str) {
        let say = |level, message: String| {
            ctx.emit(EventMsg::Notice(NoticeEvent {
                level,
                code: "pod".into(),
                message,
            }));
        };
        let argv = std::iter::once(name.into()).chain(args.split_whitespace().map(Into::into));
        let command = match cli::parse(argv) {
            Ok(parsed) => parsed.command,
            Err(e) => {
                let level = if e.use_stderr() {
                    NoticeLevel::Warn
                } else {
                    NoticeLevel::Info
                };
                say(level, e.render().to_string().trim_end().to_string());
                return;
            }
        };
        match &command {
            Command::Reap { install: true, .. } => {
                return say(
                    NoticeLevel::Warn,
                    "/reap --install: run viy reap --install in a terminal".into(),
                );
            }
            Command::Shutdown(a) if a.foreground || a.run_at.is_some() => {
                return say(
                    NoticeLevel::Warn,
                    "/shutdown --foreground: a countdown needs a terminal of its own; /shutdown \
                     without it counts down in the background"
                        .into(),
                );
            }
            Command::Boots(a) => {
                return match pods::boots_text(&self.parts.paths, a) {
                    Ok(text) => say(NoticeLevel::Info, text),
                    Err(e) => say(NoticeLevel::Error, e),
                };
            }
            _ => {}
        }
        let (presses, interrupt) = Interrupt::channel();
        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        let job_ctx = ctx.background();
        let server = Arc::clone(&self);
        let name = name.to_string();
        // Registered before it runs, so the pod shows booting at once.
        lock(&self.jobs).push(Job {
            id,
            name: name.clone(),
            stage: None,
            presses,
            task: None,
        });
        // Said before the command is done, so the frontend knows at once
        // that Ctrl-C has something to stop.
        let background = BackgroundTask::Pod {
            command: name.clone(),
            job: id,
        };
        job_ctx.emit(EventMsg::Background(BackgroundEvent {
            task: background.clone(),
            state: BackgroundState::Started,
            progress: None,
        }));
        let task = tokio::spawn(async move {
            let mut end = JobEnd {
                server: Arc::clone(&server),
                ctx: job_ctx.clone(),
                id,
                task: background,
                finished: false,
            };
            server.run(&job_ctx, id, &name, command, interrupt).await;
            end.finished = true;
        });
        if let Some(job) = lock(&self.jobs).iter_mut().find(|j| j.id == id) {
            job.task = Some(task);
        }
        self.wake.notify_one();
    }

    /// A pod command's run, from its start to its end said.
    async fn run(
        &self,
        ctx: &TurnContext,
        id: u64,
        name: &str,
        command: Command,
        interrupt: Interrupt,
    ) {
        let task = BackgroundTask::Pod {
            command: name.to_string(),
            job: id,
        };
        let background = |state, progress: Option<String>| {
            ctx.emit(EventMsg::Background(BackgroundEvent {
                task: task.clone(),
                state,
                progress,
            }));
        };
        // Counted from now: a Ctrl-C while `/down` stops this chat's up
        // stops the down too.
        let stopped = interrupt.next();
        let cancelled_up = name == "down" && self.cancel_up(id).await;
        let report = |r: Report| match r {
            Report::Notice(n) => ctx.emit(EventMsg::Notice(NoticeEvent {
                level: match n.tone {
                    Tone::Error => NoticeLevel::Error,
                    Tone::Warn => NoticeLevel::Warn,
                    Tone::Plain | Tone::Dim | Tone::Ok => NoticeLevel::Info,
                },
                code: "pod".into(),
                message: n.text,
            })),
            Report::Clock(clock) => {
                let stage = format!("{} ({})", clock.label, clock.state);
                let moved = {
                    let mut jobs = lock(&self.jobs);
                    let job = jobs.iter_mut().find(|j| j.id == id);
                    job.is_some_and(|j| j.stage.replace(stage.clone()) != Some(stage))
                };
                if moved {
                    self.wake.notify_one();
                }
                background(
                    BackgroundState::Running,
                    Some(clock.to_string().trim().to_string()),
                );
            }
            Report::Passing(text) => background(BackgroundState::Running, Some(text)),
            Report::Alert(alert) => {
                if self.parts.desktop {
                    desktop_notify(self.parts.exec.as_ref(), &alert);
                }
            }
        };
        let asker = ctx.asker();
        let mut cfg = self.cfg();
        let result = if cancelled_up {
            // As `viy down` after the up it cancelled: a pod that up asked
            // for may still appear.
            let env = self.env();
            let down = viy_pod::commands::down(&env, &cfg, true, &report);
            tokio::select! {
                biased;
                () = stopped => Err(CommandError::Interrupted),
                result = down => result,
            }
        } else {
            self.work(&command, &mut cfg, &interrupt, &report, &asker)
                .await
        };
        if name == "up" {
            *lock(&self.cfg) = cfg;
        }
        let failed = |message: String| {
            ctx.emit(EventMsg::Notice(NoticeEvent {
                level: NoticeLevel::Error,
                code: "pod".into(),
                message,
            }));
        };
        let state = match result {
            Ok(()) => BackgroundState::Done,
            Err(error) => {
                match error {
                    CommandError::Exit(message) => failed(message),
                    CommandError::Failed => {}
                    CommandError::Interrupted => ctx.emit(EventMsg::Notice(NoticeEvent {
                        level: NoticeLevel::Warn,
                        code: "pod".into(),
                        message: format!("/{name}: interrupted"),
                    })),
                    CommandError::RunPod(e) => failed(e.to_string()),
                    CommandError::Io(e) => failed(e.to_string()),
                }
                BackgroundState::Failed
            }
        };
        background(state, None);
    }

    /// The command's work: the subcommand's, `/down` after this chat's own
    /// up as `viy down` does after a cancelled up.
    async fn work(
        &self,
        command: &Command,
        cfg: &mut Config,
        interrupt: &Interrupt,
        report: &(dyn Fn(Report) + Send + Sync),
        ask: &(dyn Fn(&viy_protocol::Question) -> Option<viy_protocol::Asking> + Send + Sync),
    ) -> Result<(), CommandError> {
        let env = self.env();
        match command {
            Command::Shutdown(args) => {
                let var = |name: &str| std::env::var_os(name);
                let host = Host {
                    paths: &self.parts.paths,
                    exec: self.parts.exec.as_ref(),
                    time: self.parts.time.as_ref(),
                    chats: &SystemChats,
                    var: &var,
                };
                let arranged = shutdown::arrange(args, &host, cfg, &self.parts.exe, report);
                tokio::select! {
                    biased;
                    () = interrupt.next() => Err(CommandError::Interrupted),
                    result = arranged => result.map(|_| ()).map_err(|f| match f.message() {
                        Some(m) => CommandError::Exit(m.to_string()),
                        None => CommandError::Failed,
                    }),
                }
            }
            // The foreground tunnel holds a terminal: the chat's runs in
            // the background (kb d068).
            Command::Tunnel(args) if !(args.stop || args.status || args.forget_key) => {
                let mut args = args.clone();
                args.background = true;
                let backend = pods::backend_for(command, cfg).map_err(CommandError::Exit)?;
                pods::work(
                    &Command::Tunnel(args),
                    backend,
                    &env,
                    cfg,
                    interrupt,
                    report,
                    ask,
                )
                .await
            }
            _ => {
                let backend = pods::backend_for(command, cfg).map_err(CommandError::Exit)?;
                pods::work(command, backend, &env, cfg, interrupt, report, ask).await
            }
        }
    }

    /// Stops this chat's own `up`, if one runs, before `/down` (job `down`
    /// itself): its lock is this process's, which `down` would not take
    /// from it. Whether there was one.
    async fn cancel_up(&self, down: u64) -> bool {
        let up = {
            let mut jobs = lock(&self.jobs);
            jobs.iter_mut()
                .find(|j| j.name == "up" && j.id != down)
                .map(|j| (j.presses.clone(), j.task.take()))
        };
        match up {
            Some((presses, Some(task))) => {
                wind_down(vec![(presses, task)]).await;
                true
            }
            _ => false,
        }
    }

    /// Presses Ctrl-C for every running command.
    fn press(&self) {
        for job in lock(&self.jobs).iter() {
            job.presses.send_modify(|n| *n += 1);
        }
    }
}

/// A boot record's GPU as the footer names it: without the vendor and the
/// compute capability after the comma ("NVIDIA RTX PRO 6000 Blackwell
/// Server Edition, 12.0" is "RTX PRO 6000 Blackwell Server Edition").
fn short_gpu(name: &str) -> String {
    let name = name.split(',').next().unwrap_or(name).trim();
    name.strip_prefix("NVIDIA ").unwrap_or(name).to_string()
}

/// The state `holder` of the pod lock puts the pod in, unless it is this
/// process (`me`), whose own commands say that.
fn holder_state(holder: Holder, me: u32) -> Option<PodState> {
    if holder.pid == me {
        return None;
    }
    Some(if holder.verb == "up" {
        PodState::Booting {
            stage: format!("{} up in another terminal", holder.tool_name()),
        }
    } else {
        PodState::Stopping
    })
}

/// Ctrl-C for every job, then waits for them to end; those still running
/// after [`WIND_DOWN`] are pressed again, and dropped after another (their
/// [`JobEnd`] still runs).
async fn wind_down(jobs: Vec<(watch::Sender<u32>, JoinHandle<()>)>) {
    let mut left = jobs;
    for round in 0..2 {
        for (presses, _) in &left {
            presses.send_modify(|n| *n += 1);
        }
        let deadline = Instant::now() + WIND_DOWN;
        let mut running = Vec::new();
        for (presses, mut task) in left {
            if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
                running.push((presses, task));
            }
        }
        left = running;
        if round == 1 {
            for (_, task) in left.drain(..) {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

/// A number RunPod sent as a number or a string.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl Server for PodServer {
    fn ready(self: Arc<Self>) -> BoxFuture<'static, Result<Ready, String>> {
        Box::pin(async move {
            let backend = self.backend.clone()?;
            let cfg = self.cfg();
            let model = backend.model_name(&cfg);
            if let Some(base) = &self.parts.url {
                return Ok(Ready {
                    base: base.trim_end_matches('/').to_string(),
                    model,
                });
            }
            let busy = self
                .own_state()
                .or_else(|| self.others_state().filter(|_| backend == Backend::Runpod));
            match busy {
                Some(PodState::Booting { stage }) => {
                    return Err(format!("the pod is still booting ({stage})"));
                }
                Some(_) => return Err("the pod is stopping".into()),
                None => {}
            }
            match self.probe(&cfg, backend).await {
                Ok(base) => Ok(Ready { base, model }),
                Err(why) => Err(why),
            }
        })
    }

    fn command(self: Arc<Self>, ctx: &TurnContext, name: &str, args: &str) -> bool {
        if !viy_agent::chat::POD_COMMANDS.contains(&name) {
            return false;
        }
        self.start(ctx, name, args);
        true
    }

    fn opened(self: Arc<Self>, ctx: &TurnContext) {
        let ctx = ctx.clone();
        let server = Arc::clone(&self);
        let watcher = tokio::spawn(async move {
            loop {
                server.look(&ctx).await;
                let _ = tokio::time::timeout(WATCH_EVERY, server.wake.notified()).await;
            }
        });
        if let Some(old) = lock(&self.watcher).replace(watcher) {
            old.abort();
        }
    }

    fn interrupt(&self) {
        self.press();
    }

    fn shutdown(self: Arc<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            if let Some(watcher) = lock(&self.watcher).take() {
                watcher.abort();
            }
            // A stop or a down is let finish: quitting must not leave a
            // pod billing that the user asked to stop. The rest are
            // stopped as Ctrl-C stops them.
            let (ending, stopped): (Vec<_>, Vec<_>) = lock(&self.jobs)
                .iter_mut()
                .filter_map(|j| Some((j.name.clone(), j.presses.clone(), j.task.take()?)))
                .partition(|(name, _, _)| name == "stop" || name == "down");
            wind_down(stopped.into_iter().map(|(_, p, t)| (p, t)).collect()).await;
            for (_, _, task) in ending {
                let _ = task.await;
            }
        })
    }
}

#[cfg(test)]
#[path = "pod_server_tests.rs"]
mod tests;
