//! The chat's side of servers lent with `/ssh` (rp's `_cmd_ssh`,
//! `_cmd_unssh`, `_print_ssh_hosts`, `_run_remote`, `_review_remote`;
//! [`viy_tools::remote`], rpctl kb d027, d029, d030, d031).
//!
//! Only `/ssh`, typed by a person at the chat, adds a host; it lasts until
//! `/unssh` or the end of the chat, is not stored in the session log, and
//! the model hears of it in the next user message.

use std::time::Duration;

use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::EventMsg;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::QuestionKind;
use viy_protocol::ToolCallOutputDeltaEvent;
use viy_tools::approve;
use viy_tools::approve::Review;
use viy_tools::approve::Risk;
use viy_tools::remote;
use viy_tools::remote::Host;

use super::Chat;
use super::notice;
use super::tools::Card;
use crate::engine::TurnContext;
use crate::files;

impl Chat {
    /// `/ssh` alone: what is lent and the config's hosts (rp's
    /// `_print_ssh_hosts`).
    fn list_ssh_hosts(&self, ctx: &TurnContext) {
        let say = |level: NoticeLevel, text: String| notice(ctx, level, "ssh", text);
        if !self.ssh_hosts.is_empty() {
            say(NoticeLevel::Info, "lent to the model:".into());
            for h in &self.ssh_hosts {
                say(
                    NoticeLevel::Info,
                    format!(
                        "  {}  {}, key {}{}, {}",
                        h.alias,
                        h.login(),
                        h.key.display(),
                        if h.agent.is_empty() {
                            ""
                        } else {
                            ", from your ssh agent"
                        },
                        if h.boxed { "sandboxed" } else { "full access" }
                    ),
                );
            }
        }
        let config = self.places.home.join(".ssh").join("config");
        let names = std::fs::read(&config)
            .map(|b| remote::config_hosts(&String::from_utf8_lossy(&b)))
            .unwrap_or_default();
        if names.is_empty() {
            say(
                NoticeLevel::Info,
                "no Host entries in ~/.ssh/config; give a host name and a key file".into(),
            );
        } else {
            say(NoticeLevel::Info, "hosts in ~/.ssh/config:".into());
            for n in &names {
                let lent = self.ssh_hosts.iter().any(|h| &h.alias == n);
                say(
                    NoticeLevel::Info,
                    format!("  {n}{}", if lent { "  lent to the model" } else { "" }),
                );
            }
        }
        say(
            NoticeLevel::Info,
            format!(
                "/ssh {} [key file] [full] lends one; the model then runs ssh <host> \
                 '<command>', sandboxed on the server unless full, and each command asks you. \
                 /unssh takes it back.",
                names.first().map_or("<host>", String::as_str)
            ),
        );
    }

