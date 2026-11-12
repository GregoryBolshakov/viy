use std::collections::BTreeSet;
use std::sync::Arc;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_testkit::fixtures;
use viy_testkit::http::FakeHttp;
use viy_testkit::http::Piece;
use viy_testkit::http::Reply;

use super::*;

/// The fixture's synthetic key (tools/fixtures.py API_KEY).
const API_KEY: &str = "rpa_SYNTHETICKEY0000";

/// Headers rp's `requests` sends that viy drops (kb d046): no
/// decompression, and HTTP/1.1's default keep-alive.
const NOT_SENT_BY_VIY: [&str; 2] = ["Accept-Encoding", "Connection"];

/// The fixture's reply as a script. A read timeout is the stream going
/// quiet, so the real per-read timeout ends it.
fn reply(spec: &Value) -> Reply {
    if spec["fail"] == "connect" {
        return Reply::Fail(HttpError::Connect("synthetic: connection refused".into()));
    }
    let chunks: Vec<String> = match spec.get("chunks") {
        Some(chunks) => serde_json::from_value(chunks.clone()).unwrap(),
        None => vec![spec["body"].as_str().unwrap_or("").to_string()],
    };
    let mut body: Vec<Piece> = chunks.into_iter().map(|c| Piece::Data(c.into())).collect();
    if spec["then"] == "read_timeout" {
        body.push(Piece::Hang);
    }
    Reply::Answer {
        status: spec["status"].as_u64().map_or(200, |s| s as u16),
        body,
    }
}

fn strings(value: &Value) -> Vec<String> {
    serde_json::from_value(value.clone()).unwrap()
}

/// Runs the fixture's call; the outcome in the fixture's JSON shape.
async fn call(rp: &RunPod, call: &[Value]) -> Result<Value, RunPodError> {
    let arg = |i: usize| call[i].as_str().unwrap();
    let unit = |()| Value::Null;
    let opt = |v: Option<Value>| v.unwrap_or(Value::Null);
    Ok(match arg(0) {
        "list_pods" => json!(rp.list_pods().await?),
        "get_pod" => opt(rp.get_pod(arg(1)).await?),
        "create_pod" => opt(rp.create_pod(call[1].clone()).await?),
        "stop_pod" => unit(rp.stop_pod(arg(1)).await?),
        "start_pod" => unit(rp.start_pod(arg(1)).await?),
        "delete_pod" => unit(rp.delete_pod(arg(1)).await?),
        "list_volumes" => json!(rp.list_volumes().await?),
        "get_volume" => opt(rp.get_volume(arg(1)).await?),
        "create_volume" => opt(rp
            .create_volume(arg(1), call[2].as_u64().unwrap(), arg(3))
            .await?),
        "delete_volume" => unit(rp.delete_volume(arg(1)).await?),
        "data_centers" => json!(rp.data_centers().await?),
        "cuda_stock" => {
            let stock = rp
                .cuda_stock(
                    &strings(&call[1]),
                    &strings(&call[2]),
                    arg(3),
                    &strings(&call[4]),
                    call[5].as_bool().unwrap(),
                )
                .await?;
            json!(
                stock
                    .into_iter()
                    .map(|((dc, gpu), s)| json!([dc, gpu, s.fit, s.old]))
                    .collect::<Vec<_>>()
            )
        }
        "pod_logs" => {
            let entries = rp
                .pod_logs(
                    arg(1),
                    call[2].as_u64().unwrap() as u32,
                    Duration::from_secs(call[3].as_u64().unwrap()),
                )
                .await?;
            json!(entries.into_iter().map(as_rpctl_entry).collect::<Vec<_>>())
        }
        other => panic!("no such call: {other}"),
    })
}

/// A log entry as rpctl's dict: only the keys the line had.
fn as_rpctl_entry(entry: LogEntry) -> Value {
    let mut map = Map::new();
    if let Some(ts) = entry.ts {
        map.insert("ts".into(), ts.into());
    }
    if let Some(source) = entry.source {
        map.insert("source".into(), source.into());
    }
    map.insert("line".into(), entry.line.into());
    Value::Object(map)
}

/// Calls whose result rpctl returns and viy drops: nothing reads it.
fn viy_discards_the_result(call: &str) -> bool {
    matches!(call, "start_pod")
}

