//! The SSH tunnel that carries traffic to a tunnel-only pod: rpctl's
//! `_tunnel_cmd`, `tunnel_running`, `_open_tunnel_log`, `tunnel_log_tail`,
//! `start_tunnel`, `stop_tunnel`, `_kill_stray_tunnels`,
//! `_resolve_ssh_target`, `tunnel_status`, `preflight`, `_tunnel_guard` and
//! `tunnel` (`viy tunnel`). Pinned by the pod/ssh fixture and run against
//! an sshd of the tests' own.
//!
//! A background tunnel is an `ssh -N` in a session of its own, its pid in
//! `pod.json` (`tunnel_pid`) and its stderr in `tunnel.log`, both shared
//! with rp (kb d049), so either tool sees, stops and repairs the other's.
//! The local end is `127.0.0.1:TUNNEL_LOCAL_PORT` for every pod
//! ([`crate::tracked::local_url`]).
//!
//! rpctl keeps the `Popen` it started to reap it; viy drops its handle and
//! leaves the reaping to tokio. A zombie is dead to
//! [`crate::process::pid_alive`] either way.

use std::fs::File;
use std::future::Future;
use std::io;
use std::io::Write;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use viy_config::config::tunnel_local_port;
use viy_exec::Command;
use viy_exec::Spawn;
use viy_exec::exit_text;

use crate::api::RunPod;
use crate::api::truthy;
use crate::notice::Alert;
use crate::notice::Notice;
use crate::notice::Say;
use crate::notice::Tone;
use crate::process;
use crate::ssh::Ssh;
use crate::ssh::port_open;
use crate::ssh::ssh_endpoint;
use crate::state;
use crate::state::key;
use crate::tracked;
use crate::tracked::ResolveError;

/// `tunnel.log` starts over past this size.
const TUNNEL_LOG_MAX: u64 = 256 * 1024;

/// What `pod.json` says of the background tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Running {
    /// The recorded pid, if it is one.
    pub pid: Option<u32>,
    /// Whether anything is recorded, a pid or not.
    pub recorded: bool,
    /// Whether that pid is a live `ssh`.
    pub alive: bool,
}

impl Running {
    /// ` (pid N)`, or nothing without a pid.
    fn pid_text(&self) -> String {
        self.pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
    }
}

/// The local end's port, as TUNNEL_LOCAL_PORT (else VLLM_PORT) gives it.
fn local<'a>(ssh: &Ssh<'a>) -> &'a str {
    tunnel_local_port(ssh.cfg)
}

/// Whether something accepts on the local end. A port that is no number
/// accepts nothing.
async fn local_open(ssh: &Ssh<'_>) -> bool {
    match local(ssh).parse() {
        Ok(port) => port_open("127.0.0.1", port, Duration::from_secs(1)).await,
        Err(_) => false,
    }
}

fn vllm_port<'a>(ssh: &Ssh<'a>) -> &'a str {
    ssh.cfg.get("VLLM_PORT").unwrap_or("")
}

/// The forward: `127.0.0.1:<local>` here to vLLM's port on the pod's
/// loopback, given up at once if the local port is taken.
pub fn tunnel_cmd(ssh: &Ssh<'_>, ip: &str, port: u16) -> io::Result<Command> {
    Ok(Command::new("ssh")
        .args(["-N", "-T", "-L"])
        .arg(format!(
            "127.0.0.1:{}:127.0.0.1:{}",
            local(ssh),
            vllm_port(ssh)
        ))
        .args(["-o", "ExitOnForwardFailure=yes"])
        .args(ssh.opts("accept-new")?)
        .arg("-p")
        .arg(port.to_string())
        .arg(format!("root@{ip}")))
}

