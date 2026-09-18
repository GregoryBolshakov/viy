//! Context accounting: rpctl's `_measure`, `_tool_schema_chars`,
//! `_estimate_tokens`, `_calibrate` and `_prompt_budget` (chat.py
//! 2421-2495). The whole conversation is resent on every turn, so the prompt
//! grows until it reaches the server's max-model-len; this estimate lets the
//! chat see that coming. Chars-per-token is only a seed: it is recalibrated
//! against the `prompt_tokens` the server reports after every turn, so it
//! converges on what this tokenizer does with this user's mix of prose and
//! code.

use serde::Serialize;
use serde_json::Value;

/// The seed ratio.
pub const DEFAULT_CHARS_PER_TOKEN: f64 = 3.6;

/// An image is thousands of tokens but a few characters of message JSON, so
/// it is counted apart: a deliberately generous flat guess (Qwen-VL scales
/// tokens with resolution).
pub const IMAGE_TOKEN_COST: u64 = 1600;

/// The chat template's framing around every message.
pub const PER_MESSAGE_TOKEN_OVERHEAD: u64 = 4;

/// The least the history may be given, whatever /maxtok says.
const MIN_BUDGET: u64 = 1024;

/// (characters, images) of one message's content: a string, or the parts
/// of a message with images.
fn content_chars(content: &Value) -> (u64, u64) {
    match content {
        Value::String(text) => (chars(text), 0),
        Value::Array(parts) => {
            parts
                .iter()
                .filter(|p| p.is_object())
                .fold((0, 0), |(text, images), part| {
                    if part["type"] == "text" {
                        (
                            text + chars(part["text"].as_str().unwrap_or_default()),
                            images,
                        )
                    } else {
                        (text, images + 1)
                    }
                })
        }
        _ => (0, 0),
    }
}

fn chars(text: &str) -> u64 {
    text.chars().count() as u64
}

/// (characters, images) across `msgs`, reasoning and tool-call names and
/// arguments included: a call's arguments are prompt text on the next round,
/// and a long write_file call is mostly arguments.
pub fn measure(msgs: &[Value]) -> (u64, u64) {
    msgs.iter().fold((0, 0), |(text, images), m| {
        let (c, i) = content_chars(&m["content"]);
        let calls: u64 = m["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tc| {
                let f = &tc["function"];
                chars(f["name"].as_str().unwrap_or_default())
                    + chars(f["arguments"].as_str().unwrap_or_default())
            })
            .sum();
        let reasoning = chars(m["reasoning"].as_str().unwrap_or_default());
        (text + c + reasoning + calls, images + i)
    })
}

/// Characters of the tool schemas as the chat template renders them
/// (`tojson`: `", "` and `": "` between items, text as is).
pub fn schema_chars(specs: &[Value]) -> u64 {
    if specs.is_empty() {
        return 0;
    }
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, Spaced);
    if Value::Array(specs.to_vec()).serialize(&mut ser).is_err() {
        return 0;
    }
    chars(&String::from_utf8_lossy(&out))
}

/// JSON with a space after each `,` and `:`.
struct Spaced;

impl serde_json::ser::Formatter for Spaced {
    fn begin_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        writer.write_all(b": ")
    }
}

/// The chat's running estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimator {
    pub chars_per_token: f64,
}

impl Default for Estimator {
    fn default() -> Self {
        Self {
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
        }
    }
}

impl Estimator {
    /// Approximate prompt tokens for `msgs` plus `schema_chars` of tool
    /// schemas (0 to size a part of the history alone).
    pub fn tokens(&self, msgs: &[Value], schema_chars: u64) -> u64 {
        let (text, images) = measure(msgs);
        let estimate = (text + schema_chars) as f64 / self.chars_per_token
            + (images * IMAGE_TOKEN_COST) as f64
            + (msgs.len() as u64 * PER_MESSAGE_TOKEN_OVERHEAD) as f64;
        estimate as u64
    }

    /// Corrects chars-per-token from a real `prompt_tokens` for `msgs`
    /// (sent with `schema_chars` of schemas). Skipped for prompts with
    /// images, whose cost is a guess; for samples too small to mean
    /// anything; and for implausible ratios. Smoothed, so one turn does not
    /// lurch it.
    pub fn calibrate(&mut self, msgs: &[Value], schema_chars: u64, prompt_tokens: Option<i64>) {
        let Some(prompt_tokens) = prompt_tokens.filter(|n| *n > 0) else {
            return;
        };
        let (text, images) = measure(msgs);
        if images > 0 {
            return;
        }
        let chars = text + schema_chars;
        let text_tokens = prompt_tokens - (msgs.len() as u64 * PER_MESSAGE_TOKEN_OVERHEAD) as i64;
        if text_tokens < 200 || chars < 200 {
            return;
        }
        let ratio = chars as f64 / text_tokens as f64;
        if !(ratio > 1.0 && ratio < 12.0) {
            return;
        }
        self.chars_per_token += 0.35 * (ratio - self.chars_per_token);
    }
}

/// Tokens the conversation may occupy before a reply no longer fits: vLLM
/// refuses a request whose prompt plus max_tokens exceeds the context.
pub fn prompt_budget(max_model_len: u64, max_tokens: i64) -> u64 {
    let left = i128::from(max_model_len) - i128::from(max_tokens);
    u64::try_from(left).unwrap_or(0).max(MIN_BUDGET)
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
