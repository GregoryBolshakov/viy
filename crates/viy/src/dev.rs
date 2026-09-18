//! `viy dev`: developer tools, hidden from `--help`. `viy dev drive
//! SCENARIO --url URL` runs a scenario through the chat against the model
//! server at URL and prints every event as a JSON line; problems of the run
//! go to stderr with exit status 1. As drive_rp's child does for rp, the
//! chat gets rpctl's DEFAULTS plus the scenario's `config`, with
//! `VLLM_API_KEY` set to `fake-key`, and before the first step asks the
//! server for its context length. `--result FILE` writes what the request
//! log is normalized with: the chat's date and the questions asked.
//! `--live` drives a real server instead: viy's own config, with the
//! scenario's `config` on top, and the scripted responses unused.
//!
//! The server, the workspace (the working directory) and HOME and the XDG
//! directories are the caller's: `tools/drive_rp.py --client viy` sets them
//! up as for rp (implementation plan 3.4).

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use serde_json::json;
use viy_agent::Engine;
use viy_agent::chat::Chat;
use viy_agent::chat::ChatTurns;
use viy_agent::drive;
use viy_agent::drive::Scenario;
use viy_agent::stream::Endpoint;
use viy_config::Config;
use viy_config::Paths;
use viy_http::HttpClient;
use viy_http::ReqwestHttp;
use viy_pod::backend::Backend;
use viy_pod::idle;
use viy_protocol::Answer;
use viy_time::SystemTimeProvider;

use crate::Failure;
use crate::cli::DevArgs;
use crate::cli::DevCommand;

impl DevArgs {
    /// Whether the run takes viy's own config.
    pub fn live(&self) -> bool {
        let DevCommand::Drive { live, .. } = &self.command;
        *live
    }
}

/// What a scripted run's config starts from: rpctl's DEFAULTS and the key
/// the scenario server takes.
pub fn scripted_base() -> Config {
    let mut cfg = Config::defaults();
    cfg.set("VLLM_API_KEY", "fake-key");
    cfg
}

/// Runs the scenario with `base` under its `config`.
pub fn run(
    args: &DevArgs,
    paths: &Paths,
    base: Config,
    out: &mut (dyn Write + Send),
) -> Result<(), Failure> {
    let DevCommand::Drive {
        scenario,
        url,
        result,
        ..
    } = &args.command;
    let shown = scenario.display();
    let text = std::fs::read_to_string(scenario).map_err(|e| format!("{shown}: {e}"))?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("{shown}: {e}"))?;
    let steps = Scenario::from_json(&value).map_err(|e| format!("{shown}: {e}"))?;
    let cfg = config(base, &value).map_err(|e| format!("{shown}: {e}"))?;
    let http: Arc<dyn HttpClient> =
        Arc::new(ReqwestHttp::new().map_err(|e| Failure::from(e.to_string()))?);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::from(e.to_string()))?;
    let (run, today) = runtime.block_on(async {
        let mut chat = chat(&cfg, url, http, paths)?;
        chat.discover_max_model_len().await;
        let today = chat.today().to_string();
        let mut engine = Engine::spawn(Arc::new(ChatTurns::new(chat)));
        Ok::<_, Failure>((drive::drive(&mut engine, &steps).await, today))
    })?;
    for event in &run.events {
        let line = serde_json::to_string(event).map_err(|e| Failure::from(e.to_string()))?;
        writeln!(out, "{line}").map_err(|e| Failure::from(e.to_string()))?;
    }
    if let Some(file) = result {
        write_result(file, &today, &run.asked)?;
    }
    if run.problems.is_empty() {
        Ok(())
    } else {
        Err(Failure::from(run.problems.join("\n")))
    }
}

/// `cfg` under the scenario's `config`.
fn config(mut cfg: Config, scenario: &Value) -> Result<Config, String> {
    for (key, value) in scenario["config"].as_object().into_iter().flatten() {
        let value = value
            .as_str()
            .ok_or_else(|| format!("config {key}: not a string"))?;
        cfg.set(key, value);
    }
    if cfg.get("TOOLS_ENABLED") == Some("1") {
        return Err("build tools come with phase 4; the scenario needs TOOLS_ENABLED \"0\"".into());
    }
    Ok(cfg)
}

/// The chat at `url`, the backend's key and model, streaming touching the
/// activity file under `paths`.
fn chat(
    cfg: &Config,
    url: &str,
    http: Arc<dyn HttpClient>,
    paths: &Paths,
) -> Result<Chat, Failure> {
    let backend = Backend::resolve(cfg)?;
    let endpoint = Endpoint {
        base: url.trim_end_matches('/').to_string(),
        key: backend.api_key(cfg).to_string(),
        backend: backend.name().to_string(),
        is_vllm: backend.is_vllm(),
        http,
    };
    let paths = paths.clone();
    let touch = Arc::new(move || idle::touch(&paths, &SystemTimeProvider, false));
    Ok(Chat::new(
        cfg,
        endpoint,
        backend.model_name(cfg),
        viy_agent::prompt::today(),
        touch,
    )?)
}

/// drive_rp's `result.json`: the date and the questions asked.
fn write_result(file: &Path, today: &str, asked: &[(String, Answer)]) -> Result<(), Failure> {
    let asked: Vec<Value> = asked
        .iter()
        .map(|(question, answer)| {
            let answer = match answer {
                Answer::Yes => "y",
                Answer::No => "n",
                Answer::Always => "a",
            };
            json!({"question": question, "answer": answer})
        })
        .collect();
    let text = json!({"today": today, "asked": asked}).to_string();
    std::fs::write(file, text).map_err(|e| Failure::from(format!("{}: {e}", file.display())))
}

#[cfg(test)]
#[path = "dev_tests.rs"]
mod tests;
