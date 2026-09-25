//! `viy dev`: developer tools, hidden from `--help`. `viy dev drive
//! SCENARIO --url URL` runs a scenario through the chat against the model
//! server at URL and prints every event as a JSON line; problems of the run
//! go to stderr with exit status 1. As drive_rp's child does for rp, the
//! chat gets rpctl's DEFAULTS plus the scenario's `config`, with
//! `VLLM_API_KEY` set to `fake-key`, and before the first step asks the
//! server for its context length. `--result FILE` writes what the request
//! log is normalized with: the chat's date and the questions asked, and
//! the path of the chat's session log.
//! `--live` drives a real server instead: viy's own config, with the
//! scenario's `config` on top, and the scripted responses unused.
//! `--dir DIR` runs the chat as from DIR. A scripted run refuses
//! instruction files from outside that directory and viy's config
//! directory, as drive_rp refuses them from outside its tree: a scenario
//! never carries the files of the project it was started in.
//!
//! With CHAT_AUTOSAVE on, the chat is saved as a session in viy's sessions
//! directory, as drive_rp saves rp's.
//!
//! The server, the workspace (the working directory) and HOME and the XDG
//! directories are the caller's: `tools/drive_rp.py --client viy` sets them
//! up as for rp (implementation plan 3.4).
//!
//! `viy dev chat --url URL` opens the chat in viy's terminal UI (the shell
//! of WP 3.6) against the model server at URL, with viy's own config and
//! TOOLS_ENABLED off until phase 4 brings the tools; `viy chat` itself
//! waits for the chat without a pod (WP 3.13). Without a terminal, or with
//! `--plain`, it is the plain renderer: one message per line of stdin, and
//! the chat closes once the last has been answered.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use viy_agent::Engine;
use viy_agent::chat::Chat;
use viy_agent::chat::ChatTurns;
use viy_agent::chat::Places;
use viy_agent::drive;
use viy_agent::drive::Scenario;
use viy_agent::sessions::Sessions;
use viy_agent::stream::Endpoint;
use viy_config::Config;
use viy_config::Paths;
use viy_config::Project;
use viy_http::HttpClient;
use viy_http::ReqwestHttp;
use viy_pod::backend::Backend;
use viy_pod::idle;
use viy_protocol::Answer;
use viy_protocol::EventMsg;
use viy_protocol::Op;
use viy_term::app::App;
use viy_term::history::History;
use viy_term::plain::Plain;
use viy_term::tabtitle;
use viy_term::tui::Tui;
use viy_time::SystemTimeProvider;

use crate::Failure;
use crate::Terminal;
use crate::cli::DevArgs;
use crate::cli::DevCommand;

impl DevArgs {
    /// Whether the run takes viy's own config.
    pub fn live(&self) -> bool {
        match &self.command {
            DevCommand::Drive { live, .. } => *live,
            DevCommand::Chat { .. } => true,
        }
    }
}

/// What a scripted run's config starts from: rpctl's DEFAULTS and the key
/// the scenario server takes.
pub fn scripted_base() -> Config {
    let mut cfg = Config::defaults();
    cfg.set("VLLM_API_KEY", "fake-key");
    cfg
}

/// Runs the dev command with `base` as its config.
pub fn run(
    args: &DevArgs,
    paths: &Paths,
    base: Config,
    out: &mut (dyn Write + Send),
    terminal: Terminal,
) -> Result<(), Failure> {
    match &args.command {
        DevCommand::Drive {
            scenario,
            url,
            result,
            dir,
            live,
        } => drive_scenario(
            scenario,
            url,
            result.as_deref(),
            dir.as_deref(),
            *live,
            paths,
            base,
            out,
        ),
        DevCommand::Chat { url, dir, plain } => {
            let plain = *plain || !terminal.tty || !terminal.stdin_tty;
            chat_ui(url, dir.as_deref(), plain, paths, base, out)
        }
    }
}

