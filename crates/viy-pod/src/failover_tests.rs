//! Pod creation with region failover, the old backup's retirement, backup
//! expiry and `viy backup`, replayed from the `pod/failover` fixture:
//! rpctl's own `_create_pod`, `regions.*` against a scripted RunPod. Each
//! case's replies are played back in order; viy must send the same
//! requests, ask the same questions, say the same notices and leave the
//! same files. Named tests are rpctl's tests/test_regions.py, each on its
//! case.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use chrono::DateTime;
use pretty_assertions::assert_eq;
use serde_json::json;
use viy_config::Paths;
use viy_exec::Command;
use viy_http::HttpClient;
use viy_http::Method;
use viy_testkit::exec::FakeExec;
use viy_testkit::exec::Reply as Run;
use viy_testkit::fixtures;
use viy_testkit::http::FakeHttp;
use viy_testkit::http::Reply;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::time::FakeTime;

use super::*;
use crate::notice::Asking;
use crate::notice::Report;
use crate::regions::Snapshot;

/// tools/fixtures.py FO_NOW and API_KEY.
const NOW: i64 = 1_784_194_200;
const API_KEY: &str = "rpa_SYNTHETICKEY0000";

/// rp's words for its commands and itself, as viy says them.
fn as_viy(text: &str) -> String {
    ["up", "reap", "backup", "is up"]
        .iter()
        .fold(text.to_string(), |t, w| {
            t.replace(&format!("rp {w}"), &format!("viy {w}"))
        })
}

fn config(case: &Value) -> Config {
    let mut cfg = Config::defaults();
    for (k, v) in case["cfg"].as_object().into_iter().flatten() {
        cfg.set(k, v.as_str().unwrap_or_default());
    }
    cfg
}

fn replies(case: &Value) -> Vec<Reply> {
    case["replies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            if r["hang"] == true {
                Reply::Hang
            } else {
                let status = r["status"].as_u64().map_or(200, |s| s as u16);
                Reply::answer(status, r["body"].as_str().unwrap_or_default())
            }
        })
        .collect()
}

/// A request as the fixture shows it: a pod's body cut to the fields that
/// differ between attempts (the rest is the `pod/boot` fixture's).
fn shown_request(r: &viy_http::Request) -> Value {
    let mut body: Value = r
        .body
        .as_deref()
        .and_then(|b| serde_json::from_slice(b).ok())
        .unwrap_or(Value::Null);
    if r.method == Method::POST && r.url.ends_with("/v1/pods") {
        let keep = [
            "name",
            "dataCenterIds",
            "networkVolumeId",
            "allowedCudaVersions",
        ];
        if let Some(fields) = body.as_object_mut() {
            fields.retain(|k, _| keep.contains(&k.as_str()));
        }
    }
    json!({"method": r.method.to_string(), "url": r.url, "body": body})
}

fn tone(tone: Tone) -> &'static str {
    match tone {
        Tone::Plain => "plain",
        Tone::Dim => "dim",
        Tone::Ok => "ok",
        Tone::Warn => "warn",
        Tone::Error => "error",
    }
}

fn rows(file: &Path) -> Value {
    std::fs::read_to_string(file).map_or(Value::Null, |text| {
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap_or(Value::Null))
            .collect()
    })
}

/// What one case did in viy.
struct Replayed {
    _dir: ScratchDir,
    paths: Paths,
    http: Arc<FakeHttp>,
    said: Vec<Value>,
    asked: Vec<Value>,
    outcome: Value,
}

impl Replayed {
    fn backups(&self) -> Value {
        std::fs::read_to_string(self.paths.backups_file()).map_or(Value::Null, |t| {
            serde_json::from_str(&t).unwrap_or(Value::Null)
        })
    }

    /// The requests made, as `METHOD url-path`.
    fn calls(&self) -> Vec<String> {
        self.http
            .sent()
            .iter()
            .map(|r| {
                let path = r.url.split(".runpod.io").nth(1).unwrap_or(&r.url);
                format!("{} {path}", r.method)
            })
            .collect()
    }

    /// The pod requests, as (region, volume).
    fn creates(&self) -> Vec<(String, String)> {
        self.http
            .sent()
            .iter()
            .filter(|r| r.method == Method::POST && r.url.ends_with("/v1/pods"))
            .map(|r| {
                let body = shown_request(r)["body"].clone();
                (
                    body["dataCenterIds"][0].as_str().unwrap_or_default().into(),
                    body["networkVolumeId"].as_str().unwrap_or_default().into(),
                )
            })
            .collect()
    }

