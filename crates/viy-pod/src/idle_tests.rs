//! rpctl's test_idle.py (the reap decision and the activity stamp), and
//! `viy reap` and `viy reap --install` against a scripted RunPod and
//! systemctl, on paused time.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use chrono::TimeZone;
use pretty_assertions::assert_eq;
use viy_http::HttpClient;
use viy_http::Method;
use viy_testkit::exec::FakeExec;
use viy_testkit::exec::Reply as Run;
use viy_testkit::http::Reply;
use viy_testkit::http::RouteHttp;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::sshd::free_port;
use viy_testkit::time::FakeTime;

use super::*;
use crate::notice::Alert;

const HOUR: f64 = 3600.0;
const LIMITS: Limits = Limits {
    ttl: HOUR,
    grace: 15.0 * 60.0,
    max: 8.0 * HOUR,
};

fn seen(first_seen: f64, warned_for: Option<f64>) -> Seen {
    Seen {
        pod_id: "p1".into(),
        first_seen,
        warned_for,
    }
}

fn verdict(now: f64, activity: Option<f64>, seen: &Seen, desired: &str) -> Option<Verdict> {
    decide(now, Some(desired), activity, seen, LIMITS).0
}

// ------------------------------------------------------- ReapDecisionTests

#[test]
fn waits_for_ttl_plus_grace() {
    let fresh = seen(0.0, None);
    assert_eq!(verdict(1000.0, Some(900.0), &fresh, "RUNNING"), None);
    assert_eq!(
        verdict(900.0 + 50.0 * 60.0, Some(900.0), &fresh, "RUNNING"),
        Some(Verdict::Warn)
    );
    assert_eq!(
        verdict(
            900.0 + HOUR + 14.0 * 60.0,
            Some(900.0),
            &seen(0.0, Some(900.0)),
            "RUNNING"
        ),
        None
    );
    assert_eq!(
        verdict(900.0 + HOUR + 15.0 * 60.0, Some(900.0), &fresh, "RUNNING"),
        Some(Verdict::Down)
    );
}

#[test]
fn new_activity_rearms_the_warning() {
    let warned = seen(0.0, Some(900.0));
    assert_eq!(
        verdict(5000.0 + 51.0 * 60.0, Some(5000.0), &warned, "RUNNING"),
        Some(Verdict::Warn)
    );
}

#[test]
fn no_activity_file_counts_from_first_sighting() {
    assert_eq!(
        verdict(HOUR - 1000.0, None, &seen(600.0, None), "RUNNING"),
        None
    );
    assert_eq!(
        verdict(
            600.0 + HOUR + 15.0 * 60.0,
            None,
            &seen(600.0, None),
            "RUNNING"
        ),
        Some(Verdict::Down)
    );
}

#[test]
fn hard_cap_and_stopped_pods() {
    let now = 8.0 * HOUR + 15.0 * 60.0;
    assert_eq!(
        verdict(now, Some(now - 60.0), &seen(0.0, None), "RUNNING"),
        Some(Verdict::Down)
    );
    assert_eq!(
        verdict(10.0, Some(5.0), &seen(0.0, None), "EXITED"),
        Some(Verdict::Down)
    );
}

#[test]
fn the_reasons_say_how_long() {
    let fresh = seen(0.0, None);
    let reason = |now, desired| decide(now, Some(desired), Some(900.0), &fresh, LIMITS).1;
    assert_eq!(reason(1000.0, "RUNNING"), "idle 2 min");
    assert_eq!(
        reason(900.0 + 50.0 * 60.0, "RUNNING"),
        "idle 50 min; the pod shuts itself down in ~10 min"
    );
    assert_eq!(
        reason(900.0 + HOUR + 15.0 * 60.0, "RUNNING"),
        "nothing streamed from this machine for 75 min"
    );
    assert_eq!(
        reason(1000.0, "EXITED"),
        "the pod was stopped by its watchdog; terminating it"
    );
    let off = Limits { ttl: 0.0, ..LIMITS };
    assert_eq!(
        decide(1000.0, Some("RUNNING"), None, &fresh, off).1,
        "IDLE_TTL_MIN=0"
    );
    let capped = decide(
        8.0 * HOUR + 15.0 * 60.0,
        Some("RUNNING"),
        Some(8.0 * HOUR),
        &fresh,
        LIMITS,
    );
    assert_eq!(
        capped,
        (Some(Verdict::Down), "pod running for 8.2 h".into())
    );
}

