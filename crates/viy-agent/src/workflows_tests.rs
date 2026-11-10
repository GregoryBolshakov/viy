//! rpctl test_workflows.py ExploreTests, and the explore halves of
//! BackgroundCompactionTests and CompactionTests: the helpers' requests
//! answered by a routing HTTP client, run_shell off (rpctl patches
//! `_sandbox_ok` false).

use std::sync::Mutex;

use pretty_assertions::assert_eq;
use viy_testkit::http::Piece;
use viy_testkit::http::Reply;
use viy_testkit::http::RouteHttp;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::chat::tests::Direct;
use crate::chat::tests::shell_chat;

/// rpctl's `_resp`: a non-streamed reply with content or tool calls.
fn resp(content: &str, calls: &[(&str, Value)]) -> Reply {
    let mut m = json!({"content": content, "reasoning": "r"});
    if !calls.is_empty() {
        m["tool_calls"] = calls
            .iter()
            .enumerate()
            .map(|(i, (n, a))| {
                json!({"id": format!("t{i}"), "type": "function",
                                      "function": {"name": n, "arguments": a.to_string()}})
            })
            .collect();
    }
    let finish = if calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let body = json!({"choices": [{"message": m, "finish_reason": finish}],
                      "usage": {"completion_tokens": 5}});
    Reply::answer(200, body.to_string().into_bytes())
}

fn body_of(req: &viy_http::Request) -> Value {
    serde_json::from_slice(req.body.as_deref().unwrap_or_default()).unwrap_or_default()
}

/// A chat with the build tools on in a workspace holding mod_a.py and
/// mod_b.py, its trace in the scratch folder, no sandbox, turn 1.
fn explore_chat(over: &[(&str, &str)]) -> (Direct, ScratchDir) {
    let (mut d, s) = shell_chat(json!([]), over);
    std::fs::write(s.path().join("ws").join("mod_a.py"), "A = 1\n").unwrap();
    std::fs::write(s.path().join("ws").join("mod_b.py"), "B = 2\n").unwrap();
    d.chat.sandbox_check = Some(Err("no sandbox in this test".into()));
    d.chat.turn_n = 1;
    (d, s)
}

fn route(d: &mut Direct, answer: impl Fn(&Value) -> Reply + Send + Sync + 'static) {
    d.chat.endpoint.http = Arc::new(RouteHttp::new(move |req| answer(&body_of(req))));
}

async fn explore_now(d: &mut Direct, tasks: Value) -> (Value, Value) {
    let ctx = d.ctx.clone();
    let echo: crate::trace::Echo = Arc::new(|_, _| {});
    d.chat.monitor = Some(TurnMonitor::new(
        d.chat.tracer().clone(),
        "main",
        Some(1),
        echo,
    ));
    let out = d
        .chat
        .run_explore(&ctx, &json!({"tasks": tasks}).to_string())
        .await;
    let row = d.chat.monitor.take().unwrap().summary(Map::new());
    (out, row)
}

fn transcripts(d: &Direct) -> usize {
    let dir = d.chat.tracer().trace_dir().join(d.chat.tracer().sid());
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.starts_with("t1-") && n.contains("-helper") && n.ends_with(".json")
                })
                .count()
        })
        .unwrap_or(0)
}

