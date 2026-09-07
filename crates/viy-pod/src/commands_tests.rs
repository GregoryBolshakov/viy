//! The pod commands against a scripted RunPod and vLLM (RouteHttp), on
//! paused time. Every config sets its own VLLM_PORT and TUNNEL_LOCAL_PORT,
//! so `down`'s sweep for stray forwards can never match a real tunnel.

use std::sync::Mutex;
use std::sync::PoisonError;

use chrono::TimeZone;
use chrono::Utc;
use pretty_assertions::assert_eq;
use viy_exec::Command;
use viy_testkit::exec::FakeExec;
use viy_testkit::exec::Reply as Run;
use viy_testkit::http::Reply;
use viy_testkit::http::RouteHttp;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::sshd::free_port;
use viy_testkit::time::FakeTime;

use super::*;
use crate::bootreport::Records;

const API_KEY: &str = "rpa_SYNTHETICKEY0000";
const PROXY: &str = "https://pod111-18999.proxy.runpod.net";

/// What a command was told, in order.
#[derive(Default, Clone)]
struct Told(Arc<Mutex<Vec<Report>>>);

impl Told {
    fn reporter(&self) -> impl Fn(Report) + Send + Sync + 'static {
        let told = Arc::clone(&self.0);
        move |r| told.lock().unwrap_or_else(PoisonError::into_inner).push(r)
    }

    fn notices(&self) -> Vec<(Tone, String)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                Report::Notice(n) => Some((n.tone, n.text.clone())),
                _ => None,
            })
            .collect()
    }

    fn texts(&self) -> Vec<String> {
        self.notices().into_iter().map(|(_, t)| t).collect()
    }

    fn alerts(&self) -> Vec<Alert> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                Report::Alert(a) => Some(a.clone()),
                _ => None,
            })
            .collect()
    }

    fn passing(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                Report::Passing(p) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }
}

struct Home {
    _dir: ScratchDir,
    paths: Paths,
    cfg: Config,
    time: FakeTime,
    exec: FakeExec,
    http: Arc<dyn HttpClient>,
    api: Arc<RouteHttp>,
}

impl Home {
    fn new(
        state: Option<Value>,
        answer: impl Fn(&viy_http::Request) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let dir = ScratchDir::new("commands").unwrap();
        let paths = Paths::under(dir.path());
        if let Some(state) = state {
            std::fs::create_dir_all(paths.pod_state_dir()).unwrap();
            std::fs::write(paths.pod_state_file(), state.to_string()).unwrap();
        }
        let mut cfg = Config::defaults();
        for (k, v) in [
            ("RUNPOD_API_KEY", API_KEY),
            ("POD_NAME", "qwen-pod"),
            ("NETWORK_VOLUME_ID", "vol222"),
            ("VLLM_API_KEY", "sk-SYNTHETICENDPOINT"),
            ("TUNNEL", "0"),
            ("VLLM_PORT", "18999"),
        ] {
            cfg.set(k, v);
        }
        cfg.set("TUNNEL_LOCAL_PORT", &free_port().unwrap().to_string());
        let api = Arc::new(RouteHttp::new(answer));
        Self {
            _dir: dir,
            paths,
            cfg,
            time: FakeTime::at(Utc.with_ymd_and_hms(2026, 7, 16, 9, 30, 0).unwrap()),
            exec: FakeExec::new([Run::StandIn(Command::new("true"))]),
            http: api.clone(),
            api,
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

    /// The requests so far, as `METHOD URL`, without the log stream's.
    fn sent(&self) -> Vec<String> {
        self.api
            .sent()
            .iter()
            .filter(|r| !r.url.contains("/logs"))
            .map(|r| format!("{} {}", r.method, r.url))
            .collect()
    }

    fn state(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.paths.pod_state_file()).unwrap())
            .unwrap()
    }
}

fn json_reply(value: Value) -> Reply {
    Reply::answer(200, value.to_string())
}

/// A platform log with one boot in it.
fn boot_log() -> Reply {
    let lines = [
        ("2026-07-16T09:30:01Z", "=== boot Thu ==="),
        ("2026-07-16T09:30:02Z", "RPCTL_PHASE starting vLLM"),
        ("2026-07-16T09:30:03Z", "RPCTL_PHASE watchdog armed"),
        ("2026-07-16T09:30:04Z", "API server: HTTP server started"),
    ];
    let body: String = lines
        .iter()
        .map(|(ts, line)| {
            format!(
                "data: {}\n",
                json!({"ts": ts, "line": line, "source": "container"})
            )
        })
        .collect();
    Reply::answer(200, body)
}