#[test]
fn limits_read_as_rp_reads_them() {
    let mut cfg = Config::defaults();
    assert_eq!(
        limits(&cfg),
        Limits {
            ttl: 3600.0,
            grace: 900.0,
            max: 28800.0
        }
    );
    cfg.set("IDLE_TTL_MIN", "not a number");
    cfg.set("REAP_GRACE_MIN", "");
    cfg.set("MAX_POD_HOURS", "-1");
    assert_eq!(
        limits(&cfg),
        Limits {
            ttl: 0.0,
            grace: 0.0,
            max: -3600.0
        }
    );
}

// --------------------------------------------------------------- TouchTests

#[tokio::test(start_paused = true)]
async fn touch_is_throttled() {
    let dir = ScratchDir::new("idle-touch").unwrap();
    let paths = Paths::under(dir.path());
    let time = FakeTime::at(Utc.with_ymd_and_hms(2026, 7, 16, 9, 30, 0).unwrap());
    touch(&paths, &time, false);
    let first = last_activity(&paths).unwrap();
    std::fs::write(paths.activity_file(), "1\n").unwrap();
    time.advance(Duration::from_secs(29));
    touch(&paths, &time, false); // within 30 s: no write
    assert_eq!(activity_seconds(&paths), Some(1.0));
    touch(&paths, &time, true);
    assert!(last_activity(&paths).unwrap() >= first);
}

// --------------------------------------------------------------------- seen

#[test]
fn reap_state_is_kept_per_pod() {
    let dir = ScratchDir::new("idle-seen").unwrap();
    let paths = Paths::under(dir.path());
    assert_eq!(Seen::load(&paths, "p1", 5.0), seen(5.0, None));
    seen(1.5, Some(2.0)).save(&paths).unwrap();
    assert_eq!(
        std::fs::read_to_string(paths.reap_state_file()).unwrap(),
        r#"{"pod_id":"p1","first_seen":1.5,"warned_for":2.0}"#
    );
    assert_eq!(Seen::load(&paths, "p1", 5.0), seen(1.5, Some(2.0)));
    // rp's own spelling reads the same; another pod's starts afresh.
    std::fs::write(
        paths.reap_state_file(),
        r#"{"pod_id": "p1", "first_seen": 1784194200.0}"#,
    )
    .unwrap();
    assert_eq!(Seen::load(&paths, "p1", 5.0), seen(1784194200.0, None));
    assert_eq!(Seen::load(&paths, "p2", 5.0).first_seen, 5.0);
}

// --------------------------------------------------------------------- reap

const API_KEY: &str = "rpa_SYNTHETICKEY0000";
/// 2026-07-16T09:30:00Z, the fake clock's start.
const NOW: f64 = 1_784_194_200.0;

#[derive(Default, Clone)]
struct Told(Arc<Mutex<Vec<Report>>>);

