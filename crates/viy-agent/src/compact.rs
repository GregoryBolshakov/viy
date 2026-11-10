//! Context management (rpctl chat.py `_maybe_elide_tool_results`,
//! `_elide_before`, `_apply_elision`, `_maybe_auto_compact`, `_compact` and
//! its steps, `_cmd_autocompact`; rpctl kb d014).
//!
//! Compaction shrinks the history as cheaply as will do: first without the
//! model, eliding the tool output of every turn but the newest when that
//! alone gets under COMPACT_TRIM_TARGET; then a summary a background pass
//! already wrote, if the history has not moved under it; then a model pass
//! that summarises everything after the last summary but the newest turn,
//! appended to the earlier summary (rewritten as one by the background pass
//! once the summaries pass COMPACT_CONSOLIDATE_AT). Elision before each
//! turn comes first of all:
//!
//! Tool output is most of a chat's history (75% in Gregory's saved chats,
//! rpctl's comment; 81-93% of real chats, rpctl kb f101) and rarely needed
//! once its turn is over: the model can run the tool again. Once the
//! history passes TOOL_ELIDE_THRESHOLD of the prompt budget, the tool
//! results and write_file contents of the turns before the last
//! TOOL_ELIDE_KEEP_TURNS are replaced by a short note, in one batch per
//! crossing (each change to old messages costs a prefix-cache miss from
//! there on). The session log keeps the originals: the next checkpoint
//! writes the new history as a `replace`. Codex keeps the same split: tool
//! output is truncated in the live history, the rollout keeps it whole
//! (core/src/context_manager/history.rs).
//!
//! The elision itself ([`elide_before`]) is a pure function of the
//! history, so compaction (WP 6.2) can use it for its trim.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;
use tokio::task::JoinHandle;
use viy_http::Method;
use viy_protocol::EventMsg;
use viy_protocol::NoticeLevel;

use super::Chat;
use super::lock;
use super::notice;
use crate::context::schema_chars;
use crate::engine::TurnContext;
use crate::payload::Tools;
use crate::stream::Round;
use crate::stream::Sink;
use crate::stream::stream_once;

/// Tool output and write_file contents shorter than this are kept: the
/// note replacing them would not be much shorter.
pub const ELIDE_MIN_CHARS: usize = 400;
/// How much of an elided tool result its note keeps: enough for the path,
/// the exit code or the first lines, so the model knows what the call
/// found.
pub const ELIDE_HEAD_CHARS: usize = 200;

/// The tools whose results stay: explore's and ask_claude's are already
/// the short, distilled form, and the only way back to them is running
/// the helpers or Claude again for minutes.
const KEPT_TOOLS: [&str; 2] = ["explore", "ask_claude"];