    /// The outcome's value, as a list.
    fn ok(&self) -> Vec<Value> {
        self.outcome["ok"].as_array().cloned().unwrap_or_default()
    }

    fn volume_calls(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.contains("/networkvolumes") && !c.starts_with("GET"))
            .collect()
    }
}

/// Runs `case` in viy and checks it against what rpctl did.
async fn replay(case: &Value) -> Replayed {
    let name = case["name"].as_str().unwrap_or_default().to_string();
    let dir = ScratchDir::new("failover").unwrap();
    let paths = Paths::under(dir.path());
    std::fs::create_dir_all(paths.pod_state_dir()).unwrap();
    if !case["backups"].is_null() {
        std::fs::write(paths.backups_file(), case["backups"].to_string()).unwrap();
    }
    for (key, file) in [
        ("stock_rows", paths.stock_file()),
        ("failure_rows", paths.up_failures_file()),
    ] {
        let lines: String = case[key]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| format!("{r}\n"))
            .collect();
        if !case[key].is_null() {
            std::fs::write(file, lines).unwrap();
        }
    }
    let cfg = config(case);
    let fake = Arc::new(FakeHttp::new(replies(case)));
    let http: Arc<dyn HttpClient> = fake.clone();
    let time = FakeTime::at(DateTime::from_timestamp(NOW, 0).unwrap());
    let exec = FakeExec::new([Run::StandIn(Command::new("true"))]);
    let env = Env {
        paths: &paths,
        exec: &exec,
        http: &http,
        time: &time,
        exe: Path::new("/opt/viy/bin/viy"),
    };
    let rp = RunPod::new(API_KEY, http.clone());
    let home = dir.path().display().to_string();
    let said = Arc::new(Mutex::new(Vec::new()));
    let note = {
        let said = Arc::clone(&said);
        let home = home.clone();
        move |tone: Tone, text: &str| {
            said.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(json!([self::tone(tone), text.replace(&home, "$HOME")]));
        }
    };
    let report = {
        let note = note.clone();
        move |r: Report| {
            if let Report::Notice(n) = r {
                note(n.tone, &n.text);
            }
        }
    };
    let asked = Arc::new(Mutex::new(Vec::new()));
    let answers: Option<Vec<bool>> = case["answers"]
        .as_array()
        .map(|a| a.iter().map(|v| v == true).collect());
    let ask = {
        let asked = Arc::clone(&asked);
        let answers = Mutex::new(answers.clone().unwrap_or_default().into_iter());
        let attended = answers_given(case);
        move |q: &Question| -> Option<Asking> {
            if !attended {
                return None;
            }
            asked
                .lock()
                .unwrap()
                .push(json!([q.prompt, q.default == Answer::Yes]));
            let yes = answers.lock().unwrap().next().unwrap();
            Some(Box::pin(async move {
                if yes { Answer::Yes } else { Answer::No }
            }))
        }
    };
    // Ctrl-C once the fixture's request count is reached.
    let (presses, interrupt) = Interrupt::channel();
    let at: Vec<usize> = case["presses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .map(|n| n as usize)
        .collect();
    let pressing = {
        let fake = Arc::clone(&fake);
        tokio::spawn(async move {
            for n in at {
                while fake.sent().len() < n {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                presses.send_modify(|c| *c += 1);
            }
            std::future::pending::<()>().await;
        })
    };
    let abort_at = AtomicU32::new(1);

    let outcome = match case["call"].as_str().unwrap_or("create_pod") {
        "create_pod" => {
            let user = User {
                report: &report,
                ask: &ask,
                interrupt: &interrupt,
                abort_at: &abort_at,
            };
            let key = cfg.get("VLLM_API_KEY").unwrap_or_default();
            let creation = Creation::new(
                &env,
                &cfg,
                &rp,
                key,
                Some("ssh-ed25519 AAAAsynthetic"),
                user,
            );
            let volume_is_new = case["volume_is_new"] == true;
            match creation.run("prim", volume_is_new).await {
                Ok(created) => {
                    assert_eq!(
                        json!(created.region),
                        case["region_after"],
                        "{name}: region"
                    );
                    let waited = created.waited.as_secs_f64();
                    assert_eq!(json!(waited), case["answer_wait"], "{name}: waited");
                    json!({"ok": [created.pod, created.volume_id, created.volume_is_new]})
                }
                Err(CommandError::Failed) => json!({"exit": 1}),
                Err(CommandError::Interrupted) => json!({"interrupted": true}),
                Err(other) => panic!("{name}: {other}"),
            }
        }
        "retire_replaced" => {
            let mut say = |t: String| note(Tone::Dim, &t);
            let volume = case["volume"].as_str().unwrap_or_default();
            let gone = regions::retire_replaced(&rp, &paths, &cfg, volume, &mut say)
                .await
                .unwrap();
            json!({"ok": gone})
        }
        "expire" => {
            let mut say = |t: String| note(Tone::Plain, &t);
            let now = case["now"].as_f64().unwrap_or_default();
            let gone = regions::expire(&rp, &paths, &cfg, now, &mut say)
                .await
                .unwrap();
            json!({"ok": gone.iter().map(regions::id).collect::<Vec<_>>()})
        }
        "backup" => {
            let samples = case["samples"].as_u64().unwrap_or_default() as u32;
            let result = regions::backup(&env, &cfg, samples, &report).await;
            let printed: String = said
                .lock()
                .unwrap()
                .drain(..)
                .map(|s| format!("{}\n", s[1].as_str().unwrap_or_default()))
                .collect();
            match result {
                Ok(()) => json!({"stdout": printed}),
                Err(CommandError::RunPod(e)) => json!({"error": e.to_string(), "stdout": printed}),
                Err(other) => panic!("{name}: {other}"),
            }
        }
        "rank" => {
            let samples: Vec<Snapshot> = case["samples"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|s| {
                    s.as_object()
                        .into_iter()
                        .flatten()
                        .map(|(r, free)| (r.clone(), free == true))
                        .collect()
                })
                .collect();
            let exclude: HashSet<&str> = case["exclude"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let past: HashMap<String, f64> = serde_json::from_value(case["past"].clone()).unwrap();
            json!({"ok": regions::rank(&samples, &exclude, &past)})
        }
        "capacity" => {
            let texts = case["texts"].as_array().cloned().unwrap_or_default();
            let capacity: Vec<bool> = texts
                .iter()
                .map(|t| {
                    regions::is_capacity_error(&RunPodError::new(
                        t.as_str().unwrap_or_default(),
                        None,
                    ))
                })
                .collect();
            json!({"ok": capacity})
        }
        other => panic!("{name}: unknown call {other}"),
    };
    pressing.abort();

    let replayed = Replayed {
        _dir: dir,
        paths,
        http: fake,
        said: said.lock().unwrap().clone(),
        asked: asked.lock().unwrap().clone(),
        outcome,
    };
    let sent: Vec<Value> = replayed.http.sent().iter().map(shown_request).collect();
    assert_eq!(json!(sent), case["sent"], "{name}: requests");
    let viy = |v: &Value| -> Value { serde_json::from_str(&as_viy(&v.to_string())).unwrap() };
    assert_eq!(
        json!(replayed.asked),
        viy(&case["asked"]),
        "{name}: questions"
    );
    assert_eq!(json!(replayed.said), viy(&case["said"]), "{name}: notices");
    for key in ["ok", "exit", "interrupted", "error"] {
        if !case[key].is_null() || !replayed.outcome[key].is_null() {
            assert_eq!(replayed.outcome[key], case[key], "{name}: {key}");
        }
    }
    if let Some(stdout) = case["stdout"].as_str() {
        let stdout = as_viy(stdout);
        assert_eq!(
            replayed.outcome["stdout"].as_str(),
            Some(stdout.as_str()),
            "{name}"
        );
    }
    assert_eq!(
        replayed.backups(),
        case["backups_after"],
        "{name}: backups.json"
    );
    assert_eq!(
        rows(&replayed.paths.stock_file()),
        case["stock_after"],
        "{name}: stock.jsonl"
    );
    assert_eq!(
        rows(&replayed.paths.up_failures_file()),
        case["failures_after"],
        "{name}: up_failures.jsonl"
    );
    replayed
}

/// Whether the case has someone to answer (`answers` not null).
fn answers_given(case: &Value) -> bool {
    !case["answers"].is_null()
}

async fn replay_named(name: &str) -> Replayed {
    let fixture = fixtures::load("pod/failover").unwrap();
    replay(fixture.case(name).unwrap()).await
}

#[tokio::test(start_paused = true)]
async fn every_case_does_what_rpctls_does() {
    let fixture = fixtures::load("pod/failover").unwrap();
    assert!(fixture.cases.len() >= 70);
    for case in &fixture.cases {
        replay(case).await;
    }
}

// ------------------------------------------- rpctl tests/test_regions.py

#[tokio::test(start_paused = true)]
async fn only_capacity_refusals_count() {
    let r = replay_named("capacity_errors").await;
    assert_eq!(r.ok()[..8], [true; 8].map(Value::from));
}

#[tokio::test(start_paused = true)]
async fn asks_before_trying_the_backup_and_moves_nothing() {
    let r = replay_named("backup_asked_and_taken").await;
    assert_eq!(r.ok()[1..], [json!("bk"), json!(false)]);
    assert_eq!(r.asked.len(), 1);
    assert!(
        r.asked[0][0]
            .as_str()
            .unwrap()
            .contains("Try the backup volume in US-MO-1")
    );
    assert!(r.volume_calls().is_empty());
    assert!(r.backups().get("bk").is_some());
}

#[tokio::test(start_paused = true)]
async fn no_to_the_backup_creates_nothing() {
    let r = replay_named("no_to_the_backup_creates_nothing").await;
    assert_eq!(r.creates(), [("EUR-IS-1".into(), "prim".into())]);
    assert_eq!(r.outcome["exit"], 1);
}

#[tokio::test(start_paused = true)]
async fn full_backup_is_reported_then_the_switch_is_asked_and_done() {
    let r = replay_named("full_backup_then_the_switch_is_asked_and_done").await;
    assert_eq!(r.ok()[1..], [json!("new1"), json!(true)]);
    assert!(r.said.contains(&json!([
        "warn",
        "The backup region US-MO-1 has no GPU free either."
    ])));
    assert!(
        r.asked[1][0]
            .as_str()
            .unwrap()
            .contains("Switch the backup to CA-MTL-3")
    );
    // The old backup stays until viy is up on the new one.
    assert!(
        !r.volume_calls()
            .contains(&"DELETE /v1/networkvolumes/bk".to_string())
    );
    assert_eq!(r.backups()["new1"]["replaces"], "bk");
    let r = replay_named("retire_deletes_the_old_backup").await;
    assert_eq!(r.outcome["ok"]["id"], "bk");
    assert_eq!(r.volume_calls(), ["DELETE /v1/networkvolumes/bk"]);
    assert!(r.backups().get("bk").is_none());
    assert!(r.backups()["new1"].get("replaces").is_none());
}

#[tokio::test(start_paused = true)]
async fn retiring_with_nothing_pending_costs_no_api_call() {
    for name in ["retire_nothing_pending", "retire_unknown_volume"] {
        let r = replay_named(name).await;
        assert_eq!(r.outcome["ok"], Value::Null);
        assert!(r.calls().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn retiring_never_deletes_a_volume_not_named_as_the_backup() {
    let r = replay_named("retire_never_deletes_a_volume_not_named_as_the_backup").await;
    assert!(r.volume_calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn no_to_the_switch_keeps_the_backup() {
    let r = replay_named("no_to_the_switch_keeps_the_backup").await;
    assert!(r.volume_calls().is_empty());
    assert_eq!(r.outcome["exit"], 1);
}

#[tokio::test(start_paused = true)]
async fn no_backup_yet_asks_to_make_one_where_an_a100_is_free() {
    let r = replay_named("no_backup_yet_asks_to_make_one").await;
    assert_eq!(r.outcome["ok"][2], true);
    assert_eq!(r.asked.len(), 1);
    assert!(
        r.asked[0][0]
            .as_str()
            .unwrap()
            .contains("Make a backup volume in CA-MTL-3")
    );
    assert_eq!(r.volume_calls(), ["POST /v1/networkvolumes"]);
}

#[tokio::test(start_paused = true)]
async fn refused_candidate_loses_its_empty_volume_and_the_next_is_tried() {
    let r = replay_named("refused_candidate_loses_its_volume_the_next_is_tried").await;
    assert_eq!(r.outcome["ok"][0]["id"], "pod-EU-RO-1");
    assert_eq!(r.asked.len(), 2);
    assert!(
        r.asked[1][0]
            .as_str()
            .unwrap()
            .contains("the first of CA-MTL-3, EU-RO-1")
    );
    assert!(
        r.volume_calls()
            .contains(&"DELETE /v1/networkvolumes/new1".to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn refusals_are_retried_on_the_same_volumes_until_one_takes_the_pod() {
    let r = replay_named("refusals_retried_until_one_takes_the_pod").await;
    assert_eq!(r.ok()[1..], [json!("new2"), json!(true)]);
    // Retry rounds try the warm volumes first, then the new ones.
    let creates = r.creates();
    assert_eq!(
        creates[3..8],
        [
            ("EU-RO-1".into(), "new2".into()),
            ("EUR-IS-1".into(), "prim".into()),
            ("US-MO-1".into(), "bk".into()),
            ("CA-MTL-3".into(), "new1".into()),
            ("EU-RO-1".into(), "new2".into()),
        ]
    );
    assert_eq!(r.backups()["new2"]["replaces"], "bk");
}

#[tokio::test(start_paused = true)]
async fn primary_freeing_up_during_the_retries_wins_and_keeps_the_backup() {
    let r = replay_named("primary_freeing_up_during_the_retries_wins").await;
    assert_eq!(r.ok()[1..], [json!("prim"), json!(false)]);
    assert!(!r.backups().to_string().contains("replaces"));
}

#[tokio::test(start_paused = true)]
async fn retries_running_out_delete_the_new_volumes_and_keep_the_backup() {
    let r = replay_named("retries_running_out_delete_the_new_volumes").await;
    assert_eq!(r.outcome["exit"], 1);
    let deleted: Vec<String> = r
        .volume_calls()
        .into_iter()
        .filter(|c| c.starts_with("DELETE"))
        .collect();
    assert_eq!(
        deleted,
        [
            "DELETE /v1/networkvolumes/new1",
            "DELETE /v1/networkvolumes/new2"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn ctrl_c_during_the_retries_deletes_the_new_volumes() {
    let r = replay_named("ctrl_c_during_the_retries_deletes_the_new_volumes").await;
    assert_eq!(r.outcome["interrupted"], true);
    assert!(r.volume_calls().ends_with(&[
        "DELETE /v1/networkvolumes/new1".to_string(),
        "DELETE /v1/networkvolumes/new2".to_string()
    ]));
}

#[tokio::test(start_paused = true)]
async fn zero_retry_minutes_tries_each_region_once() {
    let r = replay_named("zero_retry_minutes_tries_each_region_once").await;
    let regions: Vec<String> = r.creates().into_iter().map(|(r, _)| r).collect();
    assert_eq!(regions, ["EUR-IS-1", "US-MO-1", "CA-MTL-3"]);
}

#[tokio::test(start_paused = true)]
async fn nowhere_free_keeps_the_backup_without_asking() {
    let r = replay_named("nowhere_free_keeps_the_backup_without_asking").await;
    assert_eq!(r.asked.len(), 1);
    assert!(r.volume_calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_non_capacity_refusal_moves_nothing() {
    for name in [
        "backup_refuses_for_another_reason",
        "non_capacity_refusal_names_the_usual_causes",
    ] {
        let r = replay_named(name).await;
        assert!(r.volume_calls().is_empty());
        assert!(
            r.said.last().unwrap()[1]
                .as_str()
                .unwrap()
                .contains("Common causes")
        );
    }
}

#[tokio::test(start_paused = true)]
async fn every_refusal_is_logged_with_secrets_masked() {
    let r = replay_named("secrets_masked_in_the_refusal_log").await;
    let log = std::fs::read_to_string(r.paths.up_failures_file()).unwrap();
    assert!(!log.contains("sk-SYNTHETIC") && !log.contains("rpa_SYNTHETIC"));
    let r = replay_named("retries_running_out_delete_the_new_volumes").await;
    let logged = rows(&r.paths.up_failures_file());
    assert_eq!(logged.as_array().unwrap().len(), r.creates().len());
}

#[tokio::test(start_paused = true)]
async fn a100_now_beats_a_better_past() {
    assert_eq!(
        replay_named("rank_a100_now_beats_a_better_past")
            .await
            .outcome["ok"],
        json!(["A"])
    );
}

#[tokio::test(start_paused = true)]
async fn more_often_free_now_first_then_the_past_breaks_ties() {
    let r = replay_named("rank_more_often_free_first_then_the_past").await;
    assert_eq!(r.outcome["ok"], json!(["C", "B", "A"]));
}

#[tokio::test(start_paused = true)]
async fn not_due_costs_no_api_call() {
    assert!(
        replay_named("expire_not_due_costs_no_call")
            .await
            .calls()
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn idle_backup_is_deleted() {
    let r = replay_named("expire_idle_backup_is_deleted").await;
    assert_eq!(r.outcome["ok"], json!(["bk"]));
    assert_eq!(r.volume_calls(), ["DELETE /v1/networkvolumes/bk"]);
}

#[tokio::test(start_paused = true)]
async fn attached_backup_is_kept_and_its_clock_restarts() {
    let r = replay_named("expire_attached_backup_is_kept_its_clock_restarts").await;
    assert_eq!(r.outcome["ok"], json!([]));
    assert_eq!(r.backups()["bk"]["last_used"], json!(NOW as f64));
}

#[tokio::test(start_paused = true)]
async fn never_deletes_a_volume_not_named_as_a_backup() {
    let r = replay_named("expire_never_deletes_a_volume_not_named_as_a_backup").await;
    assert!(r.volume_calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn zero_days_keeps_backups() {
    let r = replay_named("expire_zero_days_keeps_backups").await;
    assert!(r.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn unrecorded_backup_starts_its_countdown_when_found() {
    let r = replay_named("unrecorded_backup_starts_its_countdown").await;
    assert_eq!(r.backups()["bk"]["last_used"], json!(NOW as f64));
}

#[tokio::test(start_paused = true)]
async fn newer_drivers_only_are_asked_for_without_the_exact_version() {
    let r = replay_named("cuda_newer_drivers_only_retried_without_the_version").await;
    assert_eq!(r.creates().len(), 2);
    assert_eq!(r.outcome["ok"][0]["id"], "pod-EUR-IS-1");
}

#[tokio::test(start_paused = true)]
async fn an_older_driver_free_keeps_the_filter() {
    let r = replay_named("cuda_older_driver_free_keeps_the_filter").await;
    assert_eq!(r.creates().len(), 1);
    assert_eq!(r.outcome["exit"], 1);
}

#[tokio::test(start_paused = true)]
async fn success_path_asks_no_stock() {
    let r = replay_named("cuda_success_path_asks_no_stock").await;
    assert_eq!(r.calls(), ["POST /v1/pods"]);
}

#[tokio::test]
async fn without_a_cuda_filter_no_stock_is_asked() {
    let mut cfg = Config::defaults();
    cfg.set("CUDA_VERSIONS", "");
    let http = Arc::new(FakeHttp::new([]));
    let rp = RunPod::new(API_KEY, http.clone());
    assert!(!regions::any_cuda_is_safe(&rp, &cfg, "EUR-IS-1").await);
    assert!(http.sent().is_empty());
}

#[tokio::test(start_paused = true)]
async fn stock_counts_only_hosts_at_or_above_the_floor() {
    let r = replay_named("stock_counts_hosts_at_or_above_the_cuda_floor").await;
    let stock = rows(&r.paths.stock_file());
    assert_eq!(
        stock[0]["a100_free"],
        json!({"US-MO-1": false, "CA-MTL-3": true})
    );
}

#[tokio::test(start_paused = true)]
async fn a_second_ctrl_c_during_cleanup_still_deletes_the_rest() {
    let r = replay_named("second_ctrl_c_during_cleanup_still_deletes_the_rest").await;
    assert!(
        r.volume_calls()
            .contains(&"DELETE /v1/networkvolumes/new2".to_string())
    );
    // new1's delete was interrupted: it stays recorded as unused for reap.
    assert_eq!(r.backups()["new1"]["unused"], true);
    assert!(r.backups().get("new2").is_none());
}

#[tokio::test(start_paused = true)]
async fn unused_volume_is_no_backup_and_reap_deletes_it() {
    let r = replay_named("unused_volume_is_no_backup").await;
    assert!(
        r.said
            .iter()
            .any(|s| s[1].as_str().unwrap().contains("no backup volume yet"))
    );
    let r = replay_named("expire_unused_within_the_grace_is_kept").await;
    assert!(r.calls().is_empty());
    let r = replay_named("expire_unused_after_the_grace_is_deleted").await;
    assert_eq!(r.outcome["ok"], json!(["new1"]));
}

#[tokio::test(start_paused = true)]
async fn the_winning_volume_is_no_longer_unused() {
    let r = replay_named("no_backup_yet_asks_to_make_one").await;
    assert!(r.backups()["new1"].get("unused").is_none());
}

#[tokio::test(start_paused = true)]
async fn without_a_terminal_the_backup_is_tried_but_nothing_is_moved() {
    let r = replay_named("without_a_terminal_the_backup_is_tried_nothing_moved").await;
    assert!(r.creates().contains(&("US-MO-1".into(), "bk".into())));
    assert!(r.volume_calls().is_empty());
    assert_eq!(r.outcome["exit"], 1);
}
