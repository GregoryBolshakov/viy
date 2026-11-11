//! Tracing and anomaly alarms for chat turns (rpctl trace.py).
//!
//! Three append-only JSONL files under viy's state folder (kb d049):
//!
//! - `trace/<session>.jsonl`: every round, tool call and chat event;
//! - `turns.jsonl`: one summary row per turn, all chats: the metrics;
//! - `alarms.jsonl`: every alarm, all chats.
//!
//! The detectors are plain code over what the turn already produced, so
//! they cost nothing on the GPU and give the same verdict every time:
//!
//! | code | when |
//! |---|---|
//! | `repeat_call` | the same call (name and arguments) got the same result 3+ times in a turn |
//! | `same_result` | different calls returned identical output twice |
//! | `going_in_circles` | 4 tool calls in a row that repeat earlier ones |
//! | `tool_errors` | 3 tool errors in a row |
//! | `repetitive_reasoning` | a sentence 4+ times in one reasoning trace |
//! | `token_limit` | a reply cut at max_tokens |
//! | `tool_cap` | the turn used every tool round it was allowed |
//! | `no_answer_retry` | the reply had no answer and was asked again |
//! | `no_answer` | the turn ended without an answer anyway |
//! | `context_full` | tool results filled the context window |
//! | `circuit_breaker` | refused actions stopped the turn |
//! | `request_failed` | a model request failed |
//! | `tool_exception`, `helper_failed`, `consult_failed` | with the tools that raise them (phase 7) |
//! | `slow_round` | one model request took over [`SLOW_ROUND_S`] seconds |
//!
//! `warn` and `error` alarms are said in the chat as they happen; `info`
//! is only logged. Nothing here may break a turn: every write swallows its
//! own error, as Codex's trace writer appends best-effort.
//!
//! The notes [`TurnMonitor::on_tool`] adds to a result reach the model, so
//! they keep rpctl's words, the name viy's (`viy_note`; flags F21, kb d131).

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use sha1::Digest;
use sha1::Sha1;

use crate::stream::Round;

/// A round slower than this is logged (`slow_round`).
pub const SLOW_ROUND_S: f64 = 300.0;
/// Identical calls with identical results in one turn.
pub const REPEAT_ALARM_AT: u64 = 3;
/// Repeated calls in a row.
pub const CIRCLE_ALARM_AT: u64 = 4;
/// Tool errors in a row.
pub const ERROR_ALARM_AT: u64 = 3;
/// The same reasoning sentence in one trace.
pub const SENTENCE_REPEAT_AT: usize = 4;

/// How loud an alarm is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Only logged.
    Info,
    Warn,
    Error,
}

