//! The chat's turn: rpctl's `Chat.send` and `_send` (chat.py 2699-3058)
//! for a chat whose tools cannot run yet, the background title
//! (`_maybe_start_title`, `_apply_title`), the context accounting around it
//! (`_context_state`, the calibration after a turn, `last_stats`) and the
//! max-model-len discovery (`_discover_max_model_len`), done before the
//! first request a ready server gets (kb d041). [`ChatTurns`] runs it for
//! the engine. A chat given a session store always keeps its session log
//! in line with the history, as Codex records every session (rpctl's
//! `_checkpoint`; rp's CHAT_AUTOSAVE is not kept, kb d083): the user's
//! message is on disk before the reply, and the turn when it ends, however
//! it ends. Only `viy exec --ephemeral` runs a chat without one. `/new`,
//! `/resume`, `/fork` and `/rename` are its methods here, their commands in
//! `commands`.
//!
//! A turn sends the system prompt, the history and the user's message; a
//! reply without an answer (reasoning only, or a tool call) is asked for
//! once more with [`NO_ANSWER_NOTICE`] and no tools, a reply cut at the
//! token limit is not; the answer, or a bracketed placeholder, closes the
//! turn, and the reasoning is dropped from the history (rpctl kb d008).
//! A request that fails keeps only what had streamed; a turn interrupted
//! keeps the user's message and what had streamed, marked `[interrupted]`.
//!
//! Old tool output is elided once the history passes TOOL_ELIDE_THRESHOLD,
//! and a turn that would not fit compacts the history first ([`compact`]).
//!
//! Each turn is watched by a [`TurnMonitor`] ([`crate::trace`]): its
//! notes go into the tool results the model reads, its alarms are said as
//! notices, and its row is written however the turn ends.

use crate::filehistory::FileHistory;
use crate::files::FileTools;
use crate::files::Outcome;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use viy_tools::approve::is_py_space;

use serde_json::Value;
use serde_json::json;
use tokio::task::JoinHandle;
use viy_config::Config;
use viy_config::redact::Redactor;
use viy_http::Method;
use viy_protocol::Answer;
use viy_protocol::BackgroundEvent;
use viy_protocol::BackgroundState;
use viy_protocol::BackgroundTask;
use viy_protocol::Choices;
use viy_protocol::EffortChangedEvent;
use viy_protocol::EventMsg;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::RoundKind;
use viy_protocol::RoundStartedEvent;
use viy_protocol::TitleChangedEvent;
use viy_protocol::TokenCountEvent;
use viy_protocol::TurnOutcome;

use crate::context::Estimator;
use crate::context::prompt_budget;
use crate::context::schema_chars;
use crate::engine::BoxFuture;
use crate::engine::TurnContext;
use crate::engine::Turns;
use crate::instructions;
use crate::payload::NO_ANSWER_NOTICE;
use crate::payload::Settings;
use crate::payload::TOOL_BUDGET_NOTICE;
use crate::payload::Tools;
use crate::payload::clean_title;
use crate::payload::title_pair;
use crate::payload::with_notice;
use crate::payload::without_reasoning;
use crate::prompt::SystemPrompt;
use crate::server::Server;
use crate::sessions;
use crate::sessions::SessionError;
use crate::sessions::SessionLog;
use crate::sessions::Sessions;
use crate::sessions::default_title;
use crate::stream::Endpoint;
use crate::stream::Round;
use crate::stream::Sink;
use crate::stream::stream_once;
use crate::trace::Severity;
use crate::trace::Tracer;
use crate::trace::TurnMonitor;

/// Keeps the pod alive while a reply streams: `viy_pod::idle::touch`.
pub type Touch = Arc<dyn Fn() + Send + Sync>;

/// Where a chat looks for files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Places {
    /// The workspace ([`crate::prompt::workspace`]).
    pub workspace: PathBuf,
    /// viy's config directory, for global instruction files.
    pub config_dir: PathBuf,
    /// The user's home, for `~`.
    pub home: PathBuf,
    /// The project folder (its `.viy`/`.rpctl` marker), for the file tools'
    /// root-file rules.
    pub project_dir: PathBuf,
    /// Where the workspace locks live: rpctl's state folder, shared with rp.
    pub locks_dir: PathBuf,
    /// Where quarantined paths go.
    pub quarantine_dir: PathBuf,
    /// Where each chat's sandbox /tmp is made (`<state>/sandbox-tmp`).
    pub sandbox_tmp_dir: PathBuf,
    /// viy's own state folder: the chats' traces, turns and alarms
    /// ([`crate::trace`]).
    pub state_dir: PathBuf,
    /// Where the network reviewer's doctrines are read from, in order
    /// ([`viy_tools::doctrines::sources`]).
    pub doctrine_sources: Vec<viy_tools::doctrines::Source>,
    /// Besides the workspace and project, what a mount must neither hold nor
    /// sit inside: the clients' code, both tools' config, state and data.
    pub guarded: Vec<PathBuf>,
    /// The global and project toolbox folders ([`crate::toolbox`]).
    pub toolbox: crate::toolbox::Dirs,
    /// Where the model's own git repositories live, encrypted, and are
    /// mounted ([`viy_tools::sandboxgit`]).
    pub sandbox_git: viy_tools::sandboxgit::Places,
}

/// One conversation with the model.
pub struct Chat {
    /// For what is re-read per request (the instruction files).
    cfg: Config,
    places: Places,
    /// INSTRUCTIONS_ENABLED (`/instructions on|off`).
    instructions_on: bool,
    endpoint: Endpoint,
    settings: Settings,
    touch: Touch,
    messages: Vec<Value>,
    /// The user's own system prompt (`/system`).
    system: Option<String>,
    search_on: bool,
    fetch_on: bool,
    guide_on: bool,
    /// SEARCH_MAX_ROUNDS: tool rounds a turn may take.
    max_rounds: u64,
    /// CHAT_AUTO_TITLE; the title is the session's, so a chat without one
    /// gets none.
    auto_title: bool,
    /// Every chat's log, and the project this chat is filed under.
    sessions: Option<Sessions>,
    project: Option<String>,
    /// This chat's log; shared with the background title, which writes the
    /// title as soon as it has one.
    session: Arc<Mutex<Option<SessionLog>>>,
    /// CHAT_REDACT: what masks the log.
    redactor: Option<Redactor>,
    /// Said once until a write succeeds again.
    autosave_failed: bool,
    /// What compactions summarised away of a resumed chat, for its replay.
    earlier: Vec<Value>,
    /// The system prompt's date, fixed at the chat's start.
    today: String,
    max_model_len: u64,
    /// Whether this server was asked for its context length.
    discovered: bool,
    estimator: Estimator,
    /// AUTO_COMPACT, and AUTO_COMPACT_THRESHOLD as a fraction of the budget.
    auto_compact: bool,
    auto_compact_threshold: f64,
    /// TOOL_ELIDE_THRESHOLD and TOOL_ELIDE_KEEP_TURNS ([`compact`]).
    elide_threshold: f64,
    elide_keep_turns: usize,
    /// ask_claude's settings, which `/consult` changes per chat.
    consult: consult::ConsultSettings,
    /// EXPLORE_ENABLED, SYSTEM_GUIDE_TRUST_EXPLORE, and explore's limits.
    explore_on: bool,
    guide_trust_explore: bool,
    explore: workflows::ExploreSettings,
    /// COMPACT_TRIM_TARGET, COMPACT_BACKGROUND_AT, COMPACT_CONSOLIDATE_AT.
    fractions: compact::Fractions,
    /// Set after a failed auto-compaction, so one broken pod does not mean
    /// a failed pass in front of every message; cleared once back under.
    compact_failed: bool,
    /// The summary being written in the background, if any.
    background: Option<compact::Background>,
    stats: Option<TurnStats>,
    /// The background title request.
    title_job: Option<JoinHandle<()>>,
    /// The turn in progress: its user message, until the turn closes.
    inflight: Option<Vec<Value>>,
    /// Images for the next message (`/image`): data URIs or URLs.
    pending_images: Vec<String>,
    /// Why the session ends when the chat closes: `exit`, or `exec` for
    /// `viy exec`.
    end_reason: &'static str,
    /// What the round in progress has streamed of its answer.
    partial: String,
    /// The file tools, with the mounts and what this chat has read.
    pub(crate) files: FileTools,
    /// What write_file overwrote, for `/rewind`, while the chat has no
    /// session log (a saved chat keeps it in the log's `<id>.files/`).
    mem_files: FileHistory,
    /// TOOLS_ENABLED: the build tools are offered.
    pub(crate) tools_on: bool,
    /// TOOLS_MAX_ROUNDS: tool rounds a turn may take with them on.
    tools_max_rounds: u64,
    /// TOOLS_SHELL_TIMEOUT, in seconds.
    pub(crate) shell_timeout: u64,
    /// TOOLS_PARALLEL: a read-only batch of tool calls runs side by side.
    pub(crate) parallel_tools: bool,
    /// SANDBOX_NET, `host` or `proxy`.
    sandbox_net: String,
    /// SANDBOX_NET_ALLOW: hosts every proxied command may reach.
    pub(crate) sandbox_net_allow: String,
    /// Refusals in a row this turn; three stop it.
    denials: usize,
    /// Why the turn was stopped, once it was.
    abort_turn: Option<String>,
    /// Whether bubblewrap works here, probed at the first command.
    sandbox_check: Option<Result<(), String>>,
    /// This turn's user message, for the reviewer.
    turn_user: Option<Value>,
    /// Whether this server was asked if it takes tool calls.
    probed: bool,
    /// The sandbox's /tmp, made at the first sandboxed command and removed
    /// when the chat's session ends.
    pub(crate) sandbox_tmp: viy_tools::sandbox::ChatTmp,
    /// The program that hands a proxied sandbox's port over (rpctl's
    /// launcher; tests put a failing one here).
    pub(crate) sandbox_launcher: viy_tools::sandbox::Launcher<'static>,
    /// What the next user message tells the model about the mounts.
    pub(crate) mount_note: Option<String>,
    /// Why network commands need the user's yes: internet text, or files
    /// from a mounted folder, in this chat (rp's `web_taint`).
    pub(crate) web_taint: Option<String>,
    /// web_search's client, the pages read, and which URLs and hosts the
    /// user, a search or a page vouched for ([`web`]).
    pub(crate) web: web::Web,
    /// Whether the log had been written when its session last ended.
    closed_saved: bool,
    /// The chat's trace and alarms (TRACE_ENABLED), following its session.
    tracer: Tracer,
    /// The turn in progress' monitor, until its row is written.
    monitor: Option<TurnMonitor>,
    /// Turns this chat has run, for the trace.
    turn_n: u64,
    /// `viy dev drive --rounds`: every main-chat reply, for the benches.
    rounds_log: Option<std::fs::File>,
    /// TOOLBOX_ENABLED (`/toolbox on|off`).
    toolbox_on: bool,
    /// The toolbox as this chat's system prompt shows it (rp's
    /// `_toolbox_snap`), frozen until the next fresh, resumed or compacted
    /// history: the system prompt heads every request, and a tool kept
    /// mid-chat would otherwise re-prefill the whole history (134 s at
    /// 234k tokens, rpctl kb f047). find_tool and run_shell read the live
    /// toolbox.
    toolbox_snap: Vec<crate::toolbox::Tool>,
    /// TOOLBOX_INDEX_MAX: the index's lines.
    toolbox_index_max: usize,
    /// SANDBOX_GIT: the model's own .git in the sandbox
    /// ([`viy_tools::sandboxgit`]).
    sandbox_git_on: bool,
    /// Whether this chat offers it, decided once before the first request
    /// with tools (run_shell's description says so, and a change re-prefills
    /// the history); `/git init` decides again.
    sbgit_offer: Option<bool>,
    /// The repository once prepared, for the sandbox's `git_dir`.
    sbgit: Option<PathBuf>,
    /// Why preparing it failed: .git stays read-only until `/git` retries.
    sbgit_failed: Option<String>,
    /// This process counts as a user of the mount.
    sbgit_used: bool,
    /// tok, which keeps the repositories' key.
    tok: viy_config::vault::Tok,
    /// Servers the user lent with `/ssh` ([`viy_tools::remote`]); only
    /// `/ssh` adds one, and nothing in config or the chat can.
    pub(crate) ssh_hosts: Vec<viy_tools::remote::Host>,
    /// The note the next user message carries about them.
    pub(crate) ssh_note: Option<String>,
    /// This machine's ssh tools at grant time, and ssh itself.
    pub(crate) ssh_machine: Arc<dyn viy_tools::remote::Machine>,
    pub(crate) ssh_runner: Arc<dyn viy_tools::remote::Ssh>,
    /// The name last sent as [`EventMsg::TitleChanged`]; empty at first,
    /// as the frontend starts with no name. Shared with the background
    /// title request, which sends the title it gets.
    label_sent: Arc<Mutex<String>>,
}

