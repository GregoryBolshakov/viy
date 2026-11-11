//! Running the model's tool calls (rpctl chat.py `_run_tool_calls`,
//! `_parallel_ok`, `_call_one`, `_defuse`, `_dispatch_tool`,
//! `_tool_run_shell`, `_approve_auto`'s hearing, `_report_quarantine`).
//!
//! Every call gets a result, in order: a tool that is off, or that viy
//! does not have yet (ask_claude and find_tool until phase 7),
//! answers rpctl's "is off" error, which rpctl gives when the tool is off.
//! web_search and fetch_url run in [`super::web`]. Every result passes
//! [`untrusted::neutralize`] before the model sees it.
//!
//! run_shell runs each command in the sandbox (`viy_tools::sandbox`) after
//! the approval decision (`approve::decide`), moves out what a command
//! made that runs code outside the sandbox, and reports the output. A
//! network command's only way out is the egress proxy
//! (`viy_tools::egress`): its policy is shown before the question and told
//! to the reviewer, and what it reached and blocked comes back in the
//! result. The model's own repository (sandboxgit.py) is not
//! offered, as rpctl in a workspace where it does not apply; the toolbox
//! and /ssh servers come with phases 7 and later.

use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::Instant;

use regex::Regex;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::EventMsg;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::QuestionKind;
use viy_protocol::ToolCallBeginEvent;
use viy_protocol::ToolCallEndEvent;
use viy_protocol::ToolCallOutputDeltaEvent;
use viy_protocol::ToolCallStatus;
use viy_tools::approve;
use viy_tools::approve::Decision;
use viy_tools::approve::Facts;
use viy_tools::approve::Hearing;
use viy_tools::approve::Pending;
use viy_tools::approve::Review;
use viy_tools::approve::Verdict;
use viy_tools::build;
use viy_tools::doctrines;
use viy_tools::egress;
use viy_tools::egress::Policy;
use viy_tools::egress::Proxy;
use viy_tools::guard;
use viy_tools::quarantine;
use viy_tools::sandbox;
use viy_tools::sandbox::Jail;
use viy_tools::sandbox::Net;
use viy_tools::untrusted;

use super::Chat;
use super::notice;
use crate::engine::TurnContext;
use crate::files;
use crate::stream::ToolCall;

/// A command that runs git, for run_shell's `git_note` (rp's `\bgit\b`).
static GIT_WORD: LazyLock<Regex> = LazyLock::new(|| rx(r"\bgit\b"));

/// A shell command's errors that say it had no network.
static NET_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    rx(
        r"(?i)Temporary failure in name resolution|Could not resolve host|Network is unreachable|Name or service not known|getaddrinfo|ENOTFOUND|EAI_AGAIN|NewConnectionError|Failed to establish a new connection",
    )
});

/// A tool that connected directly instead of through the egress proxy
/// (rpctl's `_DIRECT_NET_ERROR`).
static DIRECT_NET_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    rx(&format!(
        "{}|Couldn't connect to server|Connection refused|ECONNREFUSED|ENETUNREACH|connect: \
         Network|No route to host|Failed to connect",
        NET_ERROR.as_str()
    ))
});

/// Commands that usually need the network (some, like `curl -s`, fail
/// silently).
static NET_CMD: LazyLock<Regex> = LazyLock::new(|| {
    rx(
        r"\b(curl|wget|git\s+(clone|fetch|pull|push)|pip3?\s+(install|download)|npm\s+(install|i|ci)|yarn|pnpm|cargo\s+(install|fetch|build)|go\s+(get|mod)|apt|docker\s+pull|uv\s+(pip|sync|add))\b",
    )
});

/// The start of rpctl's `_PAGE_FETCH`: a program that reads web pages.
static PAGE_FETCHER: LazyLock<Regex> =
    LazyLock::new(|| rx(r"\b(curl|wget|lynx|w3m|urlopen|urllib\.request|requests\.get|httpx)\b"));
static URL_START: LazyLock<Regex> = LazyLock::new(|| rx(r"https?://"));

fn rx(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => panic!("tools.rs pattern {pattern:?}: {e}"),
    }
}

/// rpctl's `_PAGE_FETCH`: a page-reading program, then within 2000
/// characters an http(s) URL that is not the machine's own (the lookahead
/// written as code).
fn reads_a_page(cmd: &str) -> bool {
    PAGE_FETCHER.find_iter(cmd).any(|m| {
        let rest = &cmd[m.end()..];
        let window = rest.char_indices().nth(2000).map_or(rest.len(), |(i, _)| i);
        URL_START.find_iter(rest).any(|u| {
            u.start() <= window && {
                let host = &rest[u.end()..];
                !["127.", "localhost", "0.0.0.0", "[::1]"]
                    .iter()
                    .any(|p| host.starts_with(p))
            }
        })
    })
}

