//! The explore workflow (rpctl workflows.py `ExploreRunner`, `Helper`):
//! work the chat hands to helpers that run side by side on the GPU.
//!
//! The pod is rented by the hour, so requests that run side by side cost
//! nothing extra, and each keeps its own speed (rpctl kb f074). The main
//! model asks 1-EXPLORE_MAX_TASKS independent questions about the
//! workspace; each goes to a helper with a fresh context and read-only
//! tools (read_file, list_dir, and run_shell in a sandbox where the
//! workspace is mounted read-only, with no network). Each ends with a short
//! report citing file:line, and only the reports reach the main chat, so it
//! fills up more slowly. Every helper is traced: its rounds and calls go to
//! the chat's trace, its whole transcript beside it, and failures, loops
//! and empty reports raise alarms.
//!
//! Helpers are futures joined in order, at most EXPLORE_PARALLEL at a time
//! (one at a time while a background summary runs); a turn interrupted
//! drops them, which ends their requests.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use futures_util::StreamExt;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_http::Method;
use viy_protocol::NoticeLevel;
use viy_tools::explore;
use viy_tools::guard;
use viy_tools::sandbox;
use viy_tools::sandbox::Jail;
use viy_tools::untrusted;

use super::Chat;
use super::notice;
use crate::engine::TurnContext;
use crate::files::FileTools;
use crate::files::clip;
use crate::payload::Settings;
use crate::payload::Tools;
use crate::stream::Endpoint;
use crate::stream::Round;
use crate::stream::ToolCall;
use crate::trace::Severity;
use crate::trace::TurnMonitor;
use crate::trace::with_note;

/// EXPLORE_MAX_TASKS, EXPLORE_PARALLEL (MAX_NUM_SEQS - 1 when empty),
/// EXPLORE_MAX_ROUNDS, EXPLORE_MAX_CONTEXT_CHARS, EXPLORE_REPORT_CHARS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExploreSettings {
    pub max_tasks: usize,
    /// One slot stays free for the main chat's next request and the user.
    pub parallel: usize,
    pub max_rounds: usize,
    pub max_chars: usize,
    pub report_chars: usize,
}

impl ExploreSettings {
    pub fn from_config(cfg: &viy_config::config::Config) -> Self {
        let num = |k: &str, d: i64| -> i64 {
            cfg.get(k)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let seqs = match num("MAX_NUM_SEQS", 4) {
            0 => 4,
            n => n,
        };
        let wanted = match num("EXPLORE_PARALLEL", 0) {
            0 => seqs - 1,
            n => n,
        };
        let cap = if seqs - 1 == 0 { 1 } else { seqs - 1 };
        let size = |n: i64| usize::try_from(n).unwrap_or(0);
        Self {
            max_tasks: size(num("EXPLORE_MAX_TASKS", 4)),
            parallel: size(wanted.min(cap).max(1)),
            max_rounds: size(num("EXPLORE_MAX_ROUNDS", 10)),
            max_chars: size(num("EXPLORE_MAX_CONTEXT_CHARS", 150000)),
            report_chars: size(num("EXPLORE_REPORT_CHARS", 8000)),
        }
    }
}

/// One task: a question and where to start.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Task {
    question: String,
    paths: Vec<String>,
}

/// What every helper of one explore call shares.
struct Shared {
    endpoint: Endpoint,
    settings: Settings,
    files: FileTools,
    host: sandbox::Host,
    shell_ok: bool,
    touch: crate::chat::Touch,
    ctx: TurnContext,
    /// The most message characters a helper request may carry.
    ceiling: usize,
    max_rounds: usize,
    max_chars: usize,
}

/// One helper's run.
struct Helper<'a> {
    shared: &'a Shared,
    i: usize,
    task: &'a Task,
    mon: TurnMonitor,
    files: Vec<String>,
    msgs: Vec<Value>,
}

