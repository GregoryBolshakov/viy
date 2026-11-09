//! `viy chat` (and `viy` alone, kb d067), `viy resume` and `viy fork`
//! (kb d083): the chat in the terminal UI, or the plain renderer off a
//! terminal, opened whether or not a pod runs and without RUNPOD_API_KEY
//! (kb d041). Turns go where the chat's [`PodServer`] says when they are
//! sent; without a ready pod a line is given back. rpctl's `cmd_chat`, with
//! Codex's ways of naming a saved chat (`codex-rs/tui/src/lib.rs`'s
//! session selection): an id or exact name (one that matches nothing ends
//! viy before the chat opens, in Codex's words), `--last` (none: a new
//! chat), or the picker, which the chat shows as its `/resume` pick.
//!
//! On quit, as rp's chat: where the chat was saved, and the backend's hint
//! while its server still runs (rp says it always; a viy chat may have had
//! no pod at all).

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use viy_agent::Engine;
use viy_agent::OpenSession;
use viy_agent::chat::ChatTurns;
use viy_agent::sessions::LookupError;
use viy_agent::sessions::Sessions;
use viy_agent::sessions::Summary;
use viy_config::Config;
use viy_config::Paths;
use viy_config::Project;
use viy_exec::SystemExec;
use viy_http::HttpClient;
use viy_http::ReqwestHttp;
use viy_pod::backend::Backend;
use viy_pod::state;
use viy_protocol::Op;
use viy_term::history::History;
use viy_time::SystemTimeProvider;

use crate::Failure;
use crate::Terminal;
use crate::dev;
use crate::pod_server::Parts;
use crate::pod_server::PodServer;

/// Which chat opens.
#[derive(Debug, Clone, Default)]
pub struct Opening {
    /// `--url`: the model's address, taken as it is.
    pub url: Option<String>,
    /// `--system`: the new chat's system prompt.
    pub system: Option<String>,
    /// `viy resume` or `viy fork`: the saved chat, or how to pick it.
    pub session: Option<Wanted>,
}

/// A saved chat as `viy resume` and `viy fork` name it.
#[derive(Debug, Clone, Default)]
pub struct Wanted {
    pub id: Option<String>,
    pub last: bool,
    pub all: bool,
    pub include_exec: bool,
    pub fork: bool,
}

/// Runs the chat from `start`.
pub fn cmd_chat(
    opening: &Opening,
    paths: &Paths,
    mut cfg: Config,
    start: &Path,
    out: &mut (dyn Write + Send),
    terminal: Terminal,
) -> Result<(), Failure> {
    // The build tools come with phase 4.
    cfg.set("TOOLS_ENABLED", "0");
    // web_search and fetch_url come with phase 5; offered before then,
    // every call would answer "the tool is off". This is rp's chat with
    // SEARCH_ENABLED=0.
    cfg.set("SEARCH_ENABLED", "0");
    let backend = Backend::resolve(&cfg)?;
    let project = Project::find(start).map_err(|e| format!("{}: {e}", start.display()))?;
    let store = Sessions::new(paths.sessions_dir(), Arc::new(SystemTimeProvider));
    let scope = project.root().display().to_string();
    let open = match &opening.session {
        Some(wanted) => target(&store, wanted, &scope)?,
        None => None,
    };
    // A chat named that is not there is said first, as Codex looks the
    // session up before anything else.
    require_key(backend, &cfg, paths)?;
    // Where rp's chat makes its `.rpctl/`, after the key; with the
    // `.gitignore` `viy init` writes (kb d118).
    let project = Project::find_or_init(start, &paths.home)
        .map_err(|e| format!("{}: {e}", start.display()))?;
    let url = opening.url.clone().or_else(|| {
        std::env::var("VLLM_BASE_URL")
            .ok()
            .filter(|u| !u.is_empty())
    });
    let http: Arc<dyn HttpClient> =
        Arc::new(ReqwestHttp::new().map_err(|e| Failure::from(e.to_string()))?);
    let exe = std::env::current_exe().map_err(|e| Failure::from(format!("viy itself: {e}")))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::from(e.to_string()))?;
    let plain = !terminal.tty || !terminal.stdin_tty;
    let (saved, hint) = runtime.block_on(async {
        let mut chat = dev::chat(
            &cfg,
            url.as_deref().unwrap_or(""),
            Arc::clone(&http),
            paths,
            start,
            true,
        )?;
        if let Some(system) = &opening.system {
            chat.set_system(Some(system.clone()));
        }
        let server = PodServer::new(Parts {
            cfg: cfg.clone(),
            paths: paths.clone(),
            http,
            exec: Arc::new(SystemExec),
            time: Arc::new(SystemTimeProvider),
            exe,
            url,
            desktop: terminal.desktop,
        });
        let turns = Arc::new(ChatTurns::with_server(chat, server));
        // Without a terminal nobody answers, so no mode says yes either.
        let mut engine = Engine::spawn_with(turns.clone(), dev::chat_mode(&cfg), !plain);
        let first: Vec<Op> = open
            .map(|open| Op::OpenSession {
                id: open.id,
                all: open.all,
                include_exec: open.include_exec,
                fork: open.fork,
            })
            .into_iter()
            .collect();
        if plain {
            dev::plain_chat(&mut engine, first, dev::stdin_lines(), &mut *out).await?;
        } else {
            for op in first {
                let _ = engine.submit(op);
            }
            let history = project
                .saved_history()
                .map_or_else(History::default, History::load);
            dev::tui_chat(
                engine,
                history,
                dev::tab_title(&cfg),
                dev::chat_header(&cfg),
            )
            .await?;
        }
        let saved = turns
            .with_chat(|c| c.closed_saved().then(|| c.tab_label(None)))
            .flatten();
        Ok::<_, Failure>((saved, running_hint(backend, paths)))
    })?;
    let say = |out: &mut (dyn Write + Send), text: &str| {
        let text = if terminal.tty {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_string()
        };
        // After SIGHUP the terminal is gone: nothing to tell.
        let _ = writeln!(out, "{text}");
    };
    if let Some(title) = saved {
        say(
            out,
            &format!(
                "chat saved as '{title}'. Resume it with viy resume --last (or viy resume to pick \
                 one)."
            ),
        );
    }
    if let Some(hint) = hint {
        say(out, hint);
    }
    Ok(())
}

