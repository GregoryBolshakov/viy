use std::io::Read;
use std::io::Write as _;
use std::net::TcpListener;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use chrono::TimeZone;
use pretty_assertions::assert_eq;
use viy_config::Paths;
use viy_testkit::exec::FakeExec;
use viy_testkit::exec::Reply as Run;
use viy_testkit::fixtures;
use viy_testkit::http::FakeHttp;
use viy_testkit::http::Reply;
use viy_testkit::http::RouteHttp;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::sshd::LocalSshd;
use viy_testkit::sshd::free_port;
use viy_testkit::time::FakeTime;

use super::*;
use crate::bootreport::tests::entries;
use crate::notice::ClockPrinter;

const API_KEY: &str = "rpa_SYNTHETICKEY0000";
const VLLM_KEY: &str = "sk-SYNTHETICENDPOINT";

/// What a test was told, in order.
#[derive(Default, Clone)]
struct Told(Arc<Mutex<Vec<Report>>>);

impl Told {
    fn reporter(&self) -> impl Fn(Report) + Send + Sync + 'static {
        let told = Arc::clone(&self.0);
        move |r| told.lock().unwrap_or_else(PoisonError::into_inner).push(r)
    }

    fn all(&self) -> Vec<Report> {
        self.0.lock().unwrap().clone()
    }

    fn notices(&self) -> Vec<(Tone, String)> {
        self.all()
            .into_iter()
            .filter_map(|r| match r {
                Report::Notice(n) => Some((n.tone, n.text)),
                _ => None,
            })
            .collect()
    }

    fn clocks(&self) -> Vec<Clock> {
        self.all()
            .into_iter()
            .filter_map(|r| match r {
                Report::Clock(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn alerts(&self) -> Vec<Alert> {
        self.all()
            .into_iter()
            .filter_map(|r| match r {
                Report::Alert(a) => Some(a),
                _ => None,
            })
            .collect()
    }
}

/// The log stream's answer: one `data:` line per line.
fn log_reply(lines: &[&str]) -> Reply {
    let body: String = lines
        .iter()
        .map(|l| format!("data: {}\n", json!({"line": l, "source": "container"})))
        .collect();
    Reply::answer(200, body)
}

fn tone_name(tone: Tone) -> &'static str {
    match tone {
        Tone::Plain => "plain",
        Tone::Dim => "dim",
        Tone::Ok => "ok",
        Tone::Warn => "warn",
        Tone::Error => "error",
    }
}

fn with(keys: &Value) -> Config {
    let mut cfg = Config::defaults();
    for (k, v) in keys.as_object().into_iter().flatten() {
        cfg.set(k, v.as_str().unwrap());
    }
    cfg
}

/// rpctl's text with viy's commands in it.
fn as_viy(text: &str) -> String {
    text.replace("rp logs", "viy logs")
        .replace("rp reap", "viy reap")
        .replace("rp down", "viy down")
}

/// pod._redact_log, _startup_progress and _report_watchdog, case by case.
#[tokio::test]
async fn summaries_and_masks_are_rpctls() {
    let fixture = fixtures::load("pod/bootreport").unwrap();
    let mut played = 0;
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        match case["call"].as_str().unwrap() {
            "redact" => {
                let cfg = with(&case["config"]);
                let got = redact_log(case["line"].as_str().unwrap(), &cfg);
                assert_eq!(got, case["redacted"].as_str().unwrap(), "{name}");
            }
            "progress" => {
                let replies = case["fetches"].as_array().unwrap().iter().map(|fetch| {
                    let body: String = fetch
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|e| format!("data: {e}\n"))
                        .collect();
                    Reply::answer(200, body)
                });
                let http = Arc::new(FakeHttp::new(replies));
                let rp = RunPod::new(API_KEY, http.clone());
                let progress = Progress::new(&rp, "pod111", case["require_ssh"].as_bool().unwrap());
                for (i, after) in case["after"].as_array().unwrap().iter().enumerate() {
                    // rpctl's summary is fetched on its first call, again
                    // only once 20 s have passed; one fetch each here.
                    if progress.summary().failure.is_none() {
                        progress.refresh().await;
                    }
                    let result = progress.summary().result();
                    let got = match result {
                        Ok(summary) => json!({"summary": summary, "ready": progress.pod_ready()}),
                        Err(error) => {
                            json!({"error": error.to_string(), "ready": progress.pod_ready()})
                        }
                    };
                    let mut want = after.clone();
                    if let Some(error) = want["error"].as_str() {
                        want["error"] = json!(as_viy(error));
                    }
                    assert_eq!(got, want, "{name}, fetch {i}");
                }
                assert!(
                    http.sent()
                        .iter()
                        .all(|r| r.url.ends_with("/pod111/logs?tail=30"))
                );
            }
            "watchdog" => {
                let told = Told::default();
                report_watchdog(
                    &with(&case["config"]),
                    case["state"].as_str(),
                    &told.reporter(),
                );
                let got: Vec<Value> = told
                    .notices()
                    .into_iter()
                    .map(|(tone, text)| json!([tone_name(tone), text]))
                    .collect();
                let want: Vec<Value> = case["said"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| json!([s[0], as_viy(s[1].as_str().unwrap())]))
                    .collect();
                assert_eq!(got, want, "{name}");
            }
            _ => continue,
        }
        played += 1;
    }
    assert_eq!(played, 20);
}