/// A history with old tool output elided, and what changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Elision {
    pub messages: Vec<Value>,
    /// Outputs and write_file contents elided.
    pub n: usize,
    /// Characters saved (negative only if a note outgrew its text).
    pub saved: isize,
    /// Write-guard keys whose contents leave the context.
    pub forget: HashSet<PathBuf>,
    /// Write-guard keys a turn still in full read or wrote.
    pub kept: HashSet<PathBuf>,
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// `messages` with the tool results and write_file contents before index
/// `cut` replaced by a note (rp's `_elide_before`). `call_path` is the
/// write guard's key for a read_file or write_file call.
pub fn elide_before(
    messages: &[Value],
    cut: usize,
    call_path: &dyn Fn(&Value) -> Option<PathBuf>,
) -> Elision {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut paths: HashMap<String, Option<PathBuf>> = HashMap::new();
    let (mut saved, mut n) = (0isize, 0usize);
    let mut forget = HashSet::new();
    let mut out = Vec::with_capacity(messages.len());
    let id_of = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    for (i, m) in messages.iter().enumerate() {
        if i >= cut {
            out.push(m.clone());
            continue;
        }
        for tc in m["tool_calls"].as_array().into_iter().flatten() {
            let name = tc["function"]["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("tool");
            names.insert(id_of(&tc["id"]), name.to_string());
            paths.insert(id_of(&tc["id"]), call_path(tc));
        }
        let call_id = id_of(&m["tool_call_id"]);
        let name = names.get(&call_id).map(String::as_str);
        let content = m["content"].as_str().unwrap_or_default();
        if m["role"] == "tool"
            && chars(content) > ELIDE_MIN_CHARS
            && !name.is_some_and(|n| KEPT_TOOLS.contains(&n))
        {
            if name == Some("read_file")
                && let Some(Some(p)) = paths.get(&call_id)
            {
                forget.insert(p.clone());
            }
            let head: String = content.chars().take(ELIDE_HEAD_CHARS).collect();
            let head = head.split_whitespace().collect::<Vec<_>>().join(" ");
            let note = format!(
                "[elided: this {} result ({} chars) was removed to save context. It began: \
                 {head} ... Run the tool again if you need it.]",
                name.unwrap_or("tool"),
                chars(content)
            );
            saved += chars(content) as isize - chars(&note) as isize;
            n += 1;
            let mut m = m.clone();
            m["content"] = Value::String(note);
            out.push(m);
            continue;
        }
        let Some(calls) = m["tool_calls"].as_array().filter(|c| !c.is_empty()) else {
            out.push(m.clone());
            continue;
        };
        let mut new_calls = Vec::with_capacity(calls.len());
        for tc in calls {
            let mut tc = tc.clone();
            if tc["function"]["name"] == "write_file"
                && let Some(raw) = tc["function"]["arguments"].as_str()
                && let Ok(Value::Object(mut args)) =
                    serde_json::from_str::<Value>(if raw.is_empty() { "{}" } else { raw })
                && let Some(body) = args.get("content").and_then(Value::as_str)
                && chars(body) > ELIDE_MIN_CHARS
            {
                let note = format!(
                    "[elided: {} chars written; read_file to see the file]",
                    chars(body)
                );
                args.insert("content".into(), Value::String(note));
                let new = Value::Object(args).to_string();
                saved += chars(raw) as isize - chars(&new) as isize;
                n += 1;
                if let Some(p) = call_path(&tc) {
                    forget.insert(p);
                }
                tc["function"]["arguments"] = Value::String(new);
            }
            new_calls.push(tc);
        }
        let mut m = m.clone();
        m["tool_calls"] = Value::Array(new_calls);
        out.push(m);
    }
    let kept = messages[cut.min(messages.len())..]
        .iter()
        .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
        .filter_map(call_path)
        .collect();
    Elision {
        messages: out,
        n,
        saved,
        forget,
        kept,
    }
}

/// TOOL_ELIDE_THRESHOLD and TOOL_ELIDE_KEEP_TURNS; both rpctl's defaults
/// (0.6, 2) when either is not a number.
pub fn elide_settings(threshold: Option<&str>, keep: Option<&str>) -> (f64, usize) {
    let threshold = threshold.unwrap_or("0.6").trim().parse::<f64>();
    let keep = keep.unwrap_or("2").trim().parse::<usize>();
    match (threshold, keep) {
        (Ok(t), Ok(k)) => (t, k),
        _ => (0.6, 2),
    }
}

impl Chat {
    /// Before a turn: once the history with `content` passes the elision
    /// threshold, elide the old tool output, and say so (rp's
    /// `_maybe_elide_tool_results`).
    pub(crate) fn maybe_elide_tool_results(&mut self, ctx: &TurnContext, content: &Value) {
        if self.messages.is_empty() || self.elide_threshold <= 0.0 {
            return;
        }
        let (_, frac) = self.context_state(&[json!({"role": "user", "content": content})]);
        if frac < self.elide_threshold {
            return;
        }
        let users: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "user")
            .map(|(i, _)| i)
            .collect();
        if users.len() <= self.elide_keep_turns {
            return;
        }
        let cut = if self.elide_keep_turns == 0 {
            self.messages.len()
        } else {
            users[users.len() - self.elide_keep_turns]
        };
        let el = {
            let files = &self.files;
            elide_before(&self.messages, cut, &|tc| files.call_path(tc))
        };
        if el.n == 0 {
            return;
        }
        let saved_tokens = (el.saved as f64 / self.estimator.chars_per_token) as i64;
        let n = el.n;
        self.apply_elision(el);
        notice(
            ctx,
            NoticeLevel::Info,
            "elision",
            format!(
                "  [context {:.0}% of budget: elided {n} old tool outputs, ~{saved_tokens} \
                 tokens; the session log keeps them]",
                frac * 100.0
            ),
        );
    }

    /// Takes the elided history (rp's `_apply_elision`). The write guard
    /// trusts that a file read in this chat is known to the model; once
    /// its contents are elided that is no longer true, so the model must
    /// read it again before overwriting it, unless a turn still in full
    /// read or wrote it.
    pub(crate) fn apply_elision(&mut self, el: Elision) {
        self.messages = el.messages;
        let gone: Vec<PathBuf> = el.forget.difference(&el.kept).cloned().collect();
        self.files.forget_paths(&gone);
    }
}

// ---- compaction -----------------------------------------------------------

/// The compaction prompt's headings and rules: it asks for a structured,
/// exhaustive summary and says the summary replaces the conversation.
pub const COMPACT_BODY: &str = "\
Structure the summary under these headings (drop a heading only if it is truly \
empty):

1. Intent and requests -- what the user is ultimately trying to do, and every    \
distinct thing they have asked for, in order. Quote the user's own words    \
for their key requests.
2. Key facts and decisions -- technical details, names, versions, paths,    \
flags, the choices made and the reasons for them, any preference or    \
constraint the user stated, and the findings of explore reports with their    \
file:line citations.
3. Artifacts -- files, code, commands, configs, and outputs produced or    \
discussed. Include the important code and text closely enough to    \
reconstruct it, not just name it.
4. State and progress -- what is done and working, what failed and why, and    \
anything currently in flight.
5. Open threads and next steps -- unfinished tasks, unanswered questions, and    \
what would naturally come next.

If the conversation opens with a summary of an earlier part, carry \
everything in it that still matters into this one: it will be replaced too. \
Leave out the system prompt and the project instruction files (AGENTS.md, \
CLAUDE.md, QWEN.md): they are sent again with every request.

Rules: be faithful -- invent nothing, resolve nothing, soften nothing. Keep \
exact names, numbers, paths, flags, and error text. Do NOT perform any pending \
task now; only describe it. Output the summary and nothing else.";

/// A first summary, or a rewrite of an earlier one with what followed it.
pub fn compact_instruction() -> String {
    format!(
        "Write a detailed summary of the conversation above. This summary will REPLACE \
the conversation in your context: from your next reply on it is all you will \
remember, so anything you leave out is lost. Be thorough and specific -- err \
on the side of keeping more.

{COMPACT_BODY}"
    )
}

/// A summary of only what came after an earlier summary, appended to it.
pub const COMPACT_INCREMENTAL: &str = "\
The first message above is a summary of the earlier part of this \
conversation; it stays as it is. Summarize ONLY what comes after it@START@. \
Your text is appended after the earlier summary, so restating anything from \
it wastes the space this is meant to save: do not repeat it, only say what \
changed (a decision reversed, a task finished, a value updated). Keep it \
under about @WORDS@ words.