/// How long closing, switching or forking a chat waits for its title.
const TITLE_WAIT: Duration = Duration::from_secs(5);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Chat {
    /// A chat with `model` at `endpoint`, its settings from `cfg`; `today`
    /// is the date its system prompt carries.
    pub fn new(
        cfg: &Config,
        places: Places,
        endpoint: Endpoint,
        model: String,
        today: String,
        touch: Touch,
    ) -> Result<Self, String> {
        let on = |key: &str, default: &str| cfg.get(key).unwrap_or(default) == "1";
        let max_rounds = cfg.get("SEARCH_MAX_ROUNDS").unwrap_or("4");
        let sandbox_tmp = viy_tools::sandbox::ChatTmp::new(places.sandbox_tmp_dir.clone());
        let mut files = FileTools::new(
            places.workspace.clone(),
            places.project_dir.clone(),
            places.locks_dir.clone(),
            places.quarantine_dir.clone(),
        );
        files.write_requires_read = on("WRITE_REQUIRES_READ", "1");
        files.max_output = cfg
            .get("TOOLS_MAX_OUTPUT")
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(20000);
        files.clients = viy_tools::guard::client_paths();
        let session: Arc<Mutex<Option<SessionLog>>> = Arc::default();
        let tracer = {
            let session = Arc::clone(&session);
            Tracer::new(
                places.state_dir.clone(),
                on("TRACE_ENABLED", "1"),
                Arc::new(move || lock(&session).as_ref().map(|log| log.id().to_string())),
            )
        };
        let web = web::Web::new(cfg, web::WebNet::system(Arc::clone(&endpoint.http)));
        let elide = compact::elide_settings(
            cfg.get("TOOL_ELIDE_THRESHOLD"),
            cfg.get("TOOL_ELIDE_KEEP_TURNS"),
        );
        let consult = consult::ConsultSettings::from_config(cfg, &places.home);
        let ssh_home = places.home.clone();
        let toolbox_on = on("TOOLBOX_ENABLED", "1");
        let toolbox_snap = if toolbox_on {
            places.toolbox.scan()
        } else {
            Vec::new()
        };
        // int() of the value, a negative one showing no lines; 40 otherwise
        let toolbox_index_max = cfg
            .get("TOOLBOX_INDEX_MAX")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .map_or(crate::toolbox::INDEX_MAX_DEFAULT, |n| {
                usize::try_from(n.max(0)).unwrap_or(usize::MAX)
            });
        Ok(Self {
            cfg: cfg.clone(),
            places,
            instructions_on: on("INSTRUCTIONS_ENABLED", "1"),
            settings: Settings::from_config(cfg, model, endpoint.is_vllm)?,
            endpoint,
            touch,
            messages: Vec::new(),
            system: None,
            search_on: cfg.get("SEARCH_ENABLED") == Some("1"),
            fetch_on: on("FETCH_ENABLED", "1"),
            guide_on: on("SYSTEM_GUIDE", "1"),
            max_rounds: max_rounds
                .trim()
                .parse()
                .map_err(|_| format!("SEARCH_MAX_ROUNDS={max_rounds:?} is not a number"))?,
            auto_title: on("CHAT_AUTO_TITLE", "1"),
            sessions: None,
            project: None,
            session,
            redactor: (cfg.get("CHAT_REDACT") == Some("1")).then(|| Redactor::from_config(cfg)),
            autosave_failed: false,
            earlier: Vec::new(),
            today,
            max_model_len: max_model_len(cfg),
            discovered: false,
            estimator: Estimator::default(),
            auto_compact: on("AUTO_COMPACT", "1"),
            elide_threshold: elide.0,
            elide_keep_turns: elide.1,
            consult,
            explore_on: on("EXPLORE_ENABLED", "1"),
            guide_trust_explore: on("SYSTEM_GUIDE_TRUST_EXPLORE", "1"),
            explore: workflows::ExploreSettings::from_config(cfg),
            fractions: compact::Fractions::from_config(cfg),
            compact_failed: false,
            background: None,
            auto_compact_threshold: cfg
                .get("AUTO_COMPACT_THRESHOLD")
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(0.85),
            stats: None,
            title_job: None,
            inflight: None,
            pending_images: Vec::new(),
            end_reason: "exit",
            partial: String::new(),
            files,
            mem_files: FileHistory::in_memory(),
            sandbox_tmp,
            sandbox_launcher: viy_tools::sandbox::Launcher::RPCTL,
            tools_on: on("TOOLS_ENABLED", "1"),
            tools_max_rounds: number(cfg, "TOOLS_MAX_ROUNDS", "25")?,
            shell_timeout: number(cfg, "TOOLS_SHELL_TIMEOUT", "120")?,
            sandbox_net: if cfg
                .get("SANDBOX_NET")
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("host")
            {
                "host".into()
            } else {
                "proxy".into()
            },
            sandbox_net_allow: cfg.get("SANDBOX_NET_ALLOW").unwrap_or_default().to_string(),
            parallel_tools: cfg.get("TOOLS_PARALLEL").unwrap_or("1") == "1",
            denials: 0,
            abort_turn: None,
            sandbox_check: None,
            turn_user: None,
            probed: false,
            mount_note: None,
            web_taint: None,
            web,
            closed_saved: false,
            tracer,
            monitor: None,
            turn_n: 0,
            rounds_log: None,
            toolbox_on,
            toolbox_snap,
            toolbox_index_max,
            sandbox_git_on: on("SANDBOX_GIT", "1"),
            sbgit_offer: None,
            sbgit: None,
            sbgit_failed: None,
            sbgit_used: false,
            tok: viy_config::vault::Tok::from_env(),
            ssh_hosts: Vec::new(),
            ssh_note: None,
            ssh_machine: Arc::new(viy_tools::remote::SshTools { home: ssh_home }),
            ssh_runner: Arc::new(viy_tools::remote::SystemSsh),
            label_sent: Arc::default(),
        })
    }

    /// The history: every closed turn.
    pub fn messages(&self) -> &[Value] {
        &self.messages
    }

    /// The system prompt's date.
    pub fn today(&self) -> &str {
        &self.today
    }

    /// The context length the server runs with, as far as is known.
    pub fn max_model_len(&self) -> u64 {
        self.max_model_len
    }

    /// The chat's title: the session's, once it has one.
    pub fn title(&self) -> Option<String> {
        lock(&self.session)
            .as_ref()
            .and_then(|log| log.title().map(str::to_string))
    }

    /// The chat's name for the tab title and the footer (rp's
    /// `_tab_label`): its session's title, else its first user message,
    /// `user` on a chat's first turn; empty while it has neither.
    pub fn tab_label(&self, user: Option<&str>) -> String {
        if let Some(title) = self.title() {
            return title;
        }
        let first;
        let msgs: &[Value] = match user {
            Some(text) if self.messages.is_empty() => {
                first = [json!({"role": "user", "content": text})];
                &first
            }
            _ => &self.messages,
        };
        default_title(msgs).unwrap_or_default()
    }

    /// Sends the chat's name if it changed since last sent.
    fn announce_label(&mut self, ctx: &TurnContext, user: Option<&str>) {
        let label = self.tab_label(user);
        let mut sent = lock(&self.label_sent);
        if *sent != label {
            ctx.emit(EventMsg::TitleChanged(TitleChangedEvent {
                title: label.clone(),
            }));
            *sent = label;
        }
    }

    /// What the footer shows from the start (Codex's `SessionConfigured`):
    /// the reasoning effort, the context gauge, the chat's name.
    pub fn opened(&mut self, ctx: &TurnContext) {
        ctx.emit(EventMsg::EffortChanged(EffortChangedEvent {
            effort: self.settings.reasoning_effort.clone(),
        }));
        let (used, _) = self.context_state(&[]);
        ctx.emit(EventMsg::TokenCount(self.token_count(used)));
        self.announce_label(ctx, None);
    }

    /// The context gauge at `used` tokens.
    fn token_count(&self, used: u64) -> TokenCountEvent {
        let budget = self.prompt_budget();
        TokenCountEvent {
            used,
            budget,
            auto_compact_at: self
                .auto_compact
                .then_some((budget as f64 * self.auto_compact_threshold) as u64),
        }
    }

    /// Keeps this chat in `store`, filed under `project`: a new session
    /// log, written from the first message on.
    pub fn use_sessions(&mut self, store: Sessions, project: Option<String>) {
        let log = store.create(project.as_deref(), Some(&self.settings.model));
        self.install(log);
        self.sessions = Some(store);
        self.project = project;
    }

    /// Sets the user's own system prompt (`--system`, `/system`).
    pub fn set_system(&mut self, system: Option<String>) {
        self.system = system;
    }

    /// Ends the session with `reason` when the chat closes: `exec` for
    /// `viy exec`, where rp's ask said `ask`.
    pub fn end_with(&mut self, reason: &'static str) {
        self.end_reason = reason;
    }

    /// Whether this chat's log has been written.
    pub fn saved(&self) -> bool {
        lock(&self.session).as_ref().is_some_and(SessionLog::exists)
    }

    /// The id of this chat's log, if it has one.
    pub fn session_id(&self) -> Option<String> {
        lock(&self.session).as_ref().map(|log| log.id().to_string())
    }

    /// What compactions summarised away of a resumed chat, each run
    /// followed by [`crate::sessions::compacted`].
    pub fn earlier(&self) -> &[Value] {
        &self.earlier
    }

    fn install(&mut self, mut log: SessionLog) {
        log.set_redactor(self.redactor.clone());
        *lock(&self.session) = Some(log);
    }

    /// Persists the history, including a turn still in progress. Never
    /// fails: a full disk must not take the conversation down with it.
    pub fn checkpoint(&mut self, ctx: &TurnContext) {
        let inflight = without_reasoning(self.inflight.as_deref().unwrap_or_default());
        let written = {
            let mut session = lock(&self.session);
            let Some(log) = session.as_mut() else {
                return;
            };
            if inflight.is_empty() {
                log.sync(self.system.as_deref(), &self.messages, false)
            } else {
                let mut msgs = self.messages.clone();
                msgs.extend(inflight);
                log.sync(self.system.as_deref(), &msgs, false)
            }
        };
        match written {
            Err(e) => {
                if !self.autosave_failed {
                    notice(
                        ctx,
                        NoticeLevel::Error,
                        "autosave_failed",
                        format!("[autosave failed: {e}]"),
                    );
                }
                self.autosave_failed = true;
            }
            Ok(_) => self.autosave_failed = false,
        }
    }

    /// Waits a while for the background title, so a chat left behind is
    /// named after it.
    async fn await_title(&mut self) {
        if let Some(job) = self.title_job.take() {
            let _ = tokio::time::timeout(TITLE_WAIT, job).await;
        }
    }

    /// Flushes and releases the session's log.
    pub async fn end_session(&mut self, ctx: &TurnContext, reason: &str) {
        // A /tmp full of downloads takes a while to remove: off the runtime.
        if let Some(dir) = self.sandbox_tmp.detach() {
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
        }
        if lock(&self.session).is_none() {
            return;
        }
        self.checkpoint(ctx);
        self.await_title().await;
        if let Some(log) = lock(&self.session).as_mut() {
            self.closed_saved = log.exists();
            let _ = log.close(reason);
        }
    }

    /// Whether the log was written when the chat last ended its session,
    /// for what the chat says on quit (rp's `saved`, taken before
    /// `_end_session`).
    pub fn closed_saved(&self) -> bool {
        self.closed_saved
    }

    /// Starts a fresh chat (`/new`): the current one is closed, not erased.
    pub async fn new_session(&mut self, ctx: &TurnContext, system: Option<String>) {
        let old = lock(&self.session)
            .as_ref()
            .map(|log| (log.exists(), shown_title(log)));
        self.end_session(ctx, "new").await;
        if let Some((true, title)) = &old {
            notice(
                ctx,
                NoticeLevel::Info,
                "session_saved",
                format!("saved '{title}' -- viy resume to come back to it"),
            );
        }
        self.messages.clear();
        self.system = system;
        self.today = crate::prompt::today();
        self.snapshot_toolbox();
        self.earlier.clear();
        self.inflight = None;
        // Files read by the old chat are unknown to this one.
        self.files.forget_reads();
        self.compact_failed = false;
        self.abort_background();
        self.mem_files = FileHistory::in_memory();
        self.reset_web(&[]);
        self.mount_note = self.mounts_note();
        // a lent server stays lent in a new chat, and it is told again
        self.ssh_note = self.ssh_hosts_note();
        let fresh = match (&old, &self.sessions) {
            (Some(_), Some(store)) => {
                Some(store.create(self.project.as_deref(), Some(&self.settings.model)))
            }
            _ => None,
        };
        *lock(&self.session) = None;
        if let Some(log) = fresh {
            self.install(log);
        }
        self.announce_label(ctx, None);
        notice(ctx, NoticeLevel::Info, "new_chat", "new chat");
    }

    /// Continues a saved chat (`/resume`). Offers a copy when another chat
    /// has it open, since two writers on one log would interleave. True on
    /// success.
    pub async fn attach_session(&mut self, ctx: &TurnContext, id: &str) -> bool {
        let Some(store) = self.sessions.clone() else {
            return false;
        };
        let opened = match store.open(id) {
            Err(SessionError::Busy(_)) => {
                let question = Question {
                    kind: None,
                    prompt: "that chat is open in another viy chat. Continue a copy of it \
                             instead?"
                        .into(),
                    choices: Choices::YesNo,
                    default: Answer::Yes,
                    unattended: Answer::No,
                    reason: Vec::new(),
                    options: Vec::new(),
                };
                if ctx.ask(&question).await != Answer::Yes {
                    return false;
                }
                store.copy(id, self.redactor.clone(), false)
            }
            Err(SessionError::Io(e)) => Err(e),
            Ok(opened) => Ok(opened),
        };
        let (log, st) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                notice(
                    ctx,
                    NoticeLevel::Error,
                    "session_open_failed",
                    format!("could not open chat {id}: {e}"),
                );
                return false;
            }
        };
        self.end_session(ctx, "switch").await;
        self.continue_in(log, st);
        self.announce_label(ctx, None);
        true
    }

    /// Goes on in `log`, opened with its state `st`: its history becomes
    /// this chat's (`/resume`, `viy exec resume`).
    pub fn continue_in(&mut self, log: SessionLog, st: sessions::State) {
        self.install(log);
        self.load_history(st);
    }

    /// Takes `st`'s history without its log: `viy exec --ephemeral` on a
    /// saved chat reads it and keeps nothing.
    pub fn load_history(&mut self, st: sessions::State) {
        self.system = st.system;
        self.messages = st.messages;
        self.earlier = st.earlier;
        self.today = crate::prompt::today();
        self.snapshot_toolbox();
        self.inflight = None;
        // Reads made by the chat left behind do not count.
        self.files.forget_reads();
        self.compact_failed = false;
        self.abort_background();
        let history = std::mem::take(&mut self.messages);
        self.reset_web(&history);
        self.messages = history;
        self.mount_note = self.mounts_note();
        self.ssh_note = self.ssh_hosts_note();
    }

    /// Runs `f` on this chat's record of what write_file overwrote (rp's
    /// `_files`): the session log's, or the one in memory.
    pub(crate) fn with_file_history<T>(&mut self, f: impl FnOnce(&mut FileHistory) -> T) -> T {
        let folder = lock(&self.session).as_ref().map(SessionLog::files_dir);
        match folder {
            Some(folder) => f(&mut FileHistory::at(folder)),
            None => f(&mut self.mem_files),
        }
    }

    /// write_file for tool call `call_id`, what it overwrote kept for
    /// `/rewind` (rp's `_tool_write_file` with `_record_write`). Failing
    /// to keep it costs only that: the write has happened.
    pub async fn write_file(
        &mut self,
        ctx: &TurnContext,
        call_id: &str,
        args: &serde_json::Map<String, Value>,
        stamp: &str,
    ) -> Outcome {
        let mut outcome = self.files.write_file(args, ctx, stamp).await;
        if let Some(c) = outcome.conflict.take() {
            let [path, reason, error] = *c;
            self.tracer.event(
                "write_refused",
                json!({"path": path, "reason": reason, "error": error}),
            );
        }
        if let Some(wrote) = outcome.wrote.take() {
            let kept = self.with_file_history(|history| {
                history.record(call_id, &wrote.path, wrote.before.as_deref(), &wrote.after)
            });
            if let Err(e) = kept {
                notice(
                    ctx,
                    NoticeLevel::Info,
                    "record_write",
                    format!(
                        "  [/rewind will not be able to restore {}: {e}]",
                        self.files.ws_rel(&wrote.path)
                    ),
                );
            }
        }
        outcome
    }

    /// The note the mounts give a fresh history, if any are mounted.
    fn mounts_note(&self) -> Option<String> {
        (!self.files.mounts.is_empty()).then(|| viy_tools::mounts::note(&self.files.mounts, &[]))
    }

    /// Forgets the old chat's web state; taints again if `messages` (a
    /// resumed chat) already hold internet text or files from a mount (rp's
    /// `_reset_web`).
    pub(crate) fn reset_web(&mut self, messages: &[Value]) {
        const WEB: &str = "web results earlier in this chat";
        self.web.clear();
        let mut taint: Option<&str> = None;
        for m in messages {
            if m["role"] == "tool"
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("<<<UNTRUSTED "))
            {
                taint = Some(WEB);
            }
            for tc in m["tool_calls"].as_array().into_iter().flatten() {
                let name = tc["function"]["name"].as_str().unwrap_or_default();
                let args = tc["function"]["arguments"].as_str().unwrap_or_default();
                if matches!(name, "web_search" | "fetch_url")
                    || (name == "run_shell" && args.contains("\"needs_network\": true"))
                {
                    taint = Some(WEB);
                } else if matches!(name, "read_file" | "list_dir" | "write_file")
                    && absolute_path_arg(args)
                {
                    // Only a mounted folder takes an absolute path.
                    taint = taint.or(Some("files from a mounted folder earlier in this chat"));
                } else if name == "run_shell" && ssh_command_arg(args) {
                    taint = taint.or(Some("output from a lent server earlier in this chat"));
                }
            }
        }
        if !self.files.mounts.is_empty() {
            taint = taint.or(Some("files from a mounted folder"));
        }
        self.web_taint = taint.map(str::to_string);
    }

    /// Marks the chat as holding outside text, if it does not already.
    pub(crate) fn taint(&mut self, why: String) {
        if self.web_taint.is_none() {
            self.tracer.event("web_taint", json!({"why": why}));
            self.web_taint = Some(why);
        }
    }

    /// The user's doctrines, read now (each review reads them afresh).
    pub fn doctrines(&self) -> viy_tools::doctrines::Loaded {
        viy_tools::doctrines::load(&self.places.doctrine_sources)
    }

    /// `/doctrines`' lines.
    pub fn report_doctrines(&self) -> Vec<String> {
        viy_tools::doctrines::report(&self.doctrines(), &self.places.doctrine_sources)
    }

    /// The chat's trace.
    pub fn tracer(&self) -> &Tracer {
        &self.tracer
    }

    /// An alarm the turn decided (rp's `mon.mark`).
    fn mark(&mut self, code: &str, msg: &str) {
        if let Some(m) = self.monitor.as_mut() {
            m.mark(code, msg);
        }
    }

    /// Writes the turn's metrics row and points at the trace if it
    /// alarmed (rp's `_finish_monitor`).
    fn finish_monitor(&mut self, ctx: &TurnContext, extra: Value) {
        let Some(m) = self.monitor.take() else {
            return;
        };
        let row = m.finish(match extra {
            Value::Object(e) => e,
            _ => serde_json::Map::new(),
        });
        let codes: Vec<&str> = row["alarms"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if codes.is_empty() {
            return;
        }
        let mut seen: Vec<&str> = Vec::new();
        for c in &codes {
            if !seen.contains(c) {
                seen.push(c);
            }
        }
        notice(
            ctx,
            NoticeLevel::Info,
            "finish_monitor",
            format!(
                "  [trace: {} alarm(s) this turn: {} -- viy trace {}]",
                codes.len(),
                seen.join(", "),
                self.tracer.sid()
            ),
        );
    }

    /// Why network commands need the user's yes, if they do.
    pub fn web_taint(&self) -> Option<&str> {
        self.web_taint.as_deref()
    }

    /// What the web tools go through: tests and `viy dev drive` give fakes.
    pub fn set_web_net(&mut self, net: web::WebNet) {
        self.web.net = net;
    }

    /// Marks this chat, not yet written, as one-shot (`viy exec`).
    /// No tool-parser preflight: rp chat checks at its start, a one-shot
    /// `viy exec` has no start to check at, and a pod without the parser
    /// answers its turn with the error itself.
    pub fn skip_tool_probe(&mut self) {
        self.probed = true;
    }

    pub fn mark_exec(&mut self) {
        if let Some(log) = lock(&self.session).as_mut() {
            log.mark_exec();
        }
    }

    /// The redactor CHAT_REDACT asks for, for a log opened outside.
    pub fn redactor(&self) -> Option<Redactor> {
        self.redactor.clone()
    }

    /// `/rename [title]`: names the chat, or says its name and id.
    pub fn rename_session(&mut self, ctx: &TurnContext, title: &str) {
        let shown = lock(&self.session)
            .as_ref()
            .map(|log| (log.title().map(str::to_string), log.id().to_string()));
        let Some((current, id)) = shown else {
            notice(
                ctx,
                NoticeLevel::Warn,
                "not_saved",
                "this chat is not saved; nothing to name",
            );
            return;
        };
        if title.is_empty() {
            let current = current
                .as_deref()
                .unwrap_or("(untitled until the first message)");
            notice(
                ctx,
                NoticeLevel::Info,
                "session_title",
                format!("{current}  [{id}]"),
            );
            return;
        }
        self.checkpoint(ctx);
        let renamed = lock(&self.session).as_mut().map(|log| log.rename(title));
        match renamed {
            Some(Err(e)) => notice(
                ctx,
                NoticeLevel::Error,
                "autosave_failed",
                format!("[autosave failed: {e}]"),
            ),
            _ => {
                notice(
                    ctx,
                    NoticeLevel::Info,
                    "renamed",
                    format!("renamed to '{title}'"),
                );
                ctx.emit(EventMsg::TitleChanged(TitleChangedEvent {
                    title: title.to_string(),
                }));
                *lock(&self.label_sent) = title.to_string();
            }
        }
    }

    /// `/fork [title]`: goes on in a copy of this chat; the original stays.
    pub async fn fork_session(&mut self, ctx: &TurnContext, title: &str) {
        if lock(&self.session).is_none() {
            notice(
                ctx,
                NoticeLevel::Warn,
                "not_saved",
                "this chat is not saved; there is nothing to fork",
            );
            return;
        }
        if self.messages.is_empty() {
            notice(
                ctx,
                NoticeLevel::Info,
                "nothing_to_fork",
                "nothing to fork yet",
            );
            return;
        }
        self.checkpoint(ctx);
        // So the fork is named after its final title.
        self.await_title().await;
        let forked = {
            let mut session = lock(&self.session);
            let Some(old) = session.as_mut() else {
                return;
            };
            let twin = old.fork(
                self.system.as_deref(),
                &self.messages,
                Some(title).filter(|t| !t.is_empty()),
            );
            if twin.is_ok() {
                let _ = old.close("fork");
            }
            twin.map(|twin| (twin, shown_title(old), old.id().to_string()))
        };
        match forked {
            Err(e) => notice(
                ctx,
                NoticeLevel::Error,
                "fork_failed",
                format!("could not fork: {e}"),
            ),
            Ok((twin, old_title, old_id)) => {
                // Everything else carries over: the history is the same.
                let line = format!(
                    "forked: you are in '{}' ({}); '{old_title}' stays as it was -- /resume \
                     {old_id} goes back",
                    shown_title(&twin),
                    twin.id()
                );
                self.install(twin);
                self.tracer.event(
                    "fork",
                    json!({"parent": old_id, "messages": self.messages.len()}),
                );
                notice(ctx, NoticeLevel::Info, "forked", line);
            }
        }
    }

    /// The last turn's numbers (`/stats`).
    pub fn stats(&self) -> Option<&TurnStats> {
        self.stats.as_ref()
    }

    /// The running chars-per-token estimate.
    pub fn chars_per_token(&self) -> f64 {
        self.estimator.chars_per_token
    }

    /// Forgets the server's context length, to be asked again before the
    /// next request: the pod was replaced or restarted.
    pub fn server_changed(&mut self) {
        self.discovered = false;
    }

    /// Tokens the conversation may occupy before a reply no longer fits.
    pub fn prompt_budget(&self) -> u64 {
        prompt_budget(self.max_model_len, self.settings.max_tokens)
    }

    /// (estimated tokens, fraction of the budget) for the system prompt,
    /// the history and `extra`.
    pub fn context_state(&self, extra: &[Value]) -> (u64, f64) {
        let mut msgs = Vec::new();
        if let Some(system) = self.system_prompt() {
            msgs.push(json!({"role": "system", "content": system}));
        }
        msgs.extend(self.messages.iter().cloned());
        msgs.extend(extra.iter().cloned());
        let tokens = self
            .estimator
            .tokens(&msgs, schema_chars(&self.tool_specs()));
        (tokens, tokens as f64 / self.prompt_budget() as f64)
    }

    /// What a chat says once it knows the context length: rp chat's line
    /// at its start.
    pub fn context_line(&self) -> String {
        let compact = if self.auto_compact {
            format!(
                "auto-compact at {:.0}%",
                self.auto_compact_threshold * 100.0
            )
        } else {
            "auto-compact off".into()
        };
        format!(
            "context {} tokens, {} usable for history after the {}-token reply budget ({compact})",
            self.max_model_len,
            self.prompt_budget(),
            self.settings.max_tokens
        )
    }

    /// Sends the next turn to `ready`: a new address or model is asked for
    /// its context length again before the turn (kb d041).
    pub fn connect(&mut self, ready: crate::server::Ready) {
        if self.endpoint.base != ready.base || self.settings.model != ready.model {
            self.endpoint.base = ready.base;
            self.settings.model = ready.model;
            self.discovered = false;
            self.probed = false;
        }
    }

    /// The model id requests name.
    pub fn model(&self) -> &str {
        &self.settings.model
    }

    /// Asks the server what context it was launched with: MAX_MODEL_LEN is
    /// only what was asked for, and a running pod can predate a config
    /// edit. Keeps the configured value when the server does not say.
    pub async fn discover_max_model_len(&mut self) -> u64 {
        self.discovered = true;
        let request = self
            .endpoint
            .request(Method::GET, "/v1/models")
            .timeouts(Duration::from_secs(10), Duration::from_secs(30));
        if let Ok(response) = viy_http::send(self.endpoint.http.as_ref(), request).await
            && response.status == 200
            && let Ok(bytes) = response.bytes().await
            && let Ok(models) = serde_json::from_slice::<Value>(&bytes)
            && let Some(n) = models["data"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|m| m["max_model_len"].as_u64().filter(|n| *n > 0))
        {
            self.max_model_len = n;
        }
        self.max_model_len
    }

    /// Whether the server accepts tool calls at all (rp's
    /// `_tools_supported`): one launched without `--tool-call-parser`
    /// answers a request carrying tools with HTTP 400. `None`: could not
    /// tell.
    pub async fn tools_supported(&self) -> (Option<bool>, String) {
        let body = json!({
            "model": self.settings.model,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1, "stream": false,
            "tools": viy_tools::build::tools(self.sandbox_net == "host"),
            "tool_choice": "auto",
        });
        let request = self
            .endpoint
            .request(Method::POST, "/v1/chat/completions")
            .header("Content-Type", "application/json")
            .body(body.to_string().into_bytes())
            .timeouts(Duration::from_secs(10), Duration::from_secs(60));
        match viy_http::send(self.endpoint.http.as_ref(), request).await {
            Err(e) => (None, format!("could not reach the pod ({e})")),
            Ok(r) if r.status == 200 => (Some(true), String::new()),
            Ok(r) => {
                let status = r.status;
                let text = r.bytes().await.unwrap_or_default();
                if status == 400
                    && String::from_utf8_lossy(&text)
                        .to_lowercase()
                        .contains("tool")
                {
                    (
                        Some(false),
                        "this pod was launched without the tool-call parser".into(),
                    )
                } else {
                    (None, format!("HTTP {status}"))
                }
            }
        }
    }

    /// Every tool offered to the model.
    fn tool_specs(&self) -> Vec<Value> {
        let mut specs = Vec::new();
        if self.search_on {
            specs.push(viy_tools::web::search_tool());
            if self.fetch_on {
                specs.push(viy_tools::web::fetch_tool());
            }
        }
        if self.tools_on {
            specs.extend(viy_tools::build::tools_for(
                self.sandbox_net == "host",
                self.sbgit_offer == Some(true),
            ));
            if self.explore_on {
                specs.push(viy_tools::explore::explore_tool());
            }
            if self.consult_available() {
                specs.push(consult::consult_tool());
            }
            if !self.toolbox_snap.is_empty() {
                specs.push(crate::toolbox::find_tool());
            }
        }
        specs
    }

    /// Freezes the toolbox for this history (rp's `_snapshot_toolbox`):
    /// at a fresh, resumed or compacted history and at `/toolbox on|off`.
    pub(crate) fn snapshot_toolbox(&mut self) {
        self.toolbox_snap = if self.toolbox_on {
            self.places.toolbox.scan()
        } else {
            Vec::new()
        };
    }

    /// The live toolbox run_shell and find_tool read, so a tool kept
    /// mid-chat is usable at once; empty while the toolbox is off.
    pub(crate) fn live_toolbox(&self) -> Vec<crate::toolbox::Tool> {
        if self.toolbox_on {
            self.places.toolbox.scan()
        } else {
            Vec::new()
        }
    }

    /// The estimated tokens of `msgs` with this request's tool schemas
    /// (rp's `_estimate_tokens`).
    fn estimate_tokens(&self, msgs: &[Value]) -> u64 {
        self.estimator
            .tokens(msgs, schema_chars(&self.tool_specs()))
    }

    /// The instruction files in force, re-read so an edit lands on the next
    /// message.
    pub fn instructions(&self) -> Vec<instructions::Loaded> {
        if !self.instructions_on {
            return Vec::new();
        }
        instructions::load(&self.cfg, &self.places.config_dir, &self.places.workspace)
    }

    /// Where the chat looks for files.
    pub fn places(&self) -> &Places {
        &self.places
    }

    /// What a chat's start and `/instructions` say about them.
    pub fn report_instructions(&self) -> Vec<String> {
        instructions::report(
            self.instructions_on,
            &instructions::names(&self.cfg),
            &self.instructions(),
        )
    }

    fn system_prompt(&self) -> Option<String> {
        let block = instructions::render(&self.instructions());
        let index = crate::toolbox::render_index(&self.toolbox_snap, self.toolbox_index_max);
        SystemPrompt {
            user: self.system.as_deref(),
            tools_on: self.tools_on,
            search_on: self.search_on,
            guide_on: self.guide_on,
            explore_trust: self.explore_on && self.guide_trust_explore,
            consult: self.consult_available(),
            toolbox_index: index.as_deref(),
            workspace: &self.places.workspace,
            today: &self.today,
            instructions: block.as_deref(),
        }
        .render()
    }

    /// One user turn; the title is asked for after it. A server not yet
    /// asked for its context length is asked first.
    pub async fn send(&mut self, ctx: &TurnContext, content: Value) -> TurnOutcome {
        self.inflight = None;
        self.partial.clear();
        if !self.discovered {
            self.discover_max_model_len().await;
            notice(
                ctx,
                NoticeLevel::Info,
                "context_length",
                self.context_line(),
            );
        }
        // rp chat checks at its start that the pod can take tool calls.
        if self.tools_on && !self.probed {
            self.probed = true;
            if let (Some(false), note) = self.tools_supported().await {
                notice(
                    ctx,
                    NoticeLevel::Error,
                    "tools_supported",
                    format!(
                        "  but {note}, so the model cannot call tools. Recreate the pod to pick \
                         up the parser: viy down && viy up."
                    ),
                );
            }
        }
        // Before the first request with tools: run_shell's description
        // says whether .git is the model's own (rp's `_private_git`).
        if self.tools_on {
            self.decide_private_git().await;
        }
        self.announce_label(ctx, content.as_str());
        let outcome = self.turn(ctx, content).await;
        self.maybe_start_title(ctx);
        self.maybe_start_background();
        self.inflight = None;
        self.checkpoint(ctx);
        // A turn cut short (a failed request) still gets its row.
        self.finish_monitor(ctx, json!({"interrupted": true}));
        outcome
    }

    async fn turn(&mut self, ctx: &TurnContext, content: Value) -> TurnOutcome {
        let started = tokio::time::Instant::now();
        // Before anything is sent: shrink the history if this turn would
        // spill; dropping stale tool output first often makes compaction
        // unnecessary.
        self.maybe_elide_tool_results(ctx, &content);
        self.maybe_auto_compact(ctx, &content).await;
        let user = json!({"role": "user", "content": content});
        let mut msgs = Vec::new();
        if let Some(system) = self.system_prompt() {
            msgs.push(json!({"role": "system", "content": system}));
        }
        msgs.extend(self.messages.iter().cloned());
        msgs.push(user.clone());
        let user_chars = content.as_str().map_or_else(
            || content.to_string().chars().count(),
            |t| t.chars().count(),
        );
        self.turn_user = Some(content);
        self.denials = 0;
        self.abort_turn = None;
        self.turn_n += 1;
        let echo_ctx = ctx.clone();
        let echo: crate::trace::Echo = Arc::new(move |sev, line| {
            let level = if sev == Severity::Error {
                NoticeLevel::Error
            } else {
                NoticeLevel::Warn
            };
            notice(&echo_ctx, level, "alarm", line);
        });
        self.monitor = Some(TurnMonitor::new(
            self.tracer.clone(),
            "main",
            Some(self.turn_n),
            echo,
        ));
        self.tracer.event(
            "turn_start",
            json!({"turn": self.turn_n, "history_msgs": self.messages.len(),
                   "user_chars": user_chars}),
        );
        self.inflight = Some(vec![user]);
        // The user's message is on disk before the reply.
        self.checkpoint(ctx);
        let specs = self.tool_specs();
        let max_rounds = if self.tools_on {
            self.max_rounds.max(self.tools_max_rounds)
        } else {
            self.max_rounds
        };

        let mut out = Round::default();
        let mut sent = msgs.clone();
        let mut first_token = None;
        let mut usage: Option<Value> = None;
        let mut reasoning_chars = 0;
        let mut text = String::new();
        let mut stopped_full = false;
        let mut calibrate = true;
        let mut dropped_call = false;
        let mut answer_thought = false;
        // Requests made so far, for the retry's number.
        let mut requests = 0u32;
        for round_i in 0..=max_rounds {
            if round_i > 0 && self.tool_loop_overflows(ctx, &msgs) {
                stopped_full = true;
                break;
            }
            let last_round = round_i == max_rounds;
            let round_no = u32::try_from(round_i + 1).unwrap_or(u32::MAX);
            requests = round_no;
            let round_started = tokio::time::Instant::now();
            let got = if last_round {
                self.mark(
                    "tool_cap",
                    &format!(
                        "the turn used all {max_rounds} tool rounds; the model was told to answer"
                    ),
                );
                // Out of tool rounds: said, and tools forbidden, or the model
                // asks for another and the turn ends without an answer. The
                // tools stay in the prompt so the prefix cache still hits.
                notice(
                    ctx,
                    NoticeLevel::Warn,
                    "tool_budget",
                    format!(
                        "[tool-call budget reached ({max_rounds} rounds); asking for the answer]"
                    ),
                );
                sent = with_notice(&msgs, TOOL_BUDGET_NOTICE);
                self.round(
                    ctx,
                    round_no,
                    RoundKind::CappedFinal,
                    &sent,
                    &specs,
                    Tools::None,
                )
                .await
            } else {
                sent.clone_from(&msgs);
                self.round(ctx, round_no, RoundKind::Turn, &msgs, &specs, Tools::Auto)
                    .await
            };
            let Some(mut r) = got else {
                // A request that failed before anything happened is dropped;
                // once tools have run, that work is kept. The turn's row is
                // written as an interrupted one, after the checkpoint.
                self.mark(
                    "request_failed",
                    &format!("model request failed in round {round_i}"),
                );
                self.close_inflight("interrupted: the request failed", false);
                return TurnOutcome::Failed {
                    message: "the request failed".into(),
                };
            };
            let secs = round_started.elapsed().as_secs_f64();
            let mode = if last_round { "final" } else { "auto" };
            if let Some(m) = self.monitor.as_mut() {
                m.on_round(round_i, &r, secs, mode);
            }
            // Now part of `r`, not a fragment.
            self.partial.clear();
            first_token = first_token.or(r.first_token);
            if r.usage.is_some() {
                usage.clone_from(&r.usage);
            }
            reasoning_chars += r.reasoning.chars().count();
            text.clone_from(&r.text);

            // On the last round a call is dropped, never run: its result
            // could not reach the model anyway.
            if r.tool_calls.is_empty() || last_round {
                dropped_call = !r.tool_calls.is_empty();
                answer_thought = !r.reasoning.is_empty();
                out = r;
                break;
            }

            // Every call gets a unique id, even from a backend that sends
            // none: /rewind finds a turn's file writes by it.
            for c in &mut r.tool_calls {
                if c.id.is_empty() {
                    c.id = format!("call_{}", random_hex(6));
                }
            }
            let calls: Vec<Value> = r
                .tool_calls
                .iter()
                .map(|c| {
                    // Broken arguments would make vLLM refuse every later
                    // request.
                    let args = if tools::args_ok(&c.arguments) {
                        c.arguments.as_str()
                    } else {
                        "{}"
                    };
                    json!({"id": c.id, "type": "function",
                           "function": {"name": c.name, "arguments": args}})
                })
                .collect();
            let content = if r.text.is_empty() {
                Value::Null
            } else {
                json!(r.text)
            };
            let mut assistant =
                json!({"role": "assistant", "content": content, "tool_calls": calls});
            // Sent back so the next round goes on with this reasoning; dropped
            // when the turn is committed (`without_reasoning`).
            if !r.reasoning.is_empty() {
                assistant["reasoning"] = json!(r.reasoning);
            }
            msgs.push(assistant.clone());
            if let Some(turn) = self.inflight.as_mut() {
                turn.push(assistant);
            }

            let payloads = self.run_tool_calls(ctx, &r.tool_calls).await;
            for (c, payload) in r.tool_calls.iter().zip(payloads) {
                let payload =
                    if r.finish.as_deref() == Some("length") && !tools::args_ok(&c.arguments) {
                        // The bare JSON error does not tell the model why, and it
                        // tends to retry the same oversized call.
                        json!({"error": format!(
                            "not run: your reply hit the {}-token output limit in the middle of \
                             this call, so its arguments were cut off. Make the call smaller, e.g. \
                             write a long file in several parts.",
                            self.settings.max_tokens
                        )})
                        .to_string()
                    } else {
                        payload
                    };
                let payload = self.fit_tool_payload(ctx, &payload, &msgs);
                let tool_msg = json!({"role": "tool", "tool_call_id": c.id, "content": payload});
                msgs.push(tool_msg.clone());
                if let Some(turn) = self.inflight.as_mut() {
                    turn.push(tool_msg);
                }
                self.checkpoint(ctx);
            }
            if let Some(why) = self.abort_turn.clone() {
                // A model that keeps rephrasing refused actions does not get
                // to keep trying: no further round.
                text = format!("[stopped: {why}. Tell me how you want to proceed.]");
                notice(
                    ctx,
                    NoticeLevel::Error,
                    "turn_stopped",
                    format!("  [turn stopped: {why}]"),
                );
                self.mark("circuit_breaker", &format!("turn stopped: {why}"));
                stopped_full = true;
                out = r;
                break;
            }
            out = r;
        }

        // Reasoning but no answer text, or a call made where none can run:
        // asked once more with no tools in the request at all. That misses
        // the prefix cache, but a turn without an answer is worse.
        if (dropped_call || text.trim().is_empty())
            && !stopped_full
            && out.finish.as_deref() != Some("length")
        {
            notice(
                ctx,
                NoticeLevel::Warn,
                "no_answer_retry",
                "[the reply had no answer; asking for it once more]",
            );
            let tried = if dropped_call {
                "; the last round still asked for a tool"
            } else {
                ""
            };
            self.mark(
                "no_answer_retry",
                &format!("the reply had no answer{tried}; asked once more without tools"),
            );
            sent = with_notice(&msgs, NO_ANSWER_NOTICE);
            // The prompt carried no tool schemas.
            calibrate = false;
            let round_no = requests + 1;
            if let Some(retry) = self
                .round(ctx, round_no, RoundKind::Retry, &sent, &specs, Tools::Omit)
                .await
            {
                out = retry;
                text.clone_from(&out.text);
                answer_thought |= !out.reasoning.is_empty();
                reasoning_chars += out.reasoning.chars().count();
                if out.usage.is_some() {
                    usage.clone_from(&out.usage);
                }
            }
        }

        // A reasoning model spends the budget on its trace first, so at the
        // cap the stream just stops, often before any answer.
        let cut = out.finish.as_deref() == Some("length");
        if cut {
            let done = usage
                .as_ref()
                .map(|u| &u["completion_tokens"])
                .filter(|n| n.as_f64().is_some_and(|n| n != 0.0));
            let got = done
                .map(|n| format!(" after {n} tokens"))
                .unwrap_or_default();
            let tail = if text.trim().is_empty() {
                " before it reached an answer"
            } else {
                ""
            };
            notice(
                ctx,
                NoticeLevel::Warn,
                "length_cut",
                format!(
                    "[stopped at the {}-token limit{got}{tail}; the reply is cut off. Raise the \
                     budget with /maxtok <n> (it also counts the reasoning trace).]",
                    self.settings.max_tokens
                ),
            );
        }
        if stopped_full && text.trim().is_empty() {
            // The loop stopped between a tool result and an answer; the
            // history still needs a closing assistant message.
            text = "[stopped: the tool results filled the context window before I reached an \
                    answer. Ask again -- the history will be compacted first.]"
                .into();
            self.mark(
                "context_full",
                "tool results filled the context window before an answer",
            );
            notice(ctx, NoticeLevel::Info, "context_full", text.clone());
        } else if text.trim().is_empty() {
            // Never commit a silent, empty answer: the reasoning behind it
            // is dropped from history, so the next turn would have nothing.
            text = if cut {
                "[no answer: the reply hit the token limit first.]".into()
            } else {
                "[no answer: the model stopped without writing one. Ask again -- its tool \
                 results are kept in the history.]"
                    .into()
            };
            self.mark("no_answer", "the turn ended without an answer");
            notice(ctx, NoticeLevel::Info, "no_answer", text.clone());
        }

        let mut answer = json!({"role": "assistant", "content": text});
        if answer_thought {
            answer["thought"] = json!(true);
        }
        let mut turn = self.inflight.take().unwrap_or_default();
        turn.push(answer);
        self.messages.extend(without_reasoning(&turn));

        let prompt_tokens = usage.as_ref().and_then(|u| u["prompt_tokens"].as_i64());
        // Only the untouched final request can calibrate: after an overflow
        // the messages carry tool results that were never sent.
        if calibrate && !stopped_full {
            self.estimator
                .calibrate(&sent, schema_chars(&specs), prompt_tokens);
        }
        let elapsed = started.elapsed().as_secs_f64();
        let completion_tokens = usage.as_ref().and_then(|u| u["completion_tokens"].as_u64());
        let (context_tokens, fraction) = self.context_state(&[]);
        let budget = self.prompt_budget();
        self.stats = Some(TurnStats {
            elapsed_s: round_to(elapsed, 2),
            ttft_s: first_token.map(|t| round_to(t.duration_since(started).as_secs_f64(), 2)),
            prompt_tokens,
            completion_tokens,
            tok_per_s: completion_tokens
                .filter(|n| *n > 0 && elapsed > 0.0)
                .map(|n| round_to(n as f64 / elapsed, 1)),
            reasoning_chars,
            finish: out.finish.clone(),
            context_tokens,
            context_budget: budget,
            context_pct: round_to(fraction * 100.0, 1),
            chars_per_token: round_to(self.estimator.chars_per_token, 2),
        });
        ctx.emit(EventMsg::TokenCount(self.token_count(context_tokens)));
        let extra = json!({"answer_chars": text.chars().count(), "finish": out.finish,
                           "context_pct": round_to(fraction * 100.0, 1)});
        self.finish_monitor(ctx, extra);
        TurnOutcome::Answered
    }

    /// How large the messages may get inside a tool loop (rp's
    /// `_tool_loop_ceiling`): not the wall, since compaction is itself a
    /// request and a loop up to the wall would leave a history that can
    /// no longer be compacted.
    fn tool_loop_ceiling(&self) -> u64 {
        let budget = self.prompt_budget();
        if self.auto_compact {
            (budget as f64 * self.auto_compact_threshold.max(0.95)) as u64
        } else {
            budget
        }
    }

    /// True when another tool round would not fit (rp's
    /// `_tool_loop_overflows`): there is no safe compaction point mid-loop,
    /// so the loop stops and the next turn compacts at a clean edge.
    fn tool_loop_overflows(&self, ctx: &TurnContext, msgs: &[Value]) -> bool {
        let tok = self.estimate_tokens(msgs);
        if tok < self.tool_loop_ceiling() {
            return false;
        }
        notice(
            ctx,
            NoticeLevel::Warn,
            "tool_loop_full",
            format!(
                "  [tool results filled the context (~{tok} tokens); stopping the tool loop \
                 rather than sending a request the pod would reject]"
            ),
        );
        true
    }

    /// A tool result clipped to the room left under the loop's ceiling
    /// (rp's `_fit_tool_payload`).
    fn fit_tool_payload(&self, ctx: &TurnContext, payload: &str, msgs: &[Value]) -> String {
        let room = self.tool_loop_ceiling() as i64 - self.estimate_tokens(msgs) as i64;
        let max_chars = (room as f64 * self.estimator.chars_per_token) as i64;
        let len = payload.chars().count() as i64;
        if max_chars >= len {
            return payload.to_string();
        }
        let note = "\n...[truncated: no more room in the context window]";
        let note_len = note.chars().count() as i64;
        if max_chars < note_len + 200 {
            return "[tool result dropped: no room left in the context window]".into();
        }
        notice(
            ctx,
            NoticeLevel::Warn,
            "tool_result_clipped",
            "  [tool result clipped to fit the context]",
        );
        let keep = usize::try_from(max_chars - note_len).unwrap_or(0);
        let mut cut: String = payload.chars().take(keep).collect();
        cut.push_str(note);
        cut
    }

    /// One request of the turn.
    async fn round(
        &mut self,
        ctx: &TurnContext,
        round: u32,
        kind: RoundKind,
        msgs: &[Value],
        specs: &[Value],
        tools: Tools,
    ) -> Option<Round> {
        ctx.emit(EventMsg::RoundStarted(RoundStartedEvent { round, kind }));
        let body = self.settings.payload(msgs, specs, tools, true);
        let emit = |msg| ctx.emit(msg);
        let sink = Sink {
            emit: &emit,
            touch: self.touch.as_ref(),
        };
        let started = std::time::Instant::now();
        let out = stream_once(&self.endpoint, &body, &sink, &mut self.partial).await;
        if self.rounds_log.is_some() {
            let rec = self.round_record(round, msgs, tools, out.as_ref(), started.elapsed());
            if let Some(f) = self.rounds_log.as_mut() {
                use std::io::Write;
                let _ = writeln!(f, "{rec}");
            }
        }
        out
    }

    /// The system prompt this chat sends (`viy dev payload`, for the
    /// replay benches).
    pub fn bench_system(&self) -> Option<String> {
        self.system_prompt()
    }

    /// The body this chat sends for `msgs` with its tools offered as
    /// `tools`, not streamed (`viy dev payload`), as rpctl's benches built
    /// it with `_payload`.
    pub fn bench_body(&self, msgs: &[Value], tools: Tools) -> Value {
        let mut body = self.settings.payload(msgs, &self.tool_specs(), tools, true);
        body["stream"] = json!(false);
        if let Some(m) = body.as_object_mut() {
            m.remove("stream_options");
        }
        body
    }

    /// Appends every main-chat reply to `path` from now on (`viy dev drive
    /// --rounds`).
    pub fn log_rounds(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        self.rounds_log = Some(f);
        Ok(())
    }

    /// One reply as rpctl's bench harnesses logged `_stream_once`: the
    /// turn from 0, the round, the seconds, the messages sent, the tools'
    /// mode and, when tools were held back, the end of the last message.
    fn round_record(
        &self,
        round: u32,
        msgs: &[Value],
        tools: Tools,
        out: Option<&Round>,
        took: std::time::Duration,
    ) -> Value {
        let mode = match tools {
            Tools::Auto => "auto",
            Tools::None => "none",
            Tools::Omit => "omit",
        };
        let notice = (mode != "auto").then(|| {
            let last = msgs.last().map(|m| match &m["content"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            let last = last.unwrap_or_default();
            let n = last.chars().count();
            last.chars().skip(n.saturating_sub(120)).collect::<String>()
        });
        let mut rec = json!({
            "turn": self.turn_n.saturating_sub(1),
            "round": round,
            "secs": (took.as_secs_f64() * 10.0).round() / 10.0,
            "n_msgs": msgs.len(),
            "tools": mode,
            "notice": notice,
        });
        match out {
            None => rec["failed"] = json!(true),
            Some(r) => {
                rec["finish"] = json!(r.finish);
                rec["usage"] = r.usage.clone().unwrap_or(Value::Null);
                rec["text"] = json!(r.text);
                rec["reasoning"] = json!(r.reasoning);
                rec["tool_calls"] = json!(
                    r.tool_calls
                        .iter()
                        .map(|c| json!({"id": c.id, "type": "function",
                                        "function": {"name": c.name, "arguments": c.arguments}}))
                        .collect::<Vec<_>>()
                );
            }
        }
        rec
    }

    /// Commits an unfinished turn as a well-formed exchange: it ends on an
    /// assistant message carrying whatever had streamed, plus why it
    /// stopped. With `keep_bare` false, a turn with nothing to show is
    /// dropped.
    fn close_inflight(&mut self, reason: &str, keep_bare: bool) {
        let Some(mut turn) = self.inflight.take() else {
            return;
        };
        let partial = self.partial.trim().to_string();
        if turn.len() == 1 && partial.is_empty() && !keep_bare {
            return;
        }
        let answered: Vec<String> = turn
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["tool_call_id"].as_str().map(str::to_string))
            .collect();
        let missing: Vec<String> = turn
            .iter()
            .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
            .filter_map(|tc| tc["id"].as_str().map(str::to_string))
            .filter(|id| !answered.contains(id))
            .collect();
        for id in missing {
            let content = json!({"error": format!("not run: {reason}")}).to_string();
            turn.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
        }
        let note = format!("[{reason}]");
        let content = if partial.is_empty() {
            note
        } else {
            format!("{partial}\n\n{note}")
        };
        turn.push(json!({"role": "assistant", "content": content}));
        self.messages.extend(without_reasoning(&turn));
    }

    /// After a reply, names the chat in the background, once: the title
    /// is written to the session as soon as the model has given it, unless
    /// the user named the chat first, and the next turn does not wait.
    fn maybe_start_title(&mut self, ctx: &TurnContext) {
        if !self.auto_title || self.title_job.as_ref().is_some_and(|j| !j.is_finished()) {
            return;
        }
        let Some(id) = lock(&self.session)
            .as_ref()
            .filter(|log| log.title_auto() && !log.title_by_model())
            .map(|log| log.id().to_string())
        else {
            return;
        };
        let Some((user, reply)) = title_pair(&self.messages) else {
            return;
        };
        let body = self.settings.title_body(user, reply);
        let request = self
            .endpoint
            .request(Method::POST, "/v1/chat/completions")
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&body).unwrap_or_default())
            .timeouts(Duration::from_secs(10), Duration::from_secs(60));
        let http = Arc::clone(&self.endpoint.http);
        let session = Arc::clone(&self.session);
        let label_sent = Arc::clone(&self.label_sent);
        let ctx = ctx.clone();
        ctx.emit(background(BackgroundState::Started));
        self.title_job = Some(tokio::spawn(async move {
            // A title is a nicety: on any failure the default one stands.
            let named = match viy_http::send(http.as_ref(), request).await {
                Ok(response) if response.status == 200 => response
                    .bytes()
                    .await
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|r| {
                        r["choices"][0]["message"]["content"]
                            .as_str()
                            .map(clean_title)
                    })
                    .flatten(),
                _ => None,
            };
            let Some(title) = named else {
                ctx.emit(background(BackgroundState::Failed));
                return;
            };
            // Only to the session it was made for, while it is open.
            let written = lock(&session)
                .as_mut()
                .filter(|log| log.id() == id && log.exists())
                .is_some_and(|log| log.set_auto_title(&title).unwrap_or(false));
            if written {
                *lock(&label_sent) = title.clone();
                ctx.emit(EventMsg::TitleChanged(TitleChangedEvent { title }));
            }
            ctx.emit(background(BackgroundState::Done));
        }));
    }
}

