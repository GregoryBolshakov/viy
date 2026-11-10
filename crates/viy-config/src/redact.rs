//! Masks API tokens before a chat is written to disk, when CHAT_REDACT=1
//! (rpctl's `redact.py`).
//!
//! Two sources of truth: the token formats providers publish (prefix and
//! length), and the literal values of secrets this process can see: viy's
//! own config keys and environment variables whose names say they hold a
//! secret. The second catches tokens with no recognisable shape.
//!
//! Lossy by design: a resumed chat sees the mask, not the token. That is why
//! it is opt-in.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Captures;
use regex::Regex;
use serde_json::Value;

use crate::Config;

pub const MASK: &str = "[redacted]";

/// The token formats, which netguard also refuses to send out. Word
/// boundaries are ASCII: a token is ASCII, and a boundary that any other
/// character satisfies masks wherever rpctl's Unicode one does, and more
/// (kb d042).
pub static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
        r"(?-u:\b)sk-[A-Za-z0-9_\-]{20,}", // OpenAI, Anthropic, OpenRouter, vLLM
        r"(?-u:\b)hf_[A-Za-z0-9]{30,}",    // Hugging Face
        r"(?-u:\b)npm_[A-Za-z0-9]{36,}",   // npm
        r"(?-u:\b)gh[pousr]_[A-Za-z0-9]{36,}", // GitHub
        r"(?-u:\b)github_pat_[A-Za-z0-9_]{50,}",
        r"(?-u:\b)rpa_[A-Za-z0-9]{30,}",          // RunPod
        r"(?-u:\b)apify_api_[A-Za-z0-9]{30,}",    // Apify
        r"(?-u:\b)tvly-[A-Za-z0-9\-]{20,}",       // Tavily
        r"(?-u:\b)xai-[A-Za-z0-9]{40,}",          // xAI
        r"(?-u:\b)cio[A-Za-z0-9]{32}(?-u:\b)",    // crates.io
        r"(?-u:\b)AKIA[0-9A-Z]{16}(?-u:\b)",      // AWS access key id
        r"(?-u:\b)xox[abprs]-[A-Za-z0-9\-]{10,}", // Slack
        r"(?-u:\b)eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}", // JWT
    ]
    .into_iter()
    .map(compile_regex)
    .collect()
});

/// Header-shaped secrets keep their label, so the text still reads sensibly.
static LABELLED: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r"(?i)(?-u:\b)(authorization:\s*bearer\s+|x-api-key:\s*)[A-Za-z0-9._~+/\-]{16,}=*",
    )
});

static SECRET_NAME: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"(?i)(KEY|TOKEN|SECRET|PASSWORD|PASSWD|_PAT|CREDENTIALS?)$"));

/// Shorter values (ports, flags) would mask ordinary text.
const MIN_LITERAL: usize = 12;

fn compile_regex(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(err) => panic!("invalid regex pattern `{pattern}`: {err}"),
    }
}

/// Whether a config key or environment variable's name says it holds a
/// secret.
pub fn is_secret_name(name: &str) -> bool {
    SECRET_NAME.is_match(name)
}

/// The values to mask verbatim: those of secret-named keys among `vars`
/// (the config's, then the environment's), trimmed, at least 12 characters,
/// longest first, so a value containing another is masked whole.
pub fn literal_secrets<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    let values: BTreeSet<&str> = vars
        .into_iter()
        .filter(|(name, _)| is_secret_name(name))
        .map(|(_, value)| value.trim())
        .filter(|value| value.chars().count() >= MIN_LITERAL)
        .collect();
    let mut values: Vec<String> = values.into_iter().map(str::to_string).collect();
    values.sort_by_key(|value| Reverse(value.chars().count()));
    values
}

/// [`literal_secrets`] of `cfg` and this process's environment (rp's
/// `literal_secrets(cfg)`).
pub fn config_secrets(cfg: &Config) -> Vec<String> {
    // A name that is not UTF-8 is still read for its ending, as rpctl
    // reads it; a value that is not UTF-8 can't occur in a chat's text.
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(name, value)| {
            Some((
                name.to_string_lossy().into_owned(),
                value.into_string().ok()?,
            ))
        })
        .collect();
    let env = env
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()));
    literal_secrets(cfg.iter().chain(env))
}

#[derive(Debug, Clone, Default)]
pub struct Redactor {
    literals: Vec<String>,
}

impl Redactor {
    /// Masks the token formats and, verbatim, each of `literals`, in their
    /// order.
    pub fn new(literals: impl IntoIterator<Item = String>) -> Self {
        Self {
            literals: literals.into_iter().filter(|v| !v.is_empty()).collect(),
        }
    }

    /// Masks the secrets in `cfg` and in this process's environment.
    pub fn from_config(cfg: &Config) -> Self {
        Self::new(config_secrets(cfg))
    }

    /// `s` with every secret masked. An inline image (`data:`) is base64,
    /// not prose, and is left alone.
    pub fn text(&self, s: &str) -> String {
        if s.starts_with("data:") {
            return s.to_string();
        }
        let mut s = s.to_string();
        for literal in &self.literals {
            if s.contains(literal.as_str()) {
                s = s.replace(literal.as_str(), MASK);
            }
        }
        if let Cow::Owned(masked) =
            LABELLED.replace_all(&s, |caps: &Captures| format!("{}{MASK}", &caps[1]))
        {
            s = masked;
        }
        for pattern in PATTERNS.iter() {
            if let Cow::Owned(masked) = pattern.replace_all(&s, MASK) {
                s = masked;
            }
        }
        s
    }

    /// A copy of `value` with every string in it masked; object keys stay.
    pub fn value(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.value(v)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.value(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

#[cfg(test)]
#[path = "redact_tests.rs"]
mod tests;