fn pod(status: &str) -> Value {
    json!({"id": "pod111", "name": "qwen-pod", "desiredStatus": status, "costPerHr": 1.89})
}

/// RunPod with our pod in `status`, a log, and a vLLM that answers 200.
fn runpod(status: &'static str) -> impl Fn(&viy_http::Request) -> Reply + Send + Sync + 'static {
    move |r| {
        let url = r.url.as_str();
        if url.contains("/logs") {
            boot_log()
        } else if url.ends_with("/health") {
            Reply::answer(200, "")
        } else if url.ends_with("/pods/pod111") {
            json_reply(pod(status))
        } else if url.ends_with("/start") || url.ends_with("/stop") {
            json_reply(json!({}))
        } else {
            panic!("unexpected {} {url}", r.method)
        }
    }
}

fn tracked(tunnel_only: bool) -> Option<Value> {
    Some(json!({"pod_id": "pod111", "volume_id": "vol222", "tunnel_only": tunnel_only}))
}

async fn run_up(home: &mut Home, told: &Told) -> Result<(), CommandError> {
    let env = Env {
        paths: &home.paths,
        exec: &home.exec,
        http: &home.http,
        time: &home.time,
        exe: Path::new("/opt/viy/bin/viy"),
    };
    up(
        &env,
        &mut home.cfg,
        Duration::from_secs(60),
        &Interrupt::never(),
        &told.reporter(),
    )
    .await
}

// ------------------------------------------------------------------ up

/// `up` with Ctrl-C pressed `when` into it, again `again` later.
async fn up_interrupted(
    home: &mut Home,
    told: &Told,
    when: Duration,
    again: Option<Duration>,
) -> Result<(), CommandError> {
    let (tx, interrupt) = Interrupt::channel();
    let presses = async move {
        tokio::time::sleep(when).await;
        tx.send_modify(|c| *c += 1);
        if let Some(again) = again {
            tokio::time::sleep(again).await;
            tx.send_modify(|c| *c += 1);
        }
        std::future::pending::<()>().await;
    };
    let env = Env {
        paths: &home.paths,
        exec: &home.exec,
        http: &home.http,
        time: &home.time,
        exe: Path::new("/opt/viy/bin/viy"),
    };
    let report = told.reporter();
    let up = up(
        &env,
        &mut home.cfg,
        Duration::from_secs(60),
        &interrupt,
        &report,
    );
    tokio::select! {
        result = up => result,
        () = presses => unreachable!(),
    }
}

/// vLLM never answers 200.
fn never_ready(
    status: &'static str,
) -> impl Fn(&viy_http::Request) -> Reply + Send + Sync + 'static {
    let runpod = runpod(status);
    move |r| {
        if r.url.ends_with("/health") {
            Reply::answer(503, "")
        } else {
            runpod(r)
        }
    }
}

#[tokio::test(start_paused = true)]
async fn ctrl_c_during_the_wait_records_the_boot_then_ends_up() {
    let mut home = Home::new(tracked(false), never_ready("EXITED"));
    let told = Told::default();
    let error = up_interrupted(&mut home, &told, Duration::from_millis(5500), None)
        .await
        .unwrap_err();
    assert!(matches!(error, CommandError::Interrupted));
    let history = Records::new(&home.paths).history(10);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["outcome"], "interrupted");
}

#[tokio::test(start_paused = true)]
async fn ctrl_c_before_the_wait_ends_up_at_once() {
    // RunPod takes 10 s to answer whether the pod exists.
    let mut home = Home::new(tracked(false), |r: &viy_http::Request| {
        if r.url.ends_with("/pods/pod111") {
            Reply::Answer {
                status: 200,
                body: vec![viy_testkit::http::Piece::Late(
                    Duration::from_secs(10),
                    pod("EXITED").to_string().into_bytes(),
                )],
            }
        } else {
            panic!("unexpected {}", r.url)
        }
    });
    let told = Told::default();
    let begun = Instant::now();
    let error = up_interrupted(&mut home, &told, Duration::from_secs(1), None)
        .await
        .unwrap_err();
    assert!(matches!(error, CommandError::Interrupted));
    assert_eq!(begun.elapsed(), Duration::from_secs(1));
    assert!(!home.sent().iter().any(|r| r.ends_with("/start")));
}