/// rpctl's `_require_key`: the chat does not open without the key its
/// endpoint takes (VLLM_API_KEY, or ENDPOINT_API_KEY for `openai`). The
/// RunPod key is another matter: the chat opens without it (kb d041).
fn require_key(backend: Backend, cfg: &Config, paths: &Paths) -> Result<(), Failure> {
    if backend.api_key(cfg).is_empty() {
        return Err(Failure::from(backend.missing_key_hint(paths)));
    }
    Ok(())
}

/// The saved chat `wanted` names: `None` for a new chat (`--last` with
/// none to take), else what the chat opens, a pick when nothing is named.
fn target(store: &Sessions, wanted: &Wanted, scope: &str) -> Result<Option<OpenSession>, Failure> {
    let open = |id: Option<String>| OpenSession {
        id,
        all: wanted.all,
        include_exec: wanted.include_exec,
        fork: wanted.fork,
    };
    if let Some(name) = &wanted.id {
        let action = if wanted.fork { "fork" } else { "resume" };
        return match store.lookup(name) {
            Ok(row) => Ok(Some(open(Some(row.id)))),
            Err(LookupError::Missing) => Err(Failure::from(format!(
                "No saved session found with ID {name}. Run `viy {action}` without an ID to \
                 choose from existing sessions."
            ))),
            Err(LookupError::Ambiguous(a, b)) => Err(Failure::from(format!(
                "Multiple sessions match '{name}' (including {a} and {b}); use a session id to \
                 disambiguate."
            ))),
        };
    }
    if !wanted.last {
        return Ok(Some(open(None)));
    }
    let scope = (!wanted.all).then_some(scope);
    // Codex's fork --last never takes a one-shot chat.
    let take = |row: &Summary| !row.exec || (wanted.include_exec && !wanted.fork);
    Ok(store
        .list(scope)
        .into_iter()
        .find(take)
        .map(|row| open(Some(row.id))))
}

/// The backend's hint on quit, while its server runs: a tracked pod, a
/// local vLLM.
fn running_hint(backend: Backend, paths: &Paths) -> Option<&'static str> {
    let running = match backend {
        Backend::Runpod => state::load(paths).is_ok_and(|s| s.pod_id().is_some()),
        Backend::Local => viy_pod::local::alive(paths).is_some(),
        Backend::Openai => false,
    };
    running.then(|| backend.exit_hint()).flatten()
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
