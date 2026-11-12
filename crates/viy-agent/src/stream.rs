//! One request to the model and its streamed reply: rpctl's `_stream_once`
//! and `_consume` (chat.py 1098-1256). The SSE body is read line by line as
//! rp's `iter_lines` does: `data: ` lines only, `[DONE]` ends it, a line
//! that is not JSON is skipped. Reasoning comes as `reasoning` (vLLM 0.27)
//! or `reasoning_content` (older builds, other servers); tool calls are
//! reassembled by index; the last usage and finish reason are kept. Every
//! chunk touches the pod's activity file through the caller's `touch`, as
//! rp's `idle.touch()`: a streaming chat keeps the pod from being reaped.
//!
//! A failed request says why in notices (rp's red and dim lines) and gives
//! `None`, as rp's `_stream_once` does.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use viy_http::HttpClient;
use viy_http::HttpError;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;
use viy_protocol::DeltaEvent;
use viy_protocol::EventMsg;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;

/// Where the model is: the backend's base URL, key and kind, as
/// `viy_pod::backend::Backend` gives them (viy-agent sits below viy-pod).
#[derive(Clone)]
pub struct Endpoint {
    /// Without a trailing `/`.
    pub base: String,
    pub key: String,
    /// The backend's name: `runpod`, `openai` or `local`.
    pub backend: String,
    pub is_vllm: bool,
    pub http: Arc<dyn HttpClient>,
}

impl Endpoint {
    /// A request to `path` with rp's headers.
    pub fn request(&self, method: Method, path: &str) -> Request {
        Request::new(method, format!("{}{path}", self.base))
            .header("User-Agent", user_agent::RP_REQUESTS)
            .header("Accept", "*/*")
            .header("Authorization", format!("Bearer {}", self.key))
    }

    /// Whether the base URL is the local end of the SSH tunnel.
    fn is_loopback(&self) -> bool {
        self.base.starts_with("http://127.0.0.1") || self.base.starts_with("http://localhost")
    }
}

/// A tool call reassembled from its deltas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// One round's reply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Round {
    pub text: String,
    pub reasoning: String,
    /// In index order; only those that got a name.
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Value>,
    pub finish: Option<String>,
    /// When the first reasoning or answer text arrived.
    pub first_token: Option<tokio::time::Instant>,
}

/// What one SSE line held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Reasoning(String),
    Content(String),
}

/// The accumulators of a reply being read.
#[derive(Debug, Default)]
pub struct Reply {
    reasoning: String,
    calls: BTreeMap<u64, ToolCall>,
    usage: Option<Value>,
    finish: Option<String>,
    first_token: Option<tokio::time::Instant>,
    done: bool,
}

impl Reply {
    /// Takes one line of the body: `None` when it is not a chunk, else what
    /// it carried to show. The text is the caller's (`answer`), so what
    /// streamed before an interruption is still there.
    pub fn line(&mut self, line: &str, answer: &mut String) -> Option<Vec<Piece>> {
        let data = line.strip_prefix("data: ")?;
        if data == "[DONE]" {
            self.done = true;
            return None;
        }
        let chunk: Value = serde_json::from_str(data).ok()?;
        if truthy(&chunk["usage"]) {
            self.usage = Some(chunk["usage"].clone());
        }
        let mut pieces = Vec::new();
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            if let Some(finish) = choice["finish_reason"].as_str().filter(|f| !f.is_empty()) {
                self.finish = Some(finish.to_string());
            }
            let delta = &choice["delta"];
            for tc in delta["tool_calls"].as_array().into_iter().flatten() {
                let slot = self
                    .calls
                    .entry(tc["index"].as_u64().unwrap_or(0))
                    .or_default();
                if let Some(id) = nonempty(&tc["id"]) {
                    slot.id = id.to_string();
                }
                if let Some(name) = nonempty(&tc["function"]["name"]) {
                    slot.name = name.to_string();
                }
                if let Some(arguments) = nonempty(&tc["function"]["arguments"]) {
                    slot.arguments.push_str(arguments);
                }
            }
            if let Some(text) =
                nonempty(&delta["reasoning"]).or(nonempty(&delta["reasoning_content"]))
            {
                self.reasoning.push_str(text);
                pieces.push(Piece::Reasoning(text.to_string()));
            }
            if let Some(text) = nonempty(&delta["content"]) {
                answer.push_str(text);
                pieces.push(Piece::Content(text.to_string()));
            }
        }
        if !pieces.is_empty() && self.first_token.is_none() {
            self.first_token = Some(tokio::time::Instant::now());
        }
        Some(pieces)
    }

    /// The round, with `answer` as its text.
    pub fn finish(self, answer: &str) -> Round {
        Round {
            text: answer.to_string(),
            reasoning: self.reasoning,
            tool_calls: self
                .calls
                .into_values()
                .filter(|c| !c.name.is_empty())
                .collect(),
            usage: self.usage,
            finish: self.finish,
            first_token: self.first_token,
        }
    }
}