// ------------------------------------------------------------ the clock

/// When each probe started, in ms of the wait.
type Starts = Arc<Mutex<Vec<u128>>>;

#[tokio::test(start_paused = true)]
async fn probes_start_on_whole_seconds_and_count_when_they_end() {
    for (poll, takes, want) in [
        (1, 300, &[0, 1000, 2000, 3000, 4000][..]),
        (1, 1500, &[0, 2000, 4000]),
        (2, 0, &[0, 2000, 4000]),
    ] {
        let told = Told::default();
        let starts = Starts::default();
        let begun = Instant::now();
        let probe = {
            let starts = Arc::clone(&starts);
            move || {
                starts.lock().unwrap().push(begun.elapsed().as_millis());
                async move {
                    tokio::time::sleep(Duration::from_millis(takes)).await;
                    Ok(Step::<()>::Waiting("not yet".into()))
                }
            }
        };
        let got = wait_with_clock(
            "waiting",
            Duration::from_secs(5),
            Duration::from_secs(poll),
            probe,
            &told.reporter(),
        )
        .await
        .unwrap();
        assert_eq!(got, None);
        assert_eq!(begun.elapsed(), Duration::from_secs(5));
        assert_eq!(
            *starts.lock().unwrap(),
            want,
            "poll {poll} s, probe {takes} ms"
        );
        let clocks = told.clocks();
        let seconds: Vec<u64> = clocks.iter().map(|c| c.elapsed).collect();
        assert_eq!(seconds, [0, 1, 2, 3, 4]);
        assert_eq!(clocks[0].to_string(), "  00:00 waiting (starting)");
        assert_eq!(clocks[4].to_string(), "  00:04 waiting (not yet)");
    }
}

#[tokio::test(start_paused = true)]
async fn an_answer_counts_the_moment_it_comes() {
    let told = Told::default();
    let begun = Instant::now();
    let calls = AtomicUsize::new(0);
    let probe = || {
        let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(if call == 2 {
                Step::Done("up")
            } else {
                Step::Waiting("down".into())
            })
        }
    };
    let got = wait_with_clock(
        "waiting",
        Duration::from_secs(60),
        HEALTH_POLL,
        probe,
        &told.reporter(),
    )
    .await
    .unwrap();
    assert_eq!(got, Some("up"));
    // rpctl would see it on the next whole second, 2 s in (kb f073).
    assert_eq!(begun.elapsed(), Duration::from_millis(1250));
}

