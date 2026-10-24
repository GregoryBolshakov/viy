use std::path::Path;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::NoticeEvent;
use viy_protocol::Question;

use super::*;
use crate::modes;
use crate::modes::Mode;

/// A hearing that answers as the chat would for one table row: the mode
/// answers the kinds it says yes to (the engine's rule, only with a
/// terminal), the user's scripted answers the rest, one letter per question,
/// the last repeated, `-` for no terminal (every question no); it records
/// what was said, asked and reviewed as rp's steps, and the rules saved.
struct Script {
    mode: &'static Mode,
    answers: Vec<char>,
    asked: usize,
    verdict: Verdict,
    rules: Vec<Vec<String>>,
    steps: Vec<Value>,
    saved: Vec<Vec<String>>,
}

impl Script {
    fn new(mode: &'static Mode, answers: &str, verdict: Verdict, rules: &[Vec<String>]) -> Self {
        Self {
            mode,
            answers: answers.chars().collect(),
            asked: 0,
            verdict,
            rules: rules.to_vec(),
            steps: Vec::new(),
            saved: Vec::new(),
        }
    }

    fn said(&mut self, n: &NoticeEvent) {
        self.steps.push(json!({"say": n.message, "level": n.level}));
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
        let attended = self.answers != ['-'];
        let letter = self.answers[self.asked.min(self.answers.len() - 1)];
        self.asked += 1;
        let (answer, shown) = if attended && self.mode.answers(question.kind) {
            (Answer::Yes, "auto")
        } else {
            // rp's `_confirm`: "a" is always only when offered.
            match (letter, always) {
                ('y', _) => (Answer::Yes, "y"),
                ('a', true) => (Answer::Always, "always"),
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
        self.steps.push(json!({"review": true}));
        let v = self.verdict.clone();
        Box::pin(async move { v })
    }

    fn rules(&mut self) -> Vec<Vec<String>> {
        self.rules.clone()
    }

    fn save_rule(&mut self, prefix: &[String]) -> Result<(), String> {
        self.saved.push(prefix.to_vec());
        Ok(())
    }
}

fn outcome(d: &Decision) -> (bool, Value) {
    match d {
        Decision::Run => (true, Value::Null),
        Decision::Refuse(v) => (false, v.clone()),
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

/// `viy up` with the network: viy is in NET_ALWAYS_ASK (kb d014, flags
/// F49), rpctl reviews it. viy must never run where rpctl does not, refuses
/// with rpctl's refusal where rpctl refuses, reviews where rpctl reviews,
/// never offers or saves "always" for viy, and says why it asks.
fn viy_is_stricter(script: &Script, go: bool, payload: &Value, want: &Value, at: &str) {
    let steps = &script.steps;
    if want["go"] == false {
        assert!(!go, "{at}: viy runs what rpctl refuses");
        assert_eq!(payload, &want["payload"], "{at}");
    } else if !go {
        assert_eq!(
            payload,
            &json!({"declined": true, "note": "the user declined network access"}),
            "{at}"
        );
    }
    let reviewed = |s: &[Value]| s.iter().any(|x| x["review"] == true);
    assert_eq!(
        reviewed(steps),
        reviewed(want["steps"].as_array().unwrap()),
        "{at}"
    );
    assert!(script.saved.is_empty(), "{at}");
    assert!(steps.iter().all(|x| x["always"] != true), "{at}");
    if want["payload"]["denied"] != true {
        assert!(
            steps
                .iter()
                .any(|x| x["say"] == "  wants the network and controls pods or the token vault"),
            "{at}"
        );
    }
}

/// The approval decision table (implementation plan 2.3): every run rpctl
/// recorded, replayed through viy's decision with the same inputs, gives
/// the same lines, reviewer calls, questions, answers, decision, refusal and
/// saved rules; the runs of `viy up` with the network are checked as
/// stricter.
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
            let (v, answers) = key.split_once('/').unwrap();
            let mut script = Script::new(mode, answers, verdict(v), &rules);
            let got = decide(&facts, &mut script).await;
            let want = &case["results"][usize::try_from(index.as_u64().unwrap()).unwrap()];
            let at = format!(
                "{:?} net={} taint={} mode={} rules={:?} {key}",
                facts.command, facts.needs_network, case["taint"], mode.name, rules
            );
            let (go, payload) = outcome(&got);
            if facts.command.starts_with("viy ") && facts.needs_network {
                viy_is_stricter(&script, go, &payload, want, &at);
                stricter += 1;
            } else {
                let ours = json!({"steps": script.steps, "go": go,
                    "payload": payload, "saved": script.saved});
                assert_eq!(&ours, want, "{at}");
            }
            runs += 1;
        }
    }
    assert_eq!((runs, stricter), (16800, 280));
}

/// The rules file is read again after "always" (it may have changed while
/// the user answered), and a rule that could not be saved is not called
/// saved.
#[tokio::test]
async fn always_reads_the_rules_again_and_reports_a_failed_save() {
    struct Changing(Script, bool);
    impl Hearing for Changing {
        fn say(&mut self, n: NoticeEvent) {
            self.0.say(n);
        }
        fn ask(&mut self, q: Question) -> Pending<'_, Answer> {
            self.0.ask(q)
        }
        fn review(&mut self) -> Pending<'_, Verdict> {
            self.0.review()
        }
        fn rules(&mut self) -> Vec<Vec<String>> {
            vec![vec!["pip".into(), "install".into()]]
        }
        fn save_rule(&mut self, prefix: &[String]) -> Result<(), String> {
            if self.1 {
                return Err("disk full".into());
            }
            self.0.save_rule(prefix)
        }
    }
    let f = facts("pip install a && npm i b", true, None);
    let mut h = Changing(
        Script::new(modes::DEFAULT, "a", verdict("high"), &[]),
        false,
    );
    assert_eq!(decide(&f, &mut h).await, Decision::Run);
    assert_eq!(h.0.saved, [vec!["npm".to_string(), "i".to_string()]]);
    let mut h = Changing(Script::new(modes::DEFAULT, "a", verdict("high"), &[]), true);
    assert_eq!(decide(&f, &mut h).await, Decision::Run);
    let last = h.0.steps.last().unwrap();
    assert_eq!(last["say"], "  could not save net-allow npm i: disk full");
}

