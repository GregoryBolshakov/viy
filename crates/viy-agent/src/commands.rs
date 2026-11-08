//! The chat's slash commands (rpctl chat.py `handle_command`), those of a
//! chat without tools; the rest say which phase brings them. Each says
//! what it did in notices with rp's words, and a command that changes the
//! history draws it again ([`EventMsg::HistoryRewritten`], ui-plan 4).
//!
//! `/exit`, `/think`, `/md` and `/redraw` belong to the screen, which
//! handles them itself; one that reaches the chat is told so.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use viy_tools::mounts;
use viy_tools::mounts::Mount;

use base64::Engine as _;
use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::EffortChangedEvent;
use viy_protocol::EventMsg;
use viy_protocol::HistoryItem;
use viy_protocol::HistoryRewrittenEvent;
use viy_protocol::InputPrefillEvent;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::RewriteReason;
use viy_tools::modes;

use super::Chat;
use super::lock;
use super::notice;
use crate::engine::TurnContext;
use crate::filehistory::Action;
use crate::sessions;
use crate::sessions::RowView;
use crate::sessions::Summary;

/// rp's `/help`, with viy's names.
const HELP: &str = "  /help              show this
  /new               start a fresh chat (this one stays saved; alias /reset)
  /system <text>     set the system prompt (starts a fresh chat if this one
                     has messages)
  /rename [title]    name this chat (no arg shows the current name)
  /rewind [n]        go back to before one of your messages: the turns after
                     it leave the chat, the files write_file changed in them
                     can be restored, and the message is back at the prompt
                     to edit. No argument picks it in the transcript (Esc
                     twice at an empty prompt does too)
  /fork [title]      go on in a copy of this chat; the original stays saved
                     as it is (/resume goes back). /fork then /rewind
                     branches from an earlier message
  /chats [query]     list / search saved chats  (--all: every project)
  /resume [query]    switch to a saved chat
  /image <path|url>  attach an image to the next message
  /images            list pending attachments
  /temp <float>      sampling temperature (default 1.0)
  /effort [xhigh|medium|low]  how hard the model thinks (default medium)
  /maxtok <int>      max tokens per reply, reasoning trace included
  /think on|off      stream the reasoning trace, or hide it behind a spinner
  /md on|off         render Markdown in replies (tables, headings, math), or
                     print it raw
  /search [on|off]   let the model search and read the web (needs a
                     tool-capable pod); no argument shows the backends
  /tools on|off      let the model build: write/read/list files & run shell (on)
  /instructions [on|off]  show the AGENTS.md / QWEN.md / CLAUDE.md files loaded
                     into the system prompt, or stop/start loading them
  /mode [normal|auto-yes]  who answers the approver's questions: you, or
                     yes by itself for some kinds (Shift+Tab cycles the modes)
  /doctrines         show the doctrine files the network reviewer reads: your
                     rules in ~/.config/viy/doctrines/ and .viy/doctrines/ (and
                     rp's places), which can only make it stricter
  /sources           list the URLs from this turn's searches
  /compact [full] [focus]  shrink the context: drops old tool output if that
                     is enough, else summarizes (full or a focus: always
                     summarize). The session log keeps everything
  /autocompact on|off|<pct>  compact automatically before the context spills
  /consult [on|off]  ask_claude: hand hard tasks to Claude Code; no argument
                     lists this chat's consults. Also: mode act|advise,
                     model <name>, effort <level>, approval ask|auto
  /toolbox [list|show <name>|find <words>|check|on|off]  command-line tools
                     kept between chats, listed in the system prompt and on
                     run_shell's PATH (viy toolbox add/rm to manage them)
  /mount [<drive|path> [rw]]  lend the model a drive or folder outside the
                     workspace, read-only unless rw; no argument lists drives
  /unmount [<drive|path>|all]  take a mounted drive back
  /ssh [<host> [key file] [full]]  lend the model a server from ~/.ssh/config:
                     it runs ssh <host> '<command>', each one asks you first,
                     sandboxed there unless full; no argument lists hosts.
                     Only you can grant it
  /unssh [<host>|all]  take a server back
  /git [init|import [branch...]|reset]  the model's own git repository in the
                     sandbox (encrypted, local only): its branches, set it
                     up, bring branches into yours as sandbox/*, or start over
  /stats             last response timing

  The pod, as its viy subcommands, in the background (Ctrl-C stops them):
  /up [--timeout S]  create/resume the pod and wait for vLLM
  /status            pod state, GPU, hourly cost, vLLM readiness
  /stop              stop the pod (resumable; ends GPU billing)
  /down              terminate the pod (weights stay on the volume)
  /logs [-f]         read platform and container logs
  /tunnel [--stop|--status|--forget-key]  SSH forward to vLLM
  /boots [list|show] [boot] [-n N]  stage timings of recent boots
  /backup [--samples N]  primary and backup region, A100 stock per region
  /volume [list|create|delete] [id]  manage the weights network volume
  /reap [--quiet]    viy down if nothing streamed for IDLE_TTL_MIN +
                     REAP_GRACE_MIN
  /shutdown [when] [--status|--cancel] [--no-poweroff]  close open chats,
                     viy down, power off this PC

  /redraw            draw the screen again (Ctrl-L)
  /exit              quit  (the pod keeps running - use viy stop)

  End a line with \\ to continue it on the next line.";

/// The commands a later work package brings, and which.
fn later(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "ssh" | "unssh" | "git" => "phase 4",
        "search" | "sources" => "phase 5",
        "compact" | "autocompact" => "phase 6",
        "consult" | "toolbox" => "phase 7",
        _ => return None,
    })
}

/// The pod's commands (kb d041, d068), which the chat's server runs.
pub const POD_COMMANDS: [&str; 11] = [
    "up", "stop", "down", "status", "logs", "tunnel", "boots", "backup", "volume", "reap",
    "shutdown",
];

/// How a replayed tool call is labelled, as the live chat shows it: the
/// tool's name to its label and the argument shown (rp's `_REPLAY_TOOLS`).
fn replay_label(name: &str) -> (&str, Option<&'static str>) {
    match name {
        "run_shell" => ("run", Some("command")),
        "write_file" => ("write", Some("path")),
        "read_file" => ("read", Some("path")),
        "list_dir" => ("list", Some("path")),
        "web_search" => ("search", Some("query")),
        "fetch_url" => ("fetch", Some("url")),
        "find_tool" => ("find_tool", Some("query")),
        "ask_claude" => ("ask_claude", Some("task")),
        "explore" => ("explore", Some("tasks")),
        other => (other, None),
    }
}

/// One tool call of a replayed chat: its label and what it was given (rp's
/// `_replay_call`).
fn replay_call(tc: &Value) -> HistoryItem {
    let f = &tc["function"];
    let name = f["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("?");
    let (label, key) = replay_label(name);
    let raw = f["arguments"].as_str().unwrap_or_default();
    let args: Option<serde_json::Map<String, Value>> =
        serde_json::from_str(if raw.is_empty() { "{}" } else { raw }).ok();
    let what = match (args, key) {
        (None, _) => raw.to_string(),
        (Some(args), Some("tasks")) => {
            let n = args
                .get("tasks")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("{n} task{}", if n == 1 { "" } else { "s" })
        }
        (Some(args), Some(key)) => args
            .get(key)
            .or_else(|| args.get("name"))
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            })
            .unwrap_or_default(),
        (Some(args), None) if args.is_empty() => String::new(),
        (Some(args), None) => Value::Object(args).to_string(),
    };
    HistoryItem::ToolCall {
        call_id: tc["id"].as_str().unwrap_or_default().to_string(),
        tool: label.to_string(),
        summary: what.chars().take(200).collect(),
    }
}

/// What the user typed, without the client's notes; images as `[image]`
/// (rp's `_user_text`).
fn user_text(m: &Value) -> String {
    match &m["content"] {
        Value::Array(parts) => parts
            .iter()
            .filter(|p| {
                p.is_object()
                    && !p["text"]
                        .as_str()
                        .is_some_and(|t| t.starts_with(sessions::CLIENT_NOTE_PREFIX))
            })
            .map(|p| {
                if p["type"] == "image_url" {
                    "[image]".to_string()
                } else {
                    match &p["text"] {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    }
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

/// The history as the transcript draws it, from its start (rp's
/// `show_resumed`, kb d052): what compactions summarised away (`earlier`)
/// above a marker, then each message; the summary the model goes on from
/// is not drawn.
pub fn history_items(messages: &[Value], earlier: &[Value]) -> Vec<HistoryItem> {
    let mut shown: Vec<&Value> = earlier.iter().collect();
    let marker = sessions::compacted();
    let tail: &[Value] = if sessions::opens_with_summary(messages) {
        if !shown.last().is_some_and(|m| m["compacted"] == true) {
            // A fork of a compacted chat.
            shown.push(&marker);
        }
        &messages[2..]
    } else {
        messages
    };
    shown.extend(tail);
    let mut items = Vec::new();
    for m in shown {
        if m["compacted"] == true {
            items.push(HistoryItem::Compacted);
            continue;
        }
        match m["role"].as_str() {
            Some("user") => items.push(HistoryItem::User {
                text: user_text(m).trim_matches('\n').to_string(),
            }),
            Some("assistant") => {
                if sessions::truthy(&m["thought"]) {
                    items.push(HistoryItem::Thought);
                }
                let text = sessions::message_text(m, false);
                let text = text.trim_matches('\n');
                if !text.is_empty() {
                    items.push(HistoryItem::Agent {
                        text: text.to_string(),
                    });
                }
                for tc in m["tool_calls"].as_array().into_iter().flatten() {
                    items.push(replay_call(tc));
                }
            }
            _ => {}
        }
    }
    items
}

/// The chats a user picks from: one-shot ones (`viy exec`) left out, as
/// Codex's resume picker leaves out non-interactive sessions.
fn interactive(rows: Vec<Summary>) -> Vec<Summary> {
    rows.into_iter().filter(|r| !r.exec).collect()
}

/// A message's content as readable text, tool calls included (rp's
/// `_render_content`).
pub fn render_content(m: &Value) -> String {
    let mut c = match &m["content"] {
        Value::Array(parts) => parts
            .iter()
            .map(|p| match p.get("text") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "[image]".into(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    for tc in m["tool_calls"].as_array().into_iter().flatten() {
        let f = &tc["function"];
        let text = |v: &Value| match v {
            Value::String(s) => s.clone(),
            Value::Null => "None".into(),
            other => other.to_string(),
        };
        let sep = if c.is_empty() { "" } else { "\n\n" };
        c.push_str(&format!(
            "{sep}[tool call] {}({})",
            text(&f["name"]),
            text(&f["arguments"])
        ));
    }
    c
}

/// An image for the next message: a URL as it is, a local file as a data
/// URI (rp's `_encode_image`).
fn encode_image(reference: &str, home: &Path) -> std::io::Result<String> {
    if reference.starts_with("http://") || reference.starts_with("https://") {
        return Ok(reference.to_string());
    }
    let path = expand_home(reference, home);
    let bytes = std::fs::read(&path)?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let mime = match ext.as_deref() {
        Some("jpg" | "jpeg" | "jpe") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("tif" | "tiff") => "image/tiff",
        Some("ico") => "image/vnd.microsoft.icon",
        Some("avif") => "image/avif",
        _ => "image/png",
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// `~` and `~/...` under `home`.
fn expand_home(text: &str, home: &Path) -> PathBuf {
    match text.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(text),
    }
}

impl Chat {
    /// Runs the slash command `line` (`/` included).
    pub async fn command(&mut self, ctx: &TurnContext, line: &str) {
        let rest = line.strip_prefix('/').unwrap_or(line);
        let (cmd, arg) = rest.split_once(' ').unwrap_or((rest, ""));
        let arg = arg.trim();
        let info = |code: &str, text: String| notice(ctx, NoticeLevel::Info, code, text);
        let warn = |code: &str, text: String| notice(ctx, NoticeLevel::Warn, code, text);
        match cmd {
            "help" => info("help", HELP.to_string()),
            "new" | "reset" => {
                self.new_session(ctx, None).await;
                self.redraw(ctx, RewriteReason::New);
            }
            "system" => {
                let system = (!arg.is_empty()).then(|| arg.to_string());
                if self.messages.is_empty() {
                    self.system = system;
                } else {
                    self.new_session(ctx, system).await;
                    self.redraw(ctx, RewriteReason::New);
                }
                let what = if arg.is_empty() { "cleared" } else { "set" };
                info("system", format!("system prompt {what}"));
            }
            "rename" => self.rename_session(ctx, arg),
            "rewind" => self.rewind(ctx, arg).await,
            "fork" => self.fork_session(ctx, arg).await,
            "chats" => {
                let words: Vec<&str> = arg.split_whitespace().collect();
                let all = words.contains(&"--all");
                let query = words
                    .iter()
                    .filter(|w| **w != "--all")
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" ");
                self.list_chats(ctx, &query, all);
            }
            "resume" => self.resume(ctx, arg).await,
            "image" => {
                if arg.is_empty() {
                    warn("image", "usage: /image <path|url>".into());
                    return;
                }
                match encode_image(arg, &self.places.home) {
                    Ok(image) => {
                        self.pending_images.push(image);
                        info(
                            "image",
                            format!("attached ({} pending)", self.pending_images.len()),
                        );
                    }
                    Err(_) => notice(
                        ctx,
                        NoticeLevel::Error,
                        "image",
                        format!("no such file: {arg}"),
                    ),
                }
            }
            "images" => {
                if self.pending_images.is_empty() {
                    info("images", "none pending".into());
                }
                for (i, image) in self.pending_images.iter().enumerate() {
                    let more = if image.chars().count() > 70 {
                        "..."
                    } else {
                        ""
                    };
                    let head: String = image.chars().take(70).collect();
                    info("images", format!("  {}. {head}{more}", i + 1));
                }
            }
            "temp" => match arg.parse::<f64>() {
                Ok(t) => {
                    self.settings.temperature = t;
                    info("temp", format!("temperature = {t:?}"));
                }
                Err(_) => warn("temp", "usage: /temp 0.7".into()),
            },
            "maxtok" => match arg.parse::<i64>() {
                Ok(n) => {
                    self.settings.max_tokens = n;
                    info("maxtok", format!("max_tokens = {n}"));
                }
                Err(_) => warn("maxtok", "usage: /maxtok 4096".into()),
            },
            "effort" => {
                if matches!(arg, "xhigh" | "medium" | "low") {
                    self.settings.reasoning_effort = arg.to_string();
                    ctx.emit(EventMsg::EffortChanged(EffortChangedEvent {
                        effort: arg.to_string(),
                    }));
                } else if !arg.is_empty() {
                    warn("effort", "usage: /effort xhigh|medium|low".into());
                    return;
                }
                info(
                    "effort",
                    format!("reasoning effort = {}", self.settings.reasoning_effort),
                );
            }
            "tools" => self.tools_command(ctx, arg).await,
            "mode" => {
                if arg.is_empty() {
                    let now = ctx.mode();
                    for m in &modes::MODES {
                        let mark = if m.name == now.name { "*" } else { " " };
                        info("mode", format!("{mark} {:<10} {}", m.name, m.blurb));
                    }
                    info(
                        "mode",
                        "Shift+Tab cycles them; CHAT_MODE sets the one viy chat starts in".into(),
                    );
                } else if let Some(m) = modes::get(arg) {
                    ctx.set_mode(m);
                    info("mode", m.line());
                } else {
                    let names: Vec<&str> = modes::MODES.iter().map(|m| m.name).collect();
                    warn(
                        "mode",
                        format!("no mode {arg}; the modes are {}", names.join(", ")),
                    );
                }
            }
            "doctrines" => {
                if !arg.is_empty() {
                    warn("doctrines", "usage: /doctrines".into());
                    return;
                }
                for line in self.report_doctrines() {
                    info("doctrines", line);
                }
            }
            "mount" => self.mount(ctx, arg).await,
            "unmount" => self.unmount(ctx, arg),
            "instructions" => {
                if matches!(arg, "on" | "off") {
                    self.instructions_on = arg == "on";
                    info("instructions", format!("instruction files {arg}"));
                    if !self.instructions_on {
                        return;
                    }
                } else if !arg.is_empty() {
                    warn("instructions", "usage: /instructions [on|off]".into());
                    return;
                }
                for line in self.report_instructions() {
                    info("instructions", line);
                }
            }
            "stats" => {
                let stats = self.stats.as_ref().map_or_else(
                    || json!({}),
                    |s| serde_json::to_value(s).unwrap_or_default(),
                );
                info(
                    "stats",
                    serde_json::to_string_pretty(&stats).unwrap_or_default(),
                );
            }
            "exit" | "quit" | "q" | "think" | "md" | "redraw" => {
                warn(
                    "screen_only",
                    format!("/{cmd}: the chat's screen handles it"),
                );
            }
            // A chat on an address it was given (`viy dev chat`) has no pod.
            _ if POD_COMMANDS.contains(&cmd) => warn(
                "no_pod_commands",
                format!(
                    "/{cmd}: this chat talks to the address it was given; viy chat runs the pod commands"
                ),
            ),
            _ => match later(cmd) {
                Some(when) => warn("not_ported", format!("/{cmd} comes with {when}")),
                None => warn(
                    "unknown_command",
                    format!("unknown command: /{cmd}  (try /help)"),
                ),
            },
        }
    }

    /// `/mount`: lists the drives, or lends the model a drive or folder (rp's
    /// `_cmd_mount`).
    async fn mount(&mut self, ctx: &TurnContext, arg: &str) {
        let say = |level: NoticeLevel, text: String| notice(ctx, level, "mount", text);
        let (target, rw) = mounts::parse_arg(arg);
        let table = mounts::mount_table();
        if target.is_empty() {
            if arg.trim().is_empty() {
                self.list_mounts(ctx, &table);
            } else {
                say(NoticeLevel::Warn, "usage: /mount <drive|path> [rw]".into());
            }
            return;
        }
        let path = match mounts::resolve(&target, &table, &self.places.home) {
            Ok(p) => p,
            Err(e) => return say(NoticeLevel::Warn, e),
        };
        let guarded = self.mount_guarded();
        let home = self.places.home.clone();
        if let Some(why) = mounts::problem(&path, rw, &guarded, &table, &home) {
            say(NoticeLevel::Error, format!("not mounted: {why}"));
            if rw && mounts::problem(&path, false, &guarded, &table, &home).is_none() {
                say(
                    NoticeLevel::Info,
                    format!("read-only works: /mount {}", path.display()),
                );
            }
            return;
        }
        if let Some(other) = mounts::overlap(&self.files.mounts, &path) {
            return say(
                NoticeLevel::Warn,
                format!(
                    "not mounted: {} is mounted already and overlaps it; /unmount it first",
                    other.path.display()
                ),
            );
        }
        if rw {
            let mut q = Question::yes_no(None, "  mount it read-write?");
            q.reason.push(NoticeEvent {
                level: NoticeLevel::Warn,
                code: "mount".into(),
                message: format!(
                    "  read-write: the model can change and delete files in {}. /rewind puts \
                     back what write_file changed, not what shell commands did, and the drive \
                     has no other undo.",
                    path.display()
                ),
            });
            if ctx.ask(&q).await != Answer::Yes {
                return say(NoticeLevel::Info, "  not mounted".into());
            }
        }
        // The walk runs off the engine's thread; Ctrl-C (the command
        // cancelled) stops it, as rp's KeyboardInterrupt does.
        let stop = Arc::new(AtomicBool::new(false));
        let mut stopped = StopScan {
            stop: Arc::clone(&stop),
            ctx: Some(ctx.clone()),
            path: path.clone(),
        };
        let walk = {
            let (path, stop) = (path.clone(), Arc::clone(&stop));
            tokio::task::spawn_blocking(move || mounts::scan(&path, rw, &stop))
        };
        let scanned = walk.await.ok().flatten();
        stopped.ctx = None;
        let Some(m) = scanned else {
            return say(
                NoticeLevel::Warn,
                format!(
                    "not mounted: stopped while looking through {}; a smaller folder is quicker",
                    path.display()
                ),
            );
        };
        let (mode, secrets) = (m.mode(), m.secrets.len());
        let scan_s = (m.scan.as_secs_f64() * 100.0).round() / 100.0;
        self.files.mounts.retain(|x| x.path != path);
        self.files.mounts.push(m);
        self.mount_note = Some(mounts::note(&self.files.mounts, &[]));
        self.taint(format!("files from the mounted {}", path.display()));
        self.tracer.event(
            "mount",
            json!({"path": path.display().to_string(), "rw": rw, "secrets": secrets,
                   "scan_s": scan_s}),
        );
        say(
            NoticeLevel::Info,
            format!(
                "mounted {} {mode} until /unmount or the end of this viy chat; the model \
                 learns of it with your next message",
                path.display()
            ),
        );
        if secrets > 0 {
            say(
                NoticeLevel::Info,
                format!("  {secrets} secret file(s) there are hidden from commands"),
            );
        }
        say(
            NoticeLevel::Info,
            "  anything else on it is readable, keys and password stores included; files \
             there count as untrusted text, so network commands ask first"
                .into(),
        );
    }

    /// What a mount must neither hold nor sit inside.
    fn mount_guarded(&self) -> Vec<PathBuf> {
        let mut out = vec![
            self.places.workspace.clone(),
            self.places.project_dir.clone(),
        ];
        out.extend(self.files.clients.iter().cloned());
        out.extend(self.places.guarded.iter().cloned());
        out
    }

    /// `/mount` alone: what is lent, and the drives one could lend.
    fn list_mounts(&self, ctx: &TurnContext, table: &[mounts::Volume]) {
        let say = |level: NoticeLevel, text: String| notice(ctx, level, "mount", text);
        if !self.files.mounts.is_empty() {
            say(NoticeLevel::Info, "mounted for the model:".into());
            for m in &self.files.mounts {
                say(
                    NoticeLevel::Info,
                    format!("  {}  {}", m.path.display(), m.mode()),
                );
            }
        }
        let rows = mounts::drives(table);
        let names: Vec<String> = rows
            .iter()
            .map(|r| {
                mounts::short_name(&r.target, table, &self.places.home)
                    .unwrap_or_else(|| "-".into())
            })
            .collect();
        if rows.is_empty() {
            say(
                NoticeLevel::Info,
                format!(
                    "no drives are mounted under {}; open one in the file manager or run \
                     udisksctl mount -b /dev/<partition> first, or give any folder's path",
                    mounts::DRIVE_ROOTS.join(", ")
                ),
            );
        } else {
            say(
                NoticeLevel::Info,
                "drives on this machine (name, then path):".into(),
            );
            let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
            for (name, row) in names.iter().zip(&rows) {
                let lent = mounts::containing(&self.files.mounts, &row.target).is_some();
                say(
                    NoticeLevel::Info,
                    format!(
                        "  {name:<width$}  {}  {}, {}{}{}",
                        row.target.display(),
                        row.source,
                        row.fstype,
                        if row.read_only { ", read-only" } else { "" },
                        if lent { ", mounted for the model" } else { "" }
                    ),
                );
            }
        }
        let example = names
            .iter()
            .find(|n| *n != "-")
            .map_or("<name>", String::as_str);
        say(
            NoticeLevel::Info,
            format!(
                "/mount {example} (or the path), read-only; add rw to let the model write. \
                 /unmount takes it back."
            ),
        );
    }

    /// `/unmount [<drive|path>|all]`: takes mounts back (rp's
    /// `_cmd_unmount`).
    fn unmount(&mut self, ctx: &TurnContext, arg: &str) {
        let say = |level: NoticeLevel, text: String| notice(ctx, level, "unmount", text);
        let target = arg.trim().trim_matches(['\'', '"']);
        if self.files.mounts.is_empty() {
            return say(NoticeLevel::Info, "nothing is mounted".into());
        }
        let gone: Vec<Mount> =
            if target == "all" || (target.is_empty() && self.files.mounts.len() == 1) {
                self.files.mounts.clone()
            } else if target.is_empty() {
                let listed: Vec<String> = self
                    .files
                    .mounts
                    .iter()
                    .map(|m| m.path.display().to_string())
                    .collect();
                return say(
                    NoticeLevel::Warn,
                    format!(
                        "usage: /unmount <drive|path>|all  (mounted: {})",
                        listed.join(", ")
                    ),
                );
            } else {
                let path = mounts::resolve(target, &mounts::mount_table(), &self.places.home).ok();
                let by_path: Vec<Mount> = self
                    .files
                    .mounts
                    .iter()
                    .filter(|m| Some(&m.path) == path.as_ref())
                    .cloned()
                    .collect();
                let gone = if by_path.is_empty() {
                    self.files
                        .mounts
                        .iter()
                        .filter(|m| {
                            m.path.file_name().is_some_and(|n| {
                                n.to_string_lossy().to_lowercase() == target.to_lowercase()
                            })
                        })
                        .cloned()
                        .collect()
                } else {
                    by_path
                };
                if gone.is_empty() {
                    return say(NoticeLevel::Warn, format!("{target} is not mounted"));
                }
                gone
            };
        self.files.mounts.retain(|m| !gone.contains(m));
        let paths: Vec<PathBuf> = gone.iter().map(|m| m.path.clone()).collect();
        self.mount_note = Some(mounts::note(&self.files.mounts, &paths));
        let listed: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        self.tracer.event("unmount", json!({"paths": listed}));
        say(
            NoticeLevel::Info,
            format!(
                "unmounted {}. What the model read there stays in the chat, so network \
                 commands keep asking.",
                listed.join(", ")
            ),
        );
    }

    /// The next user message: the pending `/mount` note, the line, then the
    /// pending images (rp's `_user_content`; the `/ssh` note comes with
    /// remote.py).
    pub fn user_content(&mut self, line: String) -> Value {
        let note = self.mount_note.take();
        if self.pending_images.is_empty() && note.is_none() {
            return Value::String(line);
        }
        let mut parts = Vec::new();
        for text in note.into_iter().chain(Some(line.clone())) {
            if !text.is_empty() {
                parts.push(json!({"type": "text", "text": text}));
            }
        }
        parts.extend(
            std::mem::take(&mut self.pending_images)
                .into_iter()
                .map(|url| json!({"type": "image_url", "image_url": {"url": url}})),
        );
        if parts.len() > 1 {
            Value::Array(parts)
        } else {
            Value::String(line)
        }
    }

    /// Draws the history again, and the footer's gauge and name with it.
    fn redraw(&mut self, ctx: &TurnContext, reason: RewriteReason) {
        ctx.emit(EventMsg::HistoryRewritten(HistoryRewrittenEvent {
            reason,
            items: history_items(&self.messages, &self.earlier),
        }));
        let (used, _) = self.context_state(&[]);
        ctx.emit(EventMsg::TokenCount(self.token_count(used)));
        self.announce_label(ctx, None);
    }

    fn view<'a>(
        &'a self,
        all_projects: bool,
        current: Option<&'a str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> RowView<'a> {
        RowView {
            all_projects,
            current,
            here: self.project.as_deref(),
            home: &self.places.home,
            now,
        }
    }

    /// `/chats [query] [--all]`.
    fn list_chats(&self, ctx: &TurnContext, query: &str, all: bool) {
        let Some(store) = &self.sessions else {
            return;
        };
        let scope = if all { None } else { self.project.as_deref() };
        let rows = interactive(store.find(query, scope));
        if rows.is_empty() {
            let place = if all {
                "anywhere"
            } else {
                "in this project (--all for every project)"
            };
            let what = if query.is_empty() {
                String::new()
            } else {
                format!("matching '{query}' ")
            };
            notice(
                ctx,
                NoticeLevel::Info,
                "sessions",
                format!("no saved chats {what}{place}"),
            );
            return;
        }
        let current = self.session_id();
        let view = self.view(all, current.as_deref(), store.now());
        for line in sessions::listing(&rows, &view) {
            notice(ctx, NoticeLevel::Info, "rows", line);
        }
    }

    /// `/resume [query]`: a search hit taken at once, else a pick from the
    /// list (rp's `_cmd_resume` and `pick_session`).
    async fn resume(&mut self, ctx: &TurnContext, query: &str) {
        let Some(store) = self.sessions.clone() else {
            return;
        };
        let Some(current) = self.session_id() else {
            notice(
                ctx,
                NoticeLevel::Warn,
                "not_saved",
                "this chat is not saved; there is nothing to resume",
            );
            return;
        };
        self.checkpoint(ctx);
        let mut all = false;
        let mut rows = interactive(store.find(query, self.project.as_deref()));
        if rows.is_empty() {
            rows = interactive(store.find(query, None));
            all = !rows.is_empty();
            if all {
                notice(
                    ctx,
                    NoticeLevel::Info,
                    "pick_session",
                    "nothing in this project; showing every project",
                );
            }
        }
        if rows.is_empty() {
            let matching = if query.is_empty() {
                String::new()
            } else {
                format!(" matching '{query}'")
            };
            notice(
                ctx,
                NoticeLevel::Info,
                "pick_session",
                format!("no saved chats{matching}"),
            );
            return;
        }
        let row = if !query.is_empty() && rows.len() == 1 {
            rows.swap_remove(0)
        } else {
            match self.pick_row(ctx, &rows, all, &current).await {
                Some(row) => row,
                None => return,
            }
        };
        if row.id != current {
            self.resume_id(ctx, &row.id).await;
        }
    }

    /// `viy resume` and `viy fork` (kb d083): saved chat `id`, or without
    /// one a pick of this project's chats, or every project's with `all`,
    /// the one-shot ones only with `include_exec`; the chat is resumed, or
    /// forked with `fork`. Nothing picked leaves the new chat as it is.
    pub async fn open_session(&mut self, ctx: &TurnContext, open: &crate::OpenSession) {
        let Some(store) = self.sessions.clone() else {
            return;
        };
        let id = match &open.id {
            Some(id) => id.clone(),
            None => {
                let project = if open.all {
                    None
                } else {
                    self.project.as_deref()
                };
                let mut rows = store.find("", project);
                if !open.include_exec {
                    rows = interactive(rows);
                }
                if rows.is_empty() {
                    let place = if open.all { "" } else { " in this project" };
                    notice(
                        ctx,
                        NoticeLevel::Info,
                        "pick_session",
                        format!("no saved chats{place}; this is a new one"),
                    );
                    return;
                }
                let current = self.session_id().unwrap_or_default();
                match self.pick_row(ctx, &rows, open.all, &current).await {
                    Some(row) => row.id,
                    None => return,
                }
            }
        };
        if self.resume_id(ctx, &id).await && open.fork {
            self.fork_session(ctx, "").await;
        }
    }

    /// Asks which of `rows` to resume.
    async fn pick_row(
        &self,
        ctx: &TurnContext,
        rows: &[Summary],
        all: bool,
        current: &str,
    ) -> Option<Summary> {
        let store = self.sessions.as_ref()?;
        let view = self.view(all, Some(current), store.now());
        let shown = &rows[..rows.len().min(sessions::ROWS_SHOWN)];
        let options = shown
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let lines = sessions::row_lines(row, i + 1, &view);
                // Without the number and the mark: the pick numbers them.
                let first = lines[0][6..].to_string();
                match lines.get(1) {
                    Some(snippet) => format!("{first} — {}", snippet.trim()),
                    None => first,
                }
            })
            .collect();
        let question = Question::pick("resume which chat?", options, 0);
        match ctx.ask(&question).await {
            Answer::Pick(n) => shown.get(n as usize).cloned(),
            _ => None,
        }
    }

    /// Switches to chat `id` and draws it whole (kb d052).
    pub async fn resume_id(&mut self, ctx: &TurnContext, id: &str) -> bool {
        if !self.attach_session(ctx, id).await {
            return false;
        }
        self.redraw(ctx, RewriteReason::Resume);
        let shown = lock(&self.session)
            .as_ref()
            .map(|log| (super::shown_title(log), log.id().to_string()));
        if let Some((title, id)) = shown {
            notice(
                ctx,
                NoticeLevel::Info,
                "resumed",
                format!(
                    "resumed '{title}'  ({} messages, {id})",
                    self.messages.len()
                ),
            );
        }
        true
    }

    /// (index, message) of each user message `/rewind` can go back to; a
    /// compaction's summary is not one (rp's `_rewind_points`).
    fn rewind_points(&self) -> Vec<usize> {
        let start = if sessions::opens_with_summary(&self.messages) {
            2
        } else {
            0
        };
        (start..self.messages.len())
            .filter(|&i| self.messages[i]["role"] == "user")
            .collect()
    }

    /// `/rewind [n]`: back to before user message n; without one, a pick
    /// (rp's `_cmd_rewind`; the screen picks in the transcript instead,
    /// kb d045).
    async fn rewind(&mut self, ctx: &TurnContext, arg: &str) {
        let points = self.rewind_points();
        if points.is_empty() {
            let why = if self.messages.is_empty() {
                ""
            } else {
                " (the earlier turns were compacted)"
            };
            notice(
                ctx,
                NoticeLevel::Info,
                "rewind",
                format!("nothing to rewind{why}"),
            );
            return;
        }
        let k = if arg.is_empty() {
            if sessions::opens_with_summary(&self.messages) {
                notice(
                    ctx,
                    NoticeLevel::Info,
                    "rewind_points",
                    "  (earlier turns were compacted; /resume draws them)",
                );
            }
            let ends = points[1..].iter().copied().chain([self.messages.len()]);
            let options = points
                .iter()
                .zip(ends)
                .map(|(&i, j)| {
                    let text = sessions::message_text(&self.messages[i], false);
                    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    let more = if text.chars().count() > 70 { "..." } else { "" };
                    let writes = self.messages[i..j]
                        .iter()
                        .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
                        .filter(|tc| tc["function"]["name"] == "write_file")
                        .count();
                    let note = match writes {
                        0 => String::new(),
                        1 => "  (1 write)".into(),
                        n => format!("  ({n} writes)"),
                    };
                    format!("{}{more}{note}", text.chars().take(70).collect::<String>())
                })
                .collect();
            let last = u32::try_from(points.len() - 1).unwrap_or(u32::MAX);
            let question = Question::pick("rewind to before which message?", options, last);
            match ctx.ask(&question).await {
                Answer::Pick(n) => n as usize + 1,
                _ => return,
            }
        } else {
            arg.parse::<usize>().unwrap_or(0)
        };
        if !(1..=points.len()).contains(&k) {
            notice(
                ctx,
                NoticeLevel::Warn,
                "rewind",
                format!(
                    "usage: /rewind [1-{}]  (no argument lists them)",
                    points.len()
                ),
            );
            return;
        }
        self.rewind_to(ctx, points[k - 1], k).await;
    }

    /// Drops the history from message `cut` on, offering to put back the
    /// files write_file changed in it, and hands that user message back to
    /// the input box (rp's `_rewind_to`).
    async fn rewind_to(&mut self, ctx: &TurnContext, cut: usize, k: usize) {
        let calls: Vec<Value> = self.messages[cut..]
            .iter()
            .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
            .cloned()
            .collect();
        let ids = calls.iter().filter_map(|tc| tc["id"].as_str());
        let (files, actions) = self.with_file_history(|history| {
            let files = history.plan(ids);
            let actions = history.actions(&files, false);
            (files, actions)
        });
        if actions.iter().any(|a| *a != Action::Same) {
            let mut lines = vec!["write_file changed these files since then:".to_string()];
            for (f, a) in files.iter().zip(&actions) {
                let note = match a {
                    Action::Restore => String::new(),
                    Action::Delete => "  (created then; would be deleted)".into(),
                    Action::Same => "  (already as it was)".into(),
                    Action::Changed => "  (changed since by something else; stays)".into(),
                    Action::Lost => "  (its old content was not kept; stays)".into(),
                    Action::Failed(e) => format!("  (failed: {e})"),
                };
                lines.push(format!("  {}{note}", self.files.recorded_rel(&f.path)));
            }
            notice(ctx, NoticeLevel::Info, "rewind_to", lines.join("\n"));
        }
        let todo = actions.iter().filter(|a| a.changes()).count();
        let mut applied = Vec::new();
        if todo > 0 {
            let prompt = if todo == 1 {
                "put it back as it was?"
            } else {
                "put them back as they were?"
            };
            match ctx.ask(&Question::yes_no_cancel(prompt)).await {
                Answer::Cancel => return,
                Answer::Yes => {
                    // Without the lock nothing is undone and the chat
                    // stays as it is.
                    let _lock = match self.files.lock() {
                        Ok(lock) => lock,
                        Err(e) => {
                            notice(
                                ctx,
                                NoticeLevel::Error,
                                "rewind_to",
                                format!("could not lock the workspace: {e}"),
                            );
                            return;
                        }
                    };
                    applied = self.with_file_history(|h| h.actions(&files, true));
                }
                _ => {}
            }
        }
        let dropped = self.messages.split_off(cut);
        self.files.rewound(&self.messages, &files);
        let parts: Vec<Value> = match &dropped[0]["content"] {
            Value::Array(parts) => parts.clone(),
            other => vec![json!({"type": "text", "text": other.as_str().unwrap_or_default()})],
        };
        let text = parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .filter(|t| !t.starts_with(sessions::CLIENT_NOTE_PREFIX))
            .collect::<Vec<_>>()
            .join("\n");
        let images: Vec<String> = parts
            .iter()
            .filter(|p| p["type"] == "image_url")
            .filter_map(|p| p["image_url"]["url"].as_str())
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .collect();
        let n_images = images.len();
        let mut pending = images;
        pending.append(&mut self.pending_images);
        self.pending_images = pending;
        // A /mount note the dropped turns carried goes out again, up to date.
        let carried_note = dropped
            .iter()
            .filter(|m| m["role"] == "user")
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter_map(|p| p["text"].as_str())
            .any(mounts::is_note);
        if carried_note {
            self.mount_note = Some(mounts::note(&self.files.mounts, &[]));
        }
        self.checkpoint(ctx);
        self.redraw(ctx, RewriteReason::Rewind);
        let info = |text: String| notice(ctx, NoticeLevel::Info, "rewind_to", text);
        info(format!(
            "rewound to before message {k}: {} messages left the chat (the session log keeps \
             them)",
            dropped.len()
        ));
        let shown = |want: &Action| -> Vec<String> {
            files
                .iter()
                .zip(&applied)
                .filter(|(_, a)| *a == want)
                .map(|(f, _)| self.files.recorded_rel(&f.path))
                .collect()
        };
        let (restored, deleted) = (shown(&Action::Restore), shown(&Action::Delete));
        let failed = applied
            .iter()
            .filter(|a| matches!(a, Action::Failed(_)))
            .count();
        self.tracer.event(
            "rewind",
            json!({"to_message": k, "dropped": dropped.len(),
                   "files_restored": restored.len() + deleted.len(), "files_failed": failed}),
        );
        if !restored.is_empty() {
            info(format!("restored {}", restored.join(", ")));
        }
        if !deleted.is_empty() {
            info(format!("deleted {}", deleted.join(", ")));
        }
        for (f, a) in files.iter().zip(&applied) {
            if let Action::Failed(e) = a {
                notice(
                    ctx,
                    NoticeLevel::Error,
                    "rewind_to",
                    format!(
                        "could not restore {}: {e}",
                        self.files.recorded_rel(&f.path)
                    ),
                );
            }
        }
        let names: Vec<&str> = dropped
            .iter()
            .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
            .filter_map(|tc| tc["function"]["name"].as_str())
            .collect();
        let shell = names.iter().filter(|n| **n == "run_shell").count();
        let claude = names.iter().filter(|n| **n == "ask_claude").count();
        if shell > 0 || claude > 0 {
            let plural = |n: usize| if n > 1 { "s" } else { "" };
            let what: Vec<String> = [
                (shell > 0).then(|| format!("{shell} run_shell command{}", plural(shell))),
                (claude > 0).then(|| format!("{claude} ask_claude task{}", plural(claude))),
            ]
            .into_iter()
            .flatten()
            .collect();
            notice(
                ctx,
                NoticeLevel::Warn,
                "rewind_to",
                format!(
                    "those turns also ran {}; what they changed is not undone",
                    what.join(" and ")
                ),
            );
        }
        if n_images > 0 {
            info(format!(
                "{n_images} image{} attached again",
                if n_images > 1 { "s" } else { "" }
            ));
        }
        if !text.is_empty() {
            ctx.emit(EventMsg::InputPrefill(InputPrefillEvent { text }));
            info("your message is back at the prompt: edit it, or Enter to send it again".into());
        }
    }
}

/// Stops `/mount`'s walk when the command is dropped half way (Ctrl-C),
/// and says so; defused (no `ctx`) once the walk is over.
struct StopScan {
    stop: Arc<AtomicBool>,
    ctx: Option<TurnContext>,
    path: PathBuf,
}

impl Drop for StopScan {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(ctx) = self.ctx.take() {
            notice(
                &ctx,
                NoticeLevel::Warn,
                "mount",
                format!(
                    "not mounted: stopped while looking through {}; a smaller folder is quicker",
                    self.path.display()
                ),
            );
        }
    }
}