fn size_of(msgs: &[Value]) -> usize {
    msgs.iter().map(|m| m.to_string().chars().count()).sum()
}

fn io_kind(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::NotFound => "FileNotFoundError",
        std::io::ErrorKind::PermissionDenied => "PermissionError",
        std::io::ErrorKind::IsADirectory => "IsADirectoryError",
        std::io::ErrorKind::InvalidData => "UnicodeDecodeError",
        _ => "OSError",
    }
}

impl Helper<'_> {
    fn say(&self, text: String) {
        notice(&self.shared.ctx, NoticeLevel::Info, "explore", text);
    }

    fn read_file(&mut self, args: &Map<String, Value>) -> String {
        let s = self.shared;
        let rel = match args.get("path") {
            Some(Value::String(r)) if !r.is_empty() => r.clone(),
            None | Some(Value::Null) => return json!({"error": "no path given"}).to_string(),
            Some(Value::String(_)) => return json!({"error": "no path given"}).to_string(),
            Some(other) => other.to_string(),
        };
        let path = match s.files.resolve_in_workspace(&rel) {
            Ok(p) => p,
            Err(e) => return json!({"error": e}).to_string(),
        };
        if !path.is_file() {
            return json!({"error": format!("no such file: {rel}")}).to_string();
        }
        if guard::secret_reason(&path).is_some() {
            return json!({"error": format!("{rel} may hold secrets; helpers cannot read it")})
                .to_string();
        }
        let data = match std::fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    let e = format!("cannot read {rel}: UnicodeDecodeError");
                    return json!({"error": e}).to_string();
                }
            },
            Err(e) => {
                return json!({"error": format!("cannot read {rel}: {}", io_kind(&e))}).to_string();
            }
        };
        let lines: Vec<&str> = data.split('\n').collect();
        let shown_data = match explore::line_span(args, lines.len()) {
            Some((a, b)) => (a..=b)
                .map(|n| format!("{n}: {}", lines[n - 1]))
                .collect::<Vec<_>>()
                .join("\n"),
            None => data.clone(),
        };
        let clipped = clip(&shown_data, s.files.max_output);
        let shown = s.files.ws_rel(&path);
        self.files.push(shown.clone());
        json!({"path": shown, "lines": lines.len(), "content": clipped,
               "truncated": clipped != shown_data})
        .to_string()
    }

    fn list_dir(&self, args: &Map<String, Value>) -> String {
        let s = self.shared;
        let rel = args
            .get("path")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
            .unwrap_or(".");
        let path = match s.files.resolve_in_workspace(rel) {
            Ok(p) => p,
            Err(e) => return json!({"error": e}).to_string(),
        };
        if !path.is_dir() {
            let asked = match args.get("path") {
                None | Some(Value::Null) => "None".to_string(),
                Some(Value::String(r)) => r.clone(),
                Some(other) => other.to_string(),
            };
            return json!({"error": format!("not a directory: {asked}")}).to_string();
        }
        let mut names: Vec<(String, bool)> = std::fs::read_dir(&path)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| {
                        (
                            e.file_name().to_string_lossy().into_owned(),
                            e.path().is_dir(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        let entries: Vec<String> = names
            .into_iter()
            .take(500)
            .map(|(n, dir)| if dir { format!("{n}/") } else { n })
            .collect();
        json!({"path": s.files.ws_rel(&path), "entries": entries}).to_string()
    }

    async fn run_shell(&self, args: &Map<String, Value>) -> Result<String, String> {
        let s = self.shared;
        let cmd = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if cmd.is_empty() {
            return Ok(json!({"error": "no command given"}).to_string());
        }
        if !s.shell_ok {
            return Ok(json!({"error": "run_shell is unavailable (no sandbox); use read_file and list_dir"})
                .to_string());
        }
        let jail = Jail {
            readonly: true,
            mounts: &s.files.mounts,
            ..Jail::new(&s.files.workspace, &s.files.project_dir)
        };
        let argv: Vec<OsString> = ["bash", "-c", cmd.as_str()]
            .iter()
            .map(OsString::from)
            .collect();
        let ran = sandbox::run(&s.host, &jail, &argv, Duration::from_secs(60), None)
            .await
            .map_err(|_| "OSError".to_string())?;
        let limit = s.files.max_output;
        Ok(
            json!({"exit_code": ran.exit_code, "timed_out": ran.timed_out,
                  "stdout": clip(&ran.stdout, limit), "stderr": clip(&ran.stderr, 2000)})
            .to_string(),
        )
    }

    async fn dispatch(&mut self, name: &str, raw: &str) -> String {
        let args = match super::tools::parse_args(raw) {
            Ok(a) => a,
            Err(e) => return e.to_string(),
        };
        let out = match name {
            "read_file" => Ok(self.read_file(&args)),
            "list_dir" => Ok(self.list_dir(&args)),
            "run_shell" => self.run_shell(&args).await,
            _ => {
                return json!({"error": format!(
                    "unknown tool {name}; you have read_file, list_dir, run_shell")})
                .to_string();
            }
        };
        match out {
            // Defused as the main chat's tool results are.
            Ok(payload) => untrusted::neutralize(&payload).0,
            Err(kind) => {
                self.mon.mark(
                    "tool_exception",
                    &format!("helper {}: {name} crashed: {kind}", self.i),
                );
                json!({"error": format!("{name} failed: {kind}")}).to_string()
            }
        }
    }

    /// One helper request: `tools` `auto`, `none` (in the prompt, not
    /// callable) or `omit`.
    async fn request(&self, msgs: &[Value], tools: &str) -> Result<Round, String> {
        let s = self.shared;
        let mut body = s.settings.payload(msgs, &[], Tools::Omit, true);
        body["stream"] = json!(false);
        if let Some(m) = body.as_object_mut() {
            m.remove("stream_options");
        }
        if tools != "omit" {
            body["tools"] = Value::Array(explore::helper_tools(s.shell_ok));
            body["tool_choice"] = json!(tools);
        }
        let request = s
            .endpoint
            .request(Method::POST, "/v1/chat/completions")
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&body).unwrap_or_default())
            .timeouts(Duration::from_secs(15), Duration::from_secs(900));
        (s.touch)();
        let sent = viy_http::send(s.endpoint.http.as_ref(), request).await;
        (s.touch)();
        let response = sent.map_err(|e| format!("HttpError: {e}"))?;
        let status = response.status;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("HttpError: {e}"))?;
        if status != 200 {
            let text: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
            return Err(format!("RuntimeError: HTTP {status}: {text}"));
        }
        let j: Value = serde_json::from_slice(&bytes).map_err(|e| format!("ValueError: {e}"))?;
        let ch = &j["choices"][0];
        let m = &ch["message"];
        let text_of = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let tool_calls = m["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tc| ToolCall {
                id: text_of(&tc["id"]),
                name: text_of(&tc["function"]["name"]),
                arguments: text_of(&tc["function"]["arguments"]),
            })
            .filter(|c| !c.name.is_empty())
            .collect();
        let reasoning = match m["reasoning"].as_str().filter(|r| !r.is_empty()) {
            Some(r) => r.to_string(),
            None => text_of(&m["reasoning_content"]),
        };
        Ok(Round {
            text: text_of(&m["content"]),
            reasoning,
            tool_calls,
            usage: j.get("usage").cloned().filter(|u| !u.is_null()),
            finish: ch["finish_reason"].as_str().map(str::to_string),
            first_token: None,
        })
    }

    /// Clips one tool result so the helper's next request still fits.
    fn fit(payload: String, size: usize, ceiling: usize) -> String {
        let room = ceiling.saturating_sub(size);
        let n = payload.chars().count();
        if n <= room {
            return payload;
        }
        let note = "\n...[truncated: no more room in the context window]";
        if room < note.chars().count() + 200 {
            return json!({"error": "no room left in the context window; write your report now"})
                .to_string();
        }
        let mut cut: String = payload.chars().take(room - note.chars().count()).collect();
        cut.push_str(note);
        cut
    }

    /// The helper's status and report (rp's `Helper.run`).
    async fn run(&mut self) -> Result<(&'static str, String), String> {
        let s = self.shared;
        let ceiling = s.ceiling;
        // The soft cap that ends exploration leaves the final round room.
        let max_chars = s.max_chars.min((ceiling as f64 * 0.8) as usize);
        let mut user = self.task.question.clone();
        if !self.task.paths.is_empty() {
            user.push_str(&format!("\n\nStart with: {}", self.task.paths.join(", ")));
        }
        self.msgs = vec![
            json!({"role": "system", "content": explore::HELPER_SYSTEM}),
            json!({"role": "user", "content": user}),
        ];
        let mut report = String::new();
        let mut status = "ok";
        for r in 0..=s.max_rounds {
            let size = size_of(&self.msgs);
            let last = r == s.max_rounds || size > max_chars;
            let mut sent = self.msgs.clone();
            if last && let Some(tail) = sent.last_mut() {
                let content = tail["content"].as_str().unwrap_or_default().to_string();
                tail["content"] = json!(format!("{content}\n\n{}", explore::BUDGET_NOTICE));
            }
            let t0 = Instant::now();
            let out = self
                .request(&sent, if last { "none" } else { "auto" })
                .await?;
            let mode = if last { "final" } else { "auto" };
            self.mon
                .on_round(r as u64, &out, t0.elapsed().as_secs_f64(), mode);
            if out.tool_calls.is_empty() || last {
                report = out.text.clone();
                if last && r == s.max_rounds {
                    status = "capped";
                }
                break;
            }
            let calls: Vec<Value> = out
                .tool_calls
                .iter()
                .enumerate()
                .map(|(k, tc)| {
                    let id = if tc.id.is_empty() {
                        format!("h{}_{r}_{k}", self.i)
                    } else {
                        tc.id.clone()
                    };
                    json!({"id": id, "type": "function",
                           "function": {"name": tc.name, "arguments": tc.arguments}})
                })
                .collect();
            let mut am = json!({"role": "assistant",
                                "content": if out.text.is_empty() { Value::Null } else { json!(out.text) },
                                "tool_calls": calls.clone()});
            if !out.reasoning.is_empty() {
                am["reasoning"] = json!(out.reasoning);
            }
            self.msgs.push(am);
            for (k, tc) in out.tool_calls.iter().enumerate() {
                let t1 = Instant::now();
                let mut payload = self.dispatch(&tc.name, &tc.arguments).await;
                if let Some(note) = self.mon.on_tool(
                    &tc.name,
                    &tc.arguments,
                    &payload,
                    t1.elapsed().as_secs_f64(),
                ) {
                    payload = with_note(&payload, &note);
                }
                self.say(format!(
                    "  [explore {}] {} {}",
                    self.i,
                    tc.name,
                    short_args(&tc.arguments)
                ));
                let size = size_of(&self.msgs);
                let payload = Self::fit(
                    payload,
                    size + explore::BUDGET_NOTICE.chars().count() + 200,
                    ceiling,
                );
                self.msgs
                    .push(json!({"role": "tool", "tool_call_id": calls[k]["id"],
                                      "content": payload}));
            }
        }
        if report.trim().is_empty() {
            let mut sent = self.msgs.clone();
            if let Some(tail) = sent.last_mut() {
                let content = tail["content"].as_str().unwrap_or_default().to_string();
                tail["content"] = json!(format!("{content}\n\n{}", explore::EMPTY_NOTICE));
            }
            let out = self.request(&sent, "omit").await?;
            let rounds = self.mon.rounds;
            self.mon.on_round(rounds, &out, 0.0, "retry");
            report = out.text;
            status = "retried";
        }
        self.msgs
            .push(json!({"role": "assistant", "content": report}));
        Ok((status, report))
    }
}