/// Runs the scenario with `base` under its `config`.
#[allow(clippy::too_many_arguments)]
fn drive_scenario(
    scenario: &Path,
    url: &str,
    result: Option<&Path>,
    dir: Option<&Path>,
    live: bool,
    paths: &Paths,
    base: Config,
    out: &mut (dyn Write + Send),
) -> Result<(), Failure> {
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
    let (run, today, session) = runtime.block_on(async {
        let start = start_dir(dir)?;
        let mut chat = chat(&cfg, url, http, paths, &start)?;
        if !live {
            outside_files(&chat, &start)?;
        }
        chat.discover_max_model_len().await;
        let today = chat.today().to_string();
        let turns = Arc::new(ChatTurns::new(chat));
        let mut engine = Engine::spawn(turns.clone());
        let run = drive::drive(&mut engine, &steps).await;
        let session = turns
            .with_chat(|c| c.session_id())
            .flatten()
            .map(|id| paths.sessions_dir().join(format!("{id}.jsonl")));
        Ok::<_, Failure>((run, today, session))
    })?;
    for event in &run.events {
        let line = serde_json::to_string(event).map_err(|e| Failure::from(e.to_string()))?;
        writeln!(out, "{line}").map_err(|e| Failure::from(e.to_string()))?;
    }
    if let Some(file) = result {
        write_result(file, &today, &run.asked, session.as_deref())?;
    }
    if run.problems.is_empty() {
        Ok(())
    } else {
        Err(Failure::from(run.problems.join("\n")))
    }
}

/// `viy dev chat`: the chat on `url` in the terminal UI, or in the plain
/// renderer with stdin's lines.
fn chat_ui(
    url: &str,
    dir: Option<&Path>,
    plain: bool,
    paths: &Paths,
    mut cfg: Config,
    out: &mut (dyn Write + Send),
) -> Result<(), Failure> {
    cfg.set("TOOLS_ENABLED", "0");
    let http: Arc<dyn HttpClient> =
        Arc::new(ReqwestHttp::new().map_err(|e| Failure::from(e.to_string()))?);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::from(e.to_string()))?;
    let session = runtime.block_on(async {
        let start = start_dir(dir)?;
        let mut chat = chat(&cfg, url, http, paths, &start)?;
        chat.discover_max_model_len().await;
        let turns = Arc::new(ChatTurns::new(chat));
        let mut engine = Engine::spawn(turns.clone());
        if plain {
            plain_chat(&mut engine, stdin_lines(), &mut *out).await?;
        } else {
            let project = Project::find(&start).map_err(|e| Failure::from(e.to_string()))?;
            let tab = tab_title(&cfg);
            tui_chat(engine, History::load(project.history_file()), tab).await?;
        }
        Ok::<_, Failure>(turns.with_chat(|c| c.session_id()).flatten())
    })?;
    if let Some(id) = session {
        writeln!(out, "chat saved: {id}").map_err(|e| Failure::from(e.to_string()))?;
    }
    Ok(())
}

/// Whether the chat sets the tab title (rp's `CHAT_TAB_TITLE`: on only
/// for `1`, its default).
pub(crate) fn tab_title(cfg: &Config) -> bool {
    cfg.get("CHAT_TAB_TITLE").unwrap_or("1") == "1"
}

/// The chat in the terminal UI, the engine's events and the UI's ops
/// passed between them by a task; with `tab`, the tab title too.
async fn tui_chat(mut engine: Engine, history: History, tab: bool) -> Result<(), Failure> {
    let terminal = |e| Failure::from(format!("the terminal: {e}"));
    let mut tui = Tui::init().map_err(terminal)?;
    if tab {
        tui.save_title().map_err(terminal)?;
    }
    let (ops, mut ops_rx) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                op = ops_rx.recv() => {
                    let Some(op) = op else { return };
                    if engine.submit(op).is_err() {
                        return;
                    }
                }
                event = engine.next_event() => {
                    let Some(event) = event else { return };
                    if events_tx.send(event).is_err() {
                        return;
                    }
                }
            }
        }
    });
    let mut app = App::new(ops, history);
    if tab {
        app.tab_title(tabtitle::Encoding::detect());
    }
    let result = viy_term::app::run(&mut tui, &mut app, &mut events).await;
    drop(tui);
    result.map_err(|e| Failure::from(format!("the terminal: {e}")))
}