#[tokio::test(start_paused = true)]
async fn a_second_ctrl_c_ends_the_record_of_an_interrupted_boot() {
    // The boot report's log fetch hangs; a second Ctrl-C still ends up.
    let mut home = Home::new(tracked(false), move |r: &viy_http::Request| {
        if r.url.contains("/logs") {
            if r.url.contains("tail=600") {
                Reply::Hang
            } else {
                boot_log()
            }
        } else if r.url.ends_with("/health") {
            Reply::answer(503, "")
        } else {
            runpod("EXITED")(r)
        }
    });
    let told = Told::default();
    let begun = Instant::now();
    let error = up_interrupted(
        &mut home,
        &told,
        Duration::from_secs(3),
        Some(Duration::from_secs(2)),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, CommandError::Interrupted));
    assert_eq!(begun.elapsed(), Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn up_resumes_a_stopped_pod_and_records_its_boot() {
    let mut home = Home::new(tracked(false), runpod("EXITED"));
    let told = Told::default();
    run_up(&mut home, &told).await.unwrap();
    assert_eq!(
        home.sent(),
        [
            "GET https://rest.runpod.io/v1/pods/pod111",
            "POST https://rest.runpod.io/v1/pods/pod111/start",
            &format!("GET {PROXY}/health"),
        ]
    );
    let texts = told.texts();
    assert_eq!(
        texts[0],
        "Resuming stopped pod pod111 (named 'qwen-pod')..."
    );
    assert_eq!(texts[1], format!("Endpoint: {PROXY}"));
    assert_eq!(texts[3], "Ready in 0s. Run viy chat");
    assert!(
        texts[4].starts_with("Boot breakdown (budget 180s):"),
        "{texts:?}"
    );
    assert!(texts[5].starts_with("Idle watchdog armed"), "{texts:?}");
    assert!(
        texts[6].starts_with("Boot record: viy boots show"),
        "{texts:?}"
    );
    assert_eq!(
        told.alerts(),
        [Alert {
            title: "viy up: vLLM is serving".into(),
            body: "ready in 0s".into(),
            urgent: false,
        }]
    );
    let records = Records::new(&home.paths);
    let row = records.history(1).pop().unwrap();
    assert_eq!(row["outcome"], "ready");
    let record = records.read(row["boot_id"].as_str().unwrap()).unwrap();
    let events: Vec<&str> = record["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        events,
        ["viy up started", "resume requested", "vLLM healthy"]
    );
    assert_eq!(
        std::fs::read_to_string(home.paths.activity_file()).unwrap(),
        "1784194200\n"
    );
    assert_eq!(home.exec.sent()[0].argv()[1], "capture-boot");
}

#[tokio::test(start_paused = true)]
async fn up_reuses_a_running_pod_without_a_record() {
    let mut home = Home::new(tracked(false), runpod("RUNNING"));
    let told = Told::default();
    run_up(&mut home, &told).await.unwrap();
    assert_eq!(
        told.texts()[0],
        "Pod pod111 named 'qwen-pod' already exists (RUNNING); reusing it."
    );
    assert!(!home.sent().iter().any(|r| r.ends_with("/start")));
    assert_eq!(Records::new(&home.paths).history(10), Vec::<Value>::new());
    assert_eq!(home.exec.left(), 1, "no capture");
}

#[tokio::test(start_paused = true)]
async fn a_pod_from_before_tunnel_mode_is_said_to_keep_its_proxy() {
    let mut home = Home::new(Some(json!({"pod_id": "pod111"})), runpod("RUNNING"));
    home.cfg.set("TUNNEL", "1");
    let told = Told::default();
    // Without tunnel_only it is taken for tunnel-only, as TUNNEL says, and
    // this script maps no SSH port: the wait runs out.
    let error = run_up(&mut home, &told).await.unwrap_err();
    assert!(matches!(error, CommandError::Failed));
    assert_eq!(
        told.notices()[1],
        (
            Tone::Warn,
            "This pod predates tunnel mode. The tunnel works on it, but the pod also still \
             exposes an http port through RunPod's proxy; only recreating it (viy down && viy \
             up) closes that route."
                .to_string()
        )
    );
}

/// RunPod with no pod of ours, our volume, and `create`'s answer to the
/// create request.
fn empty_runpod(create: Reply) -> impl Fn(&viy_http::Request) -> Reply + Send + Sync + 'static {
    move |r| {
        let url = r.url.as_str();
        if url.contains("/logs") {
            boot_log()
        } else if url.ends_with("/health") {
            Reply::answer(200, "")
        } else if url.ends_with("/v1/pods") && r.method == Method::GET {
            json_reply(
                json!([{"id": "pod999", "name": "someone-else", "desiredStatus": "RUNNING"}]),
            )
        } else if url.ends_with("/v1/pods") {
            create.clone()
        } else if url.ends_with("/networkvolumes/vol222") {
            json_reply(
                json!({"id": "vol222", "name": "qwen38-weights", "dataCenterId": "EUR-IS-1"}),
            )
        } else if url.ends_with("/pods/pod111") {
            json_reply(pod("RUNNING"))
        } else {
            panic!("unexpected {} {url}", r.method)
        }
    }
}