/// A call's path or command, on one line, for the notice.
fn short_args(raw: &str) -> String {
    let v = match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(a)) => {
            let pick = |k: &str| {
                a.get(k).filter(|v| match v {
                    Value::Null | Value::Bool(false) => false,
                    Value::String(s) => !s.is_empty(),
                    _ => true,
                })
            };
            match pick("path").or_else(|| pick("command")) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            }
        }
        _ => raw.to_string(),
    };
    v.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(90)
        .collect()
}

/// The tasks of an explore call, or the error the model gets.
fn tasks_of(args: &Map<String, Value>, max: usize) -> Result<(Vec<Task>, usize), Value> {
    let tasks: Vec<Value> = match args.get("tasks") {
        Some(Value::String(s)) => vec![json!({"question": s})],
        Some(Value::Array(a)) if !a.is_empty() => a.clone(),
        _ => return Err(json!({"error": "give tasks: a list of 1-4 {question, paths}"})),
    };
    let text_of = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let mut clean = Vec::new();
    for t in tasks.iter().take(max) {
        let t = match t {
            Value::String(s) => json!({"question": s}),
            other => other.clone(),
        };
        let Some(obj) = t.as_object() else { continue };
        let q = obj.get("question").map(text_of).unwrap_or_default();
        if q.trim().is_empty() {
            continue;
        }
        let paths = obj
            .get("paths")
            .and_then(Value::as_array)
            .map(|p| p.iter().take(8).map(text_of).collect())
            .unwrap_or_default();
        clean.push(Task {
            question: q.trim().to_string(),
            paths,
        });
    }
    if clean.is_empty() {
        return Err(json!({"error": "every task needs a non-empty question"}));
    }
    let dropped = tasks.len() - clean.len();
    Ok((clean, dropped))
}