/// JSON text in a canonical form that keeps key order, for comparing bodies
/// and details whose spacing differs (serde's compact and pretty output
/// against Python's `json.dumps`, kb d030).
fn canonical(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()
        .map(|v| v.to_string())
}

fn check_request(name: &str, sent: &Request, rp_sent: &Value) {
    assert_eq!(sent.method.as_str(), rp_sent["method"], "{name}");
    assert_eq!(sent.url, rp_sent["url"], "{name}");
    let rp_headers = rp_sent["headers"].as_object().unwrap();
    let names: Vec<&str> = sent.headers.iter().map(|(n, _)| n.as_str()).collect();
    let rp_names: Vec<&str> = rp_headers
        .keys()
        .map(String::as_str)
        .filter(|n| !NOT_SENT_BY_VIY.contains(n))
        .collect();
    assert_eq!(names, rp_names, "{name}: header names and order");
    for (header, value) in &sent.headers {
        if header == "Content-Length" && sent.body.is_some() {
            // The bodies' bytes differ (json.dumps' spacing); the content
            // is compared below.
            let length = sent.body.as_ref().map(Vec::len).unwrap_or_default();
            assert_eq!(value, &length.to_string(), "{name}");
            continue;
        }
        if header == "User-Agent" {
            // rp's is python-requests'; viy's is the provider marker (kb d137).
            assert_eq!(value, viy_http::user_agent::PROVIDER_API, "{name}");
            continue;
        }
        assert_eq!(rp_headers[header], value.as_str(), "{name}: {header}");
        assert!(!value.to_lowercase().contains("viy"), "{name}: {header}");
    }
    let body = sent
        .body
        .as_deref()
        .map(|b| canonical(std::str::from_utf8(b).unwrap()).unwrap());
    let rp_body = rp_sent["body"].as_str().map(|b| canonical(b).unwrap());
    assert_eq!(body, rp_body, "{name}: body");
    let (connect, read) = match &rp_sent["timeout"] {
        Value::Array(pair) => (pair[0].as_u64().unwrap(), pair[1].as_u64().unwrap()),
        one => (one.as_u64().unwrap(), one.as_u64().unwrap()),
    };
    assert_eq!(
        (sent.connect_timeout, sent.read_timeout),
        (Duration::from_secs(connect), Duration::from_secs(read)),
        "{name}: timeouts"
    );
}

/// rp's message where `requests` named an exception class, viy's where it
/// names its own error kind (module docs).
fn viy_message(name: &str, rp_message: &str) -> String {
    match name {
        "transport_failure_is_transient" => {
            "GET /pods/pod111 -> connection failed: synthetic: connection refused".into()
        }
        "read_timeout_is_transient" => "GET /networkvolumes -> timeout".into(),
        "graphql_transport_failure" => {
            "GraphQL -> connection failed: synthetic: connection refused".into()
        }
        "pod_logs_stall_before_entries_is_an_error" => "Platform logs unavailable (timeout)".into(),
        "pod_logs_unreachable" => {
            "Platform logs unavailable (connection failed: synthetic: connection refused)".into()
        }
        _ => rp_message.into(),
    }
}

fn check_error(name: &str, error: &RunPodError, case: &Value) {
    assert_eq!(
        error.status_code().map(u64::from),
        case["status_code"].as_u64(),
        "{name}"
    );
    assert_eq!(error.transient(), case["transient"], "{name}");
    let expected = viy_message(name, case["error"].as_str().unwrap());
    let message = error.to_string();
    let (head, detail) = message.split_once('\n').unwrap_or((&message, ""));
    let (rp_head, rp_detail) = expected.split_once('\n').unwrap_or((&expected, ""));
    assert_eq!(head, rp_head, "{name}");
    match canonical(rp_detail) {
        Some(rp_json) => assert_eq!(canonical(detail), Some(rp_json), "{name}: {detail}"),
        None => assert_eq!(detail, rp_detail, "{name}"),
    }
}

