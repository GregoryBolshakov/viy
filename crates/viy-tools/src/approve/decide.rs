//! The approval decision for one sandboxed command (rpctl chat.py
//! `_approve_auto`): destructive questions, the network questions and the
//! reviewer, the rules file and "always". Pure: the user and the reviewer are
//! reached through [`Hearing`], so the same code runs in the chat (the engine
//! asks, and its chat mode may answer) and against the approval decision
//! table (implementation plan 2.3, the `approve/decisions` fixture).
//!
//! What rp prints before a question goes with the question
//! ([`Question::reason`]); what it prints otherwise is said as a notice.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::QuestionKind;

use super::Verdict;
use super::destructive_reason;
use super::net_ask_reason;
use super::plain_segments;
use super::prefix_for;
use super::review::Risk;
use super::rules_allow_net;

/// A future the caller boxes.
pub type Pending<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the decision knows about the command and the chat.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    pub command: &'a str,
    pub needs_network: bool,
    /// The sandbox's workspace (deletes in its own `/tmp` do not ask).
    pub workspace: &'a Path,
    /// Why network commands need the user's yes (rp's `web_taint`).
    pub web_taint: Option<&'a str>,
    /// The `net-allow` rules (`load_rules`).
    pub rules: &'a [Vec<String>],
    /// The rules file as rp names it in "allowed by your rule in ...".
    pub rules_path: &'a str,
}

/// How the decision reaches the user and the reviewer.
pub trait Hearing: Send {
    /// A line rp prints that no question follows.
    fn say(&mut self, notice: NoticeEvent);
    /// A question; the engine's chat mode may answer it.
    fn ask(&mut self, question: Question) -> Pending<'_, Answer>;
    /// The network reviewer's verdict on the command.
    fn review(&mut self) -> Pending<'_, Verdict>;
}

/// The outcome: run it, or the refusal the model reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub go: bool,
    /// `{"declined": ...}` or `{"denied": ...}` when not run.
    pub refusal: Option<Value>,
    /// Prefixes "always" saved, for the rules file, in order.
    pub save_rules: Vec<Vec<String>>,
}

impl Decision {
    fn run() -> Self {
        Self {
            go: true,
            refusal: None,
            save_rules: Vec::new(),
        }
    }

    fn refuse(refusal: Value) -> Self {
        Self {
            go: false,
            refusal: Some(refusal),
            save_rules: Vec::new(),
        }
    }
}

fn line(level: NoticeLevel, text: String) -> NoticeEvent {
    NoticeEvent {
        level,
        code: "approve".into(),
        message: text,
    }
}

/// rp's `_approve_auto(cmd, need_net)`.
pub async fn decide(facts: &Facts<'_>, hearing: &mut dyn Hearing) -> Decision {
    let cmd = facts.command;
    if let Some(why) = destructive_reason(cmd, Some(facts.workspace)) {
        let mut q = Question::yes_no(Some(QuestionKind::Destructive), "  run it?");
        q.reason.push(line(
            NoticeLevel::Warn,
            format!("  {why} -- in your real workspace, no undo (still sandboxed)"),
        ));
        if hearing.ask(q).await != Answer::Yes {
            return Decision::refuse(json!({
                "declined": true,
                "note": format!("the user declined ({why})"),
            }));
        }
    }
    if !facts.needs_network {
        return Decision::run();
    }

    let why = net_ask_reason(cmd);
    if why.is_some() || facts.web_taint.is_some() {
        // With internet text in the chat, a page may be what asked for this,
        // and the reviewer is the same model the page talked to.
        let mut q = Question::yes_no(Some(QuestionKind::Network), "  run it with network access?");
        if let Some(why) = why {
            q.reason.push(line(
                NoticeLevel::Warn,
                format!("  wants the network and {why}"),
            ));
        }
        if let Some(taint) = facts.web_taint {
            q.reason.push(line(
                NoticeLevel::Warn,
                format!(
                    "  untrusted text is in this chat ({taint}), so network commands need your yes"
                ),
            ));
        }
        if hearing.ask(q).await != Answer::Yes {
            let note = why.unwrap_or("untrusted text is in this chat");
            return Decision::refuse(json!({
                "declined": true,
                "note": format!("the user declined network access ({note})"),
            }));
        }
        return Decision::run();
    }

    if rules_allow_net(cmd, facts.rules) {
        hearing.say(line(
            NoticeLevel::Info,
            format!("  network: allowed by your rule in {}", facts.rules_path),
        ));
        return Decision::run();
    }

    let (level, text) = match hearing.review().await {
        Ok((risk @ (Risk::Low | Risk::Medium), reason)) => {
            hearing.say(line(
                NoticeLevel::Info,
                format!("  review: {} -- {reason}", risk.as_str()),
            ));
            return Decision::run();
        }
        Ok((Risk::Critical, reason)) => {
            hearing.say(line(
                NoticeLevel::Error,
                format!("  review: critical -- {reason}. Refused."),
            ));
            return Decision::refuse(json!({
                "denied": true,
                "risk": "critical",
                "reason": reason,
                "note": "an automatic security review refused this command; do not retry it in \
                         another form",
            }));
        }
        Ok((Risk::High, reason)) => (NoticeLevel::Warn, format!("  review: high -- {reason}")),
        Err(why) => (NoticeLevel::Warn, format!("  review failed: {why}")),
    };
    let prefixes: Vec<Option<Vec<String>>> = plain_segments(cmd)
        .unwrap_or_default()
        .iter()
        .map(|seg| prefix_for(seg))
        .collect();
    let can_always = !prefixes.is_empty() && prefixes.iter().all(Option::is_some);
    let mut q = if can_always {
        Question::yes_no_always(Some(QuestionKind::Network), "  run it with network access?")
    } else {
        Question::yes_no(Some(QuestionKind::Network), "  run it with network access?")
    };
    q.reason.push(line(level, text));
    match hearing.ask(q).await {
        Answer::Always if can_always => {
            let mut known: Vec<Vec<String>> = facts.rules.to_vec();
            let mut saved = Vec::new();
            for prefix in prefixes.into_iter().flatten() {
                if !known.contains(&prefix) {
                    hearing.say(line(
                        NoticeLevel::Info,
                        format!("  saved: net-allow {}", prefix.join(" ")),
                    ));
                    known.push(prefix.clone());
                    saved.push(prefix);
                }
            }
            Decision {
                save_rules: saved,
                ..Decision::run()
            }
        }
        Answer::Yes => Decision::run(),
        _ => Decision::refuse(json!({
            "declined": true,
            "note": "the user declined network access",
        })),
    }
}

#[cfg(test)]
#[path = "decide_tests.rs"]
mod tests;
