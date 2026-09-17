//! The local backend: vLLM on this machine, rpctl's `LocalBackend`. `viy
//! up` starts `vllm serve` in the background with the launch flags a pod
//! gets, bound to 127.0.0.1; `viy down` (or `viy stop`) ends it. The
//! process is tracked in `local.json`, shared with rp (kb d049).
//!
//! LOCAL_VLLM_BIN is the vllm executable (`vllm` on PATH by default);
//! LOCAL_VLLM_PORT the port on 127.0.0.1, VLLM_PORT when empty.

use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use chrono::Local;
use serde_json::Value;
use serde_json::json;
use tokio::time::Instant;
use viy_config::Config;
use viy_config::Paths;
use viy_config::atomic::write_atomically;
use viy_config::profiles;
use viy_exec::Child;
use viy_exec::Spawn;
use viy_http::HttpClient;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;

use crate::backend::Backend;
use crate::boot::build_vllm_args;
use crate::commands::CommandError;
use crate::commands::Env;
use crate::notice::Reporter;
use crate::notice::Tone;
use crate::notice::say;
use crate::process;
use crate::regions;
use crate::wait::Probe;
use crate::wait::Step;
use crate::wait::WaitError;
use crate::wait::wait_with_clock;

/// vLLM's /health is asked this often while it loads.
const POLL: Duration = Duration::from_secs(3);

fn port(cfg: &Config) -> String {
    match cfg.get("LOCAL_VLLM_PORT").unwrap_or("").trim() {
        "" => cfg.get("VLLM_PORT").unwrap_or("").to_string(),
        port => port.to_string(),
    }
}

fn url(cfg: &Config) -> String {
    format!("http://127.0.0.1:{}", port(cfg))
}

/// The tracked pid, if it is still the vllm viy or rp started: a reused
/// pid is someone else's.
fn alive(paths: &Paths) -> Option<u32> {
    let state: Value =
        serde_json::from_str(&fs::read_to_string(paths.local_state_file()).ok()?).ok()?;
    let pid = u32::try_from(state.get("pid")?.as_u64()?).ok()?;
    (process::pid_alive(pid)
        && process::cmdline(pid)
            .iter()
            .any(|a| a.windows(4).any(|w| w == b"vllm")))
    .then_some(pid)
}

async fn healthy(http: &dyn HttpClient, cfg: &Config, timeout: Duration) -> bool {
    let key = Backend::Local.api_key(cfg);
    let request = Request::new(Method::GET, format!("{}/health", url(cfg)))
        .header("User-Agent", user_agent::RP_REQUESTS)
        .header("Accept", "*/*")
        .header("Authorization", format!("Bearer {key}"))
        .timeouts(timeout, timeout);
    matches!(viy_http::send(http, request).await, Ok(r) if r.status == 200)
}

/// The endpoint, if vLLM runs here or something answers on its port.
pub async fn base_url(env: &Env<'_>, cfg: &Config) -> Option<String> {
    (alive(env.paths).is_some() || healthy(env.http.as_ref(), cfg, Duration::from_secs(2)).await)
        .then(|| url(cfg))
}

/// The last `n` lines of vLLM's log.
fn tail(paths: &Paths, n: usize) -> String {
    let Ok(bytes) = fs::read(paths.local_log_file()) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// `exe` as `shutil.which` finds it: a path with a `/` as it is, a name on
/// PATH.
fn find_program(exe: &str) -> Option<PathBuf> {
    if exe.contains('/') {
        use std::os::unix::fs::PermissionsExt;
        let path = Path::new(exe);
        return path
            .metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .then(|| path.to_path_buf());
    }
    viy_config::vault::find_executable(std::env::var_os("PATH").as_deref(), exe)
}

/// How the started vLLM's wait ended.
enum Ended {
    Ready,
    Exited(String),
}

/// The started vLLM: exited, serving, or still loading.
struct Loading<'a> {
    child: Box<dyn Child>,
    http: &'a dyn HttpClient,
    cfg: &'a Config,
}

impl Probe for Loading<'_> {
    type Done = Ended;

    async fn probe(&mut self) -> Result<Step<Ended>, WaitError> {
        if let Some(status) = self.child.try_wait()? {
            return Ok(Step::Done(Ended::Exited(viy_exec::exit_text(status))));
        }
        Ok(
            if healthy(self.http, self.cfg, Duration::from_secs(5)).await {
                Step::Done(Ended::Ready)
            } else {
                Step::Waiting("loading".into())
            },
        )
    }
}