/// rpctl test_regions.py CreatePodTests::test_success_path_is_one_create_request,
/// through `up`.
#[tokio::test(start_paused = true)]
async fn success_path_is_one_create_request() {
    let created = json_reply(json!({"id": "pod111", "costPerHr": 1.89}));
    let mut home = Home::new(None, empty_runpod(created));
    let told = Told::default();
    run_up(&mut home, &told).await.unwrap();
    let creates: Vec<viy_http::Request> = home
        .api
        .sent()
        .into_iter()
        .filter(|r| r.method == Method::POST && r.url.ends_with("/v1/pods"))
        .collect();
    assert_eq!(creates.len(), 1);
    let body: Value = serde_json::from_slice(creates[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(body["dataCenterIds"], json!(["EUR-IS-1"]));
    assert_eq!(body["networkVolumeId"], "vol222");
    let texts = told.texts();
    assert_eq!(
        &texts[..4],
        [
            "Creating pod in EUR-IS-1 (1x from: NVIDIA A100 80GB PCIe, NVIDIA A100-SXM4-80GB, NVIDIA RTX PRO 6000 Blackwell Server Edition...)",
            "  pod id:   pod111",
            "  cost:     $1.89/hr  <-- billing has started",
            "  transport: RunPod http proxy (TLS terminated by RunPod)",
        ]
    );
    assert!(texts[4].starts_with("\nReusing the network volume;"));
    assert!(texts.contains(&"vLLM is serving. Startup took 0s. Run viy chat".to_string()));
    let state = home.state();
    assert_eq!(state["pod_id"], "pod111");
    assert_eq!(state["volume_id"], "vol222");
    assert_eq!(state["tunnel_only"], false);
    assert_eq!(state["cost_per_hr"], 1.89);
    assert_eq!(state["created_at"], 1784194200.0);
    assert_eq!(state["last_startup_seconds"], 0.0);
}

/// rpctl test_regions.py CreatePodTests::test_failover_off_exits_after_the_primary.
#[tokio::test(start_paused = true)]
async fn failover_off_exits_after_the_primary() {
    let refused = Reply::answer(
        500,
        json!({"error": "There are no instances currently available"}).to_string(),
    );
    let mut home = Home::new(None, empty_runpod(refused));
    home.cfg.set("FAILOVER", "0");
    let told = Told::default();
    let error = run_up(&mut home, &told).await.unwrap_err();
    assert!(matches!(error, CommandError::Failed));
    let creates = home
        .sent()
        .into_iter()
        .filter(|r| r == "POST https://rest.runpod.io/v1/pods")
        .count();
    assert_eq!(creates, 1);
    let notices = told.notices();
    assert_eq!(
        notices[1],
        (Tone::Warn, "  EUR-IS-1 (primary): no GPU free".into())
    );
    assert_eq!(notices[2].0, Tone::Error);
    assert!(
        notices[2].1.starts_with("POST /pods -> HTTP 500"),
        "{notices:?}"
    );
    assert_eq!(
        notices[3].1,
        format!(
            "\nNo GPU free in EUR-IS-1 (FAILOVER=0: no backup offered). Retry in a minute \
             (stock flickers); viy backup shows RunPod's A100 stock.\nEvery refused pod creation \
             is logged in {}",
            home.paths.up_failures_file().display()
        )
    );
    let log = std::fs::read_to_string(home.paths.up_failures_file()).unwrap();
    let row: Value = serde_json::from_str(log.trim()).unwrap();
    assert_eq!(row["capacity"], true);
    assert_eq!(row["role"], "primary");
    assert_eq!(row["status_code"], 500);
    assert!(!home.paths.pod_state_file().exists(), "nothing tracked");
}

#[tokio::test(start_paused = true)]
async fn a_refusal_for_another_reason_names_the_usual_causes() {
    let refused = Reply::answer(402, json!({"error": "insufficient balance"}).to_string());
    let mut home = Home::new(None, empty_runpod(refused));
    let told = Told::default();
    run_up(&mut home, &told).await.unwrap_err();
    let last = told.texts().pop().unwrap();
    assert!(
        last.starts_with("\nCommon causes: insufficient RunPod credit"),
        "{last}"
    );
}

#[tokio::test(start_paused = true)]
async fn tunnel_mode_without_an_ssh_key_creates_nothing() {
    let mut home = Home::new(None, empty_runpod(Reply::answer(500, "")));
    home.cfg.set("TUNNEL", "1");
    let told = Told::default();
    let error = run_up(&mut home, &told).await.unwrap_err();
    let CommandError::Exit(message) = error else {
        panic!("{error:?}")
    };
    assert!(message.starts_with("Tunnel mode needs an SSH key"));
    assert!(message.ends_with("TUNNEL=0 viy up"));
    assert!(!home.sent().iter().any(|r| r.starts_with("POST")));
}

#[tokio::test(start_paused = true)]
async fn up_creates_nothing_when_runpod_cannot_say_whether_a_pod_exists() {
    let mut home = Home::new(None, |_| Reply::answer(503, "down"));
    let told = Told::default();
    let error = run_up(&mut home, &told).await.unwrap_err();
    assert!(matches!(error, CommandError::Failed));
    assert!(told.texts()[0].starts_with("Could not check RunPod for an existing pod"));
    assert!(!home.sent().iter().any(|r| r.starts_with("POST")));
}

#[tokio::test(start_paused = true)]
async fn up_waits_its_turn_behind_another_up_stop_or_down() {
    let mut home = Home::new(tracked(false), runpod("RUNNING"));
    let _held = PodLock::exclusive(&home.paths, "stop").unwrap();
    let told = Told::default();
    let error = run_up(&mut home, &told).await.unwrap_err();
    let CommandError::Exit(message) = error else {
        panic!("{error:?}")
    };
    assert!(message.starts_with("Another "), "{message}");
    assert!(message.ends_with("then re-run viy up."), "{message}");
    assert_eq!(home.sent(), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn a_new_volume_is_made_and_remembered_when_none_is_found() {
    let home = Home::new(None, |r: &viy_http::Request| {
        let url = r.url.as_str();
        if url.ends_with("/networkvolumes/vol222") {
            Reply::answer(404, json!({"error": "not found"}).to_string())
        } else if url.ends_with("/networkvolumes") && r.method == Method::GET {
            json_reply(json!([{"id": "vol333", "name": "other"}]))
        } else if url.ends_with("/networkvolumes") {
            json_reply(json!({"id": "vol444"}))
        } else {
            panic!("unexpected {url}")
        }
    });
    let told = Told::default();
    let env = home.env();
    let rp = RunPod::new(API_KEY, Arc::clone(&home.http));
    let mut cfg = home.cfg.clone();
    let got = ensure_volume(&env, &mut cfg, &rp, &told.reporter())
        .await
        .unwrap();
    assert_eq!(got, ("vol444".to_string(), true));
    assert_eq!(cfg.get("NETWORK_VOLUME_ID"), Some("vol444"));
    let written = std::fs::read_to_string(home.paths.config_file()).unwrap();
    assert!(written.contains("NETWORK_VOLUME_ID=vol444"), "{written}");
    assert_eq!(
        told.texts(),
        [
            "Configured NETWORK_VOLUME_ID no longer exists; making a new one.",
            "Creating 100GB network volume 'qwen38-weights' in EUR-IS-1...",
            "  volume id: vol444",
        ]
    );
    let create = home.api.sent().pop().unwrap();
    let body: Value = serde_json::from_slice(create.body.as_ref().unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"name": "qwen38-weights", "size": 100, "dataCenterId": "EUR-IS-1"})
    );
}

#[tokio::test(start_paused = true)]
async fn a_volume_found_by_name_brings_its_region() {
    let mut home = Home::new(None, |r: &viy_http::Request| {
        if r.url.ends_with("/networkvolumes") {
            json_reply(
                json!([{"id": "vol555", "name": "qwen38-weights", "dataCenterId": "US-KS-2"}]),
            )
        } else {
            panic!("unexpected {}", r.url)
        }
    });
    home.cfg.set("NETWORK_VOLUME_ID", "");
    let told = Told::default();
    let env = home.env();
    let rp = RunPod::new(API_KEY, Arc::clone(&home.http));
    let mut cfg = home.cfg.clone();
    let got = ensure_volume(&env, &mut cfg, &rp, &told.reporter())
        .await
        .unwrap();
    assert_eq!(got, ("vol555".to_string(), false));
    assert_eq!(cfg.get("DATA_CENTER_ID"), Some("US-KS-2"));
    assert_eq!(
        told.texts(),
        [
            "Volume 'qwen38-weights' is in US-KS-2 but DATA_CENTER_ID=EUR-IS-1. Using the volume's \
          region, since a pod must live where its volume lives."
        ]
    );
}

// -------------------------------------------------------------- status

#[tokio::test(start_paused = true)]
async fn status_shows_the_pod_and_whether_vllm_answers() {
    let home = Home::new(tracked(false), |r: &viy_http::Request| {
        if r.url.ends_with("/health") {
            // Unauthenticated, as rpctl asks: 401 means up.
            assert_eq!(r.header_value("Authorization"), None);
            Reply::answer(401, "")
        } else {
            json_reply(json!({
                "id": "pod111", "name": "qwen-pod", "desiredStatus": "RUNNING",
                "costPerHr": "1.89", "gpuCount": 1, "lastStartedAt": "2026-07-16T09:00:00Z",
                "gpu": {"displayName": "A100 SXM"}, "machine": {"dataCenterId": "EUR-IS-1"},
            }))
        }
    });
    std::fs::write(home.paths.activity_file(), "1784193000\n").unwrap();
    let told = Told::default();
    status(&home.env(), &home.cfg, &told.reporter())
        .await
        .unwrap();
    assert_eq!(
        told.texts(),
        [
            "  id:        pod111",
            "  name:      qwen-pod",
            "  state:     RUNNING",
            "  gpu:       A100 SXM x1",
            "  region:    EUR-IS-1",
            "  cost:      $1.89/hr",
            "  started:   2026-07-16T09:00:00Z",
            &format!("  endpoint:  {PROXY}"),
            "  transport: RunPod http proxy (TLS terminated by RunPod)",
            "  vllm:      ready",
            "  idle:      20 min since this machine last streamed; the pod shuts itself down \
             after 60 min without tokens",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn status_without_a_pod_or_runpod() {
    let home = Home::new(None, |r: &viy_http::Request| {
        assert!(r.url.ends_with("/v1/pods"));
        json_reply(json!([]))
    });
    let told = Told::default();
    status(&home.env(), &home.cfg, &told.reporter())
        .await
        .unwrap();
    assert_eq!(told.texts(), ["No pod named 'qwen-pod' found. Run viy up"]);

    let home = Home::new(None, |_| Reply::answer(401, "{\"error\": \"bad key\"}"));
    let told = Told::default();
    status(&home.env(), &home.cfg, &told.reporter())
        .await
        .unwrap();
    assert_eq!(
        told.texts()[0],
        "Could not reach RunPod to check status; local state left as-is."
    );
}

#[tokio::test(start_paused = true)]
async fn no_runpod_key_is_said_as_rp_says_it() {
    let mut home = Home::new(None, |_| panic!("no request without a key"));
    home.cfg.set("RUNPOD_API_KEY", "");
    let told = Told::default();
    let error = status(&home.env(), &home.cfg, &told.reporter())
        .await
        .unwrap_err();
    let CommandError::Exit(message) = error else {
        panic!("{error:?}")
    };
    assert!(
        message.starts_with("RUNPOD_API_KEY is not set."),
        "{message}"
    );
}

// ---------------------------------------------------------------- stop

#[tokio::test(start_paused = true)]
async fn stop_stops_the_pod() {
    let home = Home::new(tracked(false), runpod("RUNNING"));
    let told = Told::default();
    stop(&home.env(), &home.cfg, &told.reporter())
        .await
        .unwrap();
    assert_eq!(
        home.sent(),
        [
            "GET https://rest.runpod.io/v1/pods/pod111",
            "POST https://rest.runpod.io/v1/pods/pod111/stop",
        ]
    );
    assert_eq!(
        told.texts(),
        [
            "Stopped pod111. GPU billing ends; the pod can be resumed with viy up.",
            "Note: a stopped pod still incurs disk charges and may lose its GPU slot.",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn stop_without_a_pod_stops_nothing() {
    let home = Home::new(None, |_| json_reply(json!([])));
    let told = Told::default();
    stop(&home.env(), &home.cfg, &told.reporter())
        .await
        .unwrap();
    assert_eq!(told.texts(), ["No pod to stop."]);
}

// ---------------------------------------------------------------- down

/// RunPod listing `first` until a delete, then nothing; deletes answer
/// `delete`.
fn listing(
    first: Vec<Value>,
    delete: u16,
) -> impl Fn(&viy_http::Request) -> Reply + Send + Sync + 'static {
    let deleted = Mutex::new(false);
    move |r| {
        if r.method == Method::DELETE {
            *deleted.lock().unwrap() = true;
            return Reply::answer(delete, "{}");
        }
        assert!(r.url.ends_with("/v1/pods"), "{}", r.url);
        if *deleted.lock().unwrap() {
            json_reply(json!([]))
        } else {
            json_reply(Value::Array(first.clone()))
        }
    }
}

#[tokio::test(start_paused = true)]
async fn down_terminates_every_pod_of_ours_then_watches_for_quiet() {
    let ours = json!({"id": "pod111", "name": "qwen-pod", "desiredStatus": "RUNNING"});
    let tracked_other = json!({"id": "pod000", "name": "renamed", "desiredStatus": "EXITED"});
    let gone = json!({"id": "pod777", "name": "qwen-pod", "desiredStatus": "TERMINATED"});
    let theirs = json!({"id": "pod999", "name": "someone-else", "desiredStatus": "RUNNING"});
    let home = Home::new(
        Some(json!({"pod_id": "pod000"})),
        listing(vec![ours, tracked_other, gone, theirs], 200),
    );
    let told = Told::default();
    let begun = Instant::now();
    down(&home.env(), &home.cfg, false, &told.reporter())
        .await
        .unwrap();
    assert_eq!(
        home.sent()
            .into_iter()
            .filter(|r| r.starts_with("DELETE"))
            .collect::<Vec<_>>(),
        [
            "DELETE https://rest.runpod.io/v1/pods/pod111",
            "DELETE https://rest.runpod.io/v1/pods/pod000",
        ]
    );
    assert_eq!(
        told.texts(),
        [
            "  terminated pod111 (was RUNNING)",
            "  terminated pod000 (was EXITED)",
            "Terminated pod111, pod000. No pod is running or starting; GPU and pod-disk billing \
             has ended.",
            "Model and startup caches stay on the network volume for the next viy up.",
        ]
    );
    // 2 s after the deletes, then 10 s of RunPod listing nothing.
    assert_eq!(begun.elapsed(), Duration::from_secs(12));
    let countdown = told.passing();
    assert_eq!(
        countdown.first().unwrap(),
        "  confirming nothing comes back... 10s "
    );
    assert_eq!(
        countdown.last().unwrap(),
        "  confirming nothing comes back...  1s "
    );
    assert!(!home.paths.pod_state_file().exists());
}

#[tokio::test(start_paused = true)]
async fn down_with_nothing_running_says_so() {
    let home = Home::new(None, listing(vec![], 200));
    let told = Told::default();
    down(&home.env(), &home.cfg, false, &told.reporter())
        .await
        .unwrap();
    assert_eq!(
        told.texts()[0],
        "No pod was running or starting. Nothing is billing for a pod."
    );
}

#[tokio::test(start_paused = true)]
async fn down_after_cancelling_its_own_up_watches_a_minute() {
    let home = Home::new(None, listing(vec![], 200));
    let told = Told::default();
    let begun = Instant::now();
    down(&home.env(), &home.cfg, true, &told.reporter())
        .await
        .unwrap();
    assert_eq!(
        told.texts()[0],
        "The killed viy up may already have asked RunPod for a pod, so watching 60s for one to \
         appear."
    );
    assert_eq!(begun.elapsed(), Duration::from_secs(60));
}

#[tokio::test(start_paused = true)]
async fn down_that_cannot_confirm_says_money_may_be_spent() {
    let ours = json!({"id": "pod111", "name": "qwen-pod", "desiredStatus": "RUNNING"});
    let home = Home::new(tracked(false), listing(vec![ours], 403));
    let told = Told::default();
    let error = down(&home.env(), &home.cfg, false, &told.reporter())
        .await
        .unwrap_err();
    assert!(matches!(error, CommandError::Failed));
    let notices = told.notices();
    assert_eq!(
        notices[0],
        (
            Tone::Error,
            "COULD NOT CONFIRM THAT NO POD IS RUNNING. Money may still be spent.".into()
        )
    );
    assert!(notices[1].1.starts_with("DELETE /pods/pod111 -> HTTP 403"));
    assert_eq!(
        notices[2].1,
        "Re-run viy down, or check https://console.runpod.io/pods for pods named 'qwen-pod'."
    );
    assert!(home.paths.pod_state_file().exists(), "kept for the retry");
}

#[tokio::test(start_paused = true)]
async fn a_pod_already_gone_counts_as_terminated() {
    let ours = json!({"id": "pod111", "name": "qwen-pod", "desiredStatus": "RUNNING"});
    let home = Home::new(None, listing(vec![ours], 404));
    let told = Told::default();
    down(&home.env(), &home.cfg, false, &told.reporter())
        .await
        .unwrap();
    assert_eq!(told.texts()[0], "  terminated pod111 (was RUNNING)");
}

// ---------------------------------------------------------------- logs

#[tokio::test(start_paused = true)]
async fn logs_print_the_platform_log_masked() {
    let home = Home::new(tracked(false), |r: &viy_http::Request| {
        if r.url.contains("/logs") {
            let body: String = [
                json!({"ts": "2026-07-16T09:30:01Z", "source": "container", "line": "key sk-SYNTHETICENDPOINT"}),
                json!({"line": "no time"}),
            ]
            .iter()
            .map(|e| format!("data: {e}\n"))
            .collect();
            Reply::answer(200, body)
        } else {
            json_reply(pod("RUNNING"))
        }
    });
    let told = Told::default();
    logs(
        &home.env(),
        &home.cfg,
        false,
        std::future::pending(),
        &told.reporter(),
    )
    .await
    .unwrap();
    assert_eq!(
        told.texts(),
        [
            "RunPod platform + container logs (available before SSH):",
            " [?] no time",
            "2026-07-16T09:30:01Z [container] key [redacted]",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn following_logs_prints_each_line_once_until_interrupted() {
    let home = Home::new(tracked(false), |r: &viy_http::Request| {
        if r.url.contains("/logs") {
            Reply::answer(
                200,
                format!("data: {}\n", json!({"ts": "t1", "line": "same"})),
            )
        } else {
            json_reply(pod("RUNNING"))
        }
    });
    let told = Told::default();
    logs(
        &home.env(),
        &home.cfg,
        true,
        tokio::time::sleep(Duration::from_secs(7)),
        &told.reporter(),
    )
    .await
    .unwrap();
    assert_eq!(told.texts().len(), 2);
    let fetches = home
        .api
        .sent()
        .iter()
        .filter(|r| r.url.contains("/logs"))
        .count();
    assert_eq!(fetches, 4, "at 0, 2, 4 and 6 s");
}

#[tokio::test(start_paused = true)]
async fn a_failed_log_fetch_points_to_the_web_console() {
    let home = Home::new(tracked(false), |r: &viy_http::Request| {
        if r.url.contains("/logs") {
            Reply::answer(500, "")
        } else {
            json_reply(pod("RUNNING"))
        }
    });
    let told = Told::default();
    logs(
        &home.env(),
        &home.cfg,
        false,
        std::future::pending(),
        &told.reporter(),
    )
    .await
    .unwrap();
    assert_eq!(
        told.texts()[1..],
        [
            "Platform logs -> HTTP 500".to_string(),
            "Web console logs: https://console.runpod.io/pods/pod111".to_string(),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_volume_without_a_region_creates_nothing() {
    let home = Home::new(None, |r: &viy_http::Request| {
        if r.url.ends_with("/networkvolumes") {
            json_reply(json!([{"id": "vol555", "name": "qwen38-weights"}]))
        } else {
            panic!("unexpected {}", r.url)
        }
    });
    let told = Told::default();
    let env = home.env();
    let rp = RunPod::new(API_KEY, Arc::clone(&home.http));
    let mut cfg = home.cfg.clone();
    cfg.set("NETWORK_VOLUME_ID", "");
    let error = ensure_volume(&env, &mut cfg, &rp, &told.reporter())
        .await
        .unwrap_err();
    let CommandError::Exit(message) = error else {
        panic!("{error:?}")
    };
    assert!(message.contains("without its data center"), "{message}");
    assert_eq!(cfg.get("DATA_CENTER_ID"), Some("EUR-IS-1"));
}

#[tokio::test(start_paused = true)]
async fn the_activity_stamp_is_throttled_per_state_directory() {
    let a = Home::new(None, |_| panic!("no request"));
    let b = Home::new(None, |_| panic!("no request"));
    idle::touch(&a.paths, &a.time, false);
    idle::touch(&b.paths, &b.time, false);
    assert!(b.paths.activity_file().exists());
    std::fs::remove_file(a.paths.activity_file()).unwrap();
    idle::touch(&a.paths, &a.time, false);
    assert!(!a.paths.activity_file().exists(), "within 30 s");
    idle::touch(&a.paths, &a.time, true);
    assert!(a.paths.activity_file().exists());
    assert_eq!(
        idle::last_activity(&a.paths),
        Some(Utc.with_ymd_and_hms(2026, 7, 16, 9, 30, 0).unwrap())
    );
}