/// A call's arguments as an object: empty is `{}`; anything else that is
/// not a JSON object is the error the model gets.
pub(super) fn parse_args(raw: &str) -> Result<Map<String, Value>, Value> {
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(json!({"error": "arguments must be a JSON object"})),
        Err(e) => Err(json!({"error": format!("could not parse arguments: {e}")})),
    }
}

/// A batch call's arguments as `_parallel_ok` reads them:
/// `json.loads(arguments or "{}")`, an object or nothing (whitespace alone
/// is not JSON there, unlike [`parse_args`]).
fn batch_args(arguments: &str) -> Option<Map<String, Value>> {
    let raw = if arguments.is_empty() {
        "{}"
    } else {
        arguments
    };
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(args)) => Some(args),
        _ => None,
    }
}

/// Whether a call's arguments are a JSON object, which vLLM parses every
/// past call's arguments into before rendering the template (rp's
/// `_args_ok`).
pub(crate) fn args_ok(raw: &str) -> bool {
    matches!(serde_json::from_str::<Value>(raw), Ok(Value::Object(_)))
}

/// rp's `needs_network in (True, "true", "True", 1, "1")`.
fn wants_network(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true" || s == "True" || s == "1",
        Some(Value::Number(n)) => n.as_f64() == Some(1.0),
        _ => false,
    }
}

/// How a result ended, for the tool card.
fn status_of(result: &Value) -> ToolCallStatus {
    if result.get("declined").is_some() || result.get("denied").is_some() {
        ToolCallStatus::Declined
    } else if result.get("error").is_some() {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    }
}

/// What the tool card's header shows for a call: the argument rp prints.
fn summary(name: &str, args: &Map<String, Value>) -> String {
    if name == "find_tool" {
        // rp's `[find_tool]` line: the name, else the query, else "all"
        return untrusted::terminal_safe(&crate::toolbox::find_what(args), Some(80));
    }
    let key = match name {
        "run_shell" => "command",
        "web_search" => "query",
        "fetch_url" => "url",
        "ask_claude" => "task",
        _ => "path",
    };
    let shown = match args.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    untrusted::terminal_safe(&shown, Some(300))
}

/// The approval decision's way to the user, the reviewer and the rules
/// file, from inside a turn.
struct ChatHearing<'a> {
    ctx: &'a TurnContext,
    /// Where the user's doctrines are read from when the reviewer is asked
    /// (new in viy, kb d109).
    doctrines: Vec<doctrines::Source>,
    review: Review<'a>,
    http: &'a dyn viy_http::HttpClient,
    rules_path: &'a Path,
}

impl Hearing for ChatHearing<'_> {
    fn say(&mut self, n: NoticeEvent) {
        self.ctx.emit(EventMsg::Notice(n));
    }

    fn ask(&mut self, question: Question) -> Pending<'_, Answer> {
        self.ctx.ask(&question)
    }

    /// rpctl's review, and a second one with the user's doctrines when
    /// there are any; the stricter holds. They are read only now, when a
    /// command reaches the reviewer, off the engine's thread.
    fn review(&mut self) -> Pending<'_, Verdict> {
        let sources = self.doctrines.clone();
        Box::pin(async move {
            let policy = tokio::task::spawn_blocking(move || {
                doctrines::policy(&doctrines::load(&sources).doctrines)
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            approve::review_with_doctrines(&self.review, policy, self.http).await
        })
    }

    fn rules(&mut self) -> Vec<Vec<String>> {
        approve::load_rules(self.rules_path)
    }

    fn save_rule(&mut self, prefix: &[String]) -> Result<(), String> {
        approve::add_rule(self.rules_path, prefix).map_err(|e| e.to_string())
    }
}

impl Chat {
    /// The rules file this chat's network decisions read and "always"
    /// writes: viy's own (kb d090).
    fn rules_path(&self) -> PathBuf {
        self.places.config_dir.join(approve::RULES_FILE)
    }

    /// `Ok` when run_shell can sandbox here, else why not (rp's
    /// `_auto_ready`); bwrap is probed once per chat.
    pub(crate) async fn auto_ready(&mut self) -> Result<(), String> {
        if self.sandbox_check.is_none() {
            self.sandbox_check = Some(sandbox::available(&self.sandbox_host()).await);
        }
        if let Some(Err(why)) = &self.sandbox_check {
            return Err(why.clone());
        }
        let rules = self.rules_path();
        match viy_tools::guard::workspace_problem(&self.places.workspace, &self.places.home, &rules)
        {
            Some(why) => Err(why),
            None => Ok(()),
        }
    }

    pub(crate) fn sandbox_host(&self) -> sandbox::Host {
        sandbox::Host::new(self.places.home.clone(), self.files.clients.clone())
    }