#[tokio::test(start_paused = true)]
async fn each_call_sends_and_reads_what_rpctls_does() {
    let fixture = fixtures::load("pod/api").unwrap();
    let mut seen = BTreeSet::new();
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let call_spec = case["call"].as_array().unwrap();
        seen.insert(call_spec[0].as_str().unwrap().to_string());
        if name == "proxy_url" {
            assert_eq!(
                proxy_url(call_spec[1].as_str().unwrap(), "8000"),
                case["ok"]
            );
            continue;
        }
        let http = Arc::new(FakeHttp::new([reply(&case["reply"])]));
        let rp = RunPod::new(API_KEY, http.clone());
        let outcome = call(&rp, call_spec).await;

        let sent = http.sent();
        let rp_sent = case["sent"].as_array().unwrap();
        assert_eq!(sent.len(), rp_sent.len(), "{name}");
        for (sent, rp_sent) in sent.iter().zip(rp_sent) {
            check_request(name, sent, rp_sent);
        }
        match outcome {
            Ok(value) => {
                assert!(
                    case.get("error").is_none(),
                    "{name}: rp failed, viy did not"
                );
                if !viy_discards_the_result(call_spec[0].as_str().unwrap()) {
                    assert_eq!(value, case["ok"], "{name}");
                }
            }
            Err(error) => check_error(name, &error, case),
        }
    }
    // Every public call of rpctl's RunPod and proxy_url.
    assert_eq!(seen.len(), 14, "{seen:?}");
}

#[test]
fn transient_is_no_answer_429_or_a_5xx_to_504() {
    let transient: Vec<Option<u16>> = [None, Some(200), Some(400), Some(401), Some(403)]
        .into_iter()
        .chain((404..=599).map(Some))
        .filter(|&code| RunPodError::new("x", code).transient())
        .collect();
    assert_eq!(
        transient,
        [None, Some(429), Some(500), Some(502), Some(503), Some(504)]
    );
}

fn log_line(ts: &str, line: &str) -> Vec<u8> {
    format!("data: {}\n", json!({"ts": ts, "line": line})).into_bytes()
}

fn logs_rp(body: Vec<Piece>) -> RunPod {
    let http = FakeHttp::new([Reply::Answer { status: 200, body }]);
    RunPod::new(API_KEY, Arc::new(http))
}

/// rpctl checks its budget only when a line arrives, so a stream that keeps
/// sending keepalives ends there; viy ends at the budget itself.
#[tokio::test(start_paused = true)]
async fn the_log_read_ends_at_the_budget_while_lines_keep_coming() {
    let mut body = vec![Piece::Data(log_line("1", "in time"))];
    for i in 0..20 {
        body.push(Piece::Late(
            Duration::from_secs(1),
            b": keepalive\n".to_vec(),
        ));
        if i == 9 {
            body.push(Piece::Data(log_line("2", "too late")));
        }
    }
    let rp = logs_rp(body);
    let start = Instant::now();
    let entries = rp.pod_logs("pod111", 100, LOG_BUDGET).await.unwrap();
    assert_eq!(start.elapsed(), LOG_BUDGET);
    assert_eq!(
        entries,
        [LogEntry {
            ts: Some("1".into()),
            source: None,
            line: "in time".into(),
        }]
    );
}

#[tokio::test(start_paused = true)]
async fn a_budget_spent_on_keepalives_is_no_entries_not_an_error() {
    let body = (0..20)
        .map(|_| Piece::Late(Duration::from_secs(1), b": keepalive\n".to_vec()))
        .collect();
    let entries = logs_rp(body)
        .pod_logs("pod111", 100, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(entries, []);
}

#[tokio::test(start_paused = true)]
async fn a_quiet_log_stream_ends_after_two_seconds() {
    let rp = logs_rp(vec![Piece::Data(log_line("1", "a")), Piece::Hang]);
    let start = Instant::now();
    let entries = rp.pod_logs("pod111", 100, LOG_BUDGET).await.unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    assert_eq!(entries.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_log_stream_that_never_answers_gives_up_after_seven_seconds() {
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([Reply::Hang])));
    let start = Instant::now();
    let error = rp.pod_logs("pod111", 100, LOG_BUDGET).await.unwrap_err();
    assert_eq!(start.elapsed(), Duration::from_secs(7));
    assert_eq!(error.to_string(), "Platform logs unavailable (timeout)");
    assert!(error.transient());
}

#[tokio::test(start_paused = true)]
async fn a_rest_call_waits_two_minutes_for_an_answer_at_most() {
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([Reply::Hang])));
    let start = Instant::now();
    let error = rp.get_pod("pod111").await.unwrap_err();
    assert_eq!(start.elapsed(), Duration::from_secs(120));
    assert_eq!(
        (error.to_string().as_str(), error.status_code()),
        ("GET /pods/pod111 -> timeout", None)
    );
}

