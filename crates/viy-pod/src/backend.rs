//! Where the model is hosted, from the `BACKEND` key: rpctl's `backend.py`.
//! The commands and the chat ask a [`Backend`] for the endpoint, its key
//! and model, and what to say, and run the lifecycle verbs for it:
//!
//! - `runpod`: vLLM on a RunPod GPU pod, the default ([`crate::commands`]);
//! - `openai`: a hosted OpenAI-compatible endpoint (OpenRouter, Together,
//!   Ollama, a vLLM someone else runs): viy only talks to it, there is
//!   nothing to bring up or tear down;
//! - `local`: vLLM on this machine, started and stopped by viy
//!   ([`crate::local`]).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use viy_config::Config;
use viy_config::Paths;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;

use crate::commands::CommandError;
use crate::commands::Env;
use crate::local;
use crate::notice::Reporter;
use crate::notice::Tone;
use crate::notice::say;
use crate::tracked;
use crate::tunnel;
use crate::wait::failure_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Runpod,
    Openai,
    Local,
}

impl Backend {
    /// The one `BACKEND` names, in any case, empty meaning RunPod; else
    /// rpctl's message.
    pub fn resolve(cfg: &Config) -> Result<Self, String> {
        let kind = cfg.get("BACKEND").unwrap_or("").trim().to_lowercase();
        match kind.as_str() {
            "" | "runpod" => Ok(Self::Runpod),
            "openai" => Ok(Self::Openai),
            "local" => Ok(Self::Local),
            _ => Err(format!(
                "unknown BACKEND '{kind}'; supported: local, openai, runpod"
            )),
        }
    }

    /// Its `BACKEND` value.
    pub fn name(self) -> &'static str {
        match self {
            Self::Runpod => "runpod",
            Self::Openai => "openai",
            Self::Local => "local",
        }
    }

    /// Whether it has a server whose lifecycle viy manages: up, down.
    pub fn manages_server(self) -> bool {
        self != Self::Openai
    }

    /// Whether the server is vLLM, which takes vLLM-only request fields
    /// (`chat_template_kwargs`) a hosted API may refuse.
    pub fn is_vllm(self) -> bool {
        self != Self::Openai
    }

    /// Said when a chat ends.
    pub fn exit_hint(self) -> Option<&'static str> {
        match self {
            Self::Runpod => {
                Some("Pod is still running. viy stop to pause billing, viy down to terminate.")
            }
            Self::Openai => None,
            Self::Local => Some("Local vLLM is still running. viy down stops it."),
        }
    }

    /// The bearer token the endpoint expects.
    pub fn api_key(self, cfg: &Config) -> &str {
        let key = match self {
            Self::Openai => "ENDPOINT_API_KEY",
            _ => "VLLM_API_KEY",
        };
        cfg.get(key).unwrap_or("")
    }

    /// The model id requests name.
    pub fn model_name(self, cfg: &Config) -> String {
        let served = || cfg.get("SERVED_MODEL_NAME").unwrap_or("").to_string();
        match self {
            Self::Openai => match cfg.get("ENDPOINT_MODEL").unwrap_or("").trim() {
                "" => served(),
                model => model.to_string(),
            },
            _ => served(),
        }
    }

    /// Why there is no API key yet, and how to get one.
    pub fn missing_key_hint(self, paths: &Paths) -> String {
        match self {
            Self::Openai => format!(
                "ENDPOINT_API_KEY is not set. Add it to {} (or the active model profile); for \
                 an endpoint without auth, any placeholder value works.",
                paths.config_file().display()
            ),
            _ => "VLLM_API_KEY is missing. Run viy up first.".into(),
        }
    }

    /// The OpenAI-compatible base URL, without `/v1`, or `None` when the
    /// model is not reachable. `discover`: RunPod may be asked for the pod.
    pub async fn base_url(
        self,
        env: &Env<'_>,
        cfg: &Config,
        discover: bool,
        report: Reporter<'_>,
    ) -> io::Result<Option<String>> {
        match self {
            Self::Runpod => {
                let warn = |many: tracked::MultiplePods| say(report, Tone::Warn, many.to_string());
                tracked::base_url(env.paths, cfg, Arc::clone(env.http), discover, warn).await
            }
            Self::Openai => Ok(endpoint_url(cfg)),
            Self::Local => Ok(local::base_url(env, cfg).await),
        }
    }

    /// Advisory lines to say before a chat; never fatal.
    pub async fn preflight(self, env: &Env<'_>, cfg: &Config) -> io::Result<Vec<String>> {
        match self {
            Self::Runpod => tunnel::session_warnings(&env.ssh(cfg)).await,
            Self::Openai => {
                let url = endpoint_url(cfg).unwrap_or_default();
                Ok(if url.starts_with("http://") && !is_loopback(&url) {
                    vec![format!(
                        "ENDPOINT_URL is plain http: prompts, replies and the API key cross the \
                         network unencrypted ({url})."
                    )]
                } else {
                    Vec::new()
                })
            }
            Self::Local => Ok(Vec::new()),
        }
    }
}

