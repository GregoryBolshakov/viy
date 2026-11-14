//! What a request to the model carries: rpctl's `_payload` (chat.py
//! 1032-1061), the message repairs it applies, the one-request notices, and
//! the background title request (chat.py 2777-2822, `clean_title`).
//! Messages are JSON objects as the server takes them, `role` and `content`
//! first, with rp's own marks (`thought`, `reasoning`) removed where rp
//! removes them.

use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_config::Config;

/// Sent on a turn's last allowed request (rpctl kb f064, f065).
pub const TOOL_BUDGET_NOTICE: &str = "[viy: tool-call budget for this turn is used up. Do not \
    call any more tools. Give your final answer now from what you already have, and say briefly \
    what you could not verify.]";

/// Sent when a request ended with reasoning but no answer text.
pub const NO_ANSWER_NOTICE: &str = "[viy: your last reply ended without an answer to the \
    user. Do not call any more tools. Give your final answer now from what you already have, and \
    say briefly what you could not verify.]";

/// The title request's prompt; `{user}` and `{assistant}` are filled in.
const TITLE_PROMPT: &str = "Write a title of 3 to 6 words for the conversation below. Reply \
    with the title only: no quotes, no trailing punctuation, no preamble.\n\nUser: {user}\n\n\
    Assistant: {assistant}";

/// How long a title may be, rpctl `sessions.TITLE_CHARS`.
pub const TITLE_CHARS: usize = 60;

/// How a request offers the tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tools {
    /// Offered as usual.
    Auto,
    /// In the prompt, so the prefix cache still hits, but not callable.
    None,
    /// Left out of the request.
    Omit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolProtocol {
    Native,
    Structured,
}

/// The sampling settings and model of a chat's requests.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub model: String,
    /// Whether the server is vLLM, which takes `top_k` and
    /// `chat_template_kwargs`.
    pub is_vllm: bool,
    tool_protocol: ToolProtocol,
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: i64,
    pub reasoning_effort: String,
    pub max_tokens: i64,
}

impl Settings {
    /// From TEMPERATURE, TOP_P, TOP_K, REASONING_EFFORT and MAX_TOKENS; a
    /// value that is not a number is an error, as it ends rp's chat.
    pub fn from_config(cfg: &Config, model: String, is_vllm: bool) -> Result<Self, String> {
        fn number<T: std::str::FromStr>(
            cfg: &Config,
            key: &str,
            default: &str,
        ) -> Result<T, String> {
            let text = cfg.get(key).unwrap_or(default);
            text.trim()
                .parse()
                .map_err(|_| format!("{key}={text:?} is not a number"))
        }
        Ok(Self {
            model,
            is_vllm,
            tool_protocol: ToolProtocol::Native,
            temperature: number(cfg, "TEMPERATURE", "1.0")?,
            top_p: number(cfg, "TOP_P", "0.95")?,
            top_k: number(cfg, "TOP_K", "20")?,
            reasoning_effort: cfg.get("REASONING_EFFORT").unwrap_or("medium").to_string(),
            max_tokens: number(cfg, "MAX_TOKENS", "4096")?,
        })
    }

    /// Selects the constrained action protocol when the running vLLM
    /// version is known to support `structured_outputs`.
    pub(crate) fn observe_vllm_version(&mut self, version: &str) {
        self.tool_protocol = if self.is_vllm && version_at_least(version, (0, 31, 0)) {
            ToolProtocol::Structured
        } else {
            ToolProtocol::Native
        };
    }

    /// A restarted or replaced server must be discovered again.
    pub(crate) fn forget_vllm_version(&mut self) {
        self.tool_protocol = ToolProtocol::Native;
    }

    pub(crate) fn uses_structured_tools(&self) -> bool {
        self.tool_protocol == ToolProtocol::Structured
    }

