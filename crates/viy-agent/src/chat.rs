//! The chat's turn: rpctl's `Chat.send` and `_send` (chat.py 2699-3058)
//! for a chat whose tools cannot run yet, the background title
//! (`_maybe_start_title`, `_apply_title`) and the startup's max-model-len
//! discovery (`_discover_max_model_len`). [`ChatTurns`] runs it for the
//! engine.
//!
//! A turn sends the system prompt, the history and the user's message; a
//! reply without an answer (reasoning only, or a tool call) is asked for
//! once more with [`NO_ANSWER_NOTICE`] and no tools, a reply cut at the
//! token limit is not; the answer, or a bracketed placeholder, closes the
//! turn, and the reasoning is dropped from the history (rpctl kb d008).
//! A request that fails keeps only what had streamed; a turn interrupted
//! keeps the user's message and what had streamed, marked `[interrupted]`.
//!
//! Not yet: the tool loop and its cap (WP 4.10; web search and fetch, whose
//! schemas a chat already offers, phase 5), so the model's tool calls are
//! dropped as on rp's last round, and the build tools' parts of the system
//! prompt with them (phase 4); context accounting and calibration (WP 3.4); sessions, so the title is kept in
//! memory (WP 3.5); elision and compaction (phase 6); tracing (phase 7).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::task::AbortHandle;
use viy_config::Config;
use viy_http::Method;
use viy_protocol::BackgroundEvent;
use viy_protocol::BackgroundState;
use viy_protocol::BackgroundTask;
use viy_protocol::EventMsg;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::RoundKind;
use viy_protocol::RoundStartedEvent;
use viy_protocol::TitleChangedEvent;
use viy_protocol::TurnOutcome;

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
use crate::stream::Endpoint;
use crate::stream::Round;
use crate::stream::Sink;
use crate::stream::stream_once;

/// Keeps the pod alive while a reply streams: `viy_pod::idle::touch`.
pub type Touch = Arc<dyn Fn() + Send + Sync>;

/// Where a chat looks for files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Places {
    /// The workspace ([`crate::prompt::workspace`]).
    pub workspace: PathBuf,
    /// viy's config directory, for global instruction files.
    pub config_dir: PathBuf,
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
    /// CHAT_AUTO_TITLE, and CHAT_AUTOSAVE: the title is the session's.
    auto_title: bool,
    /// The system prompt's date, fixed at the chat's start.
    today: String,
    max_model_len: u64,
    title: Arc<Mutex<Title>>,
    /// The turn in progress: its user message, until the turn closes.
    inflight: Option<Vec<Value>>,
    /// What the round in progress has streamed of its answer.
    partial: String,
}

