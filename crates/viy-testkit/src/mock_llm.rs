//! A scripted model server for viy's own tests: the Rust twin of
//! `tools/fakellm.py`, serving the same scenario format
//! (`tests/scenarios/README.md`) with the same bytes, so a test needs no
//! Python. Built after Codex's `core/tests/common/streaming_sse.rs`: a small
//! HTTP/1.1 server on a loopback socket, one thread per connection, every
//! reply sent with `connection: close`; std only.
//!
//! GET `/health` and `/v1/models`, POST `/v1/chat/completions`. Each
//! chat-completions request takes the next scripted response; one the
//! scenario did not expect, or one that fails the response's `expect`, gets
//! HTTP 500 and is recorded with the reason. Every request is recorded in the
//! shape of fakellm's log lines (implementation plan 3.4). A scenario's `web`
//! is served as fakellm serves it: Tavily's answers in order at
//! `/tavily/search` and `/tavily/extract`, and its pages for requests whose
//! Host is not this server's, recorded with their `host`.

use std::collections::VecDeque;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;

/// Request headers kept in the record, as fakellm keeps them.
const LOGGED_HEADERS: [&str; 4] = ["authorization", "content-type", "accept", "user-agent"];

const DEFAULT_MODEL: &str = "qwen3.8-abliterated";

/// One request as the server saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    /// Lowercase names, only those in fakellm's list.
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
    /// Which scripted response it took.
    pub response: Option<usize>,
    pub mismatch: Option<String>,
    /// A page request's Host (a scenario's `web`).
    pub host: Option<String>,
}

impl CapturedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The line fakellm would have logged for it.
    pub fn log_entry(&self) -> Value {
        let headers: serde_json::Map<String, Value> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let mut entry = json!({"method": self.method, "path": self.path, "headers": headers});
        if let Some(host) = &self.host {
            entry["host"] = json!(host);
        }
        if let Some(body) = &self.body {
            entry["body"] = body.clone();
        }
        if let Some(n) = self.response {
            entry["response"] = json!(n);
        }
        if let Some(m) = &self.mismatch {
            entry["mismatch"] = json!(m);
        }
        entry
    }
}

struct State {
    model: String,
    max_model_len: Value,
    responses: VecDeque<Value>,
    served: usize,
    requests: Vec<CapturedRequest>,
    /// The scenario's `web`: pages by host and path, Tavily's answers.
    pages: serde_json::Map<String, Value>,
    search: VecDeque<Value>,
    extract: VecDeque<Value>,
    /// This server's own Host.
    own: String,
}

/// The running server; stopped on drop.
pub struct MockLlm {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl MockLlm {
    /// Serves a scenario's `responses`, `model` and `max_model_len`.
    pub fn start(scenario: &Value) -> io::Result<Self> {
        let state = State {
            model: scenario
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_MODEL)
                .to_string(),
            max_model_len: scenario
                .get("max_model_len")
                .cloned()
                .unwrap_or(json!(65536)),
            responses: scenario
                .get("responses")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into(),
            served: 0,
            requests: Vec::new(),
            pages: scenario["web"]["pages"]
                .as_object()
                .cloned()
                .unwrap_or_default(),
            search: queue(&scenario["web"]["search"]),
            extract: queue(&scenario["web"]["extract"]),
            own: String::new(),
        };
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let mut state = state;
        state.own = addr.to_string();
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(conn) = conn {
                        let state = Arc::clone(&state);
                        std::thread::spawn(move || {
                            let _ = serve(conn, &state);
                        });
                    }
                }
            })
        };
        Ok(Self {
            addr,
            state,
            stop,
            accept: Some(accept),
        })
    }

    /// `http://127.0.0.1:<port>`, the base URL a client is given.
    pub fn uri(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Every request so far, in arrival order.
    pub fn requests(&self) -> Vec<CapturedRequest> {
        lock(&self.state).requests.clone()
    }

    /// The bodies of the chat-completions requests, in order.
    pub fn chat_bodies(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r.response.is_some())
            .filter_map(|r| r.body)
            .collect()
    }

    /// Scripted responses no request has taken yet.
    pub fn unused_responses(&self) -> usize {
        lock(&self.state).responses.len()
    }
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop, which then sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