Use these headings, and drop any with nothing new:

1. New requests -- what the user asked for after the earlier summary, quoting \
their key words.
2. New facts and decisions -- details, names, paths, numbers, choices and \
their reasons, findings of explore reports with their file:line citations, \
and anything that changes the earlier summary.
3. Artifacts -- files written or changed, commands run and their results.
4. State and open threads -- what is done, what failed, what is in flight, \
what comes next.

Rules: be faithful -- invent nothing. Keep exact names, numbers, paths, flags \
and error text. Leave out the system prompt and the project instruction \
files. Do NOT perform any pending task now. Output only the new part.";

/// Opens the history a compaction leaves behind.
pub const SUMMARY_HEAD: &str = "[Summary of the conversation so far. Continue from here.]";
/// Joins a summary of a later part onto the earlier summary.
pub const SUMMARY_NEXT: &str = "\n\n---\n[Summary of the next part of the conversation]\n\n";
pub const SUMMARY_ACK: &str = "Understood. I'll continue from the summary above.";
/// The newest turn stays verbatim after a summary unless it is larger than
/// this share of the budget: the next message usually refers to it.
pub const KEEP_RECENT_MAX_FRAC: f64 = 0.2;

/// COMPACT_INCREMENTAL for `chunk`: where it starts, and a length that grows
/// with it (about one word per 8 tokens summarized, 150-1500).
pub fn incremental_instruction(chunk: &[Value], chunk_tokens: u64) -> String {
    let first = chunk
        .iter()
        .find(|m| m["role"] == "user" && m["content"].is_string())
        .and_then(|m| m["content"].as_str())
        .filter(|c| !c.is_empty());
    let start = first.map_or(String::new(), |first| {
        let flat: String = first
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect();
        format!(
            ", starting with the user's message that begins \"{}\"",
            flat.replace('"', "'")
        )
    });
    let words = (chunk_tokens / 8).clamp(150, 1500);
    COMPACT_INCREMENTAL
        .replace("@START@", &start)
        .replace("@WORDS@", &words.to_string())
}

/// Whether `new` repeats much of `earlier` line for line: the model rewrote
/// the whole summary instead of adding to it.
pub fn restates(earlier: &str, new: &str) -> bool {
    let lines: HashSet<String> = earlier
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| l.chars().count() > 40)
        .collect();
    if lines.len() < 5 {
        return false;
    }
    let flat = new.split_whitespace().collect::<Vec<_>>().join(" ");
    let found = lines.iter().filter(|l| flat.contains(l.as_str())).count();
    found as f64 >= 0.3 * lines.len() as f64
}

/// Whether `msgs` opens with the summary and acknowledgement an earlier
/// compaction wrote.
pub fn is_summary_pair(msgs: &[Value]) -> bool {
    msgs.len() >= 2
        && msgs[0]["role"] == "user"
        && msgs[0]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with(crate::sessions::SUMMARY_PREFIX))
        && msgs[1]["role"] == "assistant"
}

/// What a summary pass does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A first summary.
    Full,
    /// What followed the last summary, appended to it.
    Incremental,
    /// Every summary and what followed, rewritten as one (the background
    /// pass only).
    Consolidate,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Incremental => "incremental",
            Self::Consolidate => "consolidate",
        }
    }
}

/// What one summary pass sends and replaces (rp's `_plan_summary`).
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The earlier summary and acknowledgement, kept as the prefix.
    pub pinned: Vec<Value>,
    /// What gets summarized: everything after `pinned` but the newest turn,
    /// oldest turns dropped when the pass would not fit.
    pub chunk: Vec<Value>,
    /// Where the chunk ends in the history; from there on stays verbatim.
    pub end: usize,
    pub dropped: usize,
    pub mode: Mode,
    /// COMPACT_THINKING.
    pub think: bool,
    /// The request: the system prompt, `pinned`, `chunk`, the instruction.
    pub msgs: Vec<Value>,
    /// The same with only the user's own system prompt, for the retry
    /// without tools.
    pub plain_msgs: Vec<Value>,
}

/// COMPACT_TRIM_TARGET, COMPACT_BACKGROUND_AT, COMPACT_CONSOLIDATE_AT: each
/// its default when it is not a number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fractions {
    pub trim_target: f64,
    pub background_at: f64,
    pub consolidate_at: f64,
}

impl Fractions {
    pub fn from_config(cfg: &viy_config::config::Config) -> Self {
        let frac = |key: &str, default: f64| {
            cfg.get(key)
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default)
        };
        Self {
            trim_target: frac("COMPACT_TRIM_TARGET", 0.5),
            background_at: frac("COMPACT_BACKGROUND_AT", 0.7),
            consolidate_at: frac("COMPACT_CONSOLIDATE_AT", 0.25),
        }
    }
}

/// The history as a background summary saw it: which messages it covers
/// and what the user said in them (elision rewrites tool output only, so
/// it does not invalidate the summary).
type Fingerprint = (Option<String>, usize, Vec<(Value, Option<String>)>);

/// What a summary request answered.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub text: String,
    pub reasoning: String,
    pub usage: Option<Value>,
    pub finish: Option<String>,
    /// When the first reasoning or answer text arrived (streamed passes).
    pub first_token: Option<tokio::time::Instant>,
}

impl From<Round> for Reply {
    fn from(r: Round) -> Self {
        Self {
            text: r.text,
            reasoning: r.reasoning,
            usage: r.usage,
            finish: r.finish,
            first_token: r.first_token,
        }
    }
}