    /// The body of a streamed chat request. `specs` are the tools this chat
    /// offers; `think: false` turns the reasoning trace off, which changes
    /// only the end of the prompt, so the prefix cache still hits.
    pub fn payload(&self, msgs: &[Value], specs: &[Value], tools: Tools, think: bool) -> Value {
        let structured = self.uses_structured_tools() && !specs.is_empty() && tools != Tools::Omit;
        let messages = with_valid_tool_args(&without_thought(msgs));
        let messages = if structured {
            with_action_contract(messages, specs)
        } else {
            messages
        };
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "temperature": self.temperature,
            "top_p": self.top_p,
            "max_tokens": self.max_tokens,
            "stream_options": {"include_usage": true},
        });
        if self.is_vllm {
            body["top_k"] = json!(self.top_k);
            // preserve_thinking off: the template keeps the reasoning of the
            // turn in progress and drops older turns', which would otherwise
            // fill the context.
            body["chat_template_kwargs"] = if think {
                json!({"reasoning_effort": self.reasoning_effort, "preserve_thinking": false})
            } else {
                json!({"enable_thinking": false, "preserve_thinking": false})
            };
        }
        if structured {
            body["structured_outputs"] = json!({
                "json": action_schema(specs, tools),
                "disable_additional_properties": true,
            });
        } else if !specs.is_empty() && tools != Tools::Omit {
            body["tools"] = Value::Array(specs.to_vec());
            body["tool_choice"] = json!(if tools == Tools::None { "none" } else { "auto" });
        }
        body
    }

    /// The background title request for a chat whose first user message is
    /// `user` and first real reply `reply`.
    pub fn title_body(&self, user: &Value, reply: &str) -> Value {
        let prompt = TITLE_PROMPT
            .replace("{user}", &chars(&message_text(user), 2000))
            .replace("{assistant}", &chars(reply, 2000));
        let mut body = json!({
            "model": self.model,
            "stream": false,
            "max_tokens": 20,
            "temperature": 0.3,
            "messages": [{"role": "user", "content": prompt}],
        });
        if self.is_vllm {
            // Qwen3's template thinks by default; 20 tokens of <think> is no title.
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
        }
        body
    }
}

fn version_at_least(version: &str, minimum: (u64, u64, u64)) -> bool {
    let core = version
        .trim()
        .strip_prefix('v')
        .unwrap_or(version.trim())
        .split(|c| c == '-' || c == '+')
        .next()
        .unwrap_or_default();
    let mut parts = core.split('.');
    let Some(major) = parts.next().and_then(|n| n.parse().ok()) else {
        return false;
    };
    let Some(minor) = parts.next().and_then(|n| n.parse().ok()) else {
        return false;
    };
    let Some(patch) = parts.next().and_then(|n| n.parse().ok()) else {
        return false;
    };
    (major, minor, patch) >= minimum
}

fn with_action_contract(mut messages: Vec<Value>, specs: &[Value]) -> Vec<Value> {
    let definitions = Value::Array(
        specs
            .iter()
            .filter_map(|spec| spec.get("function").cloned())
            .collect(),
    );
    let contract = format!(
        "Tool protocol: reply with exactly one JSON object and no text outside it. \
         Return a final answer as {{\"kind\":\"final\",\"content\":\"...\"}}. \
         To call one or more tools, return \
         {{\"kind\":\"tool_calls\",\"calls\":[{{\"name\":\"tool_name\",\
         \"arguments\":{{...}}}}]}}. Each call's arguments must satisfy that tool's schema. \
         Tool definitions: {definitions}"
    );
    if let Some(system) = messages
        .iter_mut()
        .find(|message| message["role"].as_str() == Some("system"))
    {
        match &mut system["content"] {
            Value::String(content) => {
                content.push_str("\n\n");
                content.push_str(&contract);
            }
            Value::Array(parts) => {
                parts.push(json!({"type": "text", "text": contract}));
            }
            content => *content = json!(contract),
        }
    } else {
        messages.insert(0, json!({"role": "system", "content": contract}));
    }
    messages
}

fn final_action_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {"const": "final"},
            "content": {"type": "string"}
        },
        "required": ["kind", "content"],
        "additionalProperties": false
    })
}

fn action_schema(specs: &[Value], tools: Tools) -> Value {
    let calls: Vec<Value> = specs
        .iter()
        .filter_map(|spec| {
            let function = spec.get("function")?;
            let name = function.get("name")?.as_str()?;
            let arguments = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            Some(json!({
                "type": "object",
                "properties": {
                    "name": {"const": name},
                    "arguments": arguments
                },
                "required": ["name", "arguments"],
                "additionalProperties": false
            }))
        })
        .collect();
    let final_answer = final_action_schema();
    if tools == Tools::None || calls.is_empty() {
        return final_answer;
    }
    json!({
        "oneOf": [
            final_answer,
            {
                "type": "object",
                "properties": {
                    "kind": {"const": "tool_calls"},
                    "calls": {
                        "type": "array",
                        "minItems": 1,
                        "items": {"oneOf": calls}
                    }
                },
                "required": ["kind", "calls"],
                "additionalProperties": false
            }
        ]
    })
}