// rpctl ExploreTests.test_helpers_read_and_report_in_parallel.
#[tokio::test]
async fn helpers_read_and_report_in_parallel() {
    let (mut d, _s) = explore_chat(&[]);
    route(&mut d, |body| {
        let msgs = body["messages"].as_array().unwrap();
        let q = msgs[1]["content"].as_str().unwrap();
        let f = if q.contains('A') {
            "mod_a.py"
        } else {
            "mod_b.py"
        };
        if msgs.last().unwrap()["role"] == "user" {
            return resp("", &[("read_file", json!({"path": f}))]);
        }
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        resp(&format!("answer from {f}:1"), &[])
    });
    let tasks = json!([{"question": "What is A?"}, {"question": "What is B?"}]);
    let (out, row) = explore_now(&mut d, tasks.clone()).await;
    let statuses: Vec<&str> = out["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["ok", "ok"]);
    assert_eq!(out["results"][0]["report"], "answer from mod_a.py:1");
    assert_eq!(out["results"][1]["files_read"], json!(["mod_b.py"]));
    assert_eq!(
        (row["helpers"].clone(), row["helper_failures"].clone()),
        (json!(2), json!(0))
    );
    assert_eq!(transcripts(&d), 2);
    // a second explore call in the same turn keeps the first one's transcripts
    explore_now(&mut d, tasks).await;
    assert_eq!(transcripts(&d), 4);
}

// rpctl ExploreTests.test_helper_results_are_clipped_to_the_pod_context.
#[tokio::test]
async fn helper_results_are_clipped_to_the_pod_context() {
    let (mut d, s) = explore_chat(&[]);
    d.chat.max_model_len = 8192;
    d.chat.settings.max_tokens = 2048; // a small pod
    std::fs::write(s.path().join("ws").join("big.txt"), "x".repeat(50000)).unwrap();
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&sizes);
    route(&mut d, move |body| {
        let msgs = body["messages"].as_array().unwrap();
        seen.lock().unwrap().push(size_of(msgs));
        if msgs.last().unwrap()["role"] == "user" {
            return resp("", &[("read_file", json!({"path": "big.txt"}))]);
        }
        resp("done", &[])
    });
    let (out, _) = explore_now(&mut d, json!([{"question": "big"}])).await;
    assert_eq!(out["results"][0]["report"], "done");
    let ceiling = (d.chat.prompt_budget() as f64 * d.chat.estimator.chars_per_token * 0.9) as usize;
    assert!(ceiling < 20000, "{ceiling}"); // below TOOLS_MAX_OUTPUT, so it must clip
    assert!(*sizes.lock().unwrap().iter().max().unwrap() <= ceiling);
}

// rpctl ExploreTests.test_empty_report_is_retried_and_a_crash_is_an_alarm.
#[tokio::test]
async fn empty_report_is_retried_and_a_crash_is_an_alarm() {
    let (mut d, _s) = explore_chat(&[]);
    route(&mut d, |body| {
        let q = body["messages"][1]["content"].as_str().unwrap();
        if q.contains("crash") {
            return Reply::Fail(viy_http::HttpError::Connect("pod gone".into()));
        }
        if body.get("tools").is_none() {
            return resp("late report", &[]); // the retry, without tools
        }
        resp("", &[])
    });
    let tasks = json!([{"question": "empty?"}, {"question": "crash?"}]);
    let (out, row) = explore_now(&mut d, tasks).await;
    let st: Vec<(Value, Value)> = out["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["status"].clone(), r["report"].clone()))
        .collect();
    assert_eq!(st[0], (json!("retried"), json!("late report")));
    assert_eq!(st[1].0, "failed");
    assert_eq!(row["helper_failures"], 1);
    let alarms = std::fs::read_to_string(d.chat.tracer().alarms_path()).unwrap_or_default();
    assert!(alarms.contains("helper_failed"), "{alarms}");
}

// rpctl ExploreTests.test_helper_budget_forces_a_report_and_files_stay_read_only.
#[tokio::test]
async fn helper_budget_forces_a_report_and_files_stay_read_only() {
    let (mut d, s) = explore_chat(&[("EXPLORE_MAX_ROUNDS", "2")]);
    let modes = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&modes);
    route(&mut d, move |body| {
        seen.lock().unwrap().push(body["tool_choice"].clone());
        if body["tool_choice"] == "none" {
            return resp("final", &[]);
        }
        resp("", &[("read_file", json!({"path": "mod_a.py"}))])
    });
    let (out, _) = explore_now(&mut d, json!([{"question": "loop"}])).await;
    assert_eq!(
        *modes.lock().unwrap(),
        [json!("auto"), json!("auto"), json!("none")]
    );
    assert_eq!(out["results"][0]["status"], "capped");
    assert_eq!(
        std::fs::read_to_string(s.path().join("ws").join("mod_a.py")).unwrap(),
        "A = 1\n"
    );
}