/// The background summary in flight (rp's `_bg`).
pub(crate) struct Background {
    plan: Plan,
    fp: Fingerprint,
    job: JoinHandle<(Option<Reply>, bool, f64)>,
}

/// "ok", or why a summary cannot be used.
fn summary_status(out: Option<&Reply>, summary: &str) -> &'static str {
    match out {
        None => "failed",
        Some(r) if r.finish.as_deref() == Some("length") => "length",
        Some(_) if summary.is_empty() => "empty",
        Some(_) => "ok",
    }
}

/// `f` as Python's `{:.0%}`.
fn pct(f: f64) -> String {
    format!("{:.0}%", f * 100.0)
}

impl Chat {
    fn tokens_without_schemas(&self, msgs: &[Value]) -> u64 {
        self.estimator.tokens(msgs, 0)
    }

    fn with_system(&self, msgs: &[Value]) -> Vec<Value> {
        let mut out = Vec::with_capacity(msgs.len() + 1);
        if let Some(system) = self.system_prompt() {
            out.push(json!({"role": "system", "content": system}));
        }
        out.extend(msgs.iter().cloned());
        out
    }

    /// Before a turn that would not fit: compact first (rp's
    /// `_maybe_auto_compact`), at a turn boundary, where the history is a
    /// clean sequence of finished turns.
    pub(crate) async fn maybe_auto_compact(&mut self, ctx: &TurnContext, content: &Value) {
        if !self.auto_compact || self.messages.is_empty() {
            return;
        }
        let probe = [json!({"role": "user", "content": content})];
        let (tok, frac) = self.context_state(&probe);
        if frac < self.auto_compact_threshold {
            self.compact_failed = false; // back under; re-arm for later
            return;
        }
        if self.compact_failed {
            return;
        }
        let budget = self.prompt_budget();
        notice(
            ctx,
            NoticeLevel::Warn,
            "compact",
            format!(
                "  [context ~{tok}/{budget} tokens ({}) -- auto-compacting first]",
                pct(frac)
            ),
        );
        if self.compact(ctx, "", true, &probe).await {
            let (new_tok, new_frac) = self.context_state(&probe);
            notice(
                ctx,
                NoticeLevel::Info,
                "compact",
                format!(
                    "  context ~{tok} -> ~{new_tok} tokens ({} of budget)",
                    pct(new_frac)
                ),
            );
            if new_frac >= 1.0 {
                notice(
                    ctx,
                    NoticeLevel::Warn,
                    "compact",
                    "  [still over budget -- this one message may be too large for the window \
                     on its own]",
                );
            }
        } else {
            self.compact_failed = true;
            notice(
                ctx,
                NoticeLevel::Warn,
                "compact",
                "  [auto-compaction failed; sending anyway. If the pod rejects it: /new starts \
                 a fresh chat; this one stays saved]",
            );
        }
    }

    /// Shrinks the history as cheaply as will do (rp's `_compact`). True
    /// when the history was replaced. `arg` is `/compact`'s: `full` or any
    /// focus skips the trim; `extra` is the message about to be sent.
    pub(crate) async fn compact(
        &mut self,
        ctx: &TurnContext,
        arg: &str,
        auto: bool,
        extra: &[Value],
    ) -> bool {
        if self.messages.is_empty() {
            notice(ctx, NoticeLevel::Info, "compact", "nothing to compact");
            return false;
        }
        let mut focus = arg.trim().to_string();
        let full = !focus.is_empty();
        if focus.split_whitespace().next() == Some("full") {
            focus = focus.chars().skip(4).collect::<String>().trim().to_string();
        }
        if !full {
            if self.compact_by_trimming(ctx, extra, auto) {
                return true;
            }
            if self.take_background(ctx, true, auto).await {
                let (_, frac) = self.context_state(extra);
                if frac < self.auto_compact_threshold {
                    return true;
                }
                // still too big: summarize what came after it as well
            }
        }
        let focus = (!focus.is_empty()).then_some(focus);
        let Some(plan) = self.plan_summary(focus.as_deref(), false) else {
            notice(
                ctx,
                NoticeLevel::Info,
                "compact",
                "nothing new to compact since the last summary",
            );
            return false;
        };
        if plan.chunk.is_empty() {
            notice(
                ctx,
                NoticeLevel::Error,
                "compact",
                "even a single turn will not fit in a compaction pass; raise MAX_MODEL_LEN or \
                 lower /maxtok. History unchanged.",
            );
            return false;
        }
        self.backup_before_compaction(ctx);
        let n = self.messages.len();
        let lead = if auto {
            "auto-compacting"
        } else {
            "compacting"
        };
        let part = match plan.mode {
            Mode::Full => "",
            Mode::Incremental => " since the last summary",
            Mode::Consolidate => " and merging the earlier summaries",
        };
        notice(
            ctx,
            NoticeLevel::Info,
            "compact",
            format!(
                "{lead} {} of {n} messages{part} -- one model pass; watch the spinner",
                plan.chunk.len()
            ),
        );
        if plan.dropped > 0 {
            let after = if plan.pinned.is_empty() {
                ""
            } else {
                " after the earlier summary"
            };
            notice(
                ctx,
                NoticeLevel::Warn,
                "compact",
                format!(
                    "  history exceeds one compaction pass; the {} oldest messages{after} are \
                     dropped from the summary (the session log or backup keeps them)",
                    plan.dropped
                ),
            );
        }
        let t0 = Instant::now();
        let mut t_req = tokio::time::Instant::now();
        let specs = self.tool_specs();
        let mut out = self
            .summary_pass(ctx, &plan.msgs, &specs, Tools::None, plan.think)
            .await;
        let mut retried = false;
        if out
            .as_ref()
            .is_some_and(|o| o.text.trim().is_empty() && o.finish.as_deref() != Some("length"))
            && schema_chars(&specs) > 0
        {
            // With tool_choice "none" vLLM discards a tool call the model
            // makes anyway, leaving an empty reply (rpctl kb f067). Once more
            // without the tools in the prompt; that one misses the cache.
            retried = true;
            t_req = tokio::time::Instant::now();
            out = self
                .summary_pass(ctx, &plan.plain_msgs, &specs, Tools::Omit, plan.think)
                .await;
        }
        let summary = out
            .as_ref()
            .map(|o| o.text.trim().to_string())
            .unwrap_or_default();
        let status = summary_status(out.as_ref(), &summary);
        self.trace_compact(
            out.as_ref(),
            status,
            auto,
            t0.elapsed().as_secs_f64(),
            Some(t_req),
            n,
            &plan,
            retried,
            false,
        );
        if !self.report_failed_summary(ctx, status, &summary) {
            return false;
        }
        let usage = out.and_then(|o| o.usage).unwrap_or(Value::Null);
        self.apply_summary(&plan, &summary);
        self.report_summary(
            ctx,
            &summary,
            n,
            usage["prompt_tokens"].as_u64(),
            usage["completion_tokens"].as_u64(),
            "",
        );
        true
    }