impl Drop for Chat {
    fn drop(&mut self) {
        if let Some(job) = self.title_job.take() {
            job.abort();
        }
        // rp's `_release_sandbox_git` at the chat's end: unmounted unless
        // another viy chat still uses it.
        if self.sbgit_used {
            viy_tools::sandboxgit::release(&self.places.sandbox_git);
        }
    }
}

/// A log's title as messages show it.
fn shown_title(log: &SessionLog) -> String {
    log.title().unwrap_or("(empty)").to_string()
}

/// MAX_MODEL_LEN, 65536 when it is not a number.
pub fn max_model_len(cfg: &Config) -> u64 {
    cfg.get("MAX_MODEL_LEN")
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(65536)
}

/// A turn's numbers, rp's `last_stats`, in its key order.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TurnStats {
    pub elapsed_s: f64,
    pub ttft_s: Option<f64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<u64>,
    pub tok_per_s: Option<f64>,
    pub reasoning_chars: usize,
    pub finish: Option<String>,
    pub context_tokens: u64,
    pub context_budget: u64,
    pub context_pct: f64,
    pub chars_per_token: f64,
}

fn round_to(x: f64, digits: i32) -> f64 {
    let scale = 10f64.powi(digits);
    (x * scale).round() / scale
}

fn background(state: BackgroundState) -> EventMsg {
    EventMsg::Background(BackgroundEvent {
        task: BackgroundTask::Title,
        state,
        progress: None,
    })
}

