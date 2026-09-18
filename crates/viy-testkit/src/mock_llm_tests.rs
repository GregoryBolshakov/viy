use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use super::*;
use crate::scenario;

/// What a client reassembles from an SSE body.
#[derive(Debug, Default)]
struct Reply {
    reasoning: String,
    content: String,
    /// (id, name, arguments) per tool-call index.
    calls: Vec<(String, String, String)>,
    finish: Value,
    done: bool,
}

fn reassemble(body: &[u8]) -> Reply {
    let text = String::from_utf8(body.to_vec()).unwrap();
    let mut r = Reply::default();
    for data in text.split("\n\n").filter_map(|e| e.strip_prefix("data: ")) {
        if data == "[DONE]" {
            r.done = true;
            continue;
        }
        // A scenario's raw lines may hold what is not JSON on purpose.
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        for choice in chunk["choices"].as_array().unwrap() {
            if !choice["finish_reason"].is_null() {
                r.finish = choice["finish_reason"].clone();
            }
            let d = &choice["delta"];
            r.reasoning += d["reasoning"].as_str().unwrap_or_default();
            r.content += d["content"].as_str().unwrap_or_default();
            for tc in d["tool_calls"].as_array().into_iter().flatten() {
                let i = tc["index"].as_u64().unwrap() as usize;
                if r.calls.len() <= i {
                    r.calls.resize(i + 1, Default::default());
                }
                let slot = &mut r.calls[i];
                slot.0 += tc["id"].as_str().unwrap_or_default();
                slot.1 += tc["function"]["name"].as_str().unwrap_or_default();
                slot.2 += tc["function"]["arguments"].as_str().unwrap_or_default();
            }
        }
    }
    r
}

/// The text of one kind of event in a scripted streamed response.
fn scripted(response: &Value, kind: &str) -> String {
    response["stream"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e[kind].as_str())
        .collect()
}

/// Sends a recorded request again: (status, body).
fn replay(mock: &MockLlm, entry: &Value) -> (u16, Vec<u8>) {
    http(
        &mock.uri(),
        entry["method"].as_str().unwrap(),
        entry["path"].as_str().unwrap(),
        entry.get("body"),
    )
    .unwrap()
}

#[test]
fn serves_each_scenario_to_rps_recorded_requests() {
    let names = scenario::recorded().unwrap();
    assert!(names.len() >= 2, "{names:?}");
    for name in names {
        let sc = scenario::load(&name).unwrap();
        let log = scenario::rp_log(&name).unwrap();
        let mock = MockLlm::start(&sc).unwrap();
        let mut responses = sc["responses"].as_array().unwrap().iter();
        for entry in &log.entries {
            let (status, body) = replay(&mock, entry);
            if entry["path"] != "/v1/chat/completions" {
                assert_eq!(status, 200, "{name}: {entry}");
                continue;
            }
            let r = responses.next().unwrap();
            assert_eq!(
                u64::from(status),
                r["status"].as_u64().unwrap_or(200),
                "{name}: {entry}"
            );
            if r.get("status").is_some() {
                continue;
            }
            if let Some(text) = r.get("message") {
                let reply: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(&reply["choices"][0]["message"]["content"], text);
                continue;
            }
            let reply = reassemble(&body);
            assert_eq!(reply.reasoning, scripted(r, "reasoning"), "{name}");
            assert_eq!(reply.content, scripted(r, "content"), "{name}");
            let names: Vec<&str> = reply.calls.iter().map(|c| c.1.as_str()).collect();
            let want: Vec<&str> = r["stream"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|e| e["tool_call"]["name"].as_str())
                .collect();
            assert_eq!(names, want, "{name}");
            for (_, _, args) in &reply.calls {
                serde_json::from_str::<Value>(args).unwrap();
            }
            let events = r["stream"].as_array().unwrap();
            if events.iter().any(|e| e.get("drop").is_some()) {
                assert!(!reply.done, "{name}: [DONE] after a drop");
                continue;
            }
            let expected_finish =
                events
                    .iter()
                    .find_map(|e| e["finish"].as_str())
                    .unwrap_or(if want.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    });
            assert_eq!(reply.finish, json!(expected_finish), "{name}");
            assert!(reply.done, "{name}: no [DONE]");
        }
        assert_eq!(mock.unused_responses(), 0, "{name}");
        let seen: Vec<Value> = mock
            .requests()
            .iter()
            .map(CapturedRequest::log_entry)
            .collect();
        let sent: Vec<&Value> = log.entries.iter().collect();
        assert_eq!(seen.len(), sent.len());
        for (s, e) in seen.iter().zip(sent) {
            assert_eq!(s.get("body"), e.get("body"), "{name}");
            assert_eq!(s.get("response"), e.get("response"), "{name}");
        }
    }
}

