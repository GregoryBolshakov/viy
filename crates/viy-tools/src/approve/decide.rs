//! The approval decision for one sandboxed command (rpctl chat.py
//! `_approve_auto`): destructive questions, the network questions and the
//! reviewer, the rules file and "always". Pure: the user, the reviewer and
//! the rules file are reached through [`Hearing`], so the same code runs in
//! the chat (the engine asks, and its chat mode may answer) and against the
//! approval decision table (implementation plan 2.3, the
//! `approve/decisions` fixture).
//!
//! What rp prints before a question goes with the question
//! ([`Question::reason`]); what it prints otherwise is said as a notice.
//!
//! One path is viy's own: viy is in NET_ALWAYS_ASK (kb d014), where rpctl
//! sends `viy up` to the reviewer. A command only viy's name makes ask is
//! still reviewed first, so a critical verdict refuses it as in rpctl; any
//! other verdict asks the user, where rpctl would run on low or medium.

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
use super::rp_net_ask_reason;
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

/// How the decision reaches the user, the reviewer and the rules file.
pub trait Hearing: Send {
    /// A line rp prints that no question follows.
    fn say(&mut self, notice: NoticeEvent);
    /// A question; the engine's chat mode may answer it.
    fn ask(&mut self, question: Question) -> Pending<'_, Answer>;
    /// The network reviewer's verdict on the command. The caller builds the
    /// request (recent user requests, workspace, where the command can
    /// connect, which .git it sees).
    fn review(&mut self) -> Pending<'_, Verdict>;
    /// The rules as the file holds them now (rp reads it again after an
    /// "always", which may come long after the decision started).
    fn rules(&mut self) -> Vec<Vec<String>>;
    /// Appends `net-allow <prefix>` to the rules file.
    fn save_rule(&mut self, prefix: &[String]) -> Result<(), String>;
}

/// The outcome: run it, or the refusal the model reads.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Run,
    /// `{"declined": ...}` or `{"denied": ...}`, as rpctl's.
    Refuse(Value),
}

fn line(level: NoticeLevel, text: String) -> NoticeEvent {
    NoticeEvent {
        level,
        code: "approve".into(),
        message: text,
    }
}

const NETWORK_PROMPT: &str = "  run it with network access?";
const DECLINED_NETWORK: &str = "the user declined network access";

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
            return Decision::Refuse(json!({
                "declined": true,
                "note": format!("the user declined ({why})"),
            }));
        }
    }
    if !facts.needs_network {
        return Decision::Run;
    }

    let why = net_ask_reason(cmd);
    let rp_why = rp_net_ask_reason(cmd);
    let asks = |why: Option<&str>| {
        why.map(|w| line(NoticeLevel::Warn, format!("  wants the network and {w}")))
    };
    if rp_why.is_some() || facts.web_taint.is_some() {
        // With internet text in the chat, a page may be what asked for this,
        // and the reviewer is the same model the page talked to.
        let mut q = Question::yes_no(Some(QuestionKind::Network), NETWORK_PROMPT);
        q.reason.extend(asks(why));
        if let Some(taint) = facts.web_taint {
            q.reason.push(line(
                NoticeLevel::Warn,
                format!(
                    "  untrusted text is in this chat ({taint}), so network commands need your yes"
                ),
            ));
        }
        if hearing.ask(q).await != Answer::Yes {
            // rpctl's note: the reason its own lists give.
            let note = rp_why.unwrap_or("untrusted text is in this chat");
            return Decision::Refuse(json!({
                "declined": true,
                "note": format!("{DECLINED_NETWORK} ({note})"),
            }));
        }
        return Decision::Run;
    }

    if rules_allow_net(cmd, facts.rules) {
        hearing.say(line(
            NoticeLevel::Info,
            format!("  network: allowed by your rule in {}", facts.rules_path),
        ));
        return Decision::Run;
    }

    let (level, text) = match hearing.review().await {
        Ok((risk @ (Risk::Low | Risk::Medium), reason)) => {
            let text = format!("  review: {} -- {reason}", risk.as_str());
            if why.is_none() {
                hearing.say(line(NoticeLevel::Info, text));
                return Decision::Run;
            }
            // Only viy's name asks: rpctl would run it on this verdict.
            (NoticeLevel::Info, text)
        }
        Ok((Risk::Critical, reason)) => {
            hearing.say(line(
                NoticeLevel::Error,
                format!("  review: critical -- {reason}. Refused."),
            ));
            return Decision::Refuse(json!({
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
        Question::yes_no_always(Some(QuestionKind::Network), NETWORK_PROMPT)
    } else {
        Question::yes_no(Some(QuestionKind::Network), NETWORK_PROMPT)
    };
    q.reason.push(line(level, text));
    q.reason.extend(asks(why));
    match hearing.ask(q).await {
        Answer::Always if can_always => {
            let mut known = hearing.rules();
            for prefix in prefixes.into_iter().flatten() {
                if known.contains(&prefix) {
                    continue;
                }
                match hearing.save_rule(&prefix) {
                    Ok(()) => hearing.say(line(
                        NoticeLevel::Info,
                        format!("  saved: net-allow {}", prefix.join(" ")),
                    )),
                    // The user said yes; only remembering it failed.
                    Err(e) => hearing.say(line(
                        NoticeLevel::Warn,
                        format!("  could not save net-allow {}: {e}", prefix.join(" ")),
                    )),
                }
                known.push(prefix);
            }
            Decision::Run
        }
        Answer::Yes => Decision::Run,
        _ => Decision::Refuse(json!({"declined": true, "note": DECLINED_NETWORK})),
    }
}

#[cfg(test)]
#[path = "decide_tests.rs"]
mod tests;