fn structured_action(text: &str) -> Result<(String, Vec<ToolCall>), String> {
    let action: Value =
        serde_json::from_str(text).map_err(|error| format!("invalid JSON: {error}"))?;
    match action["kind"].as_str() {
        Some("final") => {
            let content = action["content"]
                .as_str()
                .ok_or_else(|| "final action has no string content".to_string())?;
            Ok((content.to_string(), Vec::new()))
        }
        Some("tool_calls") => {
            let calls = action["calls"]
                .as_array()
                .filter(|calls| !calls.is_empty())
                .ok_or_else(|| "tool_calls action has no calls".to_string())?;
            let mut out = Vec::with_capacity(calls.len());
            for call in calls {
                let name = call["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| "tool call has no name".to_string())?;
                let arguments = call["arguments"]
                    .as_object()
                    .ok_or_else(|| format!("tool call {name:?} arguments are not an object"))?;
                out.push(ToolCall {
                    id: String::new(),
                    name: name.to_string(),
                    arguments: serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into()),
                });
            }
            Ok((String::new(), out))
        }
        Some(kind) => Err(format!("unknown action kind {kind:?}")),
        None => Err("action has no kind".into()),
    }
}

fn finish_reply(
    reply: Reply,
    answer: &mut String,
    structured: bool,
    sink: &Sink<'_>,
) -> Option<Round> {
    let mut round = reply.finish(answer);
    if !structured {
        return Some(round);
    }
    let (text, calls) = match structured_action(&round.text) {
        Ok(action) => action,
        Err(error) => {
            answer.clear();
            sink.notice(
                NoticeLevel::Error,
                "invalid_structured_output",
                format!("invalid structured model response: {error}"),
            );
            return None;
        }
    };
    answer.clone_from(&text);
    round.text = text;
    round.tool_calls = calls;
    if !round.text.is_empty() {
        (sink.emit)(EventMsg::AgentMessageContentDelta(DeltaEvent {
            delta: round.text.clone(),
        }));
    }
    Some(round)
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}

/// Python's truthiness of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// How a round reports: events to the UI, and the pod's activity file.
pub struct Sink<'a> {
    pub emit: &'a (dyn Fn(EventMsg) + Send + Sync),
    pub touch: &'a (dyn Fn() + Send + Sync),
}

impl Sink<'_> {
    fn notice(&self, level: NoticeLevel, code: &str, message: impl Into<String>) {
        (self.emit)(EventMsg::Notice(NoticeEvent {
            level,
            code: code.into(),
            message: message.into(),
        }));
    }
}