fn notice(ctx: &TurnContext, level: NoticeLevel, code: &str, message: impl Into<String>) {
    ctx.emit(EventMsg::Notice(NoticeEvent {
        level,
        code: code.into(),
        message: message.into(),
    }));
}

/// The chat as the engine's [`Turns`]. A turn takes the chat for as long
/// as it runs and gives it back when it ends, or when the engine cancels it
/// (Ctrl-C), after closing the interrupted turn into the history.
pub struct ChatTurns {
    slot: Arc<Mutex<Option<Chat>>>,
    /// The chat is being closed: what a cancelled turn says it was.
    closing: Arc<AtomicBool>,
    /// Where turns go and who runs the pod commands; without one, the
    /// chat's own address, always taken to answer.
    server: Option<Arc<dyn Server>>,
    /// The chat's trace, for the engine's mode switches.
    tracer: Tracer,
}

impl ChatTurns {
    pub fn new(chat: Chat) -> Self {
        Self {
            tracer: chat.tracer.clone(),
            slot: Arc::new(Mutex::new(Some(chat))),
            closing: Arc::default(),
            server: None,
        }
    }

    /// The chat with `server` asked before each turn (kb d041).
    pub fn with_server(chat: Chat, server: Arc<dyn Server>) -> Self {
        Self {
            server: Some(server),
            ..Self::new(chat)
        }
    }