    /// One summary request, its text kept from the screen.
    async fn summary_pass(
        &mut self,
        ctx: &TurnContext,
        msgs: &[Value],
        specs: &[Value],
        tools: Tools,
        think: bool,
    ) -> Option<Reply> {
        let body = self.settings.payload(msgs, specs, tools, think);
        let emit = |msg: EventMsg| {
            if !matches!(
                msg,
                EventMsg::ReasoningContentDelta(_) | EventMsg::AgentMessageContentDelta(_)
            ) {
                ctx.emit(msg);
            }
        };
        let sink = Sink {
            emit: &emit,
            touch: self.touch.as_ref(),
        };
        let mut text = String::new();
        stream_once(&self.endpoint, &body, &sink, &mut text)
            .await
            .map(Reply::from)
    }

    /// Says why a summary is unusable; true when it is usable.
    fn report_failed_summary(&self, ctx: &TurnContext, status: &str, summary: &str) -> bool {
        match status {
            "failed" => notice(
                ctx,
                NoticeLevel::Error,
                "compact",
                "compaction failed; history unchanged",
            ),
            "length" => {
                // Checked before "empty": a reasoning trace that eats the
                // whole budget leaves no text, and the fix is the same.
                let what = if summary.is_empty() {
                    "reasoning, before any summary,"
                } else {
                    "summary"
                };
                notice(
                    ctx,
                    NoticeLevel::Warn,
                    "compact",
                    format!(
                        "the {what} hit the {}-token limit, so history was left unchanged. \
                         Raise the budget with /maxtok <n> and run /compact again.",
                        self.settings.max_tokens
                    ),
                );
            }
            "empty" => notice(
                ctx,
                NoticeLevel::Error,
                "compact",
                "the model returned no summary; history unchanged",
            ),
            _ => {}
        }
        status == "ok"
    }

    /// The session log is the backup: a compaction is logged as a
    /// `replace`, and every message before it stays in the file. viy keeps
    /// no transcripts (kb d082), so a chat without a log keeps none.
    fn backup_before_compaction(&mut self, ctx: &TurnContext) {
        self.checkpoint(ctx);
    }

    fn log_kept(&self) -> bool {
        !self.autosave_failed
            && lock(&self.session)
                .as_ref()
                .is_some_and(crate::sessions::SessionLog::exists)
    }

    fn report_summary(
        &mut self,
        ctx: &TurnContext,
        summary: &str,
        n: usize,
        old_ctx: Option<u64>,
        summary_tokens: Option<u64>,
        how: &str,
    ) {
        let lines: Vec<&str> = summary.lines().collect();
        let tokens = summary_tokens
            .filter(|t| *t > 0)
            .map_or(String::new(), |t| format!(" / ~{t} tokens"));
        let mut preview = vec![format!(
            "--- summary{how}, {} chars{tokens} ---",
            summary.chars().count()
        )];
        preview.extend(lines.iter().take(8).map(|l| format!("  {l}")));
        if lines.len() > 8 {
            preview.push(format!("  ... ({} more lines)", lines.len() - 8));
        }
        notice(ctx, NoticeLevel::Info, "compact", preview.join("\n"));
        let (tok, _) = self.context_state(&[]);
        let context = old_ctx.filter(|t| *t > 0).map_or(String::new(), |old| {
            format!(", context ~{old} -> ~{tok} tokens")
        });
        let kept = if self.log_kept() {
            "the session log keeps the full history"
        } else {
            "no session log was written, so the replaced messages are not kept"
        };
        notice(
            ctx,
            NoticeLevel::Info,
            "compact",
            format!(
                "compacted {n} messages -> {}{context}. {kept}.",
                self.messages.len()
            ),
        );
        self.compact_failed = false;
        self.checkpoint(ctx);
    }

    // ---- step 1: no model ---------------------------------------------------