// rpctl test_modes.py ConfirmTests that need `_approve_auto`, ported
// through the decision; the engine's own answering is engine_tests.rs's.

fn script(mode: &str, answer: &'static str) -> Script {
    Script::new(modes::get(mode).unwrap(), answer, verdict("low"), &[])
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
    assert_eq!(got, Decision::Run);
    assert_eq!(answered(&s), ["auto"]);
}

#[tokio::test]
async fn network_with_internet_text_in_auto_yes() {
    let mut s = script("auto-yes", "n");
    let f = facts("pip install requests", true, Some("a fetched web page"));
    assert_eq!(decide(&f, &mut s).await, Decision::Run);
    assert_eq!(answered(&s), ["auto"]);
}

#[tokio::test]
async fn network_with_internet_text_in_normal_asks() {
    let mut s = script("normal", "n");
    let f = facts("pip install requests", true, Some("a fetched web page"));
    assert!(matches!(decide(&f, &mut s).await, Decision::Refuse(_)));
    assert_eq!(answered(&s), ["n"]);
}

/// rpctl test_netaccess.py
/// test_network_commands_under_taint_ask_and_skip_the_reviewer.
#[tokio::test]
async fn network_commands_under_taint_ask_and_skip_the_reviewer() {
    let rules = vec![vec!["pip".to_string(), "install".to_string()]];
    let mut s = Script::new(modes::DEFAULT, "n", verdict("low"), &rules);
    let f = Facts {
        rules: &rules,
        ..facts("pip install requests", true, Some("a fetched web page"))
    };
    let got = decide(&f, &mut s).await;
    assert!(matches!(&got, Decision::Refuse(v) if v["declined"] == true));
    assert_eq!(answered(&s), ["n"]);
    assert!(!s.steps.iter().any(|x| x["review"] == true));
    let mut s = script("normal", "n");
    let got = decide(&facts("pip install requests", true, None), &mut s).await;
    assert_eq!(got, Decision::Run);
    assert_eq!(s.steps.iter().filter(|x| x["review"] == true).count(), 1);
}