/// The first `n` characters of `text`.
fn chars(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// A message's text without its images, rpctl `sessions.message_text` with
/// `with_tools=False`.
pub fn message_text(m: &Value) -> String {
    match &m["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .map(|p| p["text"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// The first message that may name a chat: the first user message, and the
/// first assistant reply with text that is not a bracketed placeholder.
pub fn title_pair(messages: &[Value]) -> Option<(&Value, &str)> {
    let user = messages.iter().find(|m| m["role"] == "user")?;
    let reply = messages.iter().find_map(|m| {
        let text = m["content"].as_str()?;
        (m["role"] == "assistant" && !text.trim().is_empty() && !text.starts_with('['))
            .then_some(text)
    })?;
    Some((user, reply))
}

/// A model's title reply, reduced to one plain line, or `None`.
pub fn clean_title(text: &str) -> Option<String> {
    let text = without_think(text);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let line = strip_title_label(line);
    let line = line
        .trim()
        .trim_matches(|c| "*#`\"'“”‘’ ".contains(c))
        .trim_end_matches(['.', '!', ':', ';', ','])
        .trim();
    if line.is_empty() {
        return None;
    }
    if line.chars().count() > TITLE_CHARS {
        let cut: String = line.chars().take(TITLE_CHARS - 1).collect();
        return Some(format!("{}…", cut.trim_end()));
    }
    Some(line.to_string())
}

/// `text` without its `<think>` blocks; one never closed runs to the end.
fn without_think(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "<think>".len()..];
        match after.find("</think>") {
            Some(end) => rest = &after[end + "</think>".len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// `line` without a leading `Title:`, in any case.
fn strip_title_label(line: &str) -> &str {
    let Some(rest) = line
        .get(..5)
        .filter(|head| head.eq_ignore_ascii_case("title"))
        .and_then(|_| line.get(5..))
    else {
        return line;
    };
    let rest = rest.trim_start();
    match rest.strip_prefix(':') {
        Some(after) => after.trim_start(),
        None => line,
    }
}

/// True when a tool call's arguments are a JSON object, which vLLM parses
/// every past call's arguments into before rendering the template.
pub fn args_ok(args: &Value) -> bool {
    args.as_str()
        .and_then(|a| serde_json::from_str::<Value>(a).ok())
        .is_some_and(|v| v.is_object())
}

/// `msgs` with every past tool call's arguments a JSON object: a reply cut
/// off at max_tokens mid-call leaves unterminated JSON, and vLLM then
/// refuses every later request with HTTP 400. Such arguments become `{}`;
/// the call's tool result already says it did not run.
pub fn with_valid_tool_args(msgs: &[Value]) -> Vec<Value> {
    msgs.iter()
        .map(|m| {
            let Some(calls) = m.get("tool_calls").and_then(Value::as_array) else {
                return m.clone();
            };
            if calls.iter().all(|tc| args_ok(&tc["function"]["arguments"])) {
                return m.clone();
            }
            let mut m = m.clone();
            if let Some(calls) = m["tool_calls"].as_array_mut() {
                for tc in calls {
                    if !args_ok(&tc["function"]["arguments"]) {
                        tc["function"]["arguments"] = json!("{}");
                    }
                }
            }
            m
        })
        .collect()
}

/// `msgs` without the `thought` marks, which are for /resume, not the
/// server.
pub fn without_thought(msgs: &[Value]) -> Vec<Value> {
    msgs.iter().map(|m| without_key(m, "thought")).collect()
}

/// `msgs` minus the reasoning carried through a turn's tool rounds: once the
/// turn is over the template no longer renders it. What stays is `thought`,
/// so /resume can show that the model thought there.
pub fn without_reasoning(msgs: &[Value]) -> Vec<Value> {
    msgs.iter()
        .map(|m| {
            if m.get("reasoning").is_none() {
                return m.clone();
            }
            let mut m = without_key(m, "reasoning");
            m["thought"] = json!(true);
            m
        })
        .collect()
}

fn without_key(m: &Value, key: &str) -> Value {
    match m {
        Value::Object(map) if map.contains_key(key) => Value::Object(
            map.iter()
                .filter(|(k, _)| *k != key)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Map<_, _>>(),
        ),
        _ => m.clone(),
    }
}

/// A copy of `msgs` with `notice` appended to the last message, for one
/// request only: the history keeps the message as it was.
pub fn with_notice(msgs: &[Value], notice: &str) -> Vec<Value> {
    let mut out = msgs.to_vec();
    if let Some(last) = out.last_mut() {
        let tail = format!("\n\n{notice}");
        match &mut last["content"] {
            Value::Array(parts) => parts.push(json!({"type": "text", "text": tail})),
            content => {
                let text = content.as_str().unwrap_or_default();
                *content = json!(format!("{text}{tail}"));
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "payload_tests.rs"]
mod tests;