#[tokio::test(start_paused = true)]
async fn a_probe_error_ends_the_wait_and_no_time_means_no_probe() {
    let told = Told::default();
    let probe = || async { Err::<Step<()>, _>(WaitError::Startup("broken".into())) };
    let error = wait_with_clock(
        "w",
        Duration::from_secs(9),
        HEALTH_POLL,
        probe,
        &told.reporter(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "broken");
    let calls = AtomicUsize::new(0);
    let probe = || {
        calls.fetch_add(1, Ordering::SeqCst);
        async { Ok(Step::<()>::Waiting(String::new())) }
    };
    let got = wait_with_clock("w", Duration::ZERO, HEALTH_POLL, probe, &told.reporter()).await;
    assert_eq!(got.unwrap(), None);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_terminal_redraws_the_clock_and_a_log_gets_its_changes() {
    let clock = |elapsed, label, state: &str| Clock {
        elapsed,
        label,
        state: state.into(),
    };
    let mut tty = ClockPrinter::new(true);
    assert_eq!(tty.close(), None);
    assert_eq!(
        tty.clock(&clock(75, "waiting for vLLM", "HTTP 503"))
            .as_deref(),
        Some("\r  01:15 waiting for vLLM (HTTP 503)   \x1b[K")
    );
    assert_eq!(tty.close(), Some("\n"));
    assert_eq!(tty.close(), None);

    let mut log = ClockPrinter::new(false);
    let shown: Vec<Option<String>> = [
        clock(0, "waiting for sshd", "starting"),
        clock(1, "waiting for sshd", "starting"),
        clock(2, "waiting for sshd", "no port mapping yet"),
        clock(31, "waiting for sshd", "no port mapping yet"),
        clock(32, "waiting for sshd", "no port mapping yet"),
        clock(40, "waiting for vLLM", "no port mapping yet"),
    ]
    .iter()
    .map(|c| log.clock(c))
    .collect();
    assert_eq!(
        shown,
        [
            Some("  00:00 waiting for sshd (starting)\n".to_string()),
            None,
            Some("  00:02 waiting for sshd (no port mapping yet)\n".to_string()),
            None,
            Some("  00:32 waiting for sshd (no port mapping yet)\n".to_string()),
            Some("  00:40 waiting for vLLM (no port mapping yet)\n".to_string()),
        ]
    );
    assert_eq!(log.close(), None);
}

// ------------------------------------------------------- rpctl test_boot_budget.py

#[tokio::test]
async fn log_fetch_never_delays_the_health_poll() {
    let http = Arc::new(FakeHttp::new([Reply::Hang]));
    let rp = RunPod::new(API_KEY, http.clone());
    let progress = Progress::new(&rp, "pod111", true);
    let started = std::time::Instant::now();
    assert_eq!(progress.poll().unwrap(), "");
    assert!(started.elapsed() < Duration::from_millis(500));
    tokio::time::sleep(Duration::from_millis(50)).await;
    // One fetch at a time, and none again within 20 s.
    assert_eq!(progress.poll().unwrap(), "");
    assert_eq!(http.sent().len(), 1);
}

#[tokio::test]
async fn captured_logs_mask_secrets() {
    let mut cfg = Config::defaults();
    cfg.set("VLLM_API_KEY", "sk-abcdefghijklmnop123456");
    let line = "non-default args: {'api_key': ['sk-abcdefghijklmnop123456']}";
    assert!(!redact_log(line, &cfg).contains("abcdefghijklmnop"));
}

// --------------------------------------------------------- rpctl test_startup.py

#[tokio::test]
async fn pod_ready_flag_follows_latest_boot() {
    let ready = log_reply(&["RPCTL_PHASE model ready; saving startup caches"]);
    let rebooted = log_reply(&[
        "RPCTL_PHASE model ready; saving startup caches",
        "=== boot Sat Jun 20 ===",
    ]);
    for (reply, want) in [(ready, true), (rebooted, false)] {
        let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([reply])));
        let progress = Progress::new(&rp, "pod111", true);
        progress.refresh().await;
        assert_eq!(progress.pod_ready(), want);
    }
}

#[tokio::test(start_paused = true)]
async fn ready_in_pod_but_unreachable_warns() {
    let logs = RouteHttp::new(|_| log_reply(&["RPCTL_PHASE model ready; saving startup caches"]));
    let rp = RunPod::new(API_KEY, Arc::new(logs));
    let progress = Progress::new(&rp, "pod111", true);
    let health = RouteHttp::new(|_| Reply::Fail(HttpError::Connect("refused".into())));
    let told = Told::default();
    let url = "http://127.0.0.1:18001";
    let ready = wait_ready(
        &health,
        url,
        Some(VLLM_KEY),
        Duration::from_secs(100),
        Some(&progress),
        None,
        &told.reporter(),
    )
    .await
    .unwrap();
    assert!(!ready);
    assert_eq!(
        told.alerts(),
        [Alert {
            title: "viy up: pod ready but unreachable".into(),
            body: format!("{url} -> ConnectionError"),
            urgent: true,
        }]
    );
    assert_eq!(
        told.notices(),
        [
            (
                Tone::Error,
                format!(
                    "vLLM reports ready inside the pod, but {url} has failed for 90s \
                     (ConnectionError). The pod is billing meanwhile."
                )
            ),
            (
                Tone::Warn,
                "Check: viy tunnel --status, viy logs. Ctrl-C and re-run viy up to reattach to \
                 the same pod."
                    .into()
            ),
        ]
    );
    let last = told.clocks().pop().unwrap();
    assert_eq!(last.state, "ready in pod, local endpoint: ConnectionError");
    let sent = health.sent();
    assert_eq!(sent.len(), 100);
    assert_eq!(sent[0].url, format!("{url}/health"));
    assert_eq!(
        sent[0].header_value("Authorization"),
        Some(format!("Bearer {VLLM_KEY}").as_str())
    );
    assert_eq!(
        sent[0].header_value("User-Agent"),
        Some("python-requests/2.34.2")
    );
    assert_eq!(sent[0].connect_timeout, Duration::from_secs(5));
}