/// rpctl's `NotSupported`: `viy <verb>` for a backend without it.
pub fn not_supported(backend: Backend, verb: &str) -> CommandError {
    CommandError::Exit(format!(
        "`viy {verb}` is not available for the '{}' backend.",
        backend.name()
    ))
}

// ------------------------------------------------------------------- openai

/// ENDPOINT_URL without a trailing `/` or `/v1`: the chat appends
/// `/v1/...`. `None` when it is not set.
fn endpoint_url(cfg: &Config) -> Option<String> {
    let url = cfg.get("ENDPOINT_URL").unwrap_or("").trim();
    if url.is_empty() {
        return None;
    }
    let url = url.trim_end_matches('/');
    Some(url.strip_suffix("/v1").unwrap_or(url).to_string())
}

/// Whether `url` names this machine: localhost, `::1` or 127.x.
fn is_loopback(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    }
    .to_lowercase();
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// `viy up` with the openai backend: nothing to start.
pub fn openai_up(report: Reporter<'_>) {
    say(
        report,
        Tone::Plain,
        "The 'openai' backend is a hosted endpoint: there is nothing to start. viy status checks \
         that it answers.",
    );
}

/// `viy status` with the openai backend: the endpoint and model, and
/// whether `/v1/models` answers and lists the model.
pub async fn openai_status(
    env: &Env<'_>,
    cfg: &Config,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let Some(url) = endpoint_url(cfg) else {
        return Err(CommandError::Exit(
            "ENDPOINT_URL is not set (BACKEND=openai).".into(),
        ));
    };
    let print = |text: String| say(report, Tone::Plain, text);
    let backend = Backend::Openai;
    let model = backend.model_name(cfg);
    print("  backend:   openai (hosted endpoint, not managed by viy)".into());
    print(format!("  endpoint:  {url}"));
    print(format!("  model:     {model}"));
    let request = Request::new(Method::GET, format!("{url}/v1/models"))
        .header("User-Agent", user_agent::RP_REQUESTS)
        .header("Accept", "*/*")
        .header("Authorization", format!("Bearer {}", backend.api_key(cfg)))
        .timeouts(Duration::from_secs(15), Duration::from_secs(15));
    let response = match viy_http::send(env.http.as_ref(), request).await {
        Ok(response) => response,
        Err(error) => {
            print(format!("  reachable: no ({})", failure_name(&error)));
            return Ok(());
        }
    };
    if response.status != 200 {
        print(format!("  reachable: HTTP {}", response.status));
        return Ok(());
    }
    let listed: Vec<String> = response
        .bytes()
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|reply| reply.get("data").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .map(|m| {
            m.get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let shown = if listed.is_empty() {
        ""
    } else if listed.contains(&model) {
        "; model listed"
    } else {
        "; model NOT in /v1/models"
    };
    print(format!("  reachable: yes{shown}"));
    Ok(())
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
