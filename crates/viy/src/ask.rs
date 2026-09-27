//! `viy ask` (rpctl `cmd_ask`): one prompt, for scripting. The answer goes
//! to stdout through the plain renderer; the chat is kept only with
//! `--save`, which says so on stderr, so a script piping the answer gets
//! only the answer.

use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use viy_agent::Engine;
use viy_agent::chat::ChatTurns;
use viy_config::Config;
use viy_config::Paths;
use viy_exec::SystemExec;
use viy_http::HttpClient;
use viy_http::ReqwestHttp;
use viy_pod::backend::Backend;
use viy_pod::commands::Env;
use viy_pod::notice::Report;
use viy_protocol::EventMsg;
use viy_protocol::Op;
use viy_term::plain::Plain;
use viy_time::SystemTimeProvider;

use crate::Failure;
use crate::cli::AskArgs;
use crate::dev;

/// Runs `viy ask`.
pub fn cmd_ask(
    args: &AskArgs,
    paths: &Paths,
    mut cfg: Config,
    start: &Path,
    out: &mut (dyn Write + Send),
) -> Result<(), Failure> {
    let prompt = match &args.prompt {
        Some(prompt) => prompt.clone(),
        None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| Failure::from(format!("stdin: {e}")))?;
            text
        }
    };
    if args.save {
        // `--save` keeps the chat whatever CHAT_AUTOSAVE says, as rp's.
        cfg.set("CHAT_AUTOSAVE", "1");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::from(e.to_string()))?;
    let http: Arc<dyn HttpClient> =
        Arc::new(ReqwestHttp::new().map_err(|e| Failure::from(e.to_string()))?);
    runtime.block_on(async {
        let url = base_url(&cfg, args.url.as_deref(), paths, &http, out).await?;
        let mut chat = dev::chat(&cfg, &url, Arc::clone(&http), paths, start, args.save)?;
        if let Some(system) = &args.system {
            chat.set_system(Some(system.clone()));
        }
        chat.end_with("ask");
        let turns = Arc::new(ChatTurns::new(chat));
        let mut engine = Engine::spawn(turns.clone());
        let mut plain = Plain::new(&mut *out).show_think(args.think);
        let failed = |e: std::io::Error| Failure::from(e.to_string());
        let turn = engine
            .submit(Op::UserTurn { text: prompt })
            .map_err(|e| Failure::from(e.to_string()))?;
        while let Some(event) = engine.next_event().await {
            match &event.msg {
                EventMsg::ShutdownComplete => break,
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) if event.id == turn => {
                    let _ = engine.submit(Op::Shutdown);
                }
                // A chat's opening line; rp's ask says nothing of it.
                EventMsg::Notice(n) if n.code == "context_length" => continue,
                _ => {}
            }
            if let Some(op) = plain.event(&event.msg).map_err(failed)? {
                let _ = engine.submit(op);
            }
        }
        let out = plain.into_inner();
        out.flush().map_err(|e| Failure::from(e.to_string()))?;
        if let Some((true, Some(id))) = turns.with_chat(|c| (c.saved(), c.session_id())) {
            eprintln!("saved as chat {id}; viy chat -c continues it");
        }
        Ok(())
    })
}

/// Where the model answers (rp's `resolve_base_url`): `--url`, else
/// VLLM_BASE_URL, else the backend's; its advisory lines first.
pub(crate) async fn base_url(
    cfg: &Config,
    url: Option<&str>,
    paths: &Paths,
    http: &Arc<dyn HttpClient>,
    out: &mut (dyn Write + Send),
) -> Result<String, Failure> {
    if let Some(url) = url {
        return Ok(url.to_string());
    }
    if let Ok(url) = std::env::var("VLLM_BASE_URL")
        && !url.is_empty()
    {
        return Ok(url);
    }
    let backend = Backend::resolve(cfg)?;
    let exe = std::env::current_exe().map_err(|e| Failure::from(format!("viy itself: {e}")))?;
    let env = Env {
        paths,
        exec: &SystemExec,
        http,
        time: &SystemTimeProvider,
        exe: &exe,
    };
    let warn = |report: Report| {
        if let Report::Notice(notice) = report {
            eprintln!("{}", notice.text);
        }
    };
    let found = backend
        .base_url(&env, cfg, true, &warn)
        .await
        .map_err(|e| Failure::from(e.to_string()))?;
    let Some(url) = found else {
        return Err(Failure::from(match backend {
            Backend::Openai => "ENDPOINT_URL is not set (BACKEND=openai).".to_string(),
            Backend::Runpod => "No pod is running. Start one with:  viy up".to_string(),
            Backend::Local => "No server is running. Start one with:  viy up".to_string(),
        }));
    };
    for line in backend.preflight(&env, cfg).await.unwrap_or_default() {
        let _ = writeln!(out, "{line}");
    }
    Ok(url)
}

#[cfg(test)]
#[path = "ask_tests.rs"]
mod tests;