    /// Runs `f` on the chat between turns; `None` while a turn has it.
    pub fn with_chat<T>(&self, f: impl FnOnce(&mut Chat) -> T) -> Option<T> {
        lock(&self.slot).as_mut().map(f)
    }
}

/// The chat, taken out of its slot by a running turn.
struct Lease {
    slot: Arc<Mutex<Option<Chat>>>,
    chat: Option<Chat>,
    ctx: TurnContext,
    closing: Arc<AtomicBool>,
}

impl Lease {
    fn take(turns: &ChatTurns, ctx: &TurnContext) -> Option<Self> {
        let chat = lock(&turns.slot).take()?;
        Some(Self {
            slot: Arc::clone(&turns.slot),
            chat: Some(chat),
            ctx: ctx.clone(),
            closing: Arc::clone(&turns.closing),
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(mut chat) = self.chat.take() {
            // Nothing in flight once a turn has ended on its own.
            if chat.inflight.is_some() {
                let reason = if self.closing.load(Ordering::SeqCst) {
                    "interrupted: the chat was closed"
                } else {
                    "interrupted"
                };
                chat.close_inflight(reason, true);
                chat.checkpoint(&self.ctx);
            }
            // A turn interrupted still gets its row.
            chat.finish_monitor(&self.ctx, json!({"interrupted": true}));
            *lock(&self.slot) = Some(chat);
        }
    }
}

impl Turns for ChatTurns {
    fn turn(self: Arc<Self>, ctx: TurnContext, text: String) -> BoxFuture<'static, TurnOutcome> {
        Box::pin(async move {
            let Some(mut lease) = Lease::take(&self, &ctx) else {
                return TurnOutcome::Failed {
                    message: "the chat is busy".into(),
                };
            };
            match lease.chat.as_mut() {
                Some(chat) => {
                    let content = chat.user_content(text);
                    chat.send(&ctx, content).await
                }
                None => TurnOutcome::Failed {
                    message: "the chat is busy".into(),
                },
            }
        })
    }

    fn admit(self: Arc<Self>, ctx: TurnContext) -> BoxFuture<'static, Result<(), String>> {
        let _ = ctx;
        Box::pin(async move {
            let Some(server) = self.server.clone() else {
                return Ok(());
            };
            let ready = server.ready().await?;
            self.with_chat(|chat| chat.connect(ready));
            Ok(())
        })
    }

