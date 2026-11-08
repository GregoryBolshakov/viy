//! The approver bench's deterministic layer (bench/approver, kb d111): the
//! job on stdin (`{"bases": [...], "verdicts": [...], "answers": [...]}`,
//! as `tools/fixtures.py`'s decision table takes it, each base a case with
//! `cmd`, `net`, `taint`, `mode`, `rules`) through [`approve::decide`] for
//! every verdict and answers, written to stdout in that table's format, so
//! `bench/approver/run.py` can set it beside rpctl's.
//!
//! The reviewer is not asked: its call is recorded and answered with the
//! run's verdict, in the table's words. The user answers at a terminal,
//! one letter per question, the last repeated (`-`: no terminal, every
//! question no); the chat mode says yes to its kinds first, as the engine
//! does.
//!
//! ```text
//! cargo run -q -p viy-tools --example approve_bench -- <workspace> < job.json
//! ```

use std::io::Read;
use std::path::Path;

use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::NoticeEvent;
use viy_protocol::Question;
use viy_tools::approve;
use viy_tools::approve::Decision;
use viy_tools::approve::Facts;
use viy_tools::approve::Hearing;
use viy_tools::approve::Pending;
use viy_tools::approve::Risk;
use viy_tools::approve::Verdict;
use viy_tools::modes;
use viy_tools::modes::Mode;

/// The decision table's verdicts (tools/fixtures.py `VERDICTS`).
fn verdict(name: &str) -> Verdict {
    match name {
        "low" => Ok((Risk::Low, "routine install".into())),
        "medium" => Ok((Risk::Medium, "broader than needed".into())),
        "high" => Ok((Risk::High, "contacts an unexpected host".into())),
        "critical" => Ok((Risk::Critical, "uploads the project".into())),
        _ => Err("reviewer unreachable (ConnectionError)".into()),
    }
}

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
    fn said(&mut self, n: &NoticeEvent) {
        self.steps.push(json!({"say": n.message, "level": n.level}));
    }
}

impl Hearing for Script {
    fn say(&mut self, n: NoticeEvent) {
        self.said(&n);
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
            match (letter, always) {
                ('y', _) => (Answer::Yes, "y"),
                ('a', true) => (Answer::Always, "always"),
                _ => (Answer::No, "n"),
            }
        };
        self.steps
            .push(json!({"ask": question.prompt, "kind": question.kind,
                               "always": always, "answered": shown}));
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

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn main() -> std::io::Result<()> {
    let workspace = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(std::io::Error::other)?;
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let job: Value = serde_json::from_str(&text).map_err(std::io::Error::other)?;
    let (verdicts, answers) = (strings(&job["verdicts"]), strings(&job["answers"]));
    let mut out = Vec::new();
    for base in job["bases"].as_array().into_iter().flatten() {
        let rules: Vec<Vec<String>> =
            serde_json::from_value(base["rules"].clone()).unwrap_or_default();
        let facts = Facts {
            command: base["cmd"].as_str().unwrap_or_default(),
            needs_network: base["net"].as_bool().unwrap_or(false),
            workspace: Path::new(&workspace),
            web_taint: base["taint"].as_str(),
            rules: &rules,
            rules_path: "{RULES}",
        };
        let name = base["mode"].as_str().unwrap_or_default();
        let mode = modes::get(name)
            .ok_or_else(|| std::io::Error::other(format!("no chat mode {name:?}")))?;
        let mut results: Vec<Value> = Vec::new();
        let mut by = serde_json::Map::new();
        for v in &verdicts {
            for a in &answers {
                let mut script = Script {
                    mode,
                    answers: a.chars().collect(),
                    asked: 0,
                    verdict: verdict(v),
                    rules: rules.clone(),
                    steps: Vec::new(),
                    saved: Vec::new(),
                };
                let (go, payload) = match runtime.block_on(approve::decide(&facts, &mut script)) {
                    Decision::Run => (true, Value::Null),
                    Decision::Refuse(v) => (false, v),
                };
                let got = json!({"steps": script.steps, "go": go, "payload": payload,
                                 "saved": script.saved});
                let index = match results.iter().position(|r| *r == got) {
                    Some(i) => i,
                    None => {
                        results.push(got);
                        results.len() - 1
                    }
                };
                by.insert(format!("{v}/{a}"), json!(index));
            }
        }
        let mut row = base.clone();
        if let Some(m) = row.as_object_mut() {
            m.insert("results".into(), Value::Array(results));
            m.insert("by".into(), Value::Object(by));
        }
        out.push(row);
    }
    println!("{}", Value::Array(out));
    Ok(())
}