#[tokio::test]
async fn logs_redact_credentials() {
    let mut cfg = Config::defaults();
    cfg.set("RUNPOD_API_KEY", "opaque-secret");
    assert_eq!(
        redact_log("key=sk-secret123 token=hf_abc123 plain=opaque-secret", &cfg),
        "key=[redacted] token=[redacted] plain=[redacted]"
    );
}

// ------------------------------------------------------- rpctl test_idle.py

#[tokio::test]
async fn the_watchdog_key_is_masked_in_saved_logs() {
    let mut cfg = Config::defaults();
    cfg.set("RUNPOD_WATCHDOG_API_KEY", "rpa_SECRETVALUE1234567890");
    assert!(
        !redact_log("env RPCTL_WATCHDOG_KEY=rpa_SECRETVALUE1234567890", &cfg)
            .contains("SECRETVALUE")
    );
}

// ------------------------------------------------------------ the endpoint

/// A state directory, a config and a pod.json saying how the pod is
/// reached.
struct Home {
    _dir: ScratchDir,
    paths: Paths,
    cfg: Config,
    time: FakeTime,
}

impl Home {
    fn new(tunnel_only: bool) -> Self {
        let dir = ScratchDir::new("wait").unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(paths.pod_state_dir()).unwrap();
        fs::write(
            paths.pod_state_file(),
            json!({"pod_id": "pod111", "volume_id": "vol222", "tunnel_only": tunnel_only})
                .to_string(),
        )
        .unwrap();
        let mut cfg = Config::defaults();
        cfg.set("POD_NAME", "qwen-pod");
        cfg.set("NETWORK_VOLUME_ID", "vol222");
        cfg.set("VLLM_API_KEY", VLLM_KEY);
        Self {
            _dir: dir,
            paths,
            cfg,
            time: FakeTime::at(Utc.with_ymd_and_hms(2026, 7, 16, 9, 30, 0).unwrap()),
        }
    }

    fn wait<'a>(
        &'a self,
        exec: &'a dyn viy_exec::ExecBackend,
        rp: &'a RunPod,
        http: &'a dyn HttpClient,
    ) -> PodWait<'a> {
        PodWait {
            ssh: Ssh {
                paths: &self.paths,
                cfg: &self.cfg,
                exec,
            },
            rp,
            http,
            time: &self.time,
            pod_id: "pod111",
            exe: Path::new("/opt/viy/bin/viy"),
        }
    }
}