impl Severity {
    pub fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// An alarm code's severity; an unknown code warns.
pub fn severity(code: &str) -> Severity {
    match code {
        "no_answer" | "request_failed" | "tool_exception" => Severity::Error,
        "slow_round" => Severity::Info,
        _ => Severity::Warn,
    }
}

/// Where an alarm is said: its severity and the line.
pub type Echo = Arc<dyn Fn(Severity, String) + Send + Sync>;

/// Seconds since the epoch, as rp's `time.time()`.
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn round_to(x: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (x * f).round() / f
}

/// The first `n` characters of `s`.
fn head(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

/// The last `n` characters of `s`.
fn tail(s: &str, n: usize) -> &str {
    let len = s.chars().count();
    s.char_indices()
        .nth(len.saturating_sub(n))
        .map_or("", |(i, _)| &s[i..])
}

/// One writer per process for the files every chat appends to.
static WRITE: Mutex<()> = Mutex::new(());

/// Appends `row` as one line; a failure is dropped: tracing must never
/// cost the user a turn.
fn append(path: &Path, row: &Value) {
    let mut line = row.to_string();
    line.push('\n');
    let _guard = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `{"ts": .., <first>.., <fields>..}` as one object, in that order.
fn row(first: Value, fields: Map<String, Value>) -> Value {
    let mut out = Map::new();
    out.insert("ts".into(), json!(now()));
    if let Value::Object(m) = first {
        out.extend(m);
    }
    out.extend(fields);
    Value::Object(out)
}

/// Writes one chat's trace and its alarms. The session id is asked for at
/// each write, so the tracer follows `/new`, `/resume` and `/fork`.
#[derive(Clone)]
pub struct Tracer {
    state_dir: PathBuf,
    /// TRACE_ENABLED.
    pub enabled: bool,
    sid: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl Tracer {
    /// A tracer writing under `state_dir`, for the session `sid` names.
    pub fn new(
        state_dir: PathBuf,
        enabled: bool,
        sid: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    ) -> Self {
        Self {
            state_dir,
            enabled,
            sid,
        }
    }

    /// The session's id, or `nosession-<pid>` for a chat without a log.
    pub fn sid(&self) -> String {
        (self.sid)()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("nosession-{}", std::process::id()))
    }

    pub fn trace_dir(&self) -> PathBuf {
        self.state_dir.join("trace")
    }

    pub fn turns_path(&self) -> PathBuf {
        self.state_dir.join("turns.jsonl")
    }

    pub fn alarms_path(&self) -> PathBuf {
        self.state_dir.join("alarms.jsonl")
    }

    /// The session's trace file.
    pub fn trace_path(&self) -> PathBuf {
        self.trace_dir().join(format!("{}.jsonl", self.sid()))
    }

    /// A larger artifact beside the trace (a helper's whole transcript):
    /// its path, or `None` when tracing is off or the write failed.
    pub fn save_json(&self, name: &str, data: &Value) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let path = self.trace_dir().join(self.sid()).join(name);
        let text = serde_json::to_string_pretty(data).ok()?;
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, text).ok()?;
        Some(path.display().to_string())
    }

    /// One event in the session's trace; `fields` is a JSON object.
    pub fn event(&self, kind: &str, fields: Value) {
        if !self.enabled {
            return;
        }
        let fields = match fields {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        append(&self.trace_path(), &row(json!({"kind": kind}), fields));
    }

    /// Logs an alarm; its severity, for the caller to say it.
    fn alarm(&self, code: &str, msg: &str, fields: Map<String, Value>) -> Severity {
        let sev = severity(code);
        if self.enabled {
            let mut alarm = Map::new();
            alarm.insert("sid".into(), json!(self.sid()));
            alarm.insert("code".into(), json!(code));
            alarm.insert("severity".into(), json!(sev.name()));
            alarm.insert("msg".into(), json!(msg));
            append(
                &self.alarms_path(),
                &row(Value::Object(alarm), fields.clone()),
            );
            let mut event = json!({"code": code, "severity": sev.name(), "msg": msg});
            if let Value::Object(m) = &mut event {
                m.extend(fields);
            }
            self.event("alarm", event);
        }
        sev
    }
}

/// A call's name plus its normalised arguments, so whitespace and key
/// order don't hide a repeat.
pub fn call_signature(name: &str, args_json: &str) -> String {
    let norm = if args_json.trim().is_empty() {
        "{}".to_string()
    } else {
        match serde_json::from_str::<Value>(args_json) {
            Ok(mut args) => {
                if let Some(Value::String(c)) = args.get_mut("command") {
                    *c = c.split_whitespace().collect::<Vec<_>>().join(" ");
                }
                sorted(args).to_string()
            }
            Err(_) => args_json.split_whitespace().collect::<Vec<_>>().join(" "),
        }
    };
    format!("{name}:{norm}")
}

/// `v` with every object's keys in order.
fn sorted(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut pairs: Vec<(String, Value)> = m.into_iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(pairs.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// Python's truth of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The error text of a tool result, or `None` when it succeeded.
pub fn tool_error(payload: &str) -> Option<String> {
    let Ok(Value::Object(d)) = serde_json::from_str::<Value>(payload) else {
        return None;
    };
    if let Some(e) = d.get("error").filter(|e| truthy(e)) {
        return Some(head(&text_of(e), 200).to_string());
    }
    let refused = |k: &str| d.get(k).is_some_and(truthy);
    if refused("denied") || refused("declined") {
        let note = d
            .get("note")
            .filter(|n| truthy(n))
            .map_or_else(|| "declined".to_string(), text_of);
        return Some(head(&note, 200).to_string());
    }
    None
}

/// `text` cut after `.`, `!` or `?` and whitespace, and at line breaks
/// (rp's `(?<=[.!?])\s+|\n+`).
fn sentences(text: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let end = |j: usize| chars.get(j).map_or(text.len(), |&(p, _)| p);
    let mut parts = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < chars.len() {
        let (pos, c) = chars[i];
        let after_stop = i > 0 && matches!(chars[i - 1].1, '.' | '!' | '?');
        let run: Option<fn(char) -> bool> = if after_stop && c.is_whitespace() {
            Some(char::is_whitespace)
        } else if c == '\n' {
            Some(|c| c == '\n')
        } else {
            None
        };
        match run {
            Some(more) => {
                let mut j = i;
                while j < chars.len() && more(chars[j].1) {
                    j += 1;
                }
                parts.push(&text[start..pos]);
                start = end(j);
                i = j;
            }
            None => i += 1,
        }
    }
    parts.push(&text[start..]);
    parts
}

/// The most repeated sentence of 30+ characters and its count, when it
/// occurs [`SENTENCE_REPEAT_AT`] or more times: the model looping in its
/// reasoning ("Wait, let me re-check ..." over and over).
pub fn repeated_sentence(text: &str) -> Option<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for part in sentences(text) {
        let s = part
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if s.chars().count() < 30 {
            continue;
        }
        match counts.iter_mut().find(|(k, _)| *k == s) {
            Some((_, n)) => *n += 1,
            None => counts.push((s, 1)),
        }
    }
    // The first seen wins a tie, as Counter.most_common.
    let mut best: Option<(String, usize)> = None;
    for (s, n) in counts {
        if best.as_ref().is_none_or(|(_, m)| n > *m) {
            best = Some((s, n));
        }
    }
    best.filter(|(_, n)| *n >= SENTENCE_REPEAT_AT)
}

/// `payload` with the monitor's note: a JSON object gets it as
/// `viy_note` (rp's `rpctl_note`, flag F21), anything else on a line of its own (rp's `_with_note`).
pub fn with_note(payload: &str, note: &str) -> String {
    match serde_json::from_str::<Value>(payload) {
        Ok(Value::Object(mut d)) => {
            d.insert("viy_note".into(), json!(note));
            Value::Object(d).to_string()
        }
        _ => format!("{payload}\n{note}"),
    }
}

/// Watches one turn and raises alarms: [`TurnMonitor::on_round`] after
/// every model request, [`TurnMonitor::on_tool`] after every tool call,
/// whose note tells the model it is repeating itself.
/// [`TurnMonitor::finish`] writes the turn's metrics row.
pub struct TurnMonitor {
    t: Tracer,
    label: String,
    turn: Option<u64>,
    echo: Echo,
    t0: Instant,
    pub rounds: u64,
    completion_tokens: u64,
    reasoning_chars: u64,
    /// Calls by tool, in first-call order.
    calls: Vec<(String, u64)>,
    /// (signature, result hash) to how often it came.
    sigs: HashMap<(String, [u8; 20]), u64>,
    /// Result hash to the first signature that produced it.
    results: HashMap<[u8; 20], String>,
    same_results: u64,
    consecutive_repeats: u64,
    consecutive_errors: u64,
    errors: u64,
    repeats: u64,
    helpers: u64,
    helper_failures: u64,
    /// The codes raised, in order.
    pub alarms: Vec<String>,
    alarmed: HashSet<String>,
}

impl TurnMonitor {
    pub fn new(tracer: Tracer, label: &str, turn: Option<u64>, echo: Echo) -> Self {
        Self {
            t: tracer,
            label: label.to_string(),
            turn,
            echo,
            t0: Instant::now(),
            rounds: 0,
            completion_tokens: 0,
            reasoning_chars: 0,
            calls: Vec::new(),
            sigs: HashMap::new(),
            results: HashMap::new(),
            same_results: 0,
            consecutive_repeats: 0,
            consecutive_errors: 0,
            errors: 0,
            repeats: 0,
            helpers: 0,
            helper_failures: 0,
            alarms: Vec::new(),
            alarmed: HashSet::new(),
        }
    }

    fn where_(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("label".into(), json!(self.label));
        m.insert("turn".into(), json!(self.turn));
        m
    }

    /// Raises `code` once per `once_key` (the code itself by default).
    fn alarm(&mut self, code: &str, msg: &str, once_key: Option<String>) {
        let key = once_key.unwrap_or_else(|| code.to_string());
        if !self.alarmed.insert(key) {
            return;
        }
        self.alarms.push(code.to_string());
        let sev = self.t.alarm(code, msg, self.where_());
        if sev != Severity::Info {
            (self.echo)(sev, format!("  [alarm: {code}] {msg}"));
        }
    }

    /// After one model request: `round_i` from 0, `mode` `auto`, `final`
    /// (out of tool rounds) or `retry`.
    pub fn on_round(&mut self, round_i: u64, out: &Round, secs: f64, mode: &str) {
        self.rounds += 1;
        let usage = out.usage.clone().unwrap_or(Value::Null);
        self.completion_tokens += usage["completion_tokens"].as_u64().unwrap_or(0);
        let reasoning_chars = out.reasoning.chars().count() as u64;
        self.reasoning_chars += reasoning_chars;
        let mut fields = self.where_();
        fields.extend(
            json!({
                "round": round_i,
                "mode": mode,
                "secs": round_to(secs, 1),
                "finish": out.finish,
                "prompt_tokens": usage["prompt_tokens"],
                "completion_tokens": usage["completion_tokens"],
                "reasoning_chars": reasoning_chars,
                "text_chars": out.text.chars().count(),
                "calls": out.tool_calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                "reasoning_tail": tail(&out.reasoning, 300),
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        self.t.event("round", Value::Object(fields));
        let label = self.label.clone();
        if out.finish.as_deref() == Some("length") {
            self.alarm(
                "token_limit",
                &format!("{label}: a reply hit the token limit (round {round_i})"),
                Some(format!("len{round_i}")),
            );
        }
        if let Some((s, n)) = repeated_sentence(&out.reasoning) {
            self.alarm(
                "repetitive_reasoning",
                &format!(
                    "{label}: reasoning repeated a sentence {n}x: \"{}\"",
                    head(&s, 80)
                ),
                Some(format!("rep{round_i}")),
            );
        }
        if secs > SLOW_ROUND_S {
            self.alarm(
                "slow_round",
                &format!("{label}: round {round_i} took {secs:.0}s"),
                Some(format!("slow{round_i}")),
            );
        }
    }

    /// After one tool call: a note for the model when it repeats itself.
    pub fn on_tool(
        &mut self,
        name: &str,
        args_json: &str,
        payload: &str,
        secs: f64,
    ) -> Option<String> {
        match self.calls.iter_mut().find(|(k, _)| k == name) {
            Some((_, n)) => *n += 1,
            None => self.calls.push((name.to_string(), 1)),
        }
        let sig = call_signature(name, args_json);
        let rh: [u8; 20] = Sha1::digest(payload.as_bytes()).into();
        // A repeat is the same call getting the same answer: re-reading a
        // file after it changed, or re-running tests after a fix, is
        // progress.
        let n = {
            let n = self.sigs.entry((sig.clone(), rh)).or_insert(0);
            *n += 1;
            *n
        };
        let err = tool_error(payload);
        let same = n == 1
            && payload.chars().count() >= 200
            && *self.results.entry(rh).or_insert_with(|| sig.clone()) != sig;
        if same {
            self.same_results += 1;
        }
        let sig_hash: String = Sha1::digest(sig.as_bytes())
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut fields = self.where_();
        fields.extend(
            json!({
                "name": name,
                "args": head(args_json, 300),
                "sig": sig_hash,
                "repeat": n,
                "same_result": same.then_some(true),
                "secs": round_to(secs, 2),
                "result_chars": payload.chars().count(),
                "error": err,
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        self.t.event("tool", Value::Object(fields));
        let label = self.label.clone();
        if let Some(err) = &err {
            self.errors += 1;
            self.consecutive_errors += 1;
            if self.consecutive_errors >= ERROR_ALARM_AT {
                let msg = format!(
                    "{label}: {} tool errors in a row, last: {}",
                    self.consecutive_errors,
                    head(err, 100)
                );
                self.alarm("tool_errors", &msg, None);
            }
        } else {
            self.consecutive_errors = 0;
        }
        if n >= 2 || same {
            self.repeats += 1;
            self.consecutive_repeats += 1;
        } else {
            self.consecutive_repeats = 0;
        }
        if same && self.same_results >= 2 {
            let msg = format!(
                "{label}: {} calls returned output identical to an earlier, different call \
                 (last: {name} {}); the tool may be ignoring arguments",
                self.same_results,
                head(args_json, 80)
            );
            self.alarm("same_result", &msg, None);
        }
        if n >= REPEAT_ALARM_AT {
            let msg = format!(
                "{label}: {name} called {n}x with the same arguments and result: {}",
                head(args_json, 100)
            );
            self.alarm("repeat_call", &msg, Some(format!("rc:{sig}")));
        }
        if self.consecutive_repeats >= CIRCLE_ALARM_AT {
            let msg = format!(
                "{label}: {} repeated tool calls in a row",
                self.consecutive_repeats
            );
            self.alarm("going_in_circles", &msg, None);
        }
        if same {
            return Some(
                "viy: this returned exactly the same output as an earlier call in this turn. \
                 Use that result, or change your approach."
                    .into(),
            );
        }
        (n >= 2).then(|| {
            format!(
                "viy: you already made this exact call earlier in this turn and got the same \
                 result ({n}x now). Use it, or do something different."
            )
        })
    }

    /// Helpers an explore call ran in this turn, and how many failed.
    pub fn add_helpers(&mut self, n: u64, failed: u64) {
        self.helpers += n;
        self.helper_failures += failed;
    }

    /// An alarm the caller decided (the tool cap, the retry, no answer).
    pub fn mark(&mut self, code: &str, msg: &str) {
        self.alarm(code, msg, Some(format!("{code}:{msg}")));
    }

    /// The turn's metrics row, `extra` at its end.
    pub fn summary(&self, extra: Map<String, Value>) -> Value {
        let calls: Map<String, Value> = self
            .calls
            .iter()
            .map(|(k, n)| (k.clone(), json!(n)))
            .collect();
        let mut fields = json!({
            "sid": self.t.sid(),
            "label": self.label,
            "turn": self.turn,
            "secs": round_to(self.t0.elapsed().as_secs_f64(), 1),
            "rounds": self.rounds,
            "tool_calls": calls,
            "errors": self.errors,
            "repeats": self.repeats,
            "same_results": self.same_results,
            "completion_tokens": self.completion_tokens,
            "reasoning_chars": self.reasoning_chars,
            "helpers": self.helpers,
            "helper_failures": self.helper_failures,
            "alarms": self.alarms,
        })
        .as_object()
        .cloned()
        .unwrap_or_default();
        fields.extend(extra);
        row(Value::Object(Map::new()), fields)
    }

    /// Writes the turn's row to `turns.jsonl` and its end to the trace;
    /// the row.
    pub fn finish(&self, extra: Map<String, Value>) -> Value {
        let row = self.summary(extra);
        if self.t.enabled {
            append(&self.t.turns_path(), &row);
            let mut end = row.as_object().cloned().unwrap_or_default();
            end.remove("ts");
            self.t.event("turn_end", Value::Object(end));
        }
        row
    }
}

// ---- `viy trace` ------------------------------------------------------------------

const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const RST: &str = "\x1b[0m";

/// The JSON-object rows of a JSONL file, the last `limit` when given and
/// not zero; none when it cannot be read.
fn read_rows(path: &Path, limit: Option<usize>) -> Vec<Map<String, Value>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let rows: Vec<Map<String, Value>> = text
        .lines()
        .filter_map(|l| match serde_json::from_str(l) {
            Ok(Value::Object(m)) => Some(m),
            _ => None,
        })
        .collect();
    match limit {
        Some(n) if n > 0 => rows[rows.len().saturating_sub(n)..].to_vec(),
        _ => rows,
    }
}

/// A row's time, local, as `%m-%d %H:%M:%S`.
fn when(row: &Map<String, Value>) -> String {
    let ts = row.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()) * 1e9) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "??-?? ??:??:??".into())
}

/// A field as text: a string as it is, a missing or null one as `-`,
/// anything else as JSON.
fn field(row: &Map<String, Value>, key: &str) -> String {
    match row.get(key) {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => "-".into(),
        Some(v) => v.to_string(),
    }
}

fn num(row: &Map<String, Value>, key: &str) -> f64 {
    row.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn strings(row: &Map<String, Value>, key: &str) -> Vec<String> {
    row.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
        .collect()
}

/// What `viy trace [alarms|stats|SESSION_ID] [-n N]` prints (rp's
/// `cmd_trace`), from the trace files under `state_dir` (viy's state
/// folder): the recent turns, the recent alarms, totals over recent turns,
/// or one chat's timeline. `colour`: rp's colours, for a terminal.
pub fn view(state_dir: &Path, what: &str, n: Option<usize>, colour: bool) -> Vec<String> {
    let paint = |c: &str, text: &str| {
        if colour {
            format!("{c}{text}{RST}")
        } else {
            text.to_string()
        }
    };
    let what = what.trim();
    let n = match n {
        None if matches!(what, "" | "alarms" | "stats") => Some(30),
        n => n,
    };
    let mut out = Vec::new();
    match what {
        "alarms" => {
            let rows = read_rows(&state_dir.join("alarms.jsonl"), n);
            if rows.is_empty() {
                out.push("no alarms recorded".into());
            }
            for r in &rows {
                let c = match r.get("severity").and_then(Value::as_str) {
                    Some("error") => RED,
                    Some("warn") => YELLOW,
                    _ => DIM,
                };
                let sid = r.get("sid").and_then(Value::as_str).unwrap_or("?");
                let code = format!("{:<20}", field(r, "code"));
                let msg = r.get("msg").and_then(Value::as_str).unwrap_or_default();
                out.push(format!("{}  {sid:<24} {} {msg}", when(r), paint(c, &code)));
            }
        }
        "stats" => {
            let rows: Vec<_> = read_rows(&state_dir.join("turns.jsonl"), n)
                .into_iter()
                .filter(|r| r.get("label").and_then(Value::as_str) == Some("main"))
                .collect();
            if rows.is_empty() {
                out.push("no turns recorded".into());
                return out;
            }
            let k = rows.len();
            let mut rounds: Vec<f64> = rows.iter().map(|r| num(r, "rounds")).collect();
            let mut secs: Vec<f64> = rows.iter().map(|r| num(r, "secs")).collect();
            rounds.sort_by(f64::total_cmp);
            secs.sort_by(f64::total_cmp);
            let max = |v: &[f64]| v.iter().copied().fold(f64::MIN, f64::max);
            // counts in order of first appearance, then most first (Counter.most_common)
            let mut codes: Vec<(String, usize)> = Vec::new();
            for c in rows.iter().flat_map(|r| strings(r, "alarms")) {
                match codes.iter_mut().find(|(k, _)| *k == c) {
                    Some((_, n)) => *n += 1,
                    None => codes.push((c, 1)),
                }
            }
            codes.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            let with_alarms = rows
                .iter()
                .filter(|r| !strings(r, "alarms").is_empty())
                .count();
            let helpers: f64 = rows.iter().map(|r| num(r, "helpers")).sum();
            let failed: f64 = rows.iter().map(|r| num(r, "helper_failures")).sum();
            out.push(format!("last {k} turns"));
            out.push(format!(
                "  rounds/turn       median {}  max {}",
                rounds[k / 2],
                max(&rounds)
            ));
            out.push(format!(
                "  secs/turn         median {:.0}  max {:.0}",
                secs[k / 2],
                max(&secs)
            ));
            out.push(format!("  turns with alarms {with_alarms}/{k}"));
            out.push(format!("  explore helpers   {helpers} ({failed} failed)"));
            out.extend(codes.iter().map(|(c, n)| format!("  {c:<20} {n}")));
        }
        "" => {
            let rows = read_rows(&state_dir.join("turns.jsonl"), n);
            if rows.is_empty() {
                out.push("no turns recorded yet".into());
                return out;
            }
            out.push(format!(
                "{:<15} {:<24} {:<9} rnds calls  secs  alarms",
                "when", "session", "who"
            ));
            for r in &rows {
                let calls: f64 = r
                    .get("tool_calls")
                    .and_then(Value::as_object)
                    .map_or(0.0, |m| m.values().filter_map(Value::as_f64).sum());
                let al = strings(r, "alarms").join(",");
                let al = if al.is_empty() {
                    "-".to_string()
                } else {
                    paint(YELLOW, &al)
                };
                out.push(format!(
                    "{} {:<24} {:<9} {:>4} {:>5} {:>5.0}  {al}",
                    when(r),
                    r.get("sid").and_then(Value::as_str).unwrap_or("?"),
                    r.get("label").and_then(Value::as_str).unwrap_or_default(),
                    num(r, "rounds"),
                    calls,
                    num(r, "secs"),
                ));
            }
        }
        session => {
            let path = state_dir.join("trace").join(format!("{session}.jsonl"));
            let rows = read_rows(&path, n);
            if rows.is_empty() {
                out.push(format!(
                    "no trace for {session} (looked in {})",
                    path.display()
                ));
                return out;
            }
            for r in &rows {
                let kind = r.get("kind").and_then(Value::as_str).unwrap_or_default();
                let lab = r.get("label").and_then(Value::as_str).unwrap_or_default();
                let t = when(r);
                out.push(match kind {
                    "round" => {
                        let calls = strings(r, "calls").join(",");
                        format!(
                            "{t} {lab:<9} t{} r{:<2} {:<5} {:>6.1}s {:<10} out={} calls={}",
                            field(r, "turn"),
                            field(r, "round"),
                            r.get("mode").and_then(Value::as_str).unwrap_or_default(),
                            num(r, "secs"),
                            r.get("finish").and_then(Value::as_str).unwrap_or_default(),
                            field(r, "completion_tokens"),
                            if calls.is_empty() { "-" } else { &calls },
                        )
                    }
                    "tool" => {
                        let err = r.get("error").and_then(Value::as_str).unwrap_or_default();
                        let flag = if err.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", paint(RED, &format!("ERR {}", head(err, 60))))
                        };
                        let repeat = r.get("repeat").and_then(Value::as_f64).unwrap_or(1.0);
                        let rep = if repeat > 1.0 {
                            format!(" {}", paint(YELLOW, &format!("x{}", field(r, "repeat"))))
                        } else {
                            String::new()
                        };
                        let args = r.get("args").and_then(Value::as_str).unwrap_or_default();
                        format!(
                            "{t} {lab:<9}   {} {}{rep}{flag}",
                            field(r, "name"),
                            head(args, 70)
                        )
                    }
                    "alarm" => format!(
                        "{t} {lab:<9} {}",
                        paint(
                            YELLOW,
                            &format!("ALARM {}: {}", field(r, "code"), field(r, "msg"))
                        )
                    ),
                    "turn_end" => {
                        let al = strings(r, "alarms").join(",");
                        format!(
                            "{t} {lab:<9} end t{}: {} rounds, {}s, alarms={}",
                            field(r, "turn"),
                            field(r, "rounds"),
                            field(r, "secs"),
                            if al.is_empty() { "-" } else { &al }
                        )
                    }
                    _ => {
                        let rest: Map<String, Value> = r
                            .iter()
                            .filter(|(k, _)| !matches!(k.as_str(), "ts" | "kind"))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        format!(
                            "{t} {lab:<9} {kind} {}",
                            head(&Value::Object(rest).to_string(), 120)
                        )
                    }
                });
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "trace_tests.rs"]
mod tests;
