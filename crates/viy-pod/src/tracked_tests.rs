use std::fs;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_testkit::fixtures;
use viy_testkit::http::FakeHttp;
use viy_testkit::http::Reply;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// The fixture's synthetic key (tools/fixtures.py API_KEY).
const API_KEY: &str = "rpa_SYNTHETICKEY0000";

fn config(case: &Value) -> Config {
    let mut cfg = Config::defaults();
    cfg.set("POD_NAME", "qwen-pod");
    for (k, v) in case["config"].as_object().into_iter().flatten() {
        cfg.set(k, v.as_str().unwrap());
    }
    cfg
}

fn replies(case: &Value) -> Vec<Reply> {
    case["replies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let status = r["status"].as_u64().map_or(200, |s| s as u16);
            Reply::answer(status, r["body"].as_str().unwrap_or(""))
        })
        .collect()
}

/// A RunPodError's text with a JSON detail in one canonical form, since
/// serde's pretty printing and Python's differ only in spacing (kb d030).
fn canonical_error(text: &str) -> String {
    match text.split_once('\n') {
        Some((head, detail)) => match serde_json::from_str::<Value>(detail) {
            Ok(json) => format!("{head}\n{json}"),
            Err(_) => text.to_string(),
        },
        None => text.to_string(),
    }
}

#[tokio::test]
async fn resolve_and_base_url_do_what_rpctls_do() {
    let fixture = fixtures::load("pod/resolve").unwrap();
    assert!(fixture.cases.len() >= 30);
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let dir = ScratchDir::new("pod-tracked").unwrap();
        let paths = Paths::under(dir.path());
        if let Some(text) = case["state"].as_str() {
            fs::create_dir_all(paths.pod_state_dir()).unwrap();
            fs::write(paths.pod_state_file(), text).unwrap();
        }
        let cfg = config(case);
        let http = Arc::new(FakeHttp::new(replies(case)));
        let mut said = Vec::new();
        let outcome = match case["call"].as_str().unwrap() {
            "resolve_pod" => {
                let rp = RunPod::new(API_KEY, http.clone());
                resolve(&rp, &paths, &cfg, |n| said.push(n.to_string()))
                    .await
                    .map(|pod| pod.unwrap_or(Value::Null))
                    .map_err(|e| match e {
                        ResolveError::RunPod(e) => e,
                        ResolveError::State(e) => panic!("{name}: {e}"),
                    })
            }
            _ => {
                let discover = case["discover"].as_bool().unwrap();
                let url = base_url(&paths, &cfg, http.clone(), discover, |n| {
                    said.push(n.to_string())
                })
                .await
                .unwrap();
                Ok(url.map_or(Value::Null, Value::from))
            }
        };

        let sent: Vec<String> = http
            .sent()
            .iter()
            .map(|r| format!("{} {}", r.method, r.url))
            .collect();
        assert_eq!(json!(sent), case["sent"], "{name}: requests");
        assert_eq!(json!(said), case["said"], "{name}: notices");
        match outcome {
            Ok(value) => assert_eq!(value, case["ok"], "{name}"),
            Err(e) => {
                assert_eq!(
                    canonical_error(&e.to_string()),
                    canonical_error(case["error"].as_str().unwrap()),
                    "{name}"
                );
                assert_eq!(
                    e.status_code().map(u64::from),
                    case["status_code"].as_u64(),
                    "{name}"
                );
                assert_eq!(e.transient(), case["transient"], "{name}");
            }
        }
        let after = fs::read_to_string(paths.pod_state_file()).ok();
        assert_eq!(json!(after), case["state_after"], "{name}: pod.json");
        assert_eq!(
            json!(tunnel_only(&paths, &cfg).unwrap()),
            case["tunnel_only"],
            "{name}"
        );
        assert_eq!(
            json!(transport_label(&paths, &cfg).unwrap()),
            case["transport_label"],
            "{name}"
        );
    }
}

#[test]
fn the_warning_names_every_pod_and_the_one_taken() {
    let notice = MultiplePods {
        name: "qwen-pod".into(),
        ids: vec!["podC".into(), "podB".into()],
    };
    assert_eq!(
        notice.to_string(),
        "Multiple pods named 'qwen-pod' (podC, podB); using podC."
    );
}

/// rpctl stops with a KeyError on these answers, so the fixture cannot
/// hold them (d054).
#[tokio::test]
async fn an_answer_without_an_id_never_clears_the_cache() {
    let cached = r#"{"pod_id": "pod111", "tunnel_only": true}"#;
    let our_pods_without_ids = json!([
        {"name": "qwen-pod", "desiredStatus": "RUNNING"},
        {"id": 7, "name": "qwen-pod", "desiredStatus": "RUNNING"},
    ]);
    for (get, list) in [
        (
            Some(json!({"name": "renamed", "desiredStatus": "RUNNING"})),
            None,
        ),
        (Some(json!({"id": 7, "desiredStatus": "EXITED"})), None),
        (Some(json!("not an object")), None),
        (None, Some(our_pods_without_ids)),
    ] {
        let dir = ScratchDir::new("pod-tracked").unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(paths.pod_state_dir()).unwrap();
        fs::write(paths.pod_state_file(), cached).unwrap();
        let mut replies = vec![match &get {
            Some(pod) => Reply::answer(200, pod.to_string()),
            None => Reply::answer(404, "{}"),
        }];
        replies.extend(list.map(|l| Reply::answer(200, l.to_string())));
        let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new(replies)));
        let mut cfg = Config::defaults();
        cfg.set("POD_NAME", "qwen-pod");
        let pod = resolve(&rp, &paths, &cfg, |_| {}).await.unwrap();
        let state = state::load(&paths).unwrap();
        match get {
            Some(_) => {
                assert_eq!(pod.as_ref().and_then(pod_id), Some("pod111"), "{get:?}");
                assert_eq!(state.pod_id(), Some("pod111"), "{get:?}");
                assert_eq!(state.tunnel_only(), Some(true), "{get:?}");
            }
            // The cached pod is gone and no pod by the name can be
            // addressed: as with no pod by the name at all.
            None => assert_eq!((pod, state.pod_id()), (None, None)),
        }
    }
}

#[test]
fn the_tunnel_url_uses_the_local_port() {
    let mut cfg = Config::defaults();
    assert_eq!(local_url(&cfg), "http://127.0.0.1:8000");
    cfg.set("TUNNEL_LOCAL_PORT", " 18001 ");
    assert_eq!(local_url(&cfg), "http://127.0.0.1:18001");
}