fn queue(v: &Value) -> VecDeque<Value> {
    v.as_array().cloned().unwrap_or_default().into()
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One HTTP request off the wire: method, path, headers, body.
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_request(conn: &mut TcpStream) -> io::Result<Request> {
    let mut buf = Vec::new();
    let mut scratch = [0u8; 4096];
    let head_end = loop {
        let n = conn.read(&mut scratch)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no request"));
        }
        buf.extend_from_slice(&scratch[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_string();
    let path = first.next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    if body.len() < len {
        let mut rest = vec![0u8; len - body.len()];
        conn.read_exact(&mut rest)?;
        body.extend_from_slice(&rest);
    }
    body.truncate(len);
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

fn serve(mut conn: TcpStream, state: &Mutex<State>) -> io::Result<()> {
    let req = read_request(&mut conn)?;
    let mut captured = CapturedRequest {
        method: req.method.clone(),
        path: req.path.clone(),
        headers: LOGGED_HEADERS
            .iter()
            .filter_map(|name| {
                req.headers
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(k, v)| (k.clone(), v.clone()))
            })
            .collect(),
        body: None,
        response: None,
        mismatch: None,
        host: None,
    };
    let record = |c: CapturedRequest| lock(state).requests.push(c);
    let host = req
        .headers
        .iter()
        .find(|(k, _)| k == "host")
        .map(|(_, v)| v.clone());
    if let Some(host) = host.filter(|h| *h != lock(state).own) {
        let page = lock(state)
            .pages
            .get(&format!("{host}{}", req.path))
            .cloned();
        captured.host = Some(host);
        record(captured);
        return serve_page(&mut conn, page);
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => {
            record(captured);
            respond(&mut conn, 200, "text/plain", b"")
        }
        ("GET", "/v1/models") => {
            let body = {
                let s = lock(state);
                json!({"object": "list", "data": [{
                    "id": s.model, "object": "model", "created": 0, "owned_by": "vllm",
                    "root": s.model, "parent": null, "max_model_len": s.max_model_len}]})
            };
            record(captured);
            respond(
                &mut conn,
                200,
                "application/json",
                body.to_string().as_bytes(),
            )
        }
        ("POST", path) => {
            let Ok(body) = serde_json::from_slice::<Value>(&req.body) else {
                captured.mismatch = Some("body is not JSON".to_string());
                record(captured);
                return respond(&mut conn, 400, "text/plain", b"body is not JSON");
            };
            captured.body = Some(body.clone());
            if let Some(kind) = path
                .strip_prefix("/tavily/")
                .filter(|k| matches!(*k, "search" | "extract"))
            {
                let answer = {
                    let mut s = lock(state);
                    if kind == "search" {
                        s.search.pop_front()
                    } else {
                        s.extract.pop_front()
                    }
                };
                let Some(answer) = answer else {
                    captured.mismatch = Some("no Tavily answer queued".into());
                    record(captured);
                    return respond(&mut conn, 500, "text/plain", b"no Tavily answer queued");
                };
                record(captured);
                let status = answer["status"].as_u64().unwrap_or(200);
                let body = answer.get("body").cloned().unwrap_or(json!({}));
                return respond(
                    &mut conn,
                    status,
                    "application/json",
                    body.to_string().as_bytes(),
                );
            }
            if path != "/v1/chat/completions" {
                captured.mismatch = Some("unknown path".to_string());
                record(captured);
                return respond(&mut conn, 404, "text/plain", b"not found");
            }
            let (index, scripted, model) = {
                let mut s = lock(state);
                let index = s.served;
                s.served += 1;
                (index, s.responses.pop_front(), s.model.clone())
            };
            captured.response = Some(index);
            let problem = match &scripted {
                None => Some("no response queued".to_string()),
                Some(r) => check_expect(r.get("expect"), &body),
            };
            if let Some(problem) = problem {
                let text = format!("fakellm scenario mismatch at request {index}: {problem}");
                captured.mismatch = Some(problem);
                record(captured);
                return respond(&mut conn, 500, "text/plain", text.as_bytes());
            }
            record(captured);
            let scripted = scripted.unwrap_or_default();
            reply(&mut conn, &scripted, &model)
        }
        _ => {
            record(captured);
            respond(&mut conn, 404, "text/plain", b"not found")
        }
    }
}

fn reply(conn: &mut TcpStream, r: &Value, model: &str) -> io::Result<()> {
    if let Some(secs) = r.get("delay").and_then(Value::as_f64) {
        std::thread::sleep(Duration::from_secs_f64(secs));
    }
    if let Some(status) = r.get("status").and_then(Value::as_u64) {
        let body = r.get("body").and_then(Value::as_str).unwrap_or_default();
        return respond(conn, status, "text/plain", body.as_bytes());
    }
    if let Some(text) = r.get("message") {
        let body = json!({
            "id": "chatcmpl-fake", "object": "chat.completion", "created": 0, "model": model,
            "choices": [{"index": 0,
                         "finish_reason": r.get("finish").cloned().unwrap_or(json!("stop")),
                         "message": {"role": "assistant", "content": text}}],
            "usage": r.get("usage").cloned().unwrap_or_else(default_usage)});
        return respond(conn, 200, "application/json", body.to_string().as_bytes());
    }
    conn.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\
          connection: close\r\n\r\n",
    )?;
    let events = r
        .get("stream")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for piece in stream_pieces(model, events) {
        match piece {
            Piece::Text(s) => conn.write_all(s.as_bytes())?,
            Piece::Delay(secs) => {
                conn.flush()?;
                std::thread::sleep(Duration::from_secs_f64(secs));
            }
            Piece::Drop => {
                conn.flush()?;
                return conn.shutdown(Shutdown::Both);
            }
        }
    }
    conn.flush()?;
    conn.shutdown(Shutdown::Write)
}

/// A scenario page, as fakellm serves it: a string is an HTML body; an
/// object has `status`, `type`, `headers` and `body`.
fn serve_page(conn: &mut TcpStream, page: Option<Value>) -> io::Result<()> {
    let Some(page) = page else {
        return respond(conn, 404, "text/plain", b"not found");
    };
    let page = match page {
        Value::String(body) => json!({"body": body}),
        other => other,
    };
    let status = page["status"].as_u64().unwrap_or(200);
    let ctype = page["type"].as_str().unwrap_or("text/html; charset=utf-8");
    let body = page["body"].as_str().unwrap_or_default().as_bytes();
    let mut head = format!("HTTP/1.1 {status} Status\r\ncontent-type: {ctype}\r\n");
    for (k, v) in page["headers"].as_object().into_iter().flatten() {
        head.push_str(&format!("{k}: {}\r\n", v.as_str().unwrap_or_default()));
    }
    head.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    ));
    conn.write_all(head.as_bytes())?;
    conn.write_all(body)?;
    conn.flush()?;
    conn.shutdown(Shutdown::Write)
}