    /// The last three things the user asked, for the reviewer (rp's
    /// `_recent_user_requests`).
    fn recent_user_requests(&self) -> Vec<String> {
        let text = |c: &Value| match c {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        let reqs: Vec<String> = self
            .messages
            .iter()
            .filter(|m| m["role"] == "user")
            .map(|m| text(&m["content"]))
            .chain(self.turn_user.as_ref().map(text))
            .filter(|r| !r.trim().is_empty())
            .collect();
        reqs[reqs.len().saturating_sub(3)..].to_vec()
    }

    /// One refusal more in a row; three stop the turn (rp's
    /// `_count_denial`).
    pub(crate) fn count_denial(&mut self) {
        self.denials += 1;
        if self.denials >= approve::MAX_CONSECUTIVE_DENIALS {
            self.abort_turn = Some(format!("{} requests in a row were refused", self.denials));
        }
    }

    /// The results of one round's calls, in order (rp's `_run_tool_calls`):
    /// a batch that only reads (`_parallel_ok`) side by side, the rest one
    /// by one. Each result is shown to the turn monitor, which may add a
    /// note when the model repeats itself.
    pub(crate) async fn run_tool_calls(
        &mut self,
        ctx: &TurnContext,
        calls: &[ToolCall],
    ) -> Vec<String> {
        if self.search_on && calls.iter().any(|c| c.name == "fetch_url") {
            // fetch_url's batch check reads the vault's key, as rpctl's
            // `_parallel_ok` does through `_fetch_decision`.
            self.web.search.load_key().await;
        }
        let results = if self.abort_turn.is_none() && self.parallel_ok(calls) {
            self.run_parallel(ctx, calls).await
        } else {
            let mut out = Vec::with_capacity(calls.len());
            for c in calls {
                out.push(self.call_one(ctx, c).await);
            }
            out
        };
        calls
            .iter()
            .zip(results)
            .map(|(c, (payload, secs))| {
                match self
                    .monitor
                    .as_mut()
                    .and_then(|m| m.on_tool(&c.name, &c.arguments, &payload, secs))
                {
                    Some(note) => crate::trace::with_note(&payload, &note),
                    None => payload,
                }
            })
            .collect()
    }

    /// Whether a batch runs side by side (rp's `_parallel_ok`): every call
    /// only reads and none of them would stop to ask the user (a secret
    /// file), since a batch never holds a question.
    pub(crate) fn parallel_ok(&mut self, calls: &[ToolCall]) -> bool {
        if !self.parallel_tools || calls.len() < 2 {
            return false;
        }
        calls.iter().all(|c| {
            if !PARALLEL_SAFE.contains(&c.name.as_str()) {
                return false;
            }
            match c.name.as_str() {
                "read_file" => self.reads_without_asking(&c.arguments),
                "fetch_url" => self.fetches_without_asking(&c.arguments),
                _ => true,
            }
        })
    }

    /// Whether fetch_url with these arguments reads without a question.
    fn fetches_without_asking(&mut self, arguments: &str) -> bool {
        let Some(args) = batch_args(arguments) else {
            return false;
        };
        let Some(url) = args.get("url").and_then(Value::as_str) else {
            return false;
        };
        !matches!(self.fetch_decision(url), super::web::FetchDecision::Ask(..))
    }

    /// Whether read_file with these arguments reads without a question.
    fn reads_without_asking(&self, arguments: &str) -> bool {
        let Some(args) = batch_args(arguments) else {
            return false;
        };
        let Some(rel) = args
            .get("path")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        else {
            return false;
        };
        self.files
            .resolve_in_workspace(rel)
            .is_ok_and(|path| guard::secret_reason(&path).is_none())
    }

    /// A read-only batch side by side (rp's thread pool): the file tools on
    /// blocking threads, their results and what they read taken in call
    /// order. A read that has turned secret since [`Chat::parallel_ok`]
    /// runs on its own afterwards, with its question.
    pub(crate) async fn run_parallel(
        &mut self,
        ctx: &TurnContext,
        calls: &[ToolCall],
    ) -> Vec<(String, f64)> {
        let batch = Instant::now();
        for c in calls {
            begin_call(ctx, c);
        }
        let files = Arc::new(self.files.clone());
        let tools_on = self.tools_on;
        let runs = calls.iter().map(|c| {
            let files = Arc::clone(&files);
            let (name, arguments) = (c.name.clone(), c.arguments.clone());
            async move {
                let started = Instant::now();
                let got = quiet_call(files, name, &arguments, tools_on).await;
                (got, started)
            }
        });
        let results = futures_util::future::join_all(runs).await;
        let mut out = Vec::with_capacity(calls.len());
        for (c, (got, mut started)) in calls.iter().zip(results) {
            let (result, card) = match got {
                Some((result, card, seen)) => {
                    if let Some((path, sha1)) = seen {
                        self.files.remember_read(path, sha1);
                    }
                    (result, card)
                }
                // A call that would ask, or a web call, after the batch's
                // reads: timed from its own start, and not run once a third
                // refusal stopped the turn.
                None if self.abort_turn.is_some() => (
                    json!({"error": "not run: the turn was stopped"}),
                    Card::default(),
                ),
                None => {
                    started = Instant::now();
                    self.dispatch(ctx, c).await
                }
            };
            let secs = started.elapsed().as_secs_f64();
            out.push((self.end_call(ctx, c, &result, card, started), secs));
        }
        let round2 = |x: f64| (x * 100.0).round() / 100.0;
        self.tracer.event(
            "parallel_batch",
            json!({
                "turn": self.turn_n,
                "calls": calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                "secs": round2(batch.elapsed().as_secs_f64()),
                "sum_secs": round2(out.iter().map(|(_, s)| s).sum::<f64>()),
            }),
        );
        out
    }

    /// One call's result and its seconds (rp's `_call_one`): not run once
    /// the turn is stopped, else run and defused.
    async fn call_one(&mut self, ctx: &TurnContext, c: &ToolCall) -> (String, f64) {
        if self.abort_turn.is_some() {
            // Every call still needs a result or the history is malformed.
            return (
                json!({"error": "not run: the turn was stopped"}).to_string(),
                0.0,
            );
        }
        let started = Instant::now();
        begin_call(ctx, c);
        let (result, card) = self.dispatch(ctx, c).await;
        let secs = started.elapsed().as_secs_f64();
        (self.end_call(ctx, c, &result, card, started), secs)
    }

    /// Shows a call's end and gives its result, defused.
    fn end_call(
        &self,
        ctx: &TurnContext,
        c: &ToolCall,
        result: &Value,
        card: Card,
        started: Instant,
    ) -> String {
        ctx.emit(EventMsg::ToolCallEnd(ToolCallEndEvent {
            call_id: c.id.clone(),
            status: status_of(result),
            detail: card.detail,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            sandboxed: card.sandboxed,
            network: card.network,
        }));
        let payload = match result {
            // web_search's and fetch_url's text, fenced.
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        self.defuse(ctx, &c.name, payload)
    }

    /// Every tool result passes here: text spelling a chat-control token
    /// would become that token and could end the tool result early or
    /// forge a turn, so those spellings are swapped for look-alikes (rp's
    /// `_defuse`).
    pub(crate) fn defuse(&self, ctx: &TurnContext, name: &str, payload: String) -> String {
        let (payload, n) = untrusted::neutralize(&payload);
        if n == 0 {
            return payload;
        }
        self.tracer
            .event("tokens_defused", json!({"tool": name, "n": n}));
        notice(
            ctx,
            NoticeLevel::Info,
            "defuse",
            format!(
                "  [{n} chat-control token spelling(s) in the {name} result shown as \u{2039}...\u{203a}]"
            ),
        );
        if payload.starts_with('{')
            && let Ok(Value::Object(mut d)) = serde_json::from_str::<Value>(&payload)
        {
            d.insert(
                "control_tokens_escaped".into(),
                json!(format!(
                    "{n} spelling(s) of chat-control tokens such as <|im_end|> or \
                     </tool_response> are shown with \u{2039} \u{203a} instead of < >. The source \
                     has < >. Do not write those lines back with write_file; edit around them, or \
                     change them with sed in run_shell."
                )),
            );
            return Value::Object(d).to_string();
        }
        payload
    }

    /// Routes one call to its tool (rp's `_dispatch_tool`).
    pub(crate) async fn dispatch(&mut self, ctx: &TurnContext, c: &ToolCall) -> (Value, Card) {
        let off = |name: &str| {
            (
                json!({"error": format!("the {name} tool is off")}),
                Card::default(),
            )
        };
        let name = c.name.as_str();
        match name {
            "web_search" | "fetch_url" if !self.search_on => off(name),
            "web_search" => self.run_search(ctx, &c.arguments).await,
            "fetch_url" if !self.fetch_on => off(name),
            "fetch_url" => self.run_fetch(ctx, &c.arguments).await,
            "explore" if !(self.tools_on && self.explore_on) => off(name),
            "explore" => (self.run_explore(ctx, &c.arguments).await, Card::default()),
            "ask_claude" if !self.consult_available() => off(name),
            "ask_claude" => (self.run_consult(ctx, &c.arguments).await, Card::default()),
            "find_tool" if !(self.tools_on && self.toolbox_on) => off(name),
            "find_tool" => {
                let out = match parse_args(&c.arguments) {
                    Ok(args) => crate::toolbox::run_find(&args, &self.live_toolbox()),
                    Err(e) => e,
                };
                (out, Card::default())
            }
            _ if build::NAMES.contains(&name) => {
                if !self.tools_on {
                    let e = format!("the {name} tool is off; enable it with /tools on");
                    return (json!({"error": e}), Card::default());
                }
                let args = match parse_args(&c.arguments) {
                    Ok(a) => a,
                    Err(e) => return (e, Card::default()),
                };
                match name {
                    "write_file" | "read_file" => {
                        let out = if name == "write_file" {
                            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
                            self.write_file(ctx, &c.id, &args, &stamp).await
                        } else {
                            self.files.read_file(&args, ctx).await
                        };
                        // A declined protected write or secret read counts
                        // toward the three refusals, as rp's `_guard_path`.
                        if out.refused {
                            self.count_denial();
                        }
                        (out.result, Card::files())
                    }
                    "list_dir" => (self.files.list_dir(&args).result, Card::files()),
                    _ => self.run_shell(ctx, &c.id, &args).await,
                }
            }
            _ => (
                json!({"error": format!("unknown tool {name}")}),
                Card::default(),
            ),
        }
    }

    /// rp's `_tool_run_shell`, without /ssh servers.
    pub(crate) async fn run_shell(
        &mut self,
        ctx: &TurnContext,
        call_id: &str,
        args: &Map<String, Value>,
    ) -> (Value, Card) {
        let cmd = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if cmd.is_empty() {
            return (json!({"error": "no command given"}), Card::default());
        }
        let need_net = wants_network(args.get("needs_network"));
        let ws = self.places.workspace.clone();
        let _ = std::fs::create_dir_all(&ws);
        let ready = self.auto_ready().await;
        let boxed = ready.is_ok();
        let mut reason: Vec<NoticeEvent> = Vec::new();
        let say = |level, text: String| NoticeEvent {
            level,
            code: "run_shell".into(),
            message: text,
        };
        if let Err(why) = &ready {
            reason.push(say(
                NoticeLevel::Error,
                format!(
                    "  the sandbox is unavailable: {why}. Asking, and running unsandboxed if you \
                     say yes."
                ),
            ));
        }
        // The toolbox is read live, so a tool kept mid-chat runs at once.
        let kit = self.live_toolbox();
        for t in crate::toolbox::used_in(&cmd, &kit) {
            // A friendly name must not hide who wrote the code behind it.
            reason.push(say(
                NoticeLevel::Info,
                format!("  [toolbox] {}", crate::toolbox::label(t)),
            ));
        }
        let net = (boxed && need_net).then(|| sandbox::net_mode(&self.sandbox_net));
        let mut card = Card {
            detail: None,
            sandboxed: boxed,
            network: net.is_some(),
        };
        let policy = (net == Some(Net::Proxy)).then(|| {
            let allowed = self.web.allowed_hosts.iter().map(String::as_str);
            Policy::new(&cmd, allowed, &self.sandbox_net_allow)
        });
        let reach = policy.as_ref().map(Policy::describe);
        if let Some(reach) = &reach {
            reason.push(say(NoticeLevel::Info, format!("  network: {reach}")));
        } else if net == Some(Net::Host) {
            reason.push(say(
                NoticeLevel::Error,
                "  SANDBOX_NET=host: this command shares your machine's network and can reach \
                 your machine's services and the LAN"
                    .into(),
            ));
        }
        if !boxed {
            let question = Question {
                reason,
                ..Question::yes_no(Some(QuestionKind::Unsandboxed), "  run this command?")
            };
            if ctx.ask(&question).await != Answer::Yes {
                card.detail = Some("declined".into());
                return (
                    json!({"declined": true, "note": "the user declined to run this command"}),
                    card,
                );
            }
        } else {
            for r in reason {
                ctx.emit(EventMsg::Notice(r));
            }
            let rules = self.rules_path();
            let loaded = approve::load_rules(&rules);
            let rules_shown = rules.display().to_string();
            let taint = self.web_taint.clone();
            let facts = Facts {
                command: &cmd,
                needs_network: need_net,
                workspace: &ws,
                web_taint: taint.as_deref(),
                rules: &loaded,
                rules_path: &rules_shown,
            };
            let users = self.recent_user_requests();
            let endpoint = self.endpoint.clone();
            let mut hearing = ChatHearing {
                ctx,
                doctrines: self.places.doctrine_sources.clone(),
                review: Review {
                    network: reach.as_deref(),
                    private_git: self.private_git(),
                    ..Review::new(
                        &endpoint.base,
                        &endpoint.key,
                        &self.settings.model,
                        &users,
                        &cmd,
                        &ws,
                    )
                },
                http: endpoint.http.as_ref(),
                rules_path: &rules,
            };
            let decision = approve::decide(&facts, &mut hearing).await;
            if let Decision::Refuse(refusal) = decision {
                card.detail = Some(if refusal.get("denied").is_some() {
                    "denied".into()
                } else {
                    "declined".into()
                });
                self.count_denial();
                return (refusal, card);
            }
            self.denials = 0;
        }

        // The model's own repository, prepared on the chat's first command.
        let git_dir = if boxed {
            self.sandbox_git(ctx).await
        } else {
            None
        };
        let timeout = Duration::from_secs(self.shell_timeout);
        let proxy = policy.map(|p| Arc::new(Proxy::new(p)));
        let argv: [OsString; 3] = ["bash".into(), "-c".into(), cmd.clone().into()];
        let mut moved: Vec<(String, PathBuf)> = Vec::new();
        let mut failed_move = None;
        let ran = if boxed {
            let before = quarantine::autorun_snapshot(&ws);
            // A rw mount gets the workspace's quarantine; nothing new can
            // appear in a ro one.
            let rw: Vec<PathBuf> = self
                .files
                .mounts
                .iter()
                .filter(|m| m.rw)
                .map(|m| m.path.clone())
                .collect();
            let before_rw: Vec<_> = rw.iter().map(|p| quarantine::autorun_snapshot(p)).collect();
            let tmp = match self.sandbox_tmp.path() {
                Ok(p) => Some(p.to_path_buf()),
                Err(e) => {
                    return (
                        json!({"command": cmd, "error": format!("no /tmp for the sandbox: {e}")}),
                        card,
                    );
                }
            };
            let mounts = self.files.mounts.clone();
            let project = self.places.project_dir.clone();
            let ro = if kit.is_empty() {
                Vec::new()
            } else {
                self.places.toolbox.mount_dirs()
            };
            let path_extra = crate::toolbox::path_dirs(&kit);
            let jail = Jail {
                net: net.unwrap_or(Net::Off),
                tmp_dir: tmp.as_deref(),
                ro_paths: &ro,
                path_extra: &path_extra,
                mounts: &mounts,
                git_dir: git_dir.as_deref(),
                ..Jail::new(&ws, &project)
            };
            let egress = proxy.clone().map(|p| p as Arc<dyn sandbox::Egress>);
            let launcher = self.sandbox_launcher;
            let ran = sandbox::run_with(
                &self.sandbox_host(),
                &jail,
                &argv,
                timeout,
                egress,
                launcher,
            )
            .await;
            let dest = self
                .places
                .quarantine_dir
                .join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
            let new = quarantine::autorun_new(&before, &quarantine::autorun_snapshot(&ws));
            if !new.is_empty() {
                let (m, f) = quarantine::quarantine(&new, &ws, &dest);
                moved.extend(m);
                failed_move = f;
            }
            for (i, (root, old)) in rw.iter().zip(&before_rw).enumerate() {
                let new = quarantine::autorun_new(old, &quarantine::autorun_snapshot(root));
                if !new.is_empty() {
                    // A folder of its own per mount: the same name made in
                    // the workspace and a mount must not collide.
                    let dest = dest.join(format!("mount-{}", i + 1));
                    let (m, f) = quarantine::quarantine(&new, root, &dest);
                    moved.extend(
                        m.into_iter()
                            .map(|(r, d)| (root.join(r).display().to_string(), d)),
                    );
                    failed_move = failed_move.or(f);
                }
            }
            ran
        } else {
            let mut env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
            if !kit.is_empty() {
                let base = std::env::var_os("PATH").unwrap_or_default();
                let path = crate::toolbox::with_path(&base, &kit);
                env.retain(|(k, _)| k != "PATH");
                env.push(("PATH".into(), path));
            }
            run_unsandboxed(&argv, &ws, env, timeout).await
        };
        let ran = match ran {
            Ok(r) => r,
            Err(e) => {
                card.detail = Some("failed".into());
                return (
                    json!({"command": cmd, "error": format!("run_shell failed: {e}")}),
                    card,
                );
            }
        };

        let limit = self.files.max_output;
        let out = files::clip(&ran.stdout, limit);
        let err = files::clip(&ran.stderr, limit);
        for chunk in [&out, &err] {
            if !chunk.trim().is_empty() {
                ctx.emit(EventMsg::ToolCallOutputDelta(ToolCallOutputDeltaEvent {
                    call_id: call_id.to_string(),
                    chunk: chunk.trim_end().to_string(),
                }));
            }
        }
        if need_net || !boxed && NET_CMD.is_match(&cmd) {
            self.taint("a network command's output".into());
        }
        if let Some(why) = ran.net_error {
            notice(
                ctx,
                NoticeLevel::Error,
                "run_shell",
                format!("  [network unavailable] {why}"),
            );
            self.tracer
                .event("egress_setup_failed", json!({"reason": why}));
            card.detail = Some("network unavailable".into());
            return (
                json!({"command": cmd, "exit_code": null, "timed_out": false, "error": why,
                       "stdout": out, "stderr": err}),
                card,
            );
        }
        card.detail = Some(if ran.timed_out {
            "timed out".into()
        } else {
            format!("exit {}", ran.exit_code.unwrap_or(-1))
        });
        let mut result = json!({"command": cmd, "exit_code": ran.exit_code,
                                "timed_out": ran.timed_out, "stdout": out, "stderr": err});
        if let Some(proxy) = &proxy {
            result["network"] = report_egress(ctx, &self.tracer, proxy, &ran).into();
        }
        if !moved.is_empty() {
            self.report_quarantine(ctx, &moved);
            result["quarantined"] = json!(moved.iter().map(|(r, _)| r).collect::<Vec<_>>());
            result["quarantine_note"] = json!(
                "these were moved out of the workspace: git repositories, editor and rpctl \
                 config can run code outside the sandbox. Do not recreate them."
            );
        }
        if let Some((path, e)) = failed_move {
            notice(
                ctx,
                NoticeLevel::Error,
                "quarantine",
                format!("  [quarantine failed] {}: {e}", path.display()),
            );
            result["error"] = json!(format!(
                "could not move {} out of the workspace: {e}",
                self.files.ws_rel(&path)
            ));
        }
        if boxed
            && git_dir.is_none()
            && let Some(why) = self.sandbox_git_failed()
            && GIT_WORD.is_match(&cmd)
        {
            result["git_note"] = json!(format!(
                ".git was read-only for this command: your own repository is unavailable ({why})"
            ));
        }
        let failed = !matches!(ran.exit_code, Some(0) | None);
        if boxed
            && !need_net
            && failed
            && (NET_ERROR.is_match(&ran.stderr) || NET_CMD.is_match(&cmd))
        {
            result["hint"] = json!(
                "this ran without network access. If it needs the internet, call run_shell \
                 again with needs_network=true."
            );
        }
        if self.search_on && self.fetch_on && result.get("hint").is_none() && reads_a_page(&cmd) {
            result["hint"] = json!(
                "to read a web page, call fetch_url: clean text, paged, and no network approval."
            );
        }
        (result, card)
    }

    /// The workspace as the project sees it (rp's `_ws_rel_root`).
    fn ws_rel_root(&self) -> String {
        match self.places.workspace.strip_prefix(&self.places.project_dir) {
            Ok(rel) if rel.as_os_str().is_empty() => "./".into(),
            Ok(rel) => format!("{}/", rel.display()),
            Err(_) => self.places.workspace.display().to_string(),
        }
    }

    /// `/tools [on|off]` (rp's `_cmd_tools`).
    pub(crate) async fn tools_command(&mut self, ctx: &TurnContext, arg: &str) {
        let info = |text: String| notice(ctx, NoticeLevel::Info, "tools", text);
        let sub = arg
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_lowercase();
        let mode = ctx.mode().name;
        match sub.as_str() {
            "" => {
                let state = if self.tools_on { "on" } else { "off" };
                info(format!(
                    "build tools {state}; mode {mode}; workspace {}",
                    self.ws_rel_root()
                ));
                let ready = match self.auto_ready().await {
                    Ok(()) => "ready".to_string(),
                    Err(why) => format!("UNAVAILABLE -- {why} (asking instead)"),
                };
                info(format!("  sandbox: {ready}"));
                info(format!("  rules: {}", self.rules_path().display()));
                if self.tools_on {
                    info(
                        "the model can write/read/list files there and run shell commands on this machine"
                            .into(),
                    );
                }
            }
            "on" => {
                self.tools_on = true;
                let _ = std::fs::create_dir_all(&self.places.workspace);
                info(format!(
                    "build tools on: write_file, read_file, list_dir, run_shell. workspace: {}; mode {mode}",
                    self.ws_rel_root()
                ));
                // No server to ask yet (a chat opened without a pod, kb
                // d041): the first turn asks, as when /tools on never ran.
                if self.endpoint.base.is_empty() {
                    return;
                }
                self.probed = true;
                match self.tools_supported().await {
                    (Some(false), note) => notice(
                        ctx,
                        NoticeLevel::Error,
                        "tools",
                        format!(
                            "  but {note}, so the model cannot call tools. Recreate the pod to pick up the parser: viy down && viy up."
                        ),
                    ),
                    (None, note) if !note.is_empty() => {
                        info(format!("  (couldn't verify tool support: {note})"));
                    }
                    _ => {}
                }
            }
            "off" => {
                self.tools_on = false;
                info("build tools off".into());
            }
            // Old muscle memory: the approval modes are gone.
            "ask" | "auto" | "yolo" => notice(
                ctx,
                NoticeLevel::Warn,
                "tools",
                "there is one approval now: every command runs in the sandbox and only risky ones ask. /mode auto-yes answers some of those questions for you.",
            ),
            _ => notice(ctx, NoticeLevel::Warn, "tools", "usage: /tools [on|off]"),
        }
    }

    fn report_quarantine(&self, ctx: &TurnContext, moved: &[(String, PathBuf)]) {
        for (rel, dest) in moved {
            notice(
                ctx,
                NoticeLevel::Error,
                "quarantine",
                format!("  [quarantined] {rel} -> {}", dest.display()),
            );
        }
        notice(
            ctx,
            NoticeLevel::Info,
            "quarantine",
            "  (git repos, editor configs and .rpctl/ run code outside the sandbox, so new ones \
             are moved out)",
        );
    }
}

/// A command run on the host after the user said yes (rp's `run_process`
/// without `reap_group`): its own group, killed on timeout; what it leaves
/// running in the background outlives it.
async fn run_unsandboxed(
    argv: &[OsString],
    ws: &Path,
    env: Vec<(OsString, OsString)>,
    timeout: Duration,
) -> std::io::Result<sandbox::Ran> {
    let running = viy_exec::sandboxed::spawn(viy_exec::sandboxed::Spec {
        program: argv[0].clone(),
        args: argv[1..].to_vec(),
        cwd: ws.to_path_buf(),
        env,
        pass_fds: Vec::new(),
    })?;
    let done = running.finish(timeout, false).await;
    Ok(sandbox::Ran {
        exit_code: done.exit_code,
        timed_out: done.timed_out,
        stdout: done.stdout,
        stderr: done.stderr,
        net_error: None,
    })
}

/// Shows the proxy's `[net]` lines, traces its events and gives the
/// result's `network` entry with its hints (rp's `_report_egress`).
pub(crate) fn report_egress(
    ctx: &TurnContext,
    tracer: &crate::trace::Tracer,
    proxy: &Proxy,
    ran: &sandbox::Ran,
) -> Map<String, Value> {
    for line in proxy.lines() {
        let level = if line.starts_with("[net blocked]") || line.starts_with("[net failed]") {
            NoticeLevel::Warn
        } else {
            NoticeLevel::Info
        };
        let shown = untrusted::terminal_safe(&line, Some(600));
        notice(ctx, level, "run_shell", format!("  {shown}"));
    }
    for e in proxy.events() {
        tracer.event(e.kind, e.fields);
    }
    let mut rep = proxy.report();
    let blocked_host = rep
        .get("blocked")
        .and_then(Value::as_array)
        .is_some_and(|b| b.iter().any(|e| e["why"] == egress::NOT_ALLOWED));
    if blocked_host {
        rep.insert(
            "hint".into(),
            "write the host in the command if it is needed; it then goes through review (or to \
             the user)"
                .into(),
        );
    } else if proxy.total() == 0
        && !matches!(ran.exit_code, Some(0) | None)
        && DIRECT_NET_ERROR.is_match(&format!("{}\n{}", ran.stderr, ran.stdout))
    {
        rep.insert(
            "hint".into(),
            "this tool ignored HTTPS_PROXY: the sandbox has no direct route or DNS, only the \
             proxy in HTTPS_PROXY/HTTP_PROXY. Use the tool's proxy option or a tool that honours \
             it."
            .into(),
        );
    }
    rep
}

/// Tools that only read, so a batch of them can run side by side (rp's
/// `PARALLEL_SAFE`).
pub(crate) const PARALLEL_SAFE: [&str; 6] = [
    "read_file",
    "list_dir",
    "web_search",
    "fetch_url",
    "explore",
    "find_tool",
];

/// Shows a call starting.
fn begin_call(ctx: &TurnContext, c: &ToolCall) {
    let args = parse_args(&c.arguments).unwrap_or_default();
    ctx.emit(EventMsg::ToolCallBegin(ToolCallBeginEvent {
        call_id: c.id.clone(),
        tool: c.name.clone(),
        summary: summary(&c.name, &args),
    }));
}

/// One call of a parallel batch, as [`Chat::dispatch`] answers it but with
/// no question: `None` when the read would ask, and must run on its own.
/// What read_file read comes along, for the write guard.
async fn quiet_call(
    files: Arc<files::FileTools>,
    name: String,
    arguments: &str,
    tools_on: bool,
) -> Option<(Value, Card, Option<(PathBuf, String)>)> {
    if !build::NAMES.contains(&name.as_str()) {
        // web_search, fetch_url and explore hold the chat's web state, and
        // find_tool reads the chat's toolbox: run one by one after the
        // batch's reads, in call order.
        return None;
    }
    if !tools_on {
        let e = format!("the {name} tool is off; enable it with /tools on");
        return Some((json!({"error": e}), Card::default(), None));
    }
    let args = match parse_args(arguments) {
        Ok(a) => a,
        Err(e) => return Some((e, Card::default(), None)),
    };
    let read = tokio::task::spawn_blocking(move || {
        if name == "read_file" {
            files
                .read_quietly(&args)
                .map(|(out, seen)| (out.result, seen))
        } else {
            Some((files.list_dir(&args).result, None))
        }
    })
    .await;
    match read {
        Ok(Some((result, seen))) => Some((result, Card::files(), seen)),
        Ok(None) => None,
        Err(e) => Some((
            json!({"error": format!("the read failed: {e}")}),
            Card::files(),
            None,
        )),
    }
}

/// What the tool card shows besides the status.
#[derive(Debug, Default)]
pub(crate) struct Card {
    pub(crate) detail: Option<String>,
    pub(crate) sandboxed: bool,
    pub(crate) network: bool,
}

impl Card {
    fn files() -> Self {
        Self::default()
    }
}

// Tested through the chat: chat_tests.rs, and the phase-4 scenarios.