#[test]
fn the_key_is_not_in_debug_output() {
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::default()));
    let shown = format!("{rp:?}");
    assert!(!shown.contains(API_KEY), "{shown}");
}

/// rpctl's `_req` turns JSON `null` into None, as it does an empty body.
#[tokio::test]
async fn a_null_answer_is_no_value() {
    let http = FakeHttp::new([Reply::answer(200, "null"), Reply::answer(200, "")]);
    let rp = RunPod::new(API_KEY, Arc::new(http));
    assert_eq!(rp.get_volume("vol222").await, Ok(None));
    assert_eq!(rp.get_pod("pod111").await, Ok(None));
}

/// The budget ends while the stream is quiet and nothing has come: rpctl
/// waits out the 2 s gap and fails, so the boot report says "unavailable".
#[tokio::test(start_paused = true)]
async fn a_budget_that_ends_in_a_quiet_gap_with_nothing_read_is_an_error() {
    let rp = logs_rp(vec![Piece::Data(b": keepalive\n".to_vec()), Piece::Hang]);
    let start = Instant::now();
    let error = rp
        .pod_logs("pod111", 100, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    assert_eq!(error.to_string(), "Platform logs unavailable (timeout)");
}

#[tokio::test(start_paused = true)]
async fn lines_may_end_in_a_bare_carriage_return() {
    let body = vec![Piece::Data(
        b"data: {\"ts\": \"2\", \"line\": \"b\"}\rdata: {\"ts\": \"1\", \"line\": \"a\"}\r"
            .to_vec(),
    )];
    let lines: Vec<String> = logs_rp(body)
        .pod_logs("pod111", 100, LOG_BUDGET)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.line)
        .collect();
    assert_eq!(lines, ["a", "b"]);
}

#[tokio::test(start_paused = true)]
async fn a_budget_with_no_end_reads_until_the_gap() {
    let rp = logs_rp(vec![Piece::Data(log_line("1", "a")), Piece::Hang]);
    let entries = rp.pod_logs("pod111", 100, Duration::MAX).await.unwrap();
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn bodiless_calls_other_than_get_declare_a_zero_length() {
    let http = Arc::new(FakeHttp::new([
        Reply::answer(200, ""),
        Reply::answer(200, ""),
        Reply::answer(200, "[]"),
    ]));
    let rp = RunPod::new(API_KEY, http.clone());
    rp.stop_pod("pod111").await.unwrap();
    rp.delete_volume("vol222").await.unwrap();
    rp.list_pods().await.unwrap();
    let sent = http.sent();
    let lengths: Vec<Option<&str>> = sent
        .iter()
        .map(|r| r.header_value("Content-Length"))
        .collect();
    assert_eq!(lengths, [Some("0"), Some("0"), None]);
}

/// The same text rpctl's `api.log_time` sorts by.
#[test]
fn log_entries_sort_by_rpctls_key() {
    let entry = |ts: &str| LogEntry {
        ts: Some(ts.to_string()),
        source: None,
        line: String::new(),
    };
    assert_eq!(
        entry("2026-07-27T02:27:52Z").sort_key(),
        "2026-07-27T02:27:52.000000000"
    );
    assert_eq!(
        entry("2026-07-27T02:27:52.771976788Z").sort_key(),
        "2026-07-27T02:27:52.771976788"
    );
    assert_eq!(entry("1").sort_key(), "1");
}

#[tokio::test]
async fn client_tag_off_sends_rps_user_agent() {
    for (value, want) in [
        (None, user_agent::PROVIDER_API),
        (Some("1"), user_agent::PROVIDER_API),
        (Some("0"), user_agent::RP_REQUESTS),
        (Some("no"), user_agent::RP_REQUESTS),
    ] {
        let mut cfg = viy_config::Config::defaults();
        if let Some(value) = value {
            cfg.set("CLIENT_TAG", value);
        }
        let http = Arc::new(FakeHttp::new([Reply::answer(200, "null")]));
        let rp = RunPod::configured(API_KEY, http.clone(), &cfg);
        assert_eq!(rp.get_pod("pod111").await, Ok(None));
        let sent = http.sent();
        assert_eq!(sent[0].header_value("User-Agent"), Some(want), "{value:?}");
    }
}
