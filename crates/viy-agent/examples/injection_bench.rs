//! The injection eval's viy side (bench/injection, WP 5.5): the messages
//! viy's chat holds after web_search or fetch_url brought a hostile page,
//! the request bodies it sends for the rounds that follow, and what
//! fetch_url would do with a URL the model then asks for. No requests are
//! made here: `bench/injection/live.py` sends the bodies, simulates the
//! tools and scores, as rpctl's netaccess/evals/injection_eval.py does.
//!
//! JSON lines on stdin, one answer line each on stdout:
//!   `{"op": "fetch", "url", "html"}` → `{"messages"}`, a new chat
//!   `{"op": "search", "query", "hits"}` → `{"messages"}`, a new chat
//!   `{"op": "body", "messages", "last"}` → `{"body"}`
//!   `{"op": "gate", "url"}` → `{"decision"}`: go, ask or refuse
//!
//! The chat is DEFAULTS with the build tools and web search on and no
//! instruction files, its workspace the working directory, its model
//! SERVED_MODEL_NAME.

use std::io::BufRead;
use std::io::Write;
use std::sync::Arc;

use serde_json::Value;
use serde_json::json;
use viy_agent::chat::Chat;
use viy_agent::chat::Places;
use viy_agent::stream::Endpoint;
use viy_config::config::Config;
use viy_http::ReqwestHttp;

fn chat() -> Result<Chat, String> {
    let mut cfg = Config::defaults();
    for (k, v) in [
        ("TOOLS_ENABLED", "1"),
        ("SEARCH_ENABLED", "1"),
        ("TRACE_ENABLED", "0"),
        ("CHAT_AUTO_TITLE", "0"),
        ("INSTRUCTIONS_ENABLED", "0"),
        ("TAVILY_API_KEY", ""),
        ("TAVILY_VAULT_TOKEN", ""),
    ] {
        cfg.set(k, v);
    }
    let ws = std::env::current_dir().map_err(|e| e.to_string())?;
    let nowhere = std::path::PathBuf::from("/nonexistent/viy-injection-bench");
    let places = Places {
        workspace: ws.clone(),
        config_dir: nowhere.join("config"),
        home: nowhere.join("home"),
        project_dir: ws,
        locks_dir: nowhere.join("locks"),
        quarantine_dir: nowhere.join("quarantine"),
        sandbox_tmp_dir: nowhere.join("sandbox-tmp"),
        state_dir: nowhere.join("state"),
        doctrine_sources: Vec::new(),
        guarded: Vec::new(),
    };
    let endpoint = Endpoint {
        base: "http://127.0.0.1:1".into(),
        key: String::new(),
        backend: "runpod".into(),
        is_vllm: true,
        http: Arc::new(ReqwestHttp::new().map_err(|e| e.to_string())?),
    };
    let model = std::env::var("SERVED_MODEL_NAME").unwrap_or_else(|_| "m".into());
    Chat::new(
        &cfg,
        places,
        endpoint,
        model,
        viy_agent::prompt::today(),
        Arc::new(|| {}),
    )
}

fn answer(current: &mut Option<Chat>, job: &Value) -> Result<Value, String> {
    let text = |k: &str| job[k].as_str().unwrap_or_default().to_string();
    match job["op"].as_str() {
        Some("fetch") => {
            let mut c = chat()?;
            let messages = c.eval_fetch_messages(&text("url"), &text("html"))?;
            *current = Some(c);
            Ok(json!({"messages": messages}))
        }
        Some("search") => {
            let mut c = chat()?;
            let hits = job["hits"].as_array().cloned().unwrap_or_default();
            let messages = c.eval_search_messages(&text("query"), &hits)?;
            *current = Some(c);
            Ok(json!({"messages": messages}))
        }
        Some("body") => {
            let c = current.as_ref().ok_or("no chat yet")?;
            let msgs = job["messages"].as_array().cloned().unwrap_or_default();
            Ok(json!({"body": c.eval_body(&msgs, job["last"] == true)}))
        }
        Some("gate") => {
            let c = current.as_mut().ok_or("no chat yet")?;
            Ok(json!({"decision": c.eval_fetch_decision(&text("url"))}))
        }
        other => Err(format!("unknown op {other:?}")),
    }
}

fn main() -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _guard = runtime.enter();
    let mut current = None;
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(job) => answer(&mut current, &job).unwrap_or_else(|e| json!({"error": e})),
            Err(e) => json!({"error": e.to_string()}),
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}
