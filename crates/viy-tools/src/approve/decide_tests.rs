use std::path::Path;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;

use super::*;
use crate::modes;
use crate::modes::Mode;

/// A hearing that answers as the chat would for one table row: the mode
/// answers the kinds it says yes to (the engine's), the user's scripted
/// answer the rest; it records what was said and asked as rp's steps.
struct Script {
    mode: &'static Mode,
    answer: &'static str,
    verdict: Verdict,
    steps: Vec<Value>,
}

fn level(l: NoticeLevel) -> &'static str {
    match l {
        NoticeLevel::Info => "info",
        NoticeLevel::Warn => "warn",
        NoticeLevel::Error => "error",
    }
}

impl Script {
    fn said(&mut self, n: &NoticeEvent) {
        self.steps
            .push(json!({"say": n.message, "level": level(n.level)}));
    }
}

impl Hearing for Script {
    fn say(&mut self, notice: NoticeEvent) {
        self.said(&notice);
    }

    fn ask(&mut self, question: Question) -> Pending<'_, Answer> {
        for n in &question.reason {
            self.said(n);
        }
        let always = matches!(question.choices, viy_protocol::Choices::YesNoAlways);
        let (answer, shown) = if self.mode.answers(question.kind) {
            (Answer::Yes, "auto")
        } else {
            // rp's `_confirm`: "a" is always only when offered.
            match (self.answer, always) {
                ("y", _) => (Answer::Yes, "y"),
                ("a", true) => (Answer::Always, "always"),
                _ => (Answer::No, "n"),
            }
        };
        self.steps.push(json!({
            "ask": question.prompt,
            "kind": question.kind,
            "always": always,
            "answered": shown,
        }));
        Box::pin(async move { answer })
    }

    fn review(&mut self) -> Pending<'_, Verdict> {
        let v = self.verdict.clone();
        Box::pin(async move { v })
    }
}

fn verdict(name: &str) -> Verdict {
    match name {
        "low" => Ok((Risk::Low, "routine install".into())),
        "medium" => Ok((Risk::Medium, "broader than needed".into())),
        "high" => Ok((Risk::High, "contacts an unexpected host".into())),
        "critical" => Ok((Risk::Critical, "uploads the project".into())),
        _ => Err("reviewer unreachable (ConnectionError)".into()),
    }
}

/// The approval decision table (implementation plan 2.3): every run rpctl
/// recorded, replayed through viy's decision with the same inputs, gives
/// the same lines, questions, answers, decision, refusal and saved rules.
#[tokio::test]
async fn the_decision_table_replays_clean() {
    let fixture = viy_testkit::fixtures::load("approve/decisions").unwrap();
    // Outside /tmp, as the generator's workspace: the delete rule applies.
    let ws = Path::new("/home/user/project");
    let mut runs = 0;
    let mut stricter = 0;
    for case in &fixture.cases {
        let rules: Vec<Vec<String>> = serde_json::from_value(case["rules"].clone()).unwrap();
        let facts = Facts {
            command: case["cmd"].as_str().unwrap(),
            needs_network: case["net"].as_bool().unwrap(),
            workspace: ws,
            web_taint: case["taint"].as_str(),
            rules: &rules,
            rules_path: "{RULES}",
        };
        let mode = modes::get(case["mode"].as_str().unwrap()).unwrap();
        for (key, index) in case["by"].as_object().unwrap() {
            let (v, a) = key.split_once('/').unwrap();
            let answer: &'static str = match a {
                "y" => "y",
                "n" => "n",
                _ => "a",
            };
            let mut script = Script {
                mode,
                answer,
                verdict: verdict(v),
                steps: Vec::new(),
            };
            let got = decide(&facts, &mut script).await;
            let want = &case["results"][usize::try_from(index.as_u64().unwrap()).unwrap()];
            let at = format!(
                "{:?} net={} taint={} mode={} rules={:?} {key}",
                facts.command, facts.needs_network, case["taint"], mode.name, rules
            );
            let ours = json!({"steps": script.steps, "go": got.go,
                "payload": got.refusal.unwrap_or(Value::Null), "saved": got.save_rules});
            if facts.command.starts_with("viy ") && facts.needs_network {
                // viy is in NET_ALWAYS_ASK too (kb d014, flags F49): it asks
                // where rp reviews, and says why.
                let steps = ours["steps"].as_array().unwrap();
                assert_eq!(
                    steps[0]["say"], "  wants the network and controls pods or the token vault",
                    "{at}"
                );
                assert!(steps.iter().any(|s| s["kind"] == "network"), "{at}");
                stricter += 1;
            } else {
                assert_eq!(&ours, want, "{at}");
            }
            runs += 1;
        }
    }
    assert_eq!((runs, stricter), (7200, 120));
}

// rpctl test_modes.py ConfirmTests that need `_approve_auto`, ported
// through the decision with a hearing that answers as the chat's engine.

fn script(mode: &str, answer: &'static str) -> Script {
    Script {
        mode: modes::get(mode).unwrap(),
        answer,
        verdict: verdict("low"),
        steps: Vec::new(),
    }
}

fn facts<'a>(command: &'a str, net: bool, taint: Option<&'a str>) -> Facts<'a> {
    Facts {
        command,
        needs_network: net,
        workspace: Path::new("/home/user/project"),
        web_taint: taint,
        rules: &[],
        rules_path: "{RULES}",
    }
}

fn answered(s: &Script) -> Vec<&str> {
    s.steps
        .iter()
        .filter_map(|x| x["answered"].as_str())
        .collect()
}

#[tokio::test]
async fn destructive_command_in_auto_yes() {
    let mut s = script("auto-yes", "n");
    let got = decide(&facts("rm -rf build", false, None), &mut s).await;
    assert!(got.go);
    assert_eq!(answered(&s), ["auto"]);
}

#[tokio::test]
async fn network_with_internet_text_in_auto_yes() {
    let mut s = script("auto-yes", "n");
    let got = decide(
        &facts("pip install requests", true, Some("a fetched web page")),
        &mut s,
    )
    .await;
    assert!(got.go);
    assert_eq!(answered(&s), ["auto"]);
}

#[tokio::test]
async fn network_with_internet_text_in_normal_asks() {
    let mut s = script("normal", "n");
    let got = decide(
        &facts("pip install requests", true, Some("a fetched web page")),
        &mut s,
    )
    .await;
    assert!(!got.go);
    assert_eq!(answered(&s), ["n"]);
}