fn respond(conn: &mut TcpStream, status: u64, ctype: &str, body: &[u8]) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n",
        body.len()
    );
    conn.write_all(head.as_bytes())?;
    conn.write_all(body)?;
    conn.flush()?;
    conn.shutdown(Shutdown::Write)
}

fn default_usage() -> Value {
    json!({"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110})
}

/// What a streamed reply is made of.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Text(String),
    Delay(f64),
    /// The connection is cut here, with no `[DONE]`.
    Drop,
}

fn chunk(model: &str, delta: Value, finish: Value) -> Piece {
    let body = json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 0,
                      "model": model,
                      "choices": [{"index": 0, "delta": delta, "logprobs": null,
                                   "finish_reason": finish}]});
    Piece::Text(format!("data: {body}\n\n"))
}

/// The SSE of one streamed reply, chunk by chunk as vLLM 0.27 frames it:
/// the role first, one chunk per scripted delta, the finish reason, the
/// usage, `[DONE]`. Same bytes as fakellm's `stream_events`.
pub fn stream_pieces(model: &str, events: &[Value]) -> Vec<Piece> {
    let mut out = vec![chunk(
        model,
        json!({"role": "assistant", "content": ""}),
        Value::Null,
    )];
    let mut finish = json!("stop");
    let mut usage = default_usage();
    for ev in events {
        if let Some(text) = ev.get("reasoning") {
            out.push(chunk(model, json!({"reasoning": text}), Value::Null));
        } else if let Some(text) = ev.get("content") {
            out.push(chunk(model, json!({"content": text}), Value::Null));
        } else if let Some(tc) = ev.get("tool_call") {
            let index = tc.get("index").cloned().unwrap_or(json!(0));
            out.push(chunk(
                model,
                json!({"tool_calls": [{"index": index, "id": tc["id"], "type": "function",
                                       "function": {"name": tc["name"], "arguments": ""}}]}),
                Value::Null,
            ));
            let parts = match tc.get("argument_parts").and_then(Value::as_array) {
                Some(parts) => parts.clone(),
                None => vec![tc.get("arguments").cloned().unwrap_or(json!(""))],
            };
            for part in parts {
                out.push(chunk(
                    model,
                    json!({"tool_calls": [{"index": index, "function": {"arguments": part}}]}),
                    Value::Null,
                ));
            }
        } else if let Some(f) = ev.get("finish") {
            finish = f.clone();
        } else if let Some(u) = ev.get("usage") {
            usage = u.clone();
        } else if let Some(secs) = ev.get("delay").and_then(Value::as_f64) {
            out.push(Piece::Delay(secs));
        } else if ev.get("drop").is_some() {
            out.push(Piece::Drop);
            return out;
        } else if let Some(raw) = ev.get("raw").and_then(Value::as_str) {
            out.push(Piece::Text(raw.to_string()));
        }
    }
    out.push(chunk(model, json!({"content": ""}), finish));
    let tail = json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 0,
                      "model": model, "choices": [], "usage": usage});
    out.push(Piece::Text(format!("data: {tail}\n\n")));
    out.push(Piece::Text("data: [DONE]\n\n".to_string()));
    out
}