impl Told {
    fn reporter(&self) -> impl Fn(Report) + Send + Sync + 'static {
        let told = Arc::clone(&self.0);
        move |r| told.lock().unwrap_or_else(PoisonError::into_inner).push(r)
    }

    fn texts(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                Report::Notice(n) => Some(n.text.clone()),
                _ => None,
            })
            .collect()
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
    /// Our pod `pod111` tracked in pod.json, and RunPod as `world` says.
    fn new(world: impl Fn(&viy_http::Request) -> Reply + Send + Sync + 'static) -> Self {
        let dir = ScratchDir::new("idle-reap").unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.pod_state_dir()).unwrap();
        std::fs::write(paths.pod_state_file(), r#"{"pod_id": "pod111"}"#).unwrap();
        let mut cfg = Config::defaults();
        for (k, v) in [
            ("RUNPOD_API_KEY", API_KEY),
            ("POD_NAME", "qwen-pod"),
            ("VLLM_PORT", "18999"),
        ] {
            cfg.set(k, v);
        }
        cfg.set("TUNNEL_LOCAL_PORT", &free_port().unwrap().to_string());
        let api = Arc::new(RouteHttp::new(world));
        Self {
            _dir: dir,
            paths,
            cfg,
            time: FakeTime::at(Utc.with_ymd_and_hms(2026, 7, 16, 9, 30, 0).unwrap()),
            exec: FakeExec::default(),
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

    fn active(&self, seconds_ago: f64) {
        std::fs::write(
            self.paths.activity_file(),
            format!("{:.0}\n", NOW - seconds_ago),
        )
        .unwrap();
    }

    fn sent(&self) -> Vec<String> {
        self.api
            .sent()
            .iter()
            .map(|r| format!("{} {}", r.method, r.url))
            .collect()
    }

    async fn reap(&self, quiet: bool) -> (Result<(), CommandError>, Told) {
        let told = Told::default();
        let result = reap(&self.env(), &self.cfg, quiet, &told.reporter()).await;
        (result, told)
    }
}

/// RunPod with `pod111` in `status` until it is deleted, and nothing else.
fn runpod(status: &'static str) -> impl Fn(&viy_http::Request) -> Reply + Send + Sync + 'static {
    let deleted = Mutex::new(false);
    move |r| {
        let gone = *deleted.lock().unwrap();
        let pod = json!({"id": "pod111", "name": "qwen-pod", "desiredStatus": status});
        if r.method == Method::DELETE {
            *deleted.lock().unwrap() = true;
            Reply::answer(200, "{}")
        } else if r.url.ends_with("/v1/pods/pod111") {
            if gone {
                Reply::answer(404, "{}")
            } else {
                Reply::answer(200, pod.to_string())
            }
        } else if r.url.ends_with("/v1/pods") {
            let pods = if gone { json!([]) } else { json!([pod]) };
            Reply::answer(200, pods.to_string())
        } else {
            panic!("unexpected {} {}", r.method, r.url)
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_fresh_pod_is_seen_and_left_alone() {
    let home = Home::new(runpod("RUNNING"));
    home.active(60.0);
    let (result, told) = home.reap(false).await;
    result.unwrap();
    // A first sighting counts from now.
    assert_eq!(told.texts(), ["idle 0 min"]);
    assert!(told.alerts().is_empty());
    assert_eq!(
        Seen::load(&home.paths, "pod111", 0.0),
        Seen {
            pod_id: "pod111".into(),
            first_seen: NOW,
            warned_for: None,
        }
    );
    assert_eq!(home.sent(), ["GET https://rest.runpod.io/v1/pods/pod111"]);
}

#[tokio::test(start_paused = true)]
async fn an_idle_pod_is_warned_about_once() {
    let home = Home::new(runpod("RUNNING"));
    Seen {
        pod_id: "pod111".into(),
        first_seen: NOW - 2.0 * HOUR,
        warned_for: None,
    }
    .save(&home.paths)
    .unwrap();
    home.active(52.0 * 60.0);
    let (result, told) = home.reap(false).await;
    result.unwrap();
    let reason = "idle 52 min; the pod shuts itself down in ~8 min";
    assert_eq!(told.texts(), [reason]);
    assert_eq!(
        told.alerts(),
        [Alert {
            title: "viy: the pod is idle".into(),
            body: reason.into(),
            urgent: false
        }]
    );
    assert_eq!(
        Seen::load(&home.paths, "pod111", 0.0).warned_for,
        Some(NOW - 52.0 * 60.0)
    );
    // The next run, nothing new streamed: no second warning.
    let (result, told) = home.reap(false).await;
    result.unwrap();
    assert_eq!(told.texts(), ["idle 52 min"]);
    assert!(told.alerts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_forgotten_pod_is_taken_down() {
    let home = Home::new(runpod("RUNNING"));
    home.active(HOUR + 20.0 * 60.0);
    std::fs::write(
        home.paths.reap_state_file(),
        json!({"pod_id": "pod111", "first_seen": NOW - 3.0 * HOUR}).to_string(),
    )
    .unwrap();
    let (result, told) = home.reap(true).await;
    result.unwrap();
    let reason = "nothing streamed from this machine for 80 min";
    assert_eq!(
        told.alerts(),
        [Alert {
            title: "viy reap: shutting the pod down".into(),
            body: reason.into(),
            urgent: true
        }]
    );
    // Quiet: only what `viy down` says.
    assert_eq!(
        told.texts(),
        [
            "  terminated pod111 (was RUNNING)",
            "Terminated pod111. No pod is running or starting; GPU and pod-disk billing has \
             ended.",
            "Model and startup caches stay on the network volume for the next viy up.",
        ]
    );
    assert!(
        home.sent()
            .contains(&"DELETE https://rest.runpod.io/v1/pods/pod111".to_string())
    );
    assert!(!home.paths.reap_state_file().exists());
    assert!(!home.paths.pod_state_file().exists());
}

#[tokio::test(start_paused = true)]
async fn a_pod_its_watchdog_stopped_is_terminated() {
    let home = Home::new(runpod("EXITED"));
    home.active(60.0);
    let (result, told) = home.reap(false).await;
    result.unwrap();
    assert_eq!(
        told.texts()[0],
        "the pod was stopped by its watchdog; terminating it"
    );
    assert!(
        home.sent()
            .contains(&"DELETE https://rest.runpod.io/v1/pods/pod111".to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn no_pod_forgets_the_reap_state() {
    let home = Home::new(|r: &viy_http::Request| {
        if r.url.ends_with("/v1/pods/pod111") {
            Reply::answer(404, "{}")
        } else if r.url.ends_with("/v1/pods") {
            Reply::answer(200, "[]")
        } else {
            panic!("unexpected {}", r.url)
        }
    });
    seen(1.0, None).save(&home.paths).unwrap();
    let (result, told) = home.reap(false).await;
    result.unwrap();
    assert_eq!(told.texts(), ["no pod running"]);
    assert!(!home.paths.reap_state_file().exists());
}

#[tokio::test(start_paused = true)]
async fn runpod_out_of_reach_ends_the_check() {
    let home = Home::new(|_: &viy_http::Request| Reply::answer(401, "bad key"));
    let (result, told) = home.reap(false).await;
    result.unwrap();
    assert_eq!(told.texts().len(), 1);
    assert!(
        told.texts()[0].starts_with("could not reach RunPod: "),
        "{:?}",
        told.texts()
    );
    assert!(!home.paths.reap_state_file().exists());
}

#[tokio::test(start_paused = true)]
async fn an_idle_backup_volume_is_deleted_and_told() {
    let home = Home::new(move |r: &viy_http::Request| {
        if r.url.ends_with("/networkvolumes/bk") {
            if r.method == Method::DELETE {
                Reply::answer(200, "{}")
            } else {
                Reply::answer(
                    200,
                    json!({"id": "bk", "name": "w-backup", "dataCenterId": "US-MO-1"}).to_string(),
                )
            }
        } else {
            runpod("RUNNING")(r)
        }
    });
    home.active(60.0);
    std::fs::write(
        home.paths.backups_file(),
        json!({"bk": {"name": "w-backup", "region": "US-MO-1", "last_used": NOW - 40.0 * 86400.0}})
            .to_string(),
    )
    .unwrap();
    let (result, told) = home.reap(false).await;
    result.unwrap();
    let said = "deleted backup volume bk (w-backup, US-MO-1): unused for 40 days";
    assert_eq!(told.texts(), [said, "idle 0 min"]);
    assert_eq!(told.alerts()[0].title, "viy reap");
    assert_eq!(told.alerts()[0].body, said);
}

#[tokio::test(start_paused = true)]
async fn a_failed_backup_check_does_not_stop_the_reap() {
    let home = Home::new(|r: &viy_http::Request| {
        if r.url.ends_with("/v1/pods") {
            Reply::answer(500, "listing broke")
        } else {
            runpod("RUNNING")(r)
        }
    });
    home.active(60.0);
    std::fs::write(
        home.paths.backups_file(),
        json!({"bk": {"name": "w-backup", "unused": true, "last_used": NOW - 86400.0}}).to_string(),
    )
    .unwrap();
    let (result, told) = home.reap(false).await;
    result.unwrap();
    let texts = told.texts();
    assert!(
        texts[0].starts_with("backup volume check failed: "),
        "{texts:?}"
    );
    assert_eq!(texts[1], "idle 0 min");
}

#[tokio::test(start_paused = true)]
async fn reap_waits_while_the_pod_lock_is_held() {
    let home = Home::new(|r: &viy_http::Request| panic!("unexpected {}", r.url));
    let _held = PodLock::exclusive(&home.paths, "up").unwrap();
    let (result, told) = home.reap(false).await;
    result.unwrap();
    assert_eq!(
        told.texts(),
        ["rp or viy up/stop/down is running; not checking now"]
    );
}

// ---------------------------------------------------------- ReapBackendTests

/// Whether reap got as far as the lock check, with BACKEND as given.
async fn reaps(backend: &str) -> Result<bool, CommandError> {
    let mut home = Home::new(|r: &viy_http::Request| panic!("unexpected {}", r.url));
    home.cfg.set("BACKEND", backend);
    let _held = PodLock::exclusive(&home.paths, "up").unwrap();
    let (result, told) = home.reap(false).await;
    result.map(|()| !told.texts().is_empty())
}

#[tokio::test(start_paused = true)]
async fn runpod_in_any_spelling() {
    for backend in ["runpod", "RunPod", " runpod ", ""] {
        assert!(reaps(backend).await.unwrap(), "{backend:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn other_backends_are_left_alone() {
    for backend in ["openai", "local"] {
        assert!(!reaps(backend).await.unwrap(), "{backend}");
    }
    assert_eq!(
        reaps("azure").await.unwrap_err().to_string(),
        "unknown BACKEND 'azure'; supported: local, openai, runpod"
    );
}

#[tokio::test(start_paused = true)]
async fn reap_without_a_key_says_so() {
    let mut home = Home::new(|r: &viy_http::Request| panic!("unexpected {}", r.url));
    home.cfg.set("RUNPOD_API_KEY", "");
    let (result, _) = home.reap(false).await;
    let Err(CommandError::Exit(message)) = result else {
        panic!("{result:?}")
    };
    assert!(message.starts_with("RUNPOD_API_KEY is not set."));
}

// ------------------------------------------------------------------ install

#[tokio::test]
async fn install_writes_the_units_and_enables_the_timer() {
    let dir = ScratchDir::new("idle-install").unwrap();
    let paths = Paths::under(dir.path());
    let exec = FakeExec::new([Run::stdout(""), Run::stdout("")]);
    let told = Told::default();
    install(
        &paths,
        &exec,
        Some(Path::new("/home/u/.cargo/bin/viy")),
        &told.reporter(),
    )
    .await
    .unwrap();
    let units = paths.systemd_user_dir();
    assert_eq!(
        std::fs::read_to_string(units.join("viy-reap.service")).unwrap(),
        "[Unit]\nDescription=viy reap: terminate an idle RunPod pod (backup for the pod's own \
         watchdog)\n\n[Service]\nType=oneshot\nExecStart=/home/u/.cargo/bin/viy reap --quiet\n"
    );
    assert_eq!(
        std::fs::read_to_string(units.join("viy-reap.timer")).unwrap(),
        TIMER
    );
    let argv: Vec<Vec<String>> = exec.sent().iter().map(viy_exec::Command::argv).collect();
    assert_eq!(
        argv,
        [
            vec!["systemctl", "--user", "daemon-reload"],
            vec!["systemctl", "--user", "enable", "--now", "viy-reap.timer"],
        ]
    );
    assert_eq!(
        told.texts(),
        [
            format!(
                "installed {}/viy-reap.timer: viy reap runs every 5 minutes",
                units.display()
            ),
            "check it with:  systemctl --user list-timers viy-reap.timer".to_string(),
        ]
    );
}

#[tokio::test]
async fn install_is_refused_while_rps_timer_reaps() {
    let dir = ScratchDir::new("idle-install").unwrap();
    let paths = Paths::under(dir.path());
    let xdg = Paths::from_vars(dir.path().to_path_buf(), |name| {
        (name == "XDG_CONFIG_HOME").then(|| dir.path().join("xdg").into_os_string())
    });
    let exec = FakeExec::default();
    // rp's timer where rp puts it, or where this shell's XDG_CONFIG_HOME says.
    for units in [xdg.systemd_user_dir(), xdg.config_home.join("systemd/user")] {
        std::fs::create_dir_all(&units).unwrap();
        std::fs::write(units.join("rpctl-reap.timer"), TIMER).unwrap();
        let error = install(&xdg, &exec, Some(Path::new("/x/viy")), &|_| {})
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "rpctl-reap.timer is installed and runs rp reap, which reaps viy's pod too; \
             viy-reap.timer replaces it at cutover (ROADMAP phase 8)"
        );
        std::fs::remove_file(units.join("rpctl-reap.timer")).unwrap();
    }
    std::fs::create_dir_all(paths.systemd_user_dir()).unwrap();
    std::fs::write(paths.systemd_user_dir().join("rpctl-reap.timer"), TIMER).unwrap();
    let error = install(&paths, &exec, Some(Path::new("/x/viy")), &|_| {})
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "rpctl-reap.timer is installed and runs rp reap, which reaps viy's pod too; \
         viy-reap.timer replaces it at cutover (ROADMAP phase 8)"
    );
    assert!(!paths.systemd_user_dir().join("viy-reap.timer").exists());
}

#[tokio::test]
async fn install_needs_viy_on_path_and_a_working_systemctl() {
    let dir = ScratchDir::new("idle-install").unwrap();
    let paths = Paths::under(dir.path());
    let error = install(&paths, &FakeExec::default(), None, &|_| {})
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "viy is not on PATH; install it first");
    let exec = FakeExec::new([Run::exit(1, "", "Failed to connect to bus")]);
    let error = install(&paths, &exec, Some(Path::new("/x/viy")), &|_| {})
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "systemctl --user daemon-reload exited 1");
}

#[tokio::test]
async fn rps_timer_is_disabled_and_its_units_removed_from_both_folders() {
    let dir = ScratchDir::new("idle-install").unwrap();
    let paths = Paths::from_vars(dir.path().to_path_buf(), |name| {
        (name == "XDG_CONFIG_HOME").then(|| dir.path().join("xdg").into_os_string())
    });
    let dirs = [
        paths.systemd_user_dir(),
        paths.config_home.join("systemd/user"),
    ];
    for units in &dirs {
        std::fs::create_dir_all(units).unwrap();
        std::fs::write(units.join("rpctl-reap.timer"), TIMER).unwrap();
        std::fs::write(units.join("rpctl-reap.service"), "[Service]\n").unwrap();
        std::fs::write(units.join("other.timer"), TIMER).unwrap();
    }
    assert!(rpctl_timer_installed(&paths));
    let exec = FakeExec::new([Run::stdout(""), Run::stdout("")]);
    let told = Told::default();
    remove_rpctl_timer(&paths, &exec, &told.reporter())
        .await
        .unwrap();
    let argv: Vec<Vec<String>> = exec.sent().iter().map(viy_exec::Command::argv).collect();
    assert_eq!(
        argv,
        [
            vec![
                "systemctl",
                "--user",
                "disable",
                "--now",
                "rpctl-reap.timer"
            ],
            vec!["systemctl", "--user", "daemon-reload"],
        ]
    );
    for units in &dirs {
        assert!(!units.join("rpctl-reap.timer").exists());
        assert!(!units.join("rpctl-reap.service").exists());
        assert!(units.join("other.timer").exists());
    }
    assert!(!rpctl_timer_installed(&paths));
    assert_eq!(
        told.texts(),
        ["stopped and removed rpctl-reap.timer: rp reap no longer runs"]
    );

    // Gone already: nothing to run.
    let exec = FakeExec::default();
    remove_rpctl_timer(&paths, &exec, &|_| {}).await.unwrap();
    assert!(exec.sent().is_empty());
}

#[tokio::test]
async fn rps_timer_stays_when_systemctl_fails() {
    let dir = ScratchDir::new("idle-install").unwrap();
    let paths = Paths::under(dir.path());
    let units = paths.systemd_user_dir();
    std::fs::create_dir_all(&units).unwrap();
    std::fs::write(units.join("rpctl-reap.timer"), TIMER).unwrap();
    let exec = FakeExec::new([Run::exit(1, "", "Failed to connect to bus")]);
    let error = remove_rpctl_timer(&paths, &exec, &|_| {})
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "systemctl --user disable --now rpctl-reap.timer exited 1"
    );
    assert!(units.join("rpctl-reap.timer").exists());
}

// --------------------------------------------------------------- the oracle

/// rpctl's `idle.decide` and `idle.limits` against viy's on generated
/// cases (`cargo test -p viy-pod --features oracle`): the same verdict and
/// reason for every one, so reap is never less strict than rp's.
#[cfg(feature = "oracle")]
mod oracle {
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    use super::*;

    fn call(oracle: &mut Oracle, func: &str, args: Value) -> Value {
        match oracle.call(func, args, json!({})).unwrap().outcome {
            Outcome::Returned(v) => v,
            other => panic!("{func}: {other:?}"),
        }
    }

    /// A time near the interesting edges: the TTL, the warning, the cap.
    fn near(g: &mut Gen, edges: &[f64]) -> f64 {
        let edge = edges[g.below(edges.len())];
        let offset = [0.0, 1.0, -1.0, 0.5, -0.5, 30.0, -30.0, 599.0, 601.0][g.below(9)];
        edge + offset
    }

    #[test]
    fn decide_matches_rpctl() {
        let mut oracle = Oracle::spawn().unwrap();
        let mut g = Gen::from_env();
        let ttls = [0.0, 60.0, 600.0, 3600.0, 5400.0, -60.0, f64::NAN];
        for case in 0..3000 {
            let ttl = ttls[g.below(ttls.len())];
            let grace = [0.0, 900.0, 60.0, -30.0][g.below(4)];
            let max = [0.0, 28800.0, 3600.0, 600.0][g.below(4)];
            let first_seen = [0.0, 1000.0, 1_784_194_200.0][g.below(3)];
            let activity = match g.below(4) {
                0 => None,
                1 => Some(0.0),
                _ => Some(first_seen + near(&mut g, &[0.0, 600.0, -600.0, 3000.0])),
            };
            let last = activity.unwrap_or(0.0).max(first_seen);
            let now = last
                + near(
                    &mut g,
                    &[
                        0.0,
                        ttl,
                        ttl - 600.0,
                        ttl + grace,
                        max + grace,
                        max + grace - (last - first_seen),
                    ],
                );
            let warned_for = match g.below(3) {
                0 => None,
                1 => Some(last),
                _ => Some(last - 60.0),
            };
            let desired = ["RUNNING", "EXITED", "TERMINATED"][g.below(3)];
            if [ttl, grace, max, now].iter().any(|x| !x.is_finite()) {
                continue;
            }
            let ours = decide(
                now,
                Some(desired),
                activity,
                &Seen {
                    pod_id: "p1".into(),
                    first_seen,
                    warned_for,
                },
                Limits { ttl, grace, max },
            );
            let mut seen = json!({"pod_id": "p1", "first_seen": first_seen});
            if let Some(w) = warned_for {
                seen["warned_for"] = json!(w);
            }
            let theirs = call(
                &mut oracle,
                "idle.decide",
                json!([now, "p1", desired, activity, seen, ttl, grace, max]),
            );
            let verdict = match ours.0 {
                Some(Verdict::Down) => json!("down"),
                Some(Verdict::Warn) => json!("warn"),
                None => Value::Null,
            };
            assert_eq!(
                json!({"$tuple": [verdict, ours.1]}),
                theirs,
                "case {case}, seed {:#x}: now {now} activity {activity:?} first_seen \
                 {first_seen} warned_for {warned_for:?} {desired} ttl {ttl} grace {grace} max \
                 {max}",
                viy_testkit::fuzz::seed()
            );
        }
    }

    #[test]
    fn limits_match_rpctl() {
        let mut oracle = Oracle::spawn().unwrap();
        let values = [
            "60", "", "0", "1.5", " 15 ", "-2", "x", "1e2", "inf", "nan", "0.25",
        ];
        for ttl in values {
            for grace in ["15", "", "y"] {
                for max in ["8", "0", "-1"] {
                    let mut cfg = Config::defaults();
                    cfg.set("IDLE_TTL_MIN", ttl);
                    cfg.set("REAP_GRACE_MIN", grace);
                    cfg.set("MAX_POD_HOURS", max);
                    let ours = limits(&cfg);
                    let theirs = call(
                        &mut oracle,
                        "idle.limits",
                        json!([{"IDLE_TTL_MIN": ttl, "REAP_GRACE_MIN": grace, "MAX_POD_HOURS": max}]),
                    );
                    let shown = |x: f64| {
                        if x.is_finite() {
                            json!(x)
                        } else if x.is_nan() {
                            json!({"$repr": "nan"})
                        } else {
                            json!({"$repr": "inf"})
                        }
                    };
                    assert_eq!(
                        json!({"$tuple": [shown(ours.ttl), shown(ours.grace), shown(ours.max)]}),
                        theirs,
                        "{ttl:?} {grace:?} {max:?}"
                    );
                }
            }
        }
    }
}
