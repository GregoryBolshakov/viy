//! Reading the stream line by line, and a round against the scripted
//! model server: what a failed request says.

use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_http::ReqwestHttp;
use viy_testkit::mock_llm::MockLlm;

use super::*;

fn data(chunk: Value) -> String {
    format!("data: {chunk}")
}

fn delta(d: Value) -> String {
    data(json!({"choices": [{"index": 0, "delta": d, "finish_reason": null}]}))
}

#[test]
fn tool_calls_are_reassembled_by_index() {
    let mut reply = Reply::default();
    let mut answer = String::new();
    for line in [
        delta(
            json!({"tool_calls": [{"index": 1, "id": "b", "function": {"name": "fetch_url", "arguments": ""}}]}),
        ),
        delta(
            json!({"tool_calls": [{"index": 0, "id": "a", "function": {"name": "web_search", "arguments": "{\"q"}}]}),
        ),
        delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "uery\": 1}"}}]})),
        delta(json!({"tool_calls": [{"index": 1, "function": {"arguments": "{}"}}]})),
        // A call that never got a name is not one.
        delta(json!({"tool_calls": [{"index": 2, "function": {"arguments": "{}"}}]})),
        data(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})),
    ] {
        reply.line(&line, &mut answer).unwrap();
    }
    let round = reply.finish(&answer);
    assert_eq!(
        round.tool_calls,
        [
            ToolCall {
                id: "a".into(),
                name: "web_search".into(),
                arguments: "{\"query\": 1}".into()
            },
            ToolCall {
                id: "b".into(),
                name: "fetch_url".into(),
                arguments: "{}".into()
            },
        ]
    );
    assert_eq!(round.finish.as_deref(), Some("tool_calls"));
}

#[test]
fn reasoning_comes_under_either_name_and_content_goes_to_the_answer() {
    let mut reply = Reply::default();
    let mut answer = String::new();
    assert_eq!(
        reply.line(
            &delta(json!({"role": "assistant", "content": ""})),
            &mut answer
        ),
        Some(vec![])
    );
    assert_eq!(
        reply.line(&delta(json!({"reasoning": "a"})), &mut answer),
        Some(vec![Piece::Reasoning("a".into())])
    );
    assert_eq!(
        reply.line(
            &delta(json!({"reasoning": "", "reasoning_content": "b"})),
            &mut answer
        ),
        Some(vec![Piece::Reasoning("b".into())])
    );
    assert_eq!(
        reply.line(&delta(json!({"content": "c"})), &mut answer),
        Some(vec![Piece::Content("c".into())])
    );
    assert_eq!(reply.line(": keep-alive", &mut answer), None);
    assert_eq!(reply.line("data: not json", &mut answer), None);
    assert_eq!(reply.line("event: x", &mut answer), None);
    let usage = json!({"prompt_tokens": 1, "completion_tokens": 2});
    assert_eq!(
        reply.line(&data(json!({"choices": [], "usage": usage})), &mut answer),
        Some(vec![])
    );
    // An empty usage later does not replace it.
    reply.line(&data(json!({"choices": [], "usage": {}})), &mut answer);
    assert_eq!(reply.line("data: [DONE]", &mut answer), None);
    assert!(reply.done);
    let round = reply.finish(&answer);
    assert_eq!((round.reasoning.as_str(), round.text.as_str()), ("ab", "c"));
    assert_eq!(round.usage, Some(usage));
    assert_eq!(round.finish, None);
}

struct Seen {
    events: Mutex<Vec<EventMsg>>,
    touched: AtomicUsize,
}

impl Seen {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            touched: AtomicUsize::new(0),
        }
    }

    fn notices(&self) -> Vec<(String, String)> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                EventMsg::Notice(n) => Some((n.code.clone(), n.message.clone())),
                _ => None,
            })
            .collect()
    }
}

async fn round(base: &str, backend: &str, seen: &Seen) -> Option<Round> {
    let endpoint = Endpoint {
        base: base.into(),
        key: "fake-key".into(),
        backend: backend.into(),
        is_vllm: backend != "openai",
        http: Arc::new(ReqwestHttp::new().unwrap()),
    };
    let emit = |m| seen.events.lock().unwrap().push(m);
    let touch = || {
        seen.touched
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    };
    let sink = Sink {
        emit: &emit,
        touch: &touch,
    };
    let mut answer = String::new();
    stream_once(
        &endpoint,
        &json!({"model": "m", "messages": []}),
        &sink,
        &mut answer,
    )
    .await
}

fn serve(responses: Value) -> MockLlm {
    MockLlm::start(&json!({"responses": responses})).unwrap()
}

#[tokio::test]
async fn a_streamed_reply_touches_the_activity_file_per_chunk() {
    let mock = serve(json!([{"stream": [{"reasoning": "r"}, {"content": "hi"}]}]));
    let seen = Seen::new();
    let out = round(&mock.uri(), "runpod", &seen).await.unwrap();
    assert_eq!(
        (out.text.as_str(), out.finish.as_deref()),
        ("hi", Some("stop"))
    );
    // Role, reasoning, content, finish and usage chunks; not [DONE].
    assert_eq!(seen.touched.load(std::sync::atomic::Ordering::SeqCst), 5);
    let headers = &mock.requests()[0];
    assert_eq!(headers.header("authorization"), Some("Bearer fake-key"));
    assert!(
        headers
            .header("user-agent")
            .unwrap()
            .starts_with("python-requests/")
    );
}

#[tokio::test]
async fn http_errors_say_what_to_do() {
    let seen = Seen::new();
    let mock = serve(json!([{"status": 401, "body": "bad key"}]));
    assert_eq!(round(&mock.uri(), "runpod", &seen).await, None);
    assert_eq!(
        seen.notices(),
        [
            ("http_error".into(), "HTTP 401: bad key".into()),
            (
                "http_error_hint".into(),
                "VLLM_API_KEY in .env does not match the running server. Restart it with viy down && viy up".into()
            ),
        ]
    );
    let seen = Seen::new();
    let mock = serve(json!([{"status": 401, "body": "no"}]));
    round(&mock.uri(), "openai", &seen).await;
    assert_eq!(
        seen.notices()[1].1,
        "The endpoint rejected ENDPOINT_API_KEY."
    );
    let seen = Seen::new();
    let long = format!(
        "\"auto\" tool choice requires --enable-auto-tool-choice {}",
        "x".repeat(700)
    );
    let mock = serve(json!([{"status": 400, "body": long}]));
    round(&mock.uri(), "local", &seen).await;
    let notices = seen.notices();
    assert_eq!(notices[0].1.chars().count(), "HTTP 400: ".len() + 600);
    assert_eq!(
        notices[1].1,
        "The pod was launched without --tool-call-parser. Recreate it (viy down && viy up) or use /search off"
    );
    let seen = Seen::new();
    let mock = serve(json!([{"status": 400, "body": "tool trouble"}]));
    round(&mock.uri(), "openai", &seen).await;
    assert_eq!(seen.notices().len(), 1, "no vLLM hint for a hosted API");
}

#[tokio::test]
async fn a_refused_connection_points_at_the_tunnel_on_loopback() {
    // A port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let seen = Seen::new();
    assert_eq!(
        round(&format!("http://127.0.0.1:{port}"), "runpod", &seen).await,
        None
    );
    let codes: Vec<String> = seen.notices().into_iter().map(|(c, _)| c).collect();
    assert_eq!(codes, ["connection_failed", "tunnel_hint", "server_hint"]);
    assert!(seen.notices()[0].1.starts_with("connection failed: "));
    assert_eq!(seen.notices()[2].1, "Is the server up? viy status");
}