    /// The elision of every turn but the newest, and the context fraction
    /// it would leave with `extra` on top; `None` with nothing older to
    /// elide (rp's `_trim_plan`).
    pub(crate) fn trim_plan(&self, extra: &[Value]) -> Option<(Elision, f64)> {
        let users: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "user")
            .map(|(i, _)| i)
            .collect();
        if users.len() < 2 {
            return None;
        }
        let files = &self.files;
        let el = elide_before(&self.messages, users[users.len() - 1], &|tc| {
            files.call_path(tc)
        });
        if el.n == 0 {
            return None;
        }
        let mut msgs = self.with_system(&el.messages);
        msgs.extend(extra.iter().cloned());
        let frac = self.estimate_tokens(&msgs) as f64 / self.prompt_budget() as f64;
        Some((el, frac))
    }

    fn compact_by_trimming(&mut self, ctx: &TurnContext, extra: &[Value], auto: bool) -> bool {
        let Some((el, frac)) = self.trim_plan(extra) else {
            return false;
        };
        if frac >= self.fractions.trim_target {
            return false;
        }
        let (old_tok, _) = self.context_state(extra);
        let t0 = Instant::now();
        self.backup_before_compaction(ctx);
        let n = el.n;
        self.apply_elision(el);
        let (new_tok, new_frac) = self.context_state(extra);
        self.tracer.event(
            "compact",
            json!({"auto": auto, "status": "ok", "mode": "trim",
                   "secs": (t0.elapsed().as_secs_f64() * 100.0).round() / 100.0,
                   "elided": n, "tokens_before": old_tok, "tokens_after": new_tok}),
        );
        notice(
            ctx,
            NoticeLevel::Info,
            "compact",
            format!(
                "compacted without the model: elided {n} tool outputs older than the last turn, \
                 context ~{old_tok} -> ~{new_tok} tokens ({} of budget). The session log keeps \
                 them; /compact full writes a summary instead.",
                pct(new_frac)
            ),
        );
        self.compact_failed = false;
        self.checkpoint(ctx);
        true
    }

    // ---- step 3: the model pass -------------------------------------------------

    /// What one summary pass sends and replaces, or `None` when nothing
    /// follows the last summary (rp's `_plan_summary`). `merge` allows a pass
    /// that rewrites every summary as one: only the background pass sets it,
    /// as a merge barely shortens them and must never be what a message
    /// waits for.
    pub fn plan_summary(&self, focus: Option<&str>, merge: bool) -> Option<Plan> {
        let msgs = &self.messages;
        let pinned: Vec<Value> = if is_summary_pair(msgs) {
            msgs[..2].to_vec()
        } else {
            Vec::new()
        };
        let mut rest: Vec<Value> = msgs[pinned.len()..].to_vec();
        let budget = self.prompt_budget() as f64;
        let users: Vec<usize> = rest
            .iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "user")
            .map(|(i, _)| i)
            .collect();
        if users.len() >= 2 {
            let last = users[users.len() - 1];
            if self.tokens_without_schemas(&rest[last..]) as f64 <= KEEP_RECENT_MAX_FRAC * budget {
                rest.truncate(last);
            }
        }
        if rest.is_empty() {
            return None;
        }
        let end = pinned.len() + rest.len();
        let pinned_tok = if pinned.is_empty() {
            0
        } else {
            self.tokens_without_schemas(&pinned)
        };
        let mode = if pinned.is_empty() {
            Mode::Full
        } else {
            // A merge rewrites every summary in one reply, so it must start
            // while they still fit in half the reply budget.
            let limit =
                (self.fractions.consolidate_at * budget).min(0.5 * self.settings.max_tokens as f64);
            if merge && pinned_tok as f64 > limit {
                Mode::Consolidate
            } else {
                Mode::Incremental
            }
        };
        let tail = focus.map_or(String::new(), |f| {
            format!("\n\nPay particular attention to: {f}.")
        });
        let instr = match mode {
            // Fitted with the longest form of the instruction; the real one
            // names where the fitted chunk starts, known only after.
            Mode::Incremental => {
                let probe = [json!({"role": "user", "content": "x".repeat(80)})];
                incremental_instruction(&probe, 12000) + &tail
            }
            Mode::Consolidate => {
                let words = (pinned_tok as f64 * 0.75) as u64;
                format!(
                    "{}\n\nThe earlier summaries come to about {words} words. Merge them and \
                     what follows into one summary of at most about {} words: drop what is \
                     finished or superseded and no longer matters.{tail}",
                    compact_instruction(),
                    words / 2
                )
            }
            Mode::Full => compact_instruction() + &tail,
        };
        let mut instr_msg = json!({"role": "user", "content": instr});
        // Sent the way a normal turn is: the same system prompt, and the
        // tool schemas in the prompt with calls forbidden, so the request
        // starts with the tokens the last turn sent and the prefix cache
        // serves it.
        let base = self.with_system(&[]);
        let (chunk, dropped) = self.fit_chunk(&base, &pinned, rest, &instr_msg);
        if mode == Mode::Incremental {
            instr_msg = json!({"role": "user", "content": incremental_instruction(
                &chunk, self.tokens_without_schemas(&chunk)) + &tail});
        }
        let plain = self
            .system
            .iter()
            .map(|s| json!({"role": "system", "content": s}));
        let tail_msgs = || {
            pinned
                .iter()
                .chain(&chunk)
                .cloned()
                .chain(std::iter::once(instr_msg.clone()))
        };
        let msgs: Vec<Value> = base.iter().cloned().chain(tail_msgs()).collect();
        let plain_msgs: Vec<Value> = plain.chain(tail_msgs()).collect();
        Some(Plan {
            pinned: pinned.clone(),
            chunk: chunk.clone(),
            end,
            dropped,
            mode,
            think: self.cfg.get("COMPACT_THINKING").unwrap_or("0") == "1",
            msgs,
            plain_msgs,
        })
    }

    /// `chunk` minus its oldest turns until one pass fits, and how many
    /// messages went (rp's `_fit_chunk`). Turns go whole; only a turn too
    /// large on its own is cut message by message, never opening on a tool
    /// result whose call was cut. `pinned` is never dropped.
    fn fit_chunk(
        &self,
        base: &[Value],
        pinned: &[Value],
        mut chunk: Vec<Value>,
        instr_msg: &Value,
    ) -> (Vec<Value>, usize) {
        let budget = self.prompt_budget();
        let mut dropped = 0;
        let fits = |chunk: &[Value]| {
            let msgs: Vec<Value> = base
                .iter()
                .chain(pinned)
                .chain(chunk)
                .chain(std::iter::once(instr_msg))
                .cloned()
                .collect();
            self.estimate_tokens(&msgs) <= budget
        };
        while !chunk.is_empty() && !fits(&chunk) {
            let next = chunk
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, m)| m["role"] == "user")
                .map_or(1, |(i, _)| i);
            chunk.drain(..next);
            dropped += next;
            while chunk.first().is_some_and(|m| m["role"] == "tool") {
                chunk.remove(0);
                dropped += 1;
            }
        }
        (chunk, dropped)
    }

    /// Puts `summary` in place of the planned chunk; what follows it stays
    /// (rp's `_apply_summary`).
    pub(crate) fn apply_summary(&mut self, plan: &Plan, summary: &str) {
        let earlier = plan.pinned.first().and_then(|m| m["content"].as_str());
        let head = match earlier {
            Some(earlier) if plan.mode == Mode::Incremental && !restates(earlier, summary) => {
                format!("{earlier}{SUMMARY_NEXT}{summary}")
            }
            // a first summary, a consolidation, or an "incremental" pass that
            // rewrote everything anyway: then it replaces, never duplicates
            _ => format!("{SUMMARY_HEAD}\n\n{summary}"),
        };
        let after = self.messages.split_off(plan.end.min(self.messages.len()));
        // Files known only through the summary must be read again before
        // they are overwritten; the turns kept verbatim still show theirs.
        let keep: Vec<PathBuf> = self.files.paths_touched(&after).into_iter().collect();
        self.files.keep_reads(&keep);
        self.messages = vec![
            json!({"role": "user", "content": head}),
            json!({"role": "assistant", "content": SUMMARY_ACK}),
        ];
        self.messages.extend(after);
        self.abort_background();
    }

    // ---- step 2: the background pass ------------------------------------------

    fn fingerprint(&self, end: usize) -> Fingerprint {
        let id = lock(&self.session).as_ref().map(|log| log.id().to_string());
        let shape = self.messages[..end.min(self.messages.len())]
            .iter()
            .map(|m| {
                let said = (m["role"] == "user")
                    .then(|| m["content"].as_str().map(str::to_string))
                    .flatten();
                (m["role"].clone(), said)
            })
            .collect();
        (id, end, shape)
    }

    /// Whether a background summary is being written.
    pub fn background_running(&self) -> bool {
        self.background
            .as_ref()
            .is_some_and(|bg| !bg.job.is_finished())
    }

    /// Forgets the background summary, stopping it.
    pub(crate) fn abort_background(&mut self) {
        if let Some(bg) = self.background.take() {
            bg.job.abort();
        }
    }

    /// After a turn: summarize in the background what a compaction would
    /// summarize, so the summary is ready when the history reaches
    /// AUTO_COMPACT_THRESHOLD (rp's `_maybe_start_background`). Starts at
    /// COMPACT_BACKGROUND_AT, and only when trimming will not be enough.
    pub(crate) fn maybe_start_background(&mut self) {
        if !self.auto_compact
            || self.fractions.background_at <= 0.0
            || self.background.is_some()
            || self.messages.is_empty()
        {
            return;
        }
        let (_, frac) = self.context_state(&[]);
        if frac < self.fractions.background_at {
            return;
        }
        if self
            .trim_plan(&[])
            .is_some_and(|(_, f)| f < self.fractions.trim_target)
        {
            return;
        }
        let Some(plan) = self.plan_summary(None, true) else {
            return;
        };
        if plan.chunk.is_empty() {
            return;
        }
        let specs = self.tool_specs();
        let mut bodies = vec![
            self.settings
                .payload(&plan.msgs, &specs, Tools::None, plan.think),
        ];
        if schema_chars(&specs) > 0 {
            bodies.push(
                self.settings
                    .payload(&plan.plain_msgs, &specs, Tools::Omit, plan.think),
            );
        }
        for b in &mut bodies {
            b["stream"] = json!(false);
            if let Some(m) = b.as_object_mut() {
                m.remove("stream_options");
            }
        }
        let fp = self.fingerprint(plan.end);
        let endpoint = self.endpoint.clone();
        let touch = Arc::clone(&self.touch);
        let job = tokio::spawn(async move {
            let t0 = Instant::now();
            let mut out = None;
            let mut retried = false;
            for (i, body) in bodies.iter().enumerate() {
                touch();
                out = post_summary(&endpoint, body).await;
                touch();
                retried = i > 0;
                if out.as_ref().is_none_or(|o| {
                    !o.text.trim().is_empty() || o.finish.as_deref() == Some("length")
                }) {
                    break;
                }
            }
            (out, retried, t0.elapsed().as_secs_f64())
        });
        self.background = Some(Background { plan, fp, job });
    }

    /// Applies the background summary if it still matches the history,
    /// waiting for it when `wait`; true when the history changed (rp's
    /// `_take_background`).
    async fn take_background(&mut self, ctx: &TurnContext, wait: bool, auto: bool) -> bool {
        let Some(bg) = self.background.as_ref() else {
            return false;
        };
        if self.fingerprint(bg.plan.end) != bg.fp {
            self.abort_background(); // the history moved under it
            return false;
        }
        if !bg.job.is_finished() {
            if !wait {
                return false;
            }
            notice(
                ctx,
                NoticeLevel::Info,
                "compact",
                "finishing the summary started in the background",
            );
        }
        let Some(bg) = self.background.take() else {
            return false;
        };
        let (out, retried, secs) = bg.job.await.unwrap_or((None, false, 0.0));
        let summary = out
            .as_ref()
            .map(|o| o.text.trim().to_string())
            .unwrap_or_default();
        let status = summary_status(out.as_ref(), &summary);
        self.trace_compact(
            out.as_ref(),
            status,
            auto,
            secs,
            None,
            bg.plan.end,
            &bg.plan,
            retried,
            true,
        );
        if status != "ok" {
            return false; // the foreground pass will try itself
        }
        let n = self.messages.len();
        let (old_tok, _) = self.context_state(&[]);
        self.backup_before_compaction(ctx);
        self.apply_summary(&bg.plan, &summary);
        let completion = out
            .and_then(|o| o.usage)
            .and_then(|u| u["completion_tokens"].as_u64());
        self.report_summary(
            ctx,
            &summary,
            n,
            Some(old_tok),
            completion,
            " (written in the background)",
        );
        true
    }

    /// One `compact` trace event: how long the pass took and where.
    #[allow(clippy::too_many_arguments)]
    fn trace_compact(
        &self,
        out: Option<&Reply>,
        status: &str,
        auto: bool,
        secs: f64,
        t_req: Option<tokio::time::Instant>,
        messages: usize,
        plan: &Plan,
        retried: bool,
        background: bool,
    ) {
        let usage = out.and_then(|o| o.usage.clone()).unwrap_or(Value::Null);
        let ttft = match (t_req, out.and_then(|o| o.first_token)) {
            (Some(a), Some(b)) => json!(((b - a).as_secs_f64() * 10.0).round() / 10.0),
            _ => Value::Null,
        };
        self.tracer.event(
            "compact",
            json!({
                "auto": auto, "status": status, "mode": plan.mode.as_str(),
                "background": background, "secs": (secs * 10.0).round() / 10.0,
                "messages": messages, "dropped": plan.dropped, "retried": retried,
                "prompt_tokens": usage["prompt_tokens"],
                "completion_tokens": usage["completion_tokens"],
                "ttft_secs": ttft, "reasoning_secs": null, "writing_secs": null,
                "reasoning_chars": out.map_or(0, |o| o.reasoning.chars().count()),
                "summary_chars": out.map_or(0, |o| o.text.trim().chars().count()),
                "effort": if plan.think { self.settings.reasoning_effort.as_str() } else { "off" },
            }),
        );
    }

    /// `/compact [full] [focus]`.
    pub(crate) async fn compact_command(&mut self, ctx: &TurnContext, arg: &str) {
        self.compact(ctx, arg, false, &[]).await;
    }

    /// `/autocompact on|off|<percent>` (rp's `_cmd_autocompact`).
    pub(crate) fn autocompact_command(&mut self, ctx: &TurnContext, arg: &str) {
        let a = arg.trim().to_lowercase();
        let a = a.trim_end_matches('%');
        if a == "on" || a == "off" {
            self.auto_compact = a == "on";
        } else if !a.is_empty() {
            let mut p = a.parse::<f64>().unwrap_or(-1.0);
            // Either 85 or 0.85 reads naturally as "the threshold".
            if p > 1.0 {
                p /= 100.0;
            }
            if !(0.1..=0.98).contains(&p) {
                notice(
                    ctx,
                    NoticeLevel::Warn,
                    "autocompact",
                    "usage: /autocompact on|off|<percent>   e.g. /autocompact 80",
                );
                return;
            }
            self.auto_compact_threshold = p;
            self.auto_compact = true;
        }
        let (tok, frac) = self.context_state(&[]);
        let state = if self.auto_compact {
            format!("on, triggers at {}", pct(self.auto_compact_threshold))
        } else {
            "off".to_string()
        };
        notice(
            ctx,
            NoticeLevel::Info,
            "autocompact",
            format!(
                "auto-compact {state}. Context ~{tok}/{} tokens ({} of budget)",
                self.prompt_budget(),
                pct(frac)
            ),
        );
    }
}

/// One non-streamed summary request (the background pass shows nothing).
async fn post_summary(endpoint: &crate::stream::Endpoint, body: &Value) -> Option<Reply> {
    let request = endpoint
        .request(Method::POST, "/v1/chat/completions")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(body).unwrap_or_default())
        .timeouts(Duration::from_secs(15), Duration::from_secs(1800));
    let response = viy_http::send(endpoint.http.as_ref(), request).await.ok()?;
    if response.status != 200 {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    let j: Value = serde_json::from_slice(&bytes).ok()?;
    let ch = &j["choices"][0];
    let m = &ch["message"];
    let text_of = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let reasoning = match m["reasoning"].as_str().filter(|s| !s.is_empty()) {
        Some(r) => r.to_string(),
        None => text_of(&m["reasoning_content"]),
    };
    Some(Reply {
        text: text_of(&m["content"]),
        reasoning,
        usage: j.get("usage").cloned().filter(|u| !u.is_null()),
        finish: ch["finish_reason"].as_str().map(str::to_string),
        first_token: None,
    })
}

#[cfg(test)]
#[path = "compact_tests.rs"]
mod tests;