    fn open_session(
        self: Arc<Self>,
        ctx: TurnContext,
        open: crate::OpenSession,
    ) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            if let Some(mut lease) = Lease::take(&self, &ctx)
                && let Some(chat) = lease.chat.as_mut()
            {
                chat.open_session(&ctx, &open).await;
            }
        })
    }

    fn interrupt(&self) {
        if let Some(server) = &self.server {
            server.interrupt();
        }
    }

    fn command(self: Arc<Self>, ctx: TurnContext, text: String) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let rest = text.strip_prefix('/').unwrap_or(&text);
            let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
            if let Some(server) = self.server.clone()
                && server.command(&ctx, name, args.trim())
            {
                return;
            }
            // `/tools on` asks the model whether it takes tool calls: ask
            // the server a turn would go to now, as `admit` connects a turn.
            if name == "tools"
                && args
                    .split_whitespace()
                    .next()
                    .is_some_and(|a| a.eq_ignore_ascii_case("on"))
                && let Some(server) = self.server.clone()
                && let Ok(ready) = server.ready().await
            {
                self.with_chat(|chat| chat.connect(ready));
            }
            if let Some(mut lease) = Lease::take(&self, &ctx)
                && let Some(chat) = lease.chat.as_mut()
            {
                chat.command(&ctx, &text).await;
            }
        })
    }

    fn opened(&self, ctx: &TurnContext) {
        self.with_chat(|chat| chat.opened(ctx));
        if let Some(server) = &self.server {
            Arc::clone(server).opened(ctx);
        }
    }

    fn closing(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    fn tracer(&self) -> Option<Tracer> {
        Some(self.tracer.clone())
    }

    /// Stops the pod commands, saves the chat and releases its session log.
    fn shutdown(self: Arc<Self>, ctx: TurnContext) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            if let Some(server) = self.server.clone() {
                server.shutdown().await;
            }
            if let Some(mut lease) = Lease::take(&self, &ctx)
                && let Some(chat) = lease.chat.as_mut()
            {
                let reason = chat.end_reason;
                chat.end_session(&ctx, reason).await;
            }
        })
    }
}