/// The chat in the plain renderer: each of `lines` is sent (a slash command
/// as one), and once they have run and `lines` has ended, the chat closes.
pub(crate) async fn plain_chat(
    engine: &mut Engine,
    mut lines: mpsc::UnboundedReceiver<String>,
    out: &mut (dyn Write + Send),
) -> Result<(), Failure> {
    let failed = |e: std::io::Error| Failure::from(e.to_string());
    let mut plain = Plain::new(out);
    let mut reading = true;
    // Lines and commands sent that have not finished.
    let mut running = 0usize;
    let mut closing = false;
    loop {
        if !reading && running == 0 && !closing {
            closing = true;
            let _ = engine.submit(Op::Shutdown);
        }
        tokio::select! {
            line = lines.recv(), if reading => match line {
                Some(text) if text.trim().is_empty() => {}
                Some(text) => {
                    let op = if text.starts_with('/') {
                        Op::Command { text }
                    } else {
                        Op::UserTurn { text }
                    };
                    if engine.submit(op).is_ok() {
                        running += 1;
                    }
                }
                None => reading = false,
            },
            event = engine.next_event() => {
                let Some(event) = event else {
                    return Ok(());
                };
                match &event.msg {
                    EventMsg::ShutdownComplete => return Ok(()),
                    EventMsg::TurnComplete(_)
                    | EventMsg::TurnAborted(_)
                    | EventMsg::CommandComplete(_) => running = running.saturating_sub(1),
                    _ => {}
                }
                if let Some(op) = plain.event(&event.msg).map_err(failed)? {
                    let _ = engine.submit(op);
                }
            }
        }
    }
}

/// Stdin's lines, read by a thread.
fn stdin_lines() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else {
                return;
            };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

/// `dir`, or the working directory.
fn start_dir(dir: Option<&Path>) -> Result<std::path::PathBuf, Failure> {
    match dir {
        Some(dir) => Ok(dir.to_path_buf()),
        None => std::env::current_dir()
            .map_err(|e| Failure::from(format!("the working directory: {e}"))),
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
    start: &Path,
) -> Result<Chat, Failure> {
    let backend = Backend::resolve(cfg)?;
    let endpoint = Endpoint {
        base: url.trim_end_matches('/').to_string(),
        key: backend.api_key(cfg).to_string(),
        backend: backend.name().to_string(),
        is_vllm: backend.is_vllm(),
        http,
    };
    let project = Project::find(start).map_err(|e| format!("{}: {e}", start.display()))?;
    let places = Places {
        workspace: viy_agent::prompt::workspace(cfg, project.root(), &paths.home),
        config_dir: paths.config_dir(),
    };
    let sessions = Sessions::new(paths.sessions_dir(), Arc::new(SystemTimeProvider));
    let paths = paths.clone();
    let touch = Arc::new(move || idle::touch(&paths, &SystemTimeProvider, false));
    let mut chat = Chat::new(
        cfg,
        places,
        endpoint,
        backend.model_name(cfg),
        viy_agent::prompt::today(),
        touch,
    )?;
    chat.use_sessions(sessions, Some(project.root().display().to_string()));
    Ok(chat)
}

/// Refuses instruction files from outside `start` and the config directory.
fn outside_files(chat: &Chat, start: &Path) -> Result<(), Failure> {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (start, config_dir) = (real(start), real(&chat.places().config_dir));
    for file in chat.instructions() {
        let path = real(&file.path);
        if !path.starts_with(&start) && !path.starts_with(&config_dir) {
            return Err(Failure::from(format!(
                "an instruction file from outside the scenario would be sent: {} (run from \
                 the scenario's workspace, or give it with --dir)",
                file.path.display()
            )));
        }
    }
    Ok(())
}

/// drive_rp's `result.json`: the date and the questions asked, and the
/// session log the chat was saved to.
fn write_result(
    file: &Path,
    today: &str,
    asked: &[(String, Answer)],
    session: Option<&Path>,
) -> Result<(), Failure> {
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
    let text = json!({"today": today, "asked": asked, "session": session}).to_string();
    std::fs::write(file, text).map_err(|e| Failure::from(format!("{}: {e}", file.display())))
}

#[cfg(test)]
#[path = "dev_tests.rs"]
mod tests;