/// Sends `body` to `/v1/chat/completions` and reads the streamed reply;
/// the content goes into `answer` as it arrives. `None` when the request
/// failed or the connection was lost, after notices saying so.
pub async fn stream_once(
    endpoint: &Endpoint,
    body: &Value,
    sink: &Sink<'_>,
    answer: &mut String,
) -> Option<Round> {
    let structured = body.get("structured_outputs").is_some();
    answer.clear();
    let request = endpoint
        .request(Method::POST, "/v1/chat/completions")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(body).unwrap_or_default())
        .timeouts(Duration::from_secs(15), Duration::from_secs(600));
    let mut response = match viy_http::send(endpoint.http.as_ref(), request).await {
        Ok(response) => response,
        Err(error) => {
            sink.notice(
                NoticeLevel::Error,
                "connection_failed",
                format!("connection failed: {}", detail(&error)),
            );
            // A loopback base URL means the traffic goes through the SSH
            // forward, and a dead forward looks exactly like a dead pod.
            if endpoint.is_loopback() {
                sink.notice(
                    NoticeLevel::Info,
                    "tunnel_hint",
                    "The endpoint is the local end of the SSH tunnel. Check it with: viy tunnel \
                     --status",
                );
            }
            sink.notice(
                NoticeLevel::Info,
                "server_hint",
                "Is the server up? viy status",
            );
            return None;
        }
    };
    if response.status != 200 {
        let status = response.status;
        let bytes = response.bytes().await.unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        let shown: String = text.chars().take(600).collect();
        sink.notice(
            NoticeLevel::Error,
            "http_error",
            format!("HTTP {status}: {shown}"),
        );
        let hint = if status == 401 && endpoint.backend == "openai" {
            Some("The endpoint rejected ENDPOINT_API_KEY.")
        } else if status == 401 {
            Some(
                "VLLM_API_KEY in .env does not match the running server. Restart it with viy \
                 down && viy up",
            )
        } else if status == 400
            && body.get("tools").is_some()
            && text.to_lowercase().contains("tool")
            && endpoint.is_vllm
        {
            Some(
                "The pod was launched without --tool-call-parser. Recreate it (viy down && viy \
                 up) or use /search off",
            )
        } else {
            None
        };
        if let Some(hint) = hint {
            sink.notice(NoticeLevel::Info, "http_error_hint", hint);
        }
        return None;
    }
    let mut reply = Reply::default();
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                // The pod or the tunnel went away mid-reply: the chat goes
                // on, and what streamed so far stays in `answer`.
                sink.notice(
                    NoticeLevel::Error,
                    "connection_lost",
                    format!("connection lost mid-reply: {}", class_name(&error)),
                );
                if endpoint.is_loopback() {
                    sink.notice(
                        NoticeLevel::Info,
                        "tunnel_hint",
                        "Check the pod and the tunnel: viy status",
                    );
                }
                if structured {
                    answer.clear();
                }
                return None;
            }
        };
        pending.extend(chunk);
        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            if take_line(&mut reply, &line, sink, answer, !structured) {
                return finish_reply(reply, answer, structured, sink);
            }
        }
    }
    if !pending.is_empty() {
        take_line(&mut reply, &pending, sink, answer, !structured);
    }
    finish_reply(reply, answer, structured, sink)
}

/// Feeds one raw line to `reply`, showing what it carried; true at `[DONE]`.
fn take_line(
    reply: &mut Reply,
    line: &[u8],
    sink: &Sink<'_>,
    answer: &mut String,
    show_content: bool,
) -> bool {
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches(['\n', '\r']);
    if let Some(pieces) = reply.line(line, answer) {
        // Streaming keeps the pod alive (viy reap).
        (sink.touch)();
        for piece in pieces {
            match piece {
                Piece::Reasoning(delta) => {
                    (sink.emit)(EventMsg::ReasoningContentDelta(DeltaEvent { delta }));
                }
                Piece::Content(delta) if show_content => {
                    (sink.emit)(EventMsg::AgentMessageContentDelta(DeltaEvent { delta }));
                }
                Piece::Content(_) => {}
            }
        }
    }
    reply.done
}

/// What went wrong, without the variant's own prefix.
fn detail(error: &HttpError) -> String {
    match error {
        HttpError::Timeout => "timeout".into(),
        HttpError::Connect(text) | HttpError::Network(text) | HttpError::Build(text) => {
            text.clone()
        }
    }
}

/// The error as rp names `requests`' exception classes (kb d066).
fn class_name(error: &HttpError) -> &'static str {
    match error {
        HttpError::Timeout => "Timeout",
        HttpError::Build(_) => "InvalidURL",
        HttpError::Connect(_) | HttpError::Network(_) => "ConnectionError",
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