#[test]
fn an_unexpected_request_gets_500_and_is_recorded() {
    let sc = json!({"responses": [
        {"expect": {"stream": true, "last_user": "hi"}, "stream": [{"content": "hello"}]}]});
    let mock = MockLlm::start(&sc).unwrap();
    let body =
        json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "bye"}]});
    let (status, text) = http(&mock.uri(), "POST", "/v1/chat/completions", Some(&body)).unwrap();
    assert_eq!(status, 500);
    assert!(String::from_utf8_lossy(&text).contains("last_user"));
    let (status, _) = http(&mock.uri(), "POST", "/v1/chat/completions", Some(&body)).unwrap();
    assert_eq!(status, 500);
    let mismatches: Vec<Option<String>> = mock.requests().into_iter().map(|r| r.mismatch).collect();
    assert_eq!(
        mismatches,
        vec![
            Some(r#"expected last_user="hi", got "bye""#.to_string()),
            Some("no response queued".to_string()),
        ]
    );
}

#[test]
fn a_dropped_stream_ends_without_done() {
    let sc = json!({"responses": [{"stream": [{"content": "partial"}, {"drop": true}]}]});
    let mock = MockLlm::start(&sc).unwrap();
    let body = json!({"stream": true, "messages": []});
    let (status, raw) = http(&mock.uri(), "POST", "/v1/chat/completions", Some(&body)).unwrap();
    assert_eq!(status, 200);
    let r = reassemble(&raw);
    assert_eq!(
        (r.content.as_str(), r.finish, r.done),
        ("partial", Value::Null, false)
    );
}

#[test]
fn an_http_error_is_served_as_scripted() {
    let sc = json!({"responses": [{"status": 401, "body": "invalid key"}]});
    let mock = MockLlm::start(&sc).unwrap();
    let (status, body) = http(
        &mock.uri(),
        "POST",
        "/v1/chat/completions",
        Some(&json!({})),
    )
    .unwrap();
    assert_eq!((status, body), (401, b"invalid key".to_vec()));
}

#[test]
fn models_reports_the_scenario_context_length() {
    let mock = MockLlm::start(&json!({"max_model_len": 4096})).unwrap();
    let (status, body) = http(&mock.uri(), "GET", "/v1/models", None).unwrap();
    assert_eq!(status, 200);
    let models: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(models["data"][0]["max_model_len"], json!(4096));
    assert_eq!(models["data"][0]["id"], json!(DEFAULT_MODEL));
}

/// fakellm and this mock send the same bytes for every recorded request.
#[cfg(feature = "oracle")]
#[test]
fn same_bytes_as_fakellm() {
    use std::io::BufRead;
    use std::process::Command;
    use std::process::Stdio;

    let root = crate::repo::repo_root();
    for name in scenario::recorded().unwrap() {
        let scratch = crate::scratch::ScratchDir::new("fakellm").unwrap();
        let mut child = Command::new("python3")
            .arg("-I")
            .arg(root.join("tools").join("fakellm.py"))
            .arg(scenario::dir().join(format!("{name}.json")))
            .arg("--log")
            .arg(scratch.path().join("log.jsonl"))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let fake = line.trim().strip_prefix("listening ").unwrap().to_string();
        let mock = MockLlm::start(&scenario::load(&name).unwrap()).unwrap();
        for entry in &scenario::rp_log(&name).unwrap().entries {
            let (method, path) = (
                entry["method"].as_str().unwrap(),
                entry["path"].as_str().unwrap(),
            );
            let theirs = http(&fake, method, path, entry.get("body")).unwrap();
            let ours = http(&mock.uri(), method, path, entry.get("body")).unwrap();
            assert_eq!(
                String::from_utf8_lossy(&ours.1),
                String::from_utf8_lossy(&theirs.1),
                "{name}: {method} {path}"
            );
            assert_eq!(ours.0, theirs.0);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}