fn events(timeline: &Timeline<'_>) -> Vec<String> {
    timeline
        .rows()
        .iter()
        .map(|r| r["event"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_proxied_pod_is_awaited_at_its_proxy_url() {
    let home = Home::new(false);
    let rp = RunPod::new(
        API_KEY,
        Arc::new(RouteHttp::new(|_| log_reply(&["loading"]))),
    );
    let polls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&polls);
    let health = RouteHttp::new(move |_| match counted.fetch_add(1, Ordering::SeqCst) {
        0 | 1 => Reply::answer(503, ""),
        _ => Reply::answer(200, ""),
    });
    let exec = FakeExec::new([]);
    let w = home.wait(&exec, &rp, &health);
    let told = Told::default();
    let mut timeline = Timeline::start(&home.time);
    let begun = Instant::now();
    let url = await_endpoint(&w, Duration::from_secs(60), &mut timeline, &told.reporter())
        .await
        .unwrap();
    let proxy = "https://pod111-8000.proxy.runpod.net";
    assert_eq!(url.as_deref(), Some(proxy));
    assert_eq!(begun.elapsed(), Duration::from_secs(2));
    assert_eq!(
        told.notices(),
        [
            (Tone::Plain, format!("Endpoint: {proxy}")),
            (
                Tone::Warn,
                "Traffic to this URL is decrypted by RunPod's proxy. Set TUNNEL=1 and recreate \
                 the pod to avoid that."
                    .into()
            ),
        ]
    );
    assert_eq!(events(&timeline), ["viy up started", "vLLM healthy"]);
    assert!(
        health
            .sent()
            .iter()
            .all(|r| r.url == format!("{proxy}/health"))
    );
}

/// RunPod's answer for the pod, with its SSH port mapped or not.
fn pod_reply(port: Option<u16>) -> Reply {
    let mut pod =
        json!({"id": "pod111", "publicIp": "127.0.0.1", "machineId": "m1", "costPerHr": 1.89});
    if let Some(port) = port {
        pod["portMappings"] = json!({"22": port});
    }
    Reply::answer(200, pod.to_string())
}

#[tokio::test(start_paused = true)]
async fn sshd_that_never_comes_up_ends_the_wait() {
    let home = Home::new(true);
    let api = Arc::new(RouteHttp::new(|r| {
        if r.url.contains("/logs") {
            log_reply(&["RPCTL_PHASE starting sshd"])
        } else {
            pod_reply(None)
        }
    }));
    let rp = RunPod::new(API_KEY, api.clone());
    let exec = FakeExec::new([]);
    let health = FakeHttp::new([]);
    let w = home.wait(&exec, &rp, &health);
    let told = Told::default();
    let mut timeline = Timeline::start(&home.time);
    let url = await_endpoint(&w, Duration::from_secs(10), &mut timeline, &told.reporter())
        .await
        .unwrap();
    assert_eq!(url, None);
    assert_eq!(
        told.notices(),
        [
            (
                Tone::Dim,
                "Tunnel mode: no http port is exposed, so the endpoint is reached over SSH.".into()
            ),
            (
                Tone::Error,
                "sshd never came up, and a tunnel-only pod exposes nothing else.".into()
            ),
            (
                Tone::Warn,
                "Run viy logs for platform and SSH errors, or use the web console at \
                 https://console.runpod.io/pods/pod111."
                    .into()
            ),
        ]
    );
    assert_eq!(events(&timeline), ["viy up started", "sshd never came up"]);
    let asked = api
        .sent()
        .iter()
        .filter(|r| r.url.ends_with("/pods/pod111"))
        .count();
    assert_eq!(asked, 5, "every 2 s for 10 s");
    let last = told.clocks().pop().unwrap();
    assert_eq!(last.to_string(), "  00:09 waiting for sshd (starting sshd)");
}

#[tokio::test(start_paused = true)]
async fn a_failed_sshd_setup_ends_the_wait_at_once() {
    let home = Home::new(true);
    let api = RouteHttp::new(|r| {
        if r.url.contains("/logs") {
            log_reply(&["RPCTL_ERROR sshd setup or daemon failed (exit 23)"])
        } else {
            pod_reply(None)
        }
    });
    let rp = RunPod::new(API_KEY, Arc::new(api));
    let exec = FakeExec::new([]);
    let health = FakeHttp::new([]);
    let w = home.wait(&exec, &rp, &health);
    let told = Told::default();
    let mut timeline = Timeline::start(&home.time);
    let begun = Instant::now();
    let url = await_endpoint(
        &w,
        Duration::from_secs(600),
        &mut timeline,
        &told.reporter(),
    )
    .await
    .unwrap();
    assert_eq!(url, None);
    assert_eq!(begun.elapsed(), Duration::from_secs(2));
    let message = "SSH setup failed; run viy logs for the installation/daemon error.";
    assert_eq!(
        told.notices().last().unwrap(),
        &(Tone::Error, message.to_string())
    );
    let rows = timeline.rows();
    assert_eq!(rows[1]["event"], "startup error");
    assert_eq!(rows[1]["message"], message);
}

/// vLLM's stand-in: answers every request with the status it holds.
fn vllm_stand_in() -> (u16, Arc<AtomicU16>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let status = Arc::new(AtomicU16::new(503));
    let answer = Arc::clone(&status);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf);
            let code = answer.load(Ordering::SeqCst);
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    (port, status)
}

/// A tunnel-only pod behind a local sshd, its vLLM a stand-in; RunPod
/// maps its SSH port from the `mapped_from`th question on.
struct Pod {
    sshd: LocalSshd,
    home: Home,
    rp: RunPod,
    vllm: Arc<AtomicU16>,
}

impl Pod {
    fn new(mapped_from: usize) -> Self {
        let sshd = LocalSshd::start().unwrap();
        let mut home = Home::new(true);
        let (vllm_port, vllm) = vllm_stand_in();
        home.cfg.set("VLLM_PORT", &vllm_port.to_string());
        home.cfg
            .set("TUNNEL_LOCAL_PORT", &free_port().unwrap().to_string());
        let fingerprint = sshd.host_fingerprint().unwrap();
        let ssh_port = sshd.port;
        let asked = AtomicUsize::new(0);
        let api = RouteHttp::new(move |r| {
            if r.url.contains("/logs") {
                log_reply(&[&format!(
                    "sshd host key: 256 {fingerprint} root@pod (ED25519)"
                )])
            } else if asked.fetch_add(1, Ordering::SeqCst) + 1 >= mapped_from {
                pod_reply(Some(ssh_port))
            } else {
                pod_reply(None)
            }
        });
        Self {
            sshd,
            home,
            rp: RunPod::new(API_KEY, Arc::new(api)),
            vllm,
        }
    }

    async fn stop_tunnel(&self) {
        let ssh = Ssh {
            paths: &self.home.paths,
            cfg: &self.home.cfg,
            exec: &*self.sshd.exec(),
        };
        tunnel::stop(&ssh, true, &mut |_| {}).await.unwrap();
    }
}

#[tokio::test]
async fn a_tunnel_only_pod_is_awaited_through_the_tunnel() {
    let pod = Pod::new(1);
    pod.vllm.store(200, Ordering::SeqCst);
    let exec = pod.sshd.exec();
    let health = viy_http::ReqwestHttp::new().unwrap();
    let w = pod.home.wait(&*exec, &pod.rp, &health);
    let told = Told::default();
    let mut timeline = Timeline::start(&pod.home.time);
    let url = await_endpoint(&w, Duration::from_secs(60), &mut timeline, &told.reporter())
        .await
        .unwrap();
    pod.stop_tunnel().await;
    let local = tracked::local_url(&pod.home.cfg);
    assert_eq!(url.as_deref(), Some(local.as_str()), "{:?}", told.notices());
    assert_eq!(
        events(&timeline),
        [
            "viy up started",
            "ssh reachable",
            "tunnel up",
            "vLLM healthy"
        ]
    );
    let rows = timeline.rows();
    assert_eq!(rows[1]["host"], "127.0.0.1");
    assert_eq!(rows[1]["port"], pod.sshd.port);
    let notices = told.notices();
    assert_eq!(notices[1], (Tone::Dim, "SSH reachable after 0s.".into()));
    assert_eq!(
        notices.last().unwrap(),
        &(Tone::Plain, format!("Endpoint: {local}"))
    );
}

/// rpctl's test_deadline_shared_between_ssh_and_model: the wait for sshd
/// and the wait for vLLM share one timeout.
#[tokio::test]
async fn deadline_shared_between_ssh_and_model() {
    let pod = Pod::new(2);
    let exec = pod.sshd.exec();
    let health = viy_http::ReqwestHttp::new().unwrap();
    let w = pod.home.wait(&*exec, &pod.rp, &health);
    let told = Told::default();
    let mut timeline = Timeline::start(&pod.home.time);
    let begun = std::time::Instant::now();
    let url = await_endpoint(&w, Duration::from_secs(6), &mut timeline, &told.reporter())
        .await
        .unwrap();
    let took = begun.elapsed();
    pod.stop_tunnel().await;
    assert_eq!(url, None);
    assert_eq!(
        events(&timeline),
        [
            "viy up started",
            "ssh reachable",
            "tunnel up",
            "timed out waiting for vLLM"
        ]
    );
    // sshd answers 2 s in; vLLM then gets what is left of the 6 s.
    assert!(
        took >= Duration::from_secs(6) && took < Duration::from_millis(7500),
        "{took:?}"
    );
    let vllm: Vec<Clock> = told
        .clocks()
        .into_iter()
        .filter(|c| c.label == "waiting for vLLM")
        .collect();
    assert!(
        !vllm.is_empty() && vllm.iter().all(|c| c.elapsed < 4),
        "{vllm:?}"
    );
    assert!(
        vllm.iter().skip(1).all(|c| c.state == "HTTP 503"),
        "{vllm:?}"
    );
}

// ------------------------------------------------------------ the report

/// The platform log of a boot that took 1022 s, with the watchdog armed.
fn boot_log() -> String {
    let fixture = fixtures::load("pod/bootreport").unwrap();
    let mut log = entries(&fixture.case("measure_rpctl_test").unwrap()["entries"]);
    log.push(LogEntry {
        ts: Some("2026-06-25T23:40:03Z".into()),
        source: Some("container".into()),
        line: "RPCTL_PHASE watchdog armed".into(),
    });
    log.iter()
        .map(|e| {
            format!(
                "data: {}\n",
                json!({"ts": e.ts, "source": e.source, "line": e.line})
            )
        })
        .collect()
}

#[tokio::test]
async fn a_boot_is_reported_recorded_and_its_logs_collected() {
    let home = Home::new(true);
    let log = boot_log();
    let rp = RunPod::new(
        API_KEY,
        Arc::new(RouteHttp::new(move |_| Reply::answer(200, log.clone()))),
    );
    let exec = FakeExec::new([Run::StandIn(Command::new("true"))]);
    let health = FakeHttp::new([]);
    let w = home.wait(&exec, &rp, &health);
    let told = Told::default();
    let timeline = Timeline::start(&home.time);
    let started = ts("2026-06-25T23:23:03Z");
    let ready = ts("2026-06-25T23:40:05Z");
    let total = report_boot(
        &w,
        started,
        ready,
        Outcome::Ready,
        &timeline,
        &told.reporter(),
    )
    .await;
    assert_eq!(total, 1022.0);
    let notices = told.notices();
    assert_eq!(notices[0].0, Tone::Error, "over budget");
    assert!(
        notices[0]
            .1
            .starts_with("Boot breakdown (budget 180s):\n  GPU allocation + image pull")
    );
    assert!(
        notices[0]
            .1
            .contains("OVER BUDGET by 842s. Largest stage: GPU allocation + image pull")
    );
    assert_eq!(
        notices[1],
        (
            Tone::Dim,
            "Idle watchdog armed: the pod terminates itself after 60 min without tokens, or \
             after 8 h."
                .into()
        )
    );
    let records = Records::new(&home.paths);
    let row = records.history(1).pop().unwrap();
    let boot_id = row["boot_id"].as_str().unwrap();
    assert_eq!(boot_id, "20260716T093000Z-pod111");
    assert_eq!(
        notices[2],
        (
            Tone::Dim,
            format!(
                "Boot record: viy boots show   ({})",
                records.record_path(boot_id).display()
            )
        )
    );
    assert_eq!(row["stages"]["pull"], json!(766.5));
    let record = records.read(boot_id).unwrap();
    assert_eq!(record["facts"]["watchdog"], "armed");
    assert_eq!(
        exec.sent()[0].argv(),
        ["/opt/viy/bin/viy", "capture-boot", boot_id, "pod111"]
    );
    assert!(records.file(boot_id, "capture.err").exists());
    assert_eq!(
        told.alerts(),
        [Alert {
            title: "viy up: over the boot budget".into(),
            body: "1022s against 180s; see the breakdown".into(),
            urgent: true,
        }]
    );
}

#[tokio::test]
async fn a_report_that_fails_only_says_so() {
    let home = Home::new(true);
    // The boots directory cannot be made: a file is in its way.
    fs::write(home.paths.boots_dir(), "").unwrap();
    let rp = RunPod::new(API_KEY, Arc::new(RouteHttp::new(|_| log_reply(&[]))));
    let exec = FakeExec::new([]);
    let health = FakeHttp::new([]);
    let w = home.wait(&exec, &rp, &health);
    let told = Told::default();
    let timeline = Timeline::start(&home.time);
    let started = ts("2026-06-25T23:23:03Z");
    let ready = ts("2026-06-25T23:24:03Z");
    let total = report_boot(
        &w,
        started,
        ready,
        Outcome::Failed,
        &timeline,
        &told.reporter(),
    )
    .await;
    assert_eq!(total, 60.0);
    let (tone, text) = told.notices().pop().unwrap();
    assert_eq!(tone, Tone::Dim);
    assert!(text.starts_with("(boot report unavailable: "), "{text}");
    assert_eq!(told.alerts(), []);
}

fn ts(text: &str) -> DateTime<Utc> {
    bootreport::parse_ts(text).unwrap()
}

#[tokio::test]
async fn an_interrupted_boot_still_leaves_a_record() {
    let home = Home::new(false);
    let rp = RunPod::new(
        API_KEY,
        Arc::new(RouteHttp::new(|_| log_reply(&["loading"]))),
    );
    let health = RouteHttp::new(|_| Reply::answer(503, ""));
    for booting in [false, true] {
        let exec = FakeExec::new([Run::StandIn(Command::new("true"))]);
        let w = home.wait(&exec, &rp, &health);
        let told = Told::default();
        let mut timeline = Timeline::start(&home.time);
        let started = home.time.now();
        let error = await_recorded(
            &w,
            Duration::from_secs(60),
            started,
            booting,
            &mut timeline,
            tokio::time::sleep(Duration::from_millis(300)),
            &told.reporter(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, WaitError::Interrupted));
        let history = Records::new(&home.paths).history(10);
        if booting {
            assert_eq!(events(&timeline).last().unwrap(), "interrupted (Ctrl-C)");
            assert_eq!(history.len(), 1);
            assert_eq!(history[0]["outcome"], "interrupted");
            let breakdown = &told.notices()[2];
            assert!(
                breakdown
                    .1
                    .starts_with("Boot breakdown (INTERRUPTED, so far)"),
                "{breakdown:?}"
            );
        } else {
            assert_eq!(events(&timeline), ["viy up started"]);
            assert_eq!(history.len(), 0);
            assert_eq!(exec.left(), 1);
        }
    }
}

#[test]
fn the_wait_can_run_in_the_background() {
    fn send<T: Send>(_: &T) {}
    let home = Home::new(true);
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([])));
    let exec = FakeExec::new([]);
    let health = FakeHttp::new([]);
    let w = home.wait(&exec, &rp, &health);
    let told = Told::default();
    let report = told.reporter();
    let mut timeline = Timeline::start(&home.time);
    let started = home.time.now();
    let wait = await_recorded(
        &w,
        Duration::from_secs(1),
        started,
        true,
        &mut timeline,
        std::future::pending(),
        &report,
    );
    send(&wait);
}