#[path = "commands.rs"]
mod commands;
#[path = "compact.rs"]
pub mod compact;
#[path = "consult.rs"]
pub mod consult;
#[path = "tools.rs"]
mod tools;
#[path = "web.rs"]
pub mod web;
#[path = "workflows.rs"]
pub mod workflows;

/// `n` random bytes as hex, for a tool call that came without an id.
fn random_hex(n: usize) -> String {
    use rand::TryRngCore;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut bytes = vec![0u8; n];
    if rand::rngs::OsRng.try_fill_bytes(&mut bytes).is_err() {
        // No randomness: still unique in this process, which is what
        // /rewind needs of an id.
        let k = NEXT.fetch_add(1, Ordering::Relaxed);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((t ^ k.rotate_left(32)) >> (8 * (i % 8))) as u8;
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A whole-number setting, an error when it is not one (as rp's `int()`).
fn number(cfg: &Config, key: &str, default: &str) -> Result<u64, String> {
    let raw = cfg.get(key).unwrap_or(default);
    raw.trim()
        .parse()
        .map_err(|_| format!("{key}={raw:?} is not a number"))
}
pub use commands::POD_COMMANDS;
pub use commands::history_items;
pub use commands::render_content;

/// rp's `"path"\s*:\s*"/`: a file tool call on an absolute path.
fn absolute_path_arg(args: &str) -> bool {
    key_then_quoted(args, "\"path\"", |rest| rest.starts_with('/'))
}

/// rp's `"command"\s*:\s*"\s*ssh\s`: a command that starts with ssh.
fn ssh_command_arg(args: &str) -> bool {
    key_then_quoted(args, "\"command\"", |rest| {
        let rest = rest.trim_start_matches(is_py_space);
        rest.strip_prefix("ssh")
            .is_some_and(|r| r.starts_with(is_py_space))
    })
}

/// Somewhere in `text`: `key`, whitespace, `:`, whitespace, `"`, then what
/// `then` accepts.
fn key_then_quoted(text: &str, key: &str, then: impl Fn(&str) -> bool) -> bool {
    text.match_indices(key).any(|(i, _)| {
        let rest = text[i + key.len()..].trim_start_matches(is_py_space);
        rest.strip_prefix(':')
            .map(|r| r.trim_start_matches(is_py_space))
            .and_then(|r| r.strip_prefix('"'))
            .is_some_and(&then)
    })
}

#[path = "sandbox_git.rs"]
pub mod sandbox_git;

#[path = "ssh.rs"]
mod ssh;

#[cfg(test)]
#[path = "ssh_tests.rs"]
mod ssh_tests;

#[cfg(test)]
#[path = "sandbox_git_tests.rs"]
mod sandbox_git_tests;

#[cfg(test)]
#[path = "toolbox_chat_tests.rs"]
mod toolbox_chat_tests;

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