/// How a request offers tools: `auto`, `none` (in the prompt, not callable)
/// or `omit`.
pub fn tools_mode(body: &Value) -> Value {
    if body.get("tools").is_none() {
        return json!("omit");
    }
    body.get("tool_choice").cloned().unwrap_or(json!("auto"))
}

/// The first way `body` differs from a response's `expect`, or None.
fn check_expect(expect: Option<&Value>, body: &Value) -> Option<String> {
    let expect = expect?.as_object()?;
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (key, want) in expect {
        let got = match key.as_str() {
            "stream" => json!(body.get("stream").and_then(Value::as_bool).unwrap_or(false)),
            "tools" => tools_mode(body),
            "last_user" => messages
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .map(|m| m["content"].clone())
                .unwrap_or(Value::Null),
            "last_role" => messages
                .last()
                .map(|m| m["role"].clone())
                .unwrap_or(Value::Null),
            _ => return Some(format!("unknown expect key {key:?}")),
        };
        if &got != want {
            return Some(format!("expected {key}={want}, got {got}"));
        }
    }
    None
}

/// A plain HTTP/1.1 exchange for tests: `(status, body)`. The server closes
/// every connection, so the body is everything after the head.
pub fn http(
    uri: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> io::Result<(u16, Vec<u8>)> {
    http_as(uri, None, method, path, body)
}

/// [`http`] with another Host than the server's: a page of the
/// scenario's `web`.
pub fn http_as(
    uri: &str,
    host: Option<&str>,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> io::Result<(u16, Vec<u8>)> {
    let addr = uri.strip_prefix("http://").unwrap_or(uri);
    let mut conn = TcpStream::connect(addr)?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    let host = host.unwrap_or(addr);
    let head = format!(
        "{method} {path} HTTP/1.1\r\nhost: {host}\r\nauthorization: Bearer fake-key\r\n\
         content-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
        payload.len()
    );
    conn.write_all(head.as_bytes())?;
    conn.write_all(payload.as_bytes())?;
    let mut raw = Vec::new();
    conn.read_to_end(&mut raw)?;
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("no response head"))?;
    let status = String::from_utf8_lossy(&raw[..end])
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other("no status"))?;
    Ok((status, raw[end + 4..].to_vec()))
}

#[cfg(test)]
#[path = "mock_llm_tests.rs"]
mod tests;