/// The background title.
#[derive(Default)]
struct Title {
    running: Option<AbortHandle>,
    /// A model named this chat; it is not asked again.
    by_model: bool,
    title: Option<String>,
}

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
            auto_title: on("CHAT_AUTO_TITLE", "1") && on("CHAT_AUTOSAVE", "1"),
            today,
            max_model_len: cfg
                .get("MAX_MODEL_LEN")
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(65536),
            title: Arc::default(),
            inflight: None,
            partial: String::new(),
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

    /// The title a model gave this chat, if any.
    pub fn title(&self) -> Option<String> {
        lock(&self.title).title.clone()
    }

    /// Asks the server what context it was launched with: MAX_MODEL_LEN is
    /// only what was asked for, and a running pod can predate a config
    /// edit. Keeps the configured value when the server does not say.
    pub async fn discover_max_model_len(&mut self) -> u64 {
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

    /// Every tool offered to the model.
    fn tool_specs(&self) -> Vec<Value> {
        let mut specs = Vec::new();
        if self.search_on {
            specs.push(viy_tools::web::search_tool());
            if self.fetch_on {
                specs.push(viy_tools::web::fetch_tool());
            }
        }
        specs
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
        SystemPrompt {
            user: self.system.as_deref(),
            // The build tools, explore, ask_claude and the toolbox come with
            // phases 4 and 7.
            tools_on: false,
            search_on: self.search_on,
            guide_on: self.guide_on,
            explore_trust: false,
            consult: false,
            toolbox_index: None,
            workspace: &self.places.workspace,
            today: &self.today,
            instructions: block.as_deref(),
        }
        .render()
    }

    /// One user turn; the title is asked for after it.
    pub async fn send(&mut self, ctx: &TurnContext, content: Value) -> TurnOutcome {
        self.inflight = None;
        self.partial.clear();
        let outcome = self.turn(ctx, content).await;
        self.maybe_start_title(ctx);
        self.inflight = None;
        outcome
    }

    async fn turn(&mut self, ctx: &TurnContext, content: Value) -> TurnOutcome {
        let user = json!({"role": "user", "content": content});
        let mut msgs = Vec::new();
        if let Some(system) = self.system_prompt() {
            msgs.push(json!({"role": "system", "content": system}));
        }
        msgs.extend(self.messages.iter().cloned());
        msgs.push(user.clone());
        self.inflight = Some(vec![user]);
        let specs = self.tool_specs();

        // With no tool rounds allowed the first request is already the last:
        // tools forbidden and the model told so.
        let last_round = self.max_rounds == 0;
        let first = if last_round {
            notice(
                ctx,
                NoticeLevel::Warn,
                "tool_budget",
                format!(
                    "[tool-call budget reached ({} rounds); asking for the answer]",
                    self.max_rounds
                ),
            );
            self.round(
                ctx,
                1,
                RoundKind::CappedFinal,
                &with_notice(&msgs, TOOL_BUDGET_NOTICE),
                &specs,
                Tools::None,
            )
            .await
        } else {
            self.round(ctx, 1, RoundKind::Turn, &msgs, &specs, Tools::Auto)
                .await
        };
        let Some(mut out) = first else {
            // A request that failed before anything happened is dropped.
            self.close_inflight("interrupted: the request failed", false);
            return TurnOutcome::Failed {
                message: "the request failed".into(),
            };
        };
        self.partial.clear();
        let mut usage = out.usage.clone();
        let mut text = out.text.clone();
        // A call cannot run here, so it is dropped as on rp's last round.
        let dropped_call = !out.tool_calls.is_empty();
        let mut answer_thought = !out.reasoning.is_empty();

        // Reasoning but no answer text, or a call made where none can run:
        // asked once more with no tools in the request at all. That misses
        // the prefix cache, but a turn without an answer is worse.
        if (dropped_call || text.trim().is_empty()) && out.finish.as_deref() != Some("length") {
            notice(
                ctx,
                NoticeLevel::Warn,
                "no_answer_retry",
                "[the reply had no answer; asking for it once more]",
            );
            let sent = with_notice(&msgs, NO_ANSWER_NOTICE);
            if let Some(retry) = self
                .round(ctx, 2, RoundKind::Retry, &sent, &specs, Tools::Omit)
                .await
            {
                out = retry;
                text.clone_from(&out.text);
                answer_thought |= !out.reasoning.is_empty();
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
        if text.trim().is_empty() {
            // Never commit a silent, empty answer: the reasoning behind it
            // is dropped from history, so the next turn would have nothing.
            text = if cut {
                "[no answer: the reply hit the token limit first.]".into()
            } else {
                "[no answer: the model stopped without writing one. Ask again -- its tool \
                 results are kept in the history.]"
                    .into()
            };
            notice(ctx, NoticeLevel::Info, "no_answer", text.clone());
        }

        let mut answer = json!({"role": "assistant", "content": text});
        if answer_thought {
            answer["thought"] = json!(true);
        }
        let mut turn = self.inflight.take().unwrap_or_default();
        turn.push(answer);
        self.messages.extend(without_reasoning(&turn));
        TurnOutcome::Answered
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
        stream_once(&self.endpoint, &body, &sink, &mut self.partial).await
    }

    /// Commits an unfinished turn as a well-formed exchange: it ends on an
    /// assistant message carrying whatever had streamed, plus why it
    /// stopped. With `keep_bare` false, a turn with nothing to show is
    /// dropped.
    fn close_inflight(&mut self, reason: &str, keep_bare: bool) {
        let Some(mut turn) = self.inflight.take() else {
            return;
        };
        let partial = self.partial.trim();
        if turn.len() == 1 && partial.is_empty() && !keep_bare {
            return;
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

    /// After a reply, names the chat in the background, once a model has
    /// given a title; the request does not hold up the next turn.
    fn maybe_start_title(&mut self, ctx: &TurnContext) {
        if !self.auto_title {
            return;
        }
        let mut title = lock(&self.title);
        if title.by_model || title.running.is_some() {
            return;
        }
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
        let state = Arc::clone(&self.title);
        let ctx = ctx.clone();
        ctx.emit(background(BackgroundState::Started));
        let task = tokio::spawn(async move {
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
            {
                let mut title = lock(&state);
                title.running = None;
                if let Some(name) = &named {
                    title.by_model = true;
                    title.title = Some(name.clone());
                }
            }
            let state = match named {
                Some(title) => {
                    ctx.emit(EventMsg::TitleChanged(TitleChangedEvent { title }));
                    BackgroundState::Done
                }
                None => BackgroundState::Failed,
            };
            ctx.emit(background(state));
        });
        title.running = Some(task.abort_handle());
    }
}

impl Drop for Chat {
    fn drop(&mut self) {
        if let Some(task) = lock(&self.title).running.take() {
            task.abort();
        }
    }
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
}

impl ChatTurns {
    pub fn new(chat: Chat) -> Self {
        Self {
            slot: Arc::new(Mutex::new(Some(chat))),
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
}

impl Lease {
    fn take(slot: &Arc<Mutex<Option<Chat>>>) -> Option<Self> {
        let chat = lock(slot).take()?;
        Some(Self {
            slot: Arc::clone(slot),
            chat: Some(chat),
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(mut chat) = self.chat.take() {
            // Nothing in flight once a turn has ended on its own.
            chat.close_inflight("interrupted", true);
            *lock(&self.slot) = Some(chat);
        }
    }
}

impl Turns for ChatTurns {
    fn turn(self: Arc<Self>, ctx: TurnContext, text: String) -> BoxFuture<'static, TurnOutcome> {
        Box::pin(async move {
            let Some(mut lease) = Lease::take(&self.slot) else {
                return TurnOutcome::Failed {
                    message: "the chat is busy".into(),
                };
            };
            match lease.chat.as_mut() {
                Some(chat) => chat.send(&ctx, Value::String(text)).await,
                None => TurnOutcome::Failed {
                    message: "the chat is busy".into(),
                },
            }
        })
    }

    fn command(self: Arc<Self>, ctx: TurnContext, text: String) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            notice(
                &ctx,
                NoticeLevel::Warn,
                "not_ported",
                format!("{text}: slash commands come with WP 3.12"),
            );
        })
    }
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