// ------------------------------------------------------------ capture

#[tokio::test]
async fn capture_writes_the_logs_masked_and_the_host() {
    let home = Home::new(true);
    let mut cfg = home.cfg.clone();
    cfg.set("HF_TOKEN", "hf_SYNTHETICTOKEN0000");
    let records = Records::new(&home.paths);
    // An earlier boot on machine m0, and this one's record as up left it.
    fs::create_dir_all(home.paths.pod_state_dir()).unwrap();
    fs::write(
        home.paths.boots_history_file(),
        "{\"boot_id\": \"20260715T080000Z-pod000\"}\n{\"boot_id\": \"20260716T093000Z-pod111\"}\n",
    )
    .unwrap();
    records
        .write(
            "20260715T080000Z-pod000",
            &json!({"host": {"machine_id": "m0"}}),
        )
        .unwrap();
    records
        .write(
            "20260716T093000Z-pod111",
            &json!({"outcome": "ready", "facts": {"image": "downloaded"}, "files": {}}),
        )
        .unwrap();
    fs::write(records.file("20260716T093000Z-pod111", "capture.err"), "").unwrap();
    let api = RouteHttp::new(|r| {
        if r.url.contains("/logs") {
            log_reply(&["token hf_SYNTHETICTOKEN0000", "RPCTL_PHASE watchdog armed"])
        } else {
            pod_reply(Some(40122))
        }
    });
    let rp = RunPod::new(API_KEY, Arc::new(api));
    let exec = FakeExec::new([Run::stdout("vllm says hi\nkey sk-SYNTHETICENDPOINT\n")]);
    let ssh = Ssh {
        paths: &home.paths,
        cfg: &cfg,
        exec: &exec,
    };
    capture_boot(&ssh, &rp, "20260716T093000Z-pod111", "pod111")
        .await
        .unwrap();
    let id = "20260716T093000Z-pod111";
    let data = records.read(id).unwrap();
    assert_eq!(
        data["host"],
        json!({"machine_id": "m1", "public_ip": "127.0.0.1", "cost_per_hr": 1.89,
               "same_machine_as_previous_boot": false})
    );
    assert_eq!(
        data["facts"],
        json!({"image": "downloaded", "watchdog": "armed"})
    );
    let platform = records.file(id, "platform.log");
    let vllm = records.file(id, "vllm.log");
    assert_eq!(
        data["files"],
        json!({"platform_log": platform.display().to_string(),
               "vllm_log": vllm.display().to_string()})
    );
    assert_eq!(
        fs::read_to_string(&platform).unwrap(),
        " [container] token [redacted]\n [container] RPCTL_PHASE watchdog armed\n"
    );
    assert_eq!(
        fs::read_to_string(&vllm).unwrap(),
        "vllm says hi\nkey [redacted]\n"
    );
    let argv = exec.sent()[0].argv();
    assert_eq!(argv[0], "ssh");
    assert!(
        argv.contains(&"StrictHostKeyChecking=yes".to_string()),
        "{argv:?}"
    );
    assert_eq!(
        argv[argv.len() - 8..],
        [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-p",
            "40122",
            "root@127.0.0.1",
            VLLM_LOG_SCRIPT
        ]
    );
    assert_eq!(exec.sent()[0].timeout, Some(Duration::from_secs(60)));
    assert!(!records.file(id, "capture.err").exists());
}

#[tokio::test]
async fn capture_without_logs_or_ssh_says_why() {
    let home = Home::new(true);
    let records = Records::new(&home.paths);
    let api = RouteHttp::new(|r| {
        if r.url.contains("/logs") {
            Reply::answer(500, "down")
        } else {
            pod_reply(Some(40122))
        }
    });
    let rp = RunPod::new(API_KEY, Arc::new(api));
    let exec = FakeExec::new([Run::exit(255, "", "Connection refused")]);
    let ssh = Ssh {
        paths: &home.paths,
        cfg: &home.cfg,
        exec: &exec,
    };
    capture_boot(&ssh, &rp, "20260716T093000Z-pod111", "pod111")
        .await
        .unwrap();
    let data = records.read("20260716T093000Z-pod111").unwrap();
    assert_eq!(
        data["files"],
        json!({"platform_log": "unavailable: Platform logs -> HTTP 500",
               "vllm_log": "unavailable: ssh exit 255"})
    );
    assert_eq!(data["host"]["machine_id"], "m1");
    assert!(data["host"].get("same_machine_as_previous_boot").is_none());
}