impl Chat {
    /// Whether bubblewrap works here, probed once (rp's `_sandbox_ok`).
    async fn sandbox_ok(&mut self) -> bool {
        if self.sandbox_check.is_none() {
            self.sandbox_check = Some(sandbox::available(&self.sandbox_host()).await);
        }
        matches!(self.sandbox_check, Some(Ok(())))
    }

    /// One explore call (rp's `ExploreRunner.run`): the tasks fanned out
    /// to helpers, their reports collected.
    pub(crate) async fn run_explore(&mut self, ctx: &TurnContext, raw: &str) -> Value {
        let args = match super::tools::parse_args(raw) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let es = self.explore;
        let (tasks, dropped) = match tasks_of(&args, es.max_tasks) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let shell_ok = self.sandbox_ok().await;
        let mut parallel = es.parallel;
        if self.background_running() {
            // A background compaction holds much of a long history in the KV
            // cache (rpctl kb f051); helpers on top would make vLLM preempt.
            parallel = 1;
        }
        let turn_n = self.turn_n;
        let turn = Some(turn_n);
        notice(
            ctx,
            NoticeLevel::Info,
            "explore",
            format!(
                "  [explore] {} helper(s), {} at a time",
                tasks.len(),
                parallel.min(tasks.len())
            ),
        );
        for (i, t) in tasks.iter().enumerate() {
            let q: String = t.question.chars().take(110).collect();
            notice(
                ctx,
                NoticeLevel::Info,
                "explore",
                format!("  [explore {}] {q}", i + 1),
            );
        }
        // Turn numbers restart with every chat process and two explore calls
        // can share a turn, so the transcripts get a run id of their own.
        let tag: String = untrusted::nonce()
            .unwrap_or_default()
            .chars()
            .take(4)
            .collect();
        let run_id = format!("{}-{tag}", chrono::Local::now().format("%Y%m%dT%H%M%S"));
        let task_rows: Vec<Value> = tasks
            .iter()
            .map(|t| json!({"question": t.question, "paths": t.paths}))
            .collect();
        self.tracer.event(
            "explore_start",
            json!({"turn": turn, "tasks": task_rows, "parallel": parallel, "run": run_id}),
        );
        let shared = Shared {
            endpoint: self.endpoint.clone(),
            settings: self.settings.clone(),
            files: self.files.clone(),
            host: self.sandbox_host(),
            shell_ok,
            touch: Arc::clone(&self.touch),
            ctx: ctx.clone(),
            ceiling: (self.prompt_budget() as f64 * self.estimator.chars_per_token * 0.9) as usize,
            max_rounds: es.max_rounds,
            max_chars: es.max_chars,
        };
        let tracer = self.tracer.clone();
        let echo_ctx = ctx.clone();
        let echo: crate::trace::Echo = Arc::new(move |sev, line| {
            let level = if sev == Severity::Error {
                NoticeLevel::Error
            } else {
                NoticeLevel::Warn
            };
            notice(&echo_ctx, level, "alarm", line);
        });
        let report_chars = es.report_chars;
        let work = |i: usize| {
            let (shared, task, tracer, echo, run_id) = (
                &shared,
                &tasks[i],
                tracer.clone(),
                Arc::clone(&echo),
                run_id.clone(),
            );
            async move {
                let mon = TurnMonitor::new(tracer.clone(), &format!("helper{}", i + 1), turn, echo);
                let mut h = Helper {
                    shared,
                    i: i + 1,
                    task,
                    mon,
                    files: Vec::new(),
                    msgs: Vec::new(),
                };
                let t0 = Instant::now();
                let (mut status, report) = match h.run().await {
                    Ok(r) => r,
                    Err(e) => {
                        let shown: String = e.chars().take(150).collect();
                        h.mon.mark(
                            "helper_failed",
                            &format!("helper {} failed: {shown}", i + 1),
                        );
                        ("failed", String::new())
                    }
                };
                if matches!(status, "ok" | "capped" | "retried") && report.trim().is_empty() {
                    status = "empty";
                    h.mon.mark(
                        "helper_failed",
                        &format!("helper {} returned no report", i + 1),
                    );
                }
                let secs = t0.elapsed().as_secs_f64();
                let saved = tracer.save_json(
                    &format!("t{turn_n}-{run_id}-helper{}.json", i + 1),
                    &json!({"task": {"question": task.question, "paths": task.paths},
                            "status": status, "secs": secs, "messages": h.msgs}),
                );
                let files: Vec<&String> = h.files.iter().take(50).collect();
                let mut extra = Map::new();
                extra.insert("status".into(), json!(status));
                extra.insert("files".into(), json!(files));
                extra.insert("transcript".into(), json!(saved));
                let row = h.mon.finish(extra);
                h.say(format!(
                    "  [explore {}] {status} in {secs:.0}s, {} rounds",
                    i + 1,
                    row["rounds"]
                ));
                let read: BTreeSet<&String> = h.files.iter().collect();
                json!({"question": task.question, "status": status, "rounds": row["rounds"],
                       "files_read": read.into_iter().take(30).collect::<Vec<_>>(),
                       "report": clip(report.trim(), report_chars)})
            }
        };
        let results: Vec<Value> = futures_util::stream::iter(0..tasks.len())
            .map(work)
            .buffered(parallel.max(1))
            .collect()
            .await;
        if let Some(m) = self.monitor.as_mut() {
            let failed = results
                .iter()
                .filter(|r| matches!(r["status"].as_str(), Some("failed" | "empty" | "cancelled")))
                .count();
            m.add_helpers(results.len() as u64, failed as u64);
        }
        let mut out = json!({"results": results, "note": explore::RESULT_NOTE});
        if dropped > 0 {
            out["dropped_tasks"] = json!(dropped);
        }
        out
    }
}

#[cfg(test)]
#[path = "workflows_tests.rs"]
mod tests;