/// `viy up`: starts vLLM here unless it runs, and waits up to `timeout`
/// for it to serve.
pub async fn up(
    env: &Env<'_>,
    cfg: &mut Config,
    timeout: Duration,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let print = |text: String| say(report, Tone::Plain, text);
    if let Some(pid) = alive(env.paths) {
        print(format!(
            "local vLLM already running (pid {pid}) at {}",
            url(cfg)
        ));
        return Ok(());
    }
    let exe = cfg
        .get("LOCAL_VLLM_BIN")
        .filter(|e| !e.is_empty())
        .unwrap_or("vllm")
        .to_string();
    let Some(found) = find_program(&exe) else {
        return Err(CommandError::Exit(format!(
            "'{exe}' not found. Install vLLM (pip install vllm) or set LOCAL_VLLM_BIN to its path."
        )));
    };
    let key = profiles::ensure_vllm_api_key(cfg, env.paths)?;
    let mut local = cfg.clone();
    local.set("VLLM_PORT", &port(cfg));
    let args = build_vllm_args(&local, &key, Some("127.0.0.1"));
    let mut command = viy_exec::Command::new(&found).args(args.iter().skip(1));
    if let Some(token) = cfg.get("HF_TOKEN").filter(|t| !t.is_empty()) {
        command = command
            .env("HF_TOKEN", token)
            .env("HUGGING_FACE_HUB_TOKEN", token);
    }
    fs::create_dir_all(env.paths.pod_state_dir())?;
    let log_path = env.paths.local_log_file();
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let now = env.time.now().with_timezone(&Local);
    writeln!(log, "=== viy up {} ===", now.format("%Y-%m-%d %H:%M:%S"))?;
    let child = env.exec.spawn(command, Spawn::Logged { log })?;
    let pid = child.id();
    let state = json!({"pid": pid, "port": port(cfg), "started": regions::now(env.time)});
    write_atomically(&env.paths.local_state_file(), &state.to_string())?;
    print(format!(
        "started vllm (pid {pid}); log: {}",
        log_path.display()
    ));

    let probe = Loading {
        child,
        http: env.http.as_ref(),
        cfg,
    };
    match wait_with_clock("waiting for local vLLM", timeout, POLL, probe, report).await? {
        Some(Ended::Ready) => {
            print(format!("ready at {}", url(cfg)));
            Ok(())
        }
        Some(Ended::Exited(code)) => {
            print(tail(env.paths, 20));
            let _ = fs::remove_file(env.paths.local_state_file());
            Err(CommandError::Exit(format!(
                "vllm exited with {code}; see {}",
                log_path.display()
            )))
        }
        None => {
            print(tail(env.paths, 20));
            Err(CommandError::Exit(format!(
                "vllm did not become ready in {}s; it is still running (viy logs -f, or viy down \
                 to stop it)",
                timeout.as_secs()
            )))
        }
    }
}

/// `viy status`.
pub async fn status(env: &Env<'_>, cfg: &Config, report: Reporter<'_>) -> Result<(), CommandError> {
    let print = |text: String| say(report, Tone::Plain, text);
    let process =
        alive(env.paths).map_or_else(|| "not running".to_string(), |pid| format!("pid {pid}"));
    let ready = if healthy(env.http.as_ref(), cfg, Duration::from_secs(5)).await {
        "ready"
    } else {
        "not ready"
    };
    print("  backend:   local vLLM".into());
    print(format!("  process:   {process}"));
    print(format!("  endpoint:  {}", url(cfg)));
    print(format!("  vllm:      {ready}"));
    print(format!(
        "  log:       {}",
        env.paths.local_log_file().display()
    ));
    Ok(())
}

/// `viy down` and `viy stop`: ends vLLM's whole process group (its engine
/// workers with it), SIGTERM with 20 s to go, then SIGKILL.
pub async fn down(env: &Env<'_>, report: Reporter<'_>) -> Result<(), CommandError> {
    let print = |text: String| say(report, Tone::Plain, text);
    let Some(pid) = alive(env.paths) else {
        print("local vLLM is not running".into());
        let _ = fs::remove_file(env.paths.local_state_file());
        return Ok(());
    };
    let group = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
    for (signal, wait) in [(libc::SIGTERM, 20), (libc::SIGKILL, 5)] {
        // SAFETY: a signal to the process group our vllm leads.
        if unsafe { libc::killpg(group, signal) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            return Err(error.into());
        }
        let end = Instant::now() + Duration::from_secs(wait);
        while Instant::now() < end && process::pid_alive(pid) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if !process::pid_alive(pid) {
            break;
        }
    }
    let _ = fs::remove_file(env.paths.local_state_file());
    print(format!("stopped local vLLM (pid {pid})"));
    Ok(())
}

/// `viy logs`: the last 200 lines of vLLM's log; with `follow`, `tail -F`
/// on the terminal until Ctrl-C (`interrupted`), which ends it quietly.
pub async fn logs(
    env: &Env<'_>,
    follow: bool,
    interrupted: impl Future<Output = ()>,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let log = env.paths.local_log_file();
    if !log.exists() {
        say(
            report,
            Tone::Plain,
            format!("no log yet at {}", log.display()),
        );
        return Ok(());
    }
    if !follow {
        say(report, Tone::Plain, tail(env.paths, 200));
        return Ok(());
    }
    let command = viy_exec::Command::new("tail")
        .args(["-n", "200", "-F"])
        .arg(&log);
    let mut child = env.exec.spawn(command, Spawn::Terminal)?;
    tokio::select! {
        status = child.wait() => {
            status?;
        }
        () = interrupted => {
            // The terminal's Ctrl-C reached tail too.
            let _ = child.terminate();
            let _ = child.wait().await;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "local_tests.rs"]
mod tests;