/// The background tunnel `pod.json` records, and whether it lives: a pid
/// reused by another program is not ours.
pub fn running(ssh: &Ssh<'_>) -> io::Result<Running> {
    let state = state::load(ssh.paths)?;
    let value = state.get(key::TUNNEL_PID).filter(|v| truthy(v));
    let pid = value.and_then(|v| match v {
        Value::Number(n) => n.as_u64().and_then(|p| u32::try_from(p).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    });
    let alive = pid.is_some_and(|pid| {
        process::pid_alive(pid)
            && process::cmdline(pid)
                .first()
                .is_none_or(|argv0| basename(argv0) == b"ssh")
    });
    Ok(Running {
        pid,
        recorded: value.is_some(),
        alive,
    })
}

fn basename(path: &[u8]) -> &[u8] {
    path.rsplit(|&b| b == b'/').next().unwrap_or(path)
}

/// `tunnel.log`, opened for a new tunnel's stderr under a header line;
/// started over when it has grown past 256 KiB.
fn open_tunnel_log(ssh: &Ssh<'_>, ip: &str, port: u16) -> io::Result<File> {
    let path = ssh.paths.tunnel_log_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let big = std::fs::metadata(&path).is_ok_and(|m| m.len() > TUNNEL_LOG_MAX);
    let mut file = File::options()
        .create(true)
        .write(true)
        .append(!big)
        .truncate(big)
        .open(&path)?;
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    file.write_all(format!("=== {now} ssh tunnel to {ip}:{port} ===\n").as_bytes())?;
    Ok(file)
}

/// The last `n` lines ssh wrote for the most recent tunnel, or "".
pub fn tunnel_log_tail(ssh: &Ssh<'_>, n: usize) -> String {
    let Ok(bytes) = std::fs::read(ssh.paths.tunnel_log_file()) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let last = text.rsplit_once("=== ").map_or(&*text, |(_, last)| last);
    let body = last.split_once('\n').map_or(last, |(_, body)| body);
    let lines: Vec<&str> = body
        .split(['\n', '\r'])
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Brings up a background forward; true once its local end accepts.
/// `quiet` keeps the success lines to itself.
pub async fn start(
    ssh: &Ssh<'_>,
    rp: &RunPod,
    ip: &str,
    port: u16,
    quiet: bool,
    say: Say<'_>,
) -> io::Result<bool> {
    let tunnel = running(ssh)?;
    if tunnel.alive {
        if !quiet {
            say(Notice::new(
                Tone::Dim,
                format!("Tunnel already running{}.", tunnel.pid_text()),
            ));
        }
        return Ok(true);
    }
    let local = local(ssh);
    // ExitOnForwardFailure makes ssh give up on a taken port, and a probe
    // of the local end would then be answered by whatever took it: the
    // port is checked free before, not after.
    if local_open(ssh).await {
        say(Notice::new(
            Tone::Error,
            format!("127.0.0.1:{local} is already in use, so no tunnel was started."),
        ));
        say(Notice::new(
            Tone::Dim,
            "An untracked tunnel from an earlier session, or another service. Free the port, \
             or set TUNNEL_LOCAL_PORT to a different one.",
        ));
        return Ok(false);
    }
    if !ssh.preflight(rp, ip, port, &mut *say).await? {
        return Ok(false);
    }
    let log = open_tunnel_log(ssh, ip, port)?;
    let mut child = ssh
        .exec
        .spawn(tunnel_cmd(ssh, ip, port)?, Spawn::Detached { stderr: log })?;
    for _ in 0..60 {
        if let Some(status) = child.try_wait()? {
            say(Notice::new(
                Tone::Error,
                format!("The tunnel exited immediately (ssh {}).", exit_text(status)),
            ));
            let tail = tunnel_log_tail(ssh, 5);
            if !tail.is_empty() {
                say(Notice::new(Tone::Dim, tail));
            }
            return Ok(false);
        }
        if local_open(ssh).await {
            let pid = child.id();
            state::save(
                ssh.paths,
                [
                    (key::TUNNEL_PID, pid.into()),
                    (key::TUNNEL_LOCAL_PORT, local.into()),
                ],
            )?;
            if !quiet {
                say(Notice::new(
                    Tone::Ok,
                    format!(
                        "Tunnel up (pid {pid}): 127.0.0.1:{local} -> {ip}:{port} -> 127.0.0.1:{}",
                        vllm_port(ssh)
                    ),
                ));
            }
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    child.terminate()?;
    say(Notice::new(
        Tone::Error,
        format!("The tunnel never started listening on 127.0.0.1:{local}."),
    ));
    Ok(false)
}

/// Stops the background tunnel; false when none was running.
pub async fn stop(ssh: &Ssh<'_>, quiet: bool, say: Say<'_>) -> io::Result<bool> {
    let tunnel = running(ssh)?;
    let Some(pid) = tunnel.pid.filter(|_| tunnel.alive) else {
        if tunnel.recorded {
            state::save(ssh.paths, [(key::TUNNEL_PID, Value::Null)])?;
        }
        return Ok(false);
    };
    if let Ok(raw) = libc::pid_t::try_from(pid) {
        // SAFETY: a plain signal to the live ssh `running` found.
        unsafe { libc::kill(raw, libc::SIGTERM) };
    }
    for _ in 0..40 {
        if !process::pid_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    state::save(ssh.paths, [(key::TUNNEL_PID, Value::Null)])?;
    if !quiet {
        say(Notice::new(
            Tone::Ok,
            format!("Stopped the tunnel (pid {pid})."),
        ));
    }
    Ok(true)
}

/// Ends ssh forwards for our port that `pod.json` lost track of, such as
/// one an `up` started and was killed before it could record the pid.
/// Each gets SIGTERM, then SIGKILL after 3 s, off the async threads.
pub async fn kill_strays(ssh: &Ssh<'_>) -> io::Result<()> {
    let spec = format!("127.0.0.1:{}:127.0.0.1:{}", local(ssh), vllm_port(ssh));
    let mut strays = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let Some(pid) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let argv = process::cmdline(pid);
        if argv.first().is_some_and(|a| basename(a) == b"ssh")
            && argv.iter().any(|a| a == spec.as_bytes())
        {
            strays.push(pid);
        }
    }
    for pid in strays {
        tokio::task::spawn_blocking(move || process::kill(pid, Duration::from_secs(3)))
            .await
            .map_err(io::Error::other)?;
    }
    Ok(())
}

/// Why `viy tunnel` stopped.
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    /// The notices said why; exit 1.
    #[error("the tunnel failed")]
    Failed,
    /// Exit 1 with this message (rpctl's `SystemExit(message)`).
    #[error("{0}")]
    Exit(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The tracked pod's id and SSH endpoint.
pub async fn resolve_target(
    ssh: &Ssh<'_>,
    rp: &RunPod,
    say: Say<'_>,
) -> Result<(String, String, u16), TunnelError> {
    let unreachable = |say: Say<'_>, error: &dyn std::fmt::Display| {
        say(Notice::new(Tone::Error, "Could not reach RunPod."));
        say(Notice::new(Tone::Dim, error.to_string()));
        TunnelError::Failed
    };
    let found = tracked::resolve(rp, ssh.paths, ssh.cfg, |multiple| {
        say(Notice::new(Tone::Warn, multiple.to_string()));
    })
    .await;
    let pod = match found {
        Ok(Some(pod)) => pod,
        Ok(None) => {
            return Err(TunnelError::Exit(format!(
                "No pod named '{}'. Run viy up",
                ssh.cfg.get("POD_NAME").unwrap_or("")
            )));
        }
        Err(ResolveError::RunPod(e)) => return Err(unreachable(say, &e)),
        Err(ResolveError::State(e)) => return Err(e.into()),
    };
    let id = pod
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // A pod from the list can lack its port mappings; the detail has them.
    let pod = match rp.get_pod(&id).await {
        Ok(detail) => detail.unwrap_or(pod),
        Err(e) => return Err(unreachable(say, &e)),
    };
    let Some((ip, port)) = ssh_endpoint(&pod) else {
        say(Notice::new(
            Tone::Warn,
            "The pod has no TCP port for SSH yet; it appears about a minute after boot. Try again shortly.",
        ));
        return Err(TunnelError::Failed);
    };
    Ok((id, ip, port))
}

/// `viy tunnel --status`'s lines; true when the background tunnel lives.
pub async fn status(ssh: &Ssh<'_>, say: Say<'_>) -> io::Result<bool> {
    let local = local(ssh);
    let tunnel = running(ssh)?;
    if tunnel.alive {
        say(Notice::new(
            Tone::Ok,
            format!(
                "  tunnel:    running{} on 127.0.0.1:{local}",
                tunnel.pid_text()
            ),
        ));
    } else if local_open(ssh).await {
        say(Notice::new(
            Tone::Warn,
            format!(
                "  tunnel:    something is listening on 127.0.0.1:{local}, but it is not a \
                 tunnel rp or viy started"
            ),
        ));
    } else {
        say(Notice::new(Tone::Warn, "  tunnel:    down  (viy tunnel)"));
    }
    let fingerprint = ssh.pinned_fingerprint().await?;
    say(Notice::new(
        Tone::Plain,
        format!(
            "  host key:  {}",
            fingerprint
                .as_deref()
                .unwrap_or("not pinned yet (pinned on first connect)")
        ),
    ));
    Ok(tunnel.alive)
}

/// Warnings for the start of a chat, advisory only (rpctl's `preflight`).
/// The one that earns its keep: in tunnel mode a stopped forward shows up
/// as a refused connection on the first message, which reads like a dead
/// pod.
pub async fn session_warnings(ssh: &Ssh<'_>) -> io::Result<Vec<String>> {
    if !tracked::tunnel_only(ssh.paths, ssh.cfg)? {
        return Ok(vec![
            "This pod is reached through RunPod's proxy, which terminates TLS: prompts, replies \
             and the bearer token are readable there."
                .to_string(),
            "For an encrypted path:  viy down && TUNNEL=1 viy up".to_string(),
        ]);
    }
    let mut warn = Vec::new();
    if state::load(ssh.paths)?.tunnel_only().is_none() {
        warn.push(
            "This pod predates tunnel mode: the tunnel works, but its http port through \
             RunPod's proxy is still open. Close it with:  viy down && viy up"
                .to_string(),
        );
    }
    if !(running(ssh)?.alive || local_open(ssh).await) {
        warn.push(format!(
            "No SSH tunnel is up, so {} will refuse connections.",
            tracked::local_url(ssh.cfg)
        ));
        warn.push("Start one with:  viy tunnel -b".to_string());
    }
    Ok(warn)
}

/// Why [`Guard::check`] ended the wait for vLLM.
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    /// rpctl's `StartupError`, with the desktop alert it sends.
    #[error("{message}")]
    GaveUp { message: String, alert: Alert },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Watches the tunnel while `up` waits for vLLM through it: a forward that
/// dies is restarted on the spot, at most `max_restarts` times, rather
/// than leaving the wait to poll a dead port until its deadline.
#[derive(Debug, Clone)]
pub struct Guard {
    pod_id: String,
    ip: String,
    port: u16,
    restarts: u32,
    max_restarts: u32,
}

impl Guard {
    pub fn new(pod_id: impl Into<String>, ip: impl Into<String>, port: u16) -> Self {
        Self::with_max_restarts(pod_id, ip, port, 3)
    }

    pub fn with_max_restarts(
        pod_id: impl Into<String>,
        ip: impl Into<String>,
        port: u16,
        max_restarts: u32,
    ) -> Self {
        Self {
            pod_id: pod_id.into(),
            ip: ip.into(),
            port,
            restarts: 0,
            max_restarts,
        }
    }

    /// Runs before every poll of vLLM. Nothing while the tunnel lives.
    pub async fn check(
        &mut self,
        ssh: &Ssh<'_>,
        rp: &RunPod,
        say: Say<'_>,
    ) -> Result<(), GuardError> {
        let tunnel = running(ssh)?;
        if tunnel.alive {
            return Ok(());
        }
        say(Notice::new(
            Tone::Error,
            format!(
                "The SSH tunnel{} died while waiting for vLLM.",
                tunnel.pid_text()
            ),
        ));
        let tail = tunnel_log_tail(ssh, 5);
        if !tail.is_empty() {
            say(Notice::new(Tone::Dim, tail));
        }
        state::save(ssh.paths, [(key::TUNNEL_PID, Value::Null)])?;
        let log = ssh.paths.tunnel_log_file();
        if self.restarts >= self.max_restarts {
            return Err(GuardError::GaveUp {
                message: format!(
                    "The tunnel died {} times; giving up. ssh's output is in {}. The pod is still \
                     billing: fix the cause and run viy tunnel -b, or viy stop.",
                    self.restarts + 1,
                    log.display()
                ),
                alert: Alert {
                    title: "viy up: SSH tunnel keeps dying".to_string(),
                    body: format!("see {}", log.display()),
                    urgent: true,
                },
            });
        }
        self.restarts += 1;
        // A restarted pod can come back on another mapping.
        if let Ok(Some(pod)) = rp.get_pod(&self.pod_id).await
            && let Some((ip, port)) = ssh_endpoint(&pod)
        {
            (self.ip, self.port) = (ip, port);
        }
        say(Notice::new(
            Tone::Warn,
            format!(
                "Restarting it (attempt {} of {})...",
                self.restarts, self.max_restarts
            ),
        ));
        if !start(ssh, rp, &self.ip, self.port, false, &mut *say).await? {
            return Err(GuardError::GaveUp {
                message: "Could not restart the SSH tunnel. The pod is still billing: viy tunnel \
                          -b to retry, or viy stop."
                    .to_string(),
                alert: Alert {
                    title: "viy up: SSH tunnel failed".to_string(),
                    body: "could not restart it".to_string(),
                    urgent: true,
                },
            });
        }
        Ok(())
    }
}

/// What `viy tunnel` was asked to do. Of several flags, the first in this
/// order wins, as in rpctl: `--forget-key`, `--stop`, `--status`, then
/// `--background` or the foreground.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ForgetKey,
    Stop,
    Status,
    Background,
    Foreground,
}

/// `viy tunnel`: the encrypted path to vLLM. In the foreground by default
/// and reconnecting, since a dropped forward is a dead chat; `--background`
/// detaches one that `viy down` cleans up. `connect` makes the RunPod
/// client, asked for only when the pod is needed; the foreground ends
/// when `interrupted` resolves (Ctrl-C).
pub async fn command(
    ssh: &Ssh<'_>,
    action: Action,
    connect: impl FnOnce() -> Result<RunPod, String>,
    interrupted: impl Future<Output = ()>,
    say: Say<'_>,
) -> Result<(), TunnelError> {
    match action {
        Action::ForgetKey => return Ok(ssh.forget_host_key(say).await?),
        Action::Stop => {
            if !stop(ssh, false, &mut *say).await? {
                say(Notice::new(Tone::Dim, "No tunnel was running."));
            }
            return Ok(());
        }
        Action::Status => {
            status(ssh, say).await?;
            return Ok(());
        }
        Action::Background | Action::Foreground => {}
    }
    let tunnel = running(ssh)?;
    if tunnel.alive {
        say(Notice::new(
            Tone::Warn,
            format!(
                "A tunnel is already running in the background{}.",
                tunnel.pid_text()
            ),
        ));
        say(Notice::new(
            Tone::Dim,
            "Stop it first with:  viy tunnel --stop",
        ));
        return Ok(());
    }
    let rp = connect().map_err(TunnelError::Exit)?;
    let (pod_id, ip, port) = resolve_target(ssh, &rp, &mut *say).await?;
    if !tracked::tunnel_only(ssh.paths, ssh.cfg)? {
        say(Notice::new(
            Tone::Warn,
            "This pod was created with an exposed http port, so its traffic already goes \
             through RunPod's TLS-terminating proxy.",
        ));
        say(Notice::new(
            Tone::Dim,
            "The tunnel below still works, but the proxy route stays open. For a tunnel-only \
             pod:  viy down && viy up",
        ));
    }
    if action == Action::Background {
        if !start(ssh, &rp, &ip, port, false, &mut *say).await? {
            return Err(TunnelError::Failed);
        }
        say(Notice::new(Tone::Dim, "Stop it with:  viy tunnel --stop"));
        return Ok(());
    }
    foreground(ssh, &rp, &pod_id, ip, port, interrupted, say).await
}

/// The foreground forward, reconnecting with a backoff of 2 s doubling to
/// 30 s, and following the pod to a new endpoint, until ssh exits cleanly
/// or `interrupted` resolves.
pub async fn foreground(
    ssh: &Ssh<'_>,
    rp: &RunPod,
    pod_id: &str,
    mut ip: String,
    mut port: u16,
    interrupted: impl Future<Output = ()>,
    say: Say<'_>,
) -> Result<(), TunnelError> {
    if !ssh.preflight(rp, &ip, port, &mut *say).await? {
        return Err(TunnelError::Failed);
    }
    let local = local(ssh);
    if local_open(ssh).await {
        say(Notice::new(
            Tone::Error,
            format!("127.0.0.1:{local} is already in use. Free it, or set TUNNEL_LOCAL_PORT."),
        ));
        return Err(TunnelError::Failed);
    }
    say(Notice::new(
        Tone::Ok,
        format!(
            "Forwarding 127.0.0.1:{local} -> {ip}:{port} -> 127.0.0.1:{}",
            vllm_port(ssh)
        ),
    ));
    say(Notice::new(
        Tone::Dim,
        "Leave this running; use `viy chat` in another terminal. Ctrl-C to stop.",
    ));
    let mut child = None;
    let forward = async {
        let mut cmd = tunnel_cmd(ssh, &ip, port)?;
        let mut delay = 2;
        loop {
            let began = Instant::now();
            let status = child
                .insert(ssh.exec.spawn(cmd.clone(), Spawn::Terminal)?)
                .wait()
                .await?;
            child = None;
            if status.success() {
                return Ok(());
            }
            // A forward that lived a while and then dropped is a network
            // blip, not a misconfiguration: no inherited backoff.
            if began.elapsed() > Duration::from_secs(60) {
                delay = 2;
            }
            say(Notice::new(
                Tone::Warn,
                format!(
                    "Tunnel dropped (ssh exit {}); reconnecting in {delay}s...",
                    exit_text(status)
                ),
            ));
            tokio::time::sleep(Duration::from_secs(delay)).await;
            delay = (delay * 2).min(30);
            if let Ok(Some(pod)) = rp.get_pod(pod_id).await
                && let Some(endpoint) = ssh_endpoint(&pod)
                && endpoint != (ip.clone(), port)
            {
                (ip, port) = endpoint;
                cmd = tunnel_cmd(ssh, &ip, port)?;
                say(Notice::new(Tone::Dim, format!("Pod moved to {ip}:{port}")));
            }
        }
    };
    // The terminal's Ctrl-C ends ssh as it reaches viy: the interrupt is
    // looked at first, so its exit does not read as a clean end or a drop.
    let ended: io::Result<bool> = tokio::select! {
        biased;
        () = interrupted => Ok(true),
        result = forward => result.map(|()| false),
    };
    if ended? {
        // The terminal's Ctrl-C reached ssh too; a signal sent to viy alone
        // did not.
        if let Some(mut child) = child.take() {
            let _ = child.terminate();
            let _ = child.wait().await;
        }
        say(Notice::new(
            Tone::Dim,
            "\nTunnel closed. vLLM is no longer reachable from this machine.",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "tunnel_tests.rs"]
mod tests;
