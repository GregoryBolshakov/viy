//! rpctl's local backend tests (the lifecycle, a crash), and the rest of
//! `viy up/status/down/logs` with BACKEND=local, against a scripted
//! /health and stand-in processes: no vllm runs.

use std::sync::Arc;
use std::sync::Mutex;

use chrono::TimeZone;
use chrono::Utc;
use pretty_assertions::assert_eq;
use viy_testkit::exec::FakeExec;
use viy_testkit::exec::Reply as Run;
use viy_testkit::exec::named_process;
use viy_testkit::http::Reply;
use viy_testkit::http::RouteHttp;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::time::FakeTime;

use super::*;
use crate::notice::Report;

struct Machine {
    dir: ScratchDir,
    paths: Paths,
    cfg: Config,
    exec: FakeExec,
    http: Arc<dyn HttpClient>,
    time: FakeTime,
}

impl Machine {
    /// A scratch home with an executable `vllm` (never run: the FakeExec
    /// answers with `replies`), and /health answering `health`.
    fn new(health: u16, replies: impl IntoIterator<Item = Run>) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = ScratchDir::new("local").unwrap();
        let paths = Paths::under(dir.path());
        let bin = dir.path().join("vllm");
        fs::write(&bin, "").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let mut cfg = Config::defaults();
        cfg.set("VLLM_API_KEY", "k");
        cfg.set("LOCAL_VLLM_PORT", "18123");
        cfg.set("LOCAL_VLLM_BIN", bin.to_str().unwrap());
        Self {
            paths,
            cfg,
            exec: FakeExec::new(replies),
            http: Arc::new(RouteHttp::new(move |r| {
                assert_eq!(r.url, "http://127.0.0.1:18123/health");
                assert_eq!(r.header_value("Authorization"), Some("Bearer k"));
                Reply::answer(health, "")
            })),
            time: FakeTime::at(Utc.with_ymd_and_hms(2026, 7, 17, 21, 0, 0).unwrap()),
            dir,
        }
    }

    fn env(&self) -> Env<'_> {
        Env {
            paths: &self.paths,
            exec: &self.exec,
            http: &self.http,
            time: &self.time,
            exe: Path::new("/opt/viy/bin/viy"),
        }
    }

    fn state(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.paths.local_state_file()).unwrap()).unwrap()
    }

    /// Records `pid` as the local vLLM.
    fn track(&self, pid: u32) {
        fs::create_dir_all(self.paths.pod_state_dir()).unwrap();
        fs::write(
            self.paths.local_state_file(),
            json!({"pid": pid}).to_string(),
        )
        .unwrap();
    }
}

#[derive(Default)]
struct Said(Mutex<Vec<String>>);

impl Said {
    fn report(&self) -> impl Fn(Report) + Send + Sync + '_ {
        |r| {
            if let Report::Notice(n) = r {
                self.0.lock().unwrap().push(n.text);
            }
        }
    }

    fn texts(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// A stand-in for vllm that stays up.
fn sleeper() -> Run {
    Run::StandIn(viy_exec::Command::new("sleep").arg("30"))
}

fn flag<'a>(argv: &'a [String], name: &str) -> &'a str {
    let i = argv.iter().position(|a| a == name).unwrap();
    &argv[i + 1]
}

#[tokio::test(start_paused = true)]
async fn local_backend_lifecycle() {
    let mut machine = Machine::new(200, [sleeper()]);
    machine.cfg.set("HF_TOKEN", "hf_SYNTHETIC");
    let said = Said::default();
    let mut cfg = machine.cfg.clone();
    up(
        &machine.env(),
        &mut cfg,
        Duration::from_secs(10),
        &said.report(),
    )
    .await
    .unwrap();
    let sent = machine.exec.sent();
    let argv = sent[0].argv();
    let bin = machine.dir.path().join("vllm");
    assert_eq!(
        argv[..3],
        [
            bin.to_str().unwrap(),
            "serve",
            machine.cfg.get("MODEL_REPO").unwrap()
        ]
    );
    assert_eq!(flag(&argv, "--host"), "127.0.0.1");
    assert_eq!(flag(&argv, "--port"), "18123");
    assert_eq!(
        sent[0].env,
        [
            ("HF_TOKEN".into(), "hf_SYNTHETIC".into()),
            ("HUGGING_FACE_HUB_TOKEN".into(), "hf_SYNTHETIC".into()),
        ]
    );
    let pid = machine.state()["pid"].as_u64().unwrap();
    assert_eq!(machine.state()["port"], "18123");
    let log = machine.paths.local_log_file();
    assert_eq!(
        said.texts(),
        [
            format!("started vllm (pid {pid}); log: {}", log.display()),
            "ready at http://127.0.0.1:18123".to_string(),
        ]
    );
    assert!(fs::read_to_string(&log).unwrap().starts_with("=== viy up "));
    // A `sleep` is not vllm, so only /health says it is there.
    assert_eq!(
        base_url(&machine.env(), &machine.cfg).await.as_deref(),
        Some("http://127.0.0.1:18123")
    );
    let unhealthy = Machine::new(503, []);
    assert_eq!(base_url(&unhealthy.env(), &unhealthy.cfg).await, None);
}