    /// `/ssh [<host> [key file] [full]]`: list, or lend the model a server
    /// (rp's `_cmd_ssh`). The only place a host is added.
    pub(crate) async fn ssh_command(&mut self, ctx: &TurnContext, arg: &str) {
        let say = |level: NoticeLevel, text: String| notice(ctx, level, "ssh", text);
        let (alias, key, full) = remote::parse_arg(arg);
        if alias.is_empty() {
            self.list_ssh_hosts(ctx);
            return;
        }
        if !ctx.attended() {
            say(
                NoticeLevel::Error,
                "not lent: /ssh needs you at a terminal to answer for each command".into(),
            );
            return;
        }
        let home = self.places.home.clone();
        let resolved = {
            let (alias, key) = (alias.clone(), key.clone());
            let machine = self.ssh_machine.clone();
            tokio::task::spawn_blocking(move || {
                remote::resolve(machine.as_ref(), &alias, key.as_deref(), !full)
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        };
        let host = match resolved {
            Ok(h) => h,
            Err(e) => {
                say(NoticeLevel::Error, format!("not lent: {e}"));
                return;
            }
        };
        if host.boxed {
            say(
                NoticeLevel::Info,
                format!("  checking the sandbox on {}", host.alias),
            );
            if let Some(why) = self.ssh_runner.probe(&host, &home).await {
                say(
                    NoticeLevel::Error,
                    format!(
                        "not lent: the sandbox does not run on {}: {why}",
                        host.alias
                    ),
                );
                say(
                    NoticeLevel::Info,
                    format!(
                        "  install bubblewrap there (README, Lending a server), or lend it \
                         without the sandbox: /ssh {alias} full"
                    ),
                );
                return;
            }
        }
        self.ssh_hosts.retain(|h| h.alias != host.alias);
        self.ssh_hosts.push(host.clone());
        self.ssh_note = Some(remote::note(&self.ssh_hosts, &[]));
        self.tracer.event(
            "ssh_lend",
            json!({"host": host.alias, "login": host.login(), "key": host.key.display().to_string(),
                   "agent": !host.agent.is_empty(), "boxed": host.boxed}),
        );
        say(
            NoticeLevel::Info,
            format!(
                "lent {} ({}, key {}{}) until /unssh or the end of this chat; the model learns \
                 of it with your next message",
                host.alias,
                host.login(),
                host.key.display(),
                if host.agent.is_empty() {
                    ""
                } else {
                    ", from your ssh agent"
                }
            ),
        );
        if host.boxed {
            say(
                NoticeLevel::Info,
                format!(
                    "  sandboxed there: the server's files read-only, ~/{} the only writable \
                     place, no network, ~/.ssh, ~/.config and secret files hidden.",
                    remote::BOX_DIR
                ),
            );
        } else {
            say(
                NoticeLevel::Warn,
                format!(
                    "  full access: the model may ask to run anything {} can do there, and the \
                     server's internet is not filtered by the egress proxy.",
                    host.user
                ),
            );
        }
        say(
            NoticeLevel::Info,
            "  the reviewer rates every command; then it waits for your y/N, which auto-yes \
             answers for you"
                .into(),
        );
        if !self.tools_on {
            say(
                NoticeLevel::Info,
                "  build tools are off (/tools on)".into(),
            );
        }
    }

    /// `/unssh [<host>|all]`: take lent servers back (rp's `_cmd_unssh`).
    pub(crate) fn unssh_command(&mut self, ctx: &TurnContext, arg: &str) {
        let say = |level: NoticeLevel, text: String| notice(ctx, level, "ssh", text);
        let target = arg.trim().trim_matches(['\'', '"']);
        if self.ssh_hosts.is_empty() {
            say(NoticeLevel::Info, "no server is lent".into());
            return;
        }
        let gone: Vec<Host> = if target == "all" || target.is_empty() && self.ssh_hosts.len() == 1 {
            self.ssh_hosts.clone()
        } else if target.is_empty() {
            let lent: Vec<&str> = self.ssh_hosts.iter().map(|h| h.alias.as_str()).collect();
            say(
                NoticeLevel::Warn,
                format!("usage: /unssh <host>|all  (lent: {})", lent.join(", ")),
            );
            return;
        } else {
            let t = target.to_lowercase();
            let gone: Vec<Host> = self
                .ssh_hosts
                .iter()
                .filter(|h| h.targets().contains(&t))
                .cloned()
                .collect();
            if gone.is_empty() {
                say(NoticeLevel::Warn, format!("{target} is not lent"));
                return;
            }
            gone
        };
        self.ssh_hosts.retain(|h| !gone.contains(h));
        let names: Vec<String> = gone.iter().map(|h| h.alias.clone()).collect();
        self.ssh_note = Some(remote::note(&self.ssh_hosts, &names));
        self.tracer.event("ssh_unlend", json!({"hosts": names}));
        say(
            NoticeLevel::Info,
            format!(
                "took back {}. What the model saw there stays in the chat, so network commands \
                 keep asking.",
                names.join(", ")
            ),
        );
    }

    /// The note a fresh, resumed or rewound history gets while servers are
    /// lent.
    pub(crate) fn ssh_hosts_note(&self) -> Option<String> {
        (!self.ssh_hosts.is_empty()).then(|| remote::note(&self.ssh_hosts, &[]))
    }

    /// run_shell's `ssh <host> '<command>'` for a lent server (rp's
    /// `_run_remote`): after the reviewer and your yes the client runs ssh
    /// itself, outside the sandbox here, with the key and options fixed at
    /// `/ssh`; on the server the command runs in bubblewrap unless lent
    /// with full.
    pub(crate) async fn run_remote(
        &mut self,
        ctx: &TurnContext,
        call_id: &str,
        cmd: &str,
        host: Host,
        rcmd: &str,
    ) -> (Value, Card) {
        let line = |level: NoticeLevel, text: String| NoticeEvent {
            level,
            code: "ssh".into(),
            message: text,
        };
        let where_ = if host.boxed {
            format!("sandboxed there, writes only ~/{}", remote::BOX_DIR)
        } else {
            "not sandboxed".into()
        };
        let mut reason = vec![line(
            NoticeLevel::Info,
            format!("  [ssh {}]  (on {}, {where_})", host.alias, host.login()),
        )];
        let shown = remote::shown(rcmd);
        let lines: Vec<&str> = if shown.is_empty() {
            vec![""]
        } else {
            shown.lines().collect()
        };
        reason.extend(
            lines
                .iter()
                .map(|l| line(NoticeLevel::Info, format!("    {l}"))),
        );
        if let Some(why) = approve::destructive_reason(rcmd, None) {
            reason.push(line(
                NoticeLevel::Warn,
                format!("  looks destructive: {why}"),
            ));
        }
        match (remote::root_word(rcmd), host.boxed) {
            (Some(root), true) => reason.push(line(
                NoticeLevel::Warn,
                format!(
                    "  uses {root}, which cannot work in the sandbox (/ssh {} full lends it \
                     without one)",
                    host.alias
                ),
            )),
            (Some(root), false) => reason.push(line(
                NoticeLevel::Error,
                format!(
                    "  runs as root ({root}): every file, service and user on {}",
                    host.alias
                ),
            )),
            _ => {}
        }
        let mut card = Card {
            detail: None,
            sandboxed: host.boxed,
            // lent with full, the command has the server's own internet
            network: !host.boxed,
        };
        let (refusal, verdict) = self.review_remote(&host, rcmd).await;
        reason.push(verdict);
        if let Some(refusal) = refusal {
            for r in reason {
                ctx.emit(EventMsg::Notice(r));
            }
            self.count_denial();
            card.detail = Some("denied".into());
            return (refusal, card);
        }
        let question = Question {
            reason,
            ..Question::yes_no(
                Some(QuestionKind::Remote),
                format!("  run it on {}?", host.alias),
            )
        };
        if ctx.ask(&question).await != Answer::Yes {
            self.tracer
                .event("ssh_declined", json!({"host": host.alias}));
            self.count_denial();
            card.detail = Some("declined".into());
            return (
                json!({"declined": true,
                       "note": format!("the user declined to run this command on {}", host.alias)}),
                card,
            );
        }
        self.denials = 0;
        let timeout = self.shell_timeout;
        // A boxed command is stopped on the server at `timeout`; ssh gets a
        // little longer so its output still comes back.
        let wait = Duration::from_secs(if host.boxed { timeout + 15 } else { timeout });
        let remote_cmd = remote::command(&host, rcmd, timeout);
        let runner = std::sync::Arc::clone(&self.ssh_runner);
        let ran = runner
            .run(
                &host,
                &remote_cmd,
                &self.places.workspace,
                &self.places.home,
                wait,
            )
            .await;
        let ran = match ran {
            Ok(r) => r,
            Err(e) => {
                card.detail = Some("failed".into());
                return (
                    json!({"command": cmd, "host": host.alias,
                           "error": format!("ssh did not start: {e}")}),
                    card,
                );
            }
        };
        // Whatever the server sent is outside text, like a web page.
        self.taint(format!("output from {}", host.alias));
        let (rc, timed_out) = (ran.exit_code, ran.timed_out);
        self.tracer.event(
            "ssh_run",
            json!({"host": host.alias, "exit_code": rc, "timed_out": timed_out}),
        );
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
        card.detail = Some(if timed_out {
            "timed out".into()
        } else {
            format!("exit {}", rc.unwrap_or(-1))
        });
        let mut result = Map::new();
        result.insert("command".into(), json!(cmd));
        result.insert("host".into(), json!(host.alias));
        result.insert("exit_code".into(), json!(rc));
        result.insert("timed_out".into(), json!(timed_out));
        result.insert("stdout".into(), json!(out));
        result.insert("stderr".into(), json!(err));
        let hint = if timed_out {
            Some(format!(
                "ssh was stopped after {timeout}s; the command may still be running on the server"
            ))
        } else if host.boxed && rc == Some(124) {
            Some(format!(
                "exit 124: the server stopped the command after {timeout}s"
            ))
        } else if host.boxed && rc == Some(125) {
            Some(format!(
                "exit 125: the sandbox could not start on the server (~/{} could not be created)",
                remote::BOX_DIR
            ))
        } else if rc == Some(255) {
            Some(
                "exit 255 usually means ssh itself failed (connection, host key or key); stderr \
                 says which"
                    .into(),
            )
        } else {
            None
        };
        if let Some(hint) = hint {
            result.insert("hint".into(), json!(hint));
        }
        (Value::Object(result), card)
    }

    /// The reviewer's verdict on a command for a lent server, as the line
    /// shown before the question (rp's `_review_remote`), and the refusal
    /// for `critical`: every other verdict, a failed review included, goes
    /// on to the y/N.
    async fn review_remote(&mut self, host: &Host, rcmd: &str) -> (Option<Value>, NoticeEvent) {
        let server = if host.boxed {
            format!(
                "{} ({}), inside a sandbox there: the server's files are read-only except ~/{}, \
                 there is no network, ~/.ssh, ~/.config and secret files are hidden and no \
                 other process is visible",
                host.alias,
                host.login(),
                remote::BOX_DIR
            )
        } else {
            format!(
                "{} ({}), not sandboxed: everything that account can do there, sudo included if \
                 it has it, and the server's own unfiltered internet",
                host.alias,
                host.login()
            )
        };
        let users = self.recent_user_requests();
        let endpoint = self.endpoint.clone();
        let review = Review {
            server: Some(&server),
            ..Review::new(
                &endpoint.base,
                &endpoint.key,
                &self.settings.model,
                &users,
                rcmd,
                &self.places.workspace,
            )
        };
        let verdict = super::tools::reviewed(
            &review,
            self.places.doctrine_sources.clone(),
            endpoint.http.as_ref(),
        )
        .await;
        let risk = verdict.as_ref().ok().map(|(r, _)| r.as_str());
        self.tracer
            .event("ssh_review", json!({"host": host.alias, "risk": risk}));
        let say = |level: NoticeLevel, text: String| NoticeEvent {
            level,
            code: "ssh".into(),
            message: text,
        };
        match verdict {
            Ok((Risk::Critical, reason)) => (
                Some(json!({"denied": true, "risk": "critical", "reason": reason,
                            "note": "an automatic security review refused this command; do not \
                                     retry it in another form"})),
                say(
                    NoticeLevel::Error,
                    format!("  review: critical -- {reason}. Refused."),
                ),
            ),
            Ok((risk, reason)) => (
                None,
                say(
                    if risk == Risk::High {
                        NoticeLevel::Warn
                    } else {
                        NoticeLevel::Info
                    },
                    format!("  review: {} -- {reason}", risk.as_str()),
                ),
            ),
            Err(why) => (
                None,
                say(NoticeLevel::Warn, format!("  review failed: {why}")),
            ),
        }
    }
}
