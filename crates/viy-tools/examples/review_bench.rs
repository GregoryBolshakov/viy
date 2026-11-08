//! The approver bench's live reviewer (bench/approver, WP 4.12): each case
//! on stdin through viy's network review, [`approve::review_with_doctrines`],
//! against a served model, one JSON line per case on stdout, so
//! `bench/approver/live.py` can set it beside rpctl's `approve.review`.
//!
//! The job: `{"doctrines": "<folder>", "cases": [{"id", "user", "command",
//! "files", "doctrine"}]}`. Each case gets a workspace of its own holding
//! its `files`, the network text run_shell gives a proxied command
//! (`Policy::describe`), and, when it names one, the doctrine
//! `<folder>/<doctrine>.md` as the user's own (read as `/doctrines` reads
//! it). The model server comes from the environment: VLLM_BASE_URL,
//! VLLM_API_KEY, SERVED_MODEL_NAME (`viy dev with-model` sets them).
//!
//! Each case gets a new connection, as rpctl's `requests.post` does, so a
//! kept-alive one the server closed meanwhile is never the failure.
//!
//! ```text
//! viy dev with-model -- cargo run -q -p viy-tools --example review_bench < job.json
//! ```

use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;
use viy_http::ReqwestHttp;
use viy_tools::approve;
use viy_tools::approve::Review;
use viy_tools::doctrines;
use viy_tools::egress::Policy;

fn env(name: &str) -> std::io::Result<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            std::io::Error::other(format!("{name} is not set (run under viy dev with-model)"))
        })
}

/// The user's doctrine `name` from `dir`, as the reviewer's policy section.
fn policy(dir: &Path, name: &str, home: &Path) -> Result<Option<String>, String> {
    let global = home.join(".config/viy").join(doctrines::DIR);
    fs::create_dir_all(&global).map_err(|e| e.to_string())?;
    let file = format!("{name}.md");
    fs::copy(dir.join(&file), global.join(&file)).map_err(|e| format!("{file}: {e}"))?;
    let sources = doctrines::sources(
        &home.join(".config/viy"),
        &home.join(".config/rpctl"),
        &home.join("project"),
        home,
    );
    doctrines::policy(&doctrines::load(&sources).doctrines)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn main() -> std::io::Result<()> {
    let (base, key, model) = (
        env("VLLM_BASE_URL")?,
        env("VLLM_API_KEY")?,
        env("SERVED_MODEL_NAME")?,
    );
    let base = base.trim_end_matches('/').to_string();
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let job: Value = serde_json::from_str(&text).map_err(std::io::Error::other)?;
    let doctrine_dir = Path::new(job["doctrines"].as_str().unwrap_or_default()).to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let scratch = std::env::temp_dir().join(format!("review-bench-{}", std::process::id()));
    let mut out = std::io::stdout().lock();
    for (n, case) in job["cases"].as_array().into_iter().flatten().enumerate() {
        let home = scratch.join(n.to_string());
        let ws = home.join("project");
        fs::create_dir_all(&ws)?;
        for (name, body) in case["files"].as_object().into_iter().flatten() {
            let path = ws.join(name);
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(path, body.as_str().unwrap_or_default())?;
        }
        let command = case["command"].as_str().unwrap_or_default();
        let users = strings(&case["user"]);
        let network = Policy::new(command, [], "").describe();
        let policy = match case["doctrine"].as_str() {
            Some(name) => policy(&doctrine_dir, name, &home),
            None => Ok(None),
        };
        let with_doctrine = matches!(policy, Ok(Some(_)));
        let review = Review {
            network: Some(&network),
            ..Review::new(&base, &key, &model, &users, command, &ws)
        };
        let http = ReqwestHttp::new().map_err(|e| std::io::Error::other(e.to_string()))?;
        let started = Instant::now();
        let verdict = runtime.block_on(approve::review_with_doctrines(&review, policy, &http));
        let ms = started.elapsed().as_millis() as u64;
        let line = match verdict {
            Ok((risk, reason)) => json!({"id": case["id"], "risk": risk.as_str(), "reason": reason,
                                         "error": null, "ms": ms, "network": network,
                                         "doctrine": with_doctrine}),
            Err(why) => json!({"id": case["id"], "risk": null, "reason": null, "error": why,
                               "ms": ms, "network": network, "doctrine": with_doctrine}),
        };
        writeln!(out, "{line}")?;
        out.flush()?;
        let _ = fs::remove_dir_all(&home);
    }
    let _ = fs::remove_dir_all(&scratch);
    Ok(())
}