// rpctl ExploreTests.test_bad_arguments_are_refused.
#[tokio::test]
async fn bad_arguments_are_refused() {
    let (mut d, _s) = explore_chat(&[]);
    let ctx = d.ctx.clone();
    for raw in [r#"{"tasks": []}"#, r#"{"tasks": [{"question": " "}]}"#] {
        assert!(
            d.chat.run_explore(&ctx, raw).await.get("error").is_some(),
            "{raw}"
        );
    }
}

// rpctl ExploreTests.test_explore_is_offered_with_the_tools.
#[test]
fn explore_is_offered_with_the_tools() {
    let names = |d: &Direct| -> Vec<String> {
        d.chat
            .tool_specs()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect()
    };
    let (d, _s) = shell_chat(json!([]), &[]);
    assert!(names(&d).contains(&"explore".to_string()));
    let (d, _s) = shell_chat(json!([]), &[("EXPLORE_ENABLED", "0")]);
    assert!(!names(&d).contains(&"explore".to_string()));
}

// rpctl ExploreTests.test_parallelism_leaves_a_slot_free.
#[test]
fn parallelism_leaves_a_slot_free() {
    let at = |seqs: &str| {
        let mut cfg = viy_config::config::Config::defaults();
        cfg.set("MAX_NUM_SEQS", seqs);
        ExploreSettings::from_config(&cfg).parallel
    };
    assert_eq!((at("4"), at("1")), (3, 1));
}

// rpctl CompactionTests.test_tool_schema_estimate_counts_explore: the
// estimate's schemas grow by explore's when it is on.
#[test]
fn tool_schema_estimate_counts_explore() {
    let (on, _s) = shell_chat(json!([]), &[]);
    let (off, _t) = shell_chat(json!([]), &[("EXPLORE_ENABLED", "0")]);
    let chars = |d: &Direct| crate::context::schema_chars(&d.chat.tool_specs());
    // rpctl's len(json.dumps(EXPLORE_TOOL)) + 2: the tool and its ", ".
    let tool = crate::context::schema_chars(&[viy_tools::explore::explore_tool()]) - 2;
    assert_eq!(chars(&on) - chars(&off), tool + 2);
}

// rpctl BackgroundCompactionTests.test_explore_runs_one_helper_at_a_time_meanwhile:
// each reply takes a second (paused clock); two helpers of two rounds take
// four seconds one at a time, two side by side.
#[tokio::test(start_paused = true)]
async fn explore_runs_one_helper_at_a_time_meanwhile() {
    for (background, want) in [(true, 4), (false, 2)] {
        let (mut d, _s) = explore_chat(&[("MAX_NUM_SEQS", "4")]);
        if background {
            d.chat.background = Some(crate::chat::compact::Background::pending());
        }
        d.chat.endpoint.http = Arc::new(RouteHttp::new(|req| {
            let body = body_of(req);
            let last = body["messages"].as_array().unwrap().last().unwrap().clone();
            let reply = if last["role"] == "user" {
                resp("", &[("list_dir", json!({}))])
            } else {
                resp("report", &[])
            };
            match reply {
                Reply::Answer { status, body } => Reply::Answer {
                    status,
                    body: body
                        .into_iter()
                        .map(|p| match p {
                            Piece::Data(b) => Piece::Late(Duration::from_secs(1), b),
                            other => other,
                        })
                        .collect(),
                },
                other => other,
            }
        }));
        let t0 = tokio::time::Instant::now();
        let (out, _) = explore_now(&mut d, json!(["a?", "b?"])).await;
        assert_eq!(out["results"].as_array().unwrap().len(), 2);
        assert_eq!(t0.elapsed().as_secs(), want, "background {background}");
        d.chat.abort_background();
    }
}