#[tokio::test]
async fn local_backend_reports_crash() {
    let machine = Machine::new(503, [Run::StandIn(viy_exec::Command::new("false"))]);
    let said = Said::default();
    let mut cfg = machine.cfg.clone();
    let error = up(
        &machine.env(),
        &mut cfg,
        Duration::from_secs(10),
        &said.report(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "vllm exited with 1; see {}",
            machine.paths.local_log_file().display()
        )
    );
    assert!(!machine.paths.local_state_file().exists());
}

#[tokio::test(start_paused = true)]
async fn a_vllm_that_never_serves_is_left_running() {
    let machine = Machine::new(503, [sleeper()]);
    let said = Said::default();
    let mut cfg = machine.cfg.clone();
    let error = up(
        &machine.env(),
        &mut cfg,
        Duration::from_secs(10),
        &said.report(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "vllm did not become ready in 10s; it is still running (viy logs -f, or viy down to \
         stop it)"
    );
    assert!(machine.paths.local_state_file().exists());
    let pid = machine.state()["pid"].as_u64().unwrap();
    // SAFETY: the stand-in sleep this test started.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

#[tokio::test(start_paused = true)]
async fn up_finds_a_running_vllm_or_says_where_vllm_is_missing() {
    let machine = Machine::new(200, []);
    let vllm = named_process("vllm", &["serve", "m"]).unwrap();
    machine.track(vllm.id());
    let said = Said::default();
    let mut cfg = machine.cfg.clone();
    up(
        &machine.env(),
        &mut cfg,
        Duration::from_secs(10),
        &said.report(),
    )
    .await
    .unwrap();
    assert_eq!(
        said.texts(),
        [format!(
            "local vLLM already running (pid {}) at http://127.0.0.1:18123",
            vllm.id()
        )]
    );
    let fresh = Machine::new(200, []);
    let mut cfg = fresh.cfg.clone();
    cfg.set("LOCAL_VLLM_BIN", "/nowhere/vllm");
    let error = up(&fresh.env(), &mut cfg, Duration::from_secs(10), &|_| {})
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "'/nowhere/vllm' not found. Install vLLM (pip install vllm) or set LOCAL_VLLM_BIN to its \
         path."
    );
}

#[tokio::test(start_paused = true)]
async fn status_then_down() {
    let machine = Machine::new(200, []);
    let vllm = named_process("vllm", &["serve", "m"]).unwrap();
    machine.track(vllm.id());
    let said = Said::default();
    status(&machine.env(), &machine.cfg, &said.report())
        .await
        .unwrap();
    assert_eq!(
        said.texts(),
        [
            "  backend:   local vLLM".to_string(),
            format!("  process:   pid {}", vllm.id()),
            "  endpoint:  http://127.0.0.1:18123".to_string(),
            "  vllm:      ready".to_string(),
            format!("  log:       {}", machine.paths.local_log_file().display()),
        ]
    );
    let said = Said::default();
    down(&machine.env(), &said.report()).await.unwrap();
    assert_eq!(
        said.texts(),
        [format!("stopped local vLLM (pid {})", vllm.id())]
    );
    assert!(!machine.paths.local_state_file().exists());
    let said = Said::default();
    down(&machine.env(), &said.report()).await.unwrap();
    assert_eq!(said.texts(), ["local vLLM is not running"]);
}

#[tokio::test]
async fn logs_show_the_tail_or_follow_it() {
    let machine = Machine::new(200, [Run::StandIn(viy_exec::Command::new("true"))]);
    let said = Said::default();
    logs(
        &machine.env(),
        false,
        std::future::pending(),
        &said.report(),
    )
    .await
    .unwrap();
    assert_eq!(
        said.texts(),
        [format!(
            "no log yet at {}",
            machine.paths.local_log_file().display()
        )]
    );
    fs::create_dir_all(machine.paths.pod_state_dir()).unwrap();
    let lines: String = (1..=250).map(|i| format!("line {i}\n")).collect();
    fs::write(machine.paths.local_log_file(), lines).unwrap();
    let said = Said::default();
    logs(
        &machine.env(),
        false,
        std::future::pending(),
        &said.report(),
    )
    .await
    .unwrap();
    let shown = said.texts().remove(0);
    assert!(shown.starts_with("line 51\n") && shown.ends_with("line 250"));
    logs(&machine.env(), true, std::future::pending(), &|_| {})
        .await
        .unwrap();
    assert_eq!(
        machine.exec.sent()[0].argv(),
        [
            "tail",
            "-n",
            "200",
            "-F",
            machine.paths.local_log_file().to_str().unwrap()
        ]
    );
}
