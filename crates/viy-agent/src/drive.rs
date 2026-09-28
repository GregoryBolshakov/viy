//! The driver behind `viy dev drive`: a scenario's steps through the engine,
//! as `tools/drive_rp.py` takes them through rp's chat (implementation plan
//! 3.4, tests/scenarios/README.md). Each step is a submission: a `user` line
//! a turn, a `command` a slash command, and the next one goes in when the
//! last has ended. Questions are answered from the scenario's `approvals`,
//! in order, by the `Op` naming each question; an unscripted question, or an
//! answer never asked for, is a problem of the run, as in drive_rp. A
//! background title started by a turn is waited for before the next step,
//! as drive_rp joins rp's title thread, so the requests come in a fixed
//! order.
//!
//! The model server, the workspace and the temporary XDG directories are
//! the caller's: `tools/drive_rp.py --client viy` gives `viy dev drive`
//! all three, as it gives them to rp.

use serde_json::Value;
use viy_protocol::Answer;
use viy_protocol::BackgroundState;
use viy_protocol::BackgroundTask;
use viy_protocol::Event;
use viy_protocol::EventMsg;
use viy_protocol::Op;

use crate::engine::Engine;

/// One step of a scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    User(String),
    Command(String),
}

/// What the driver takes from a scenario file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    pub steps: Vec<Step>,
    /// Part of a question's prompt, and its answer, in the order asked.
    pub approvals: Vec<(String, Answer)>,
}

impl Scenario {
    /// From a scenario file's JSON; what is wrong with it otherwise.
    pub fn from_json(scenario: &Value) -> Result<Self, String> {
        let list = |key: &str| {
            scenario
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let steps = list("steps")
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let text = |key| step.get(key).and_then(Value::as_str).map(str::to_string);
                match (text("user"), text("command")) {
                    (Some(user), None) => Ok(Step::User(user)),
                    (None, Some(command)) => Ok(Step::Command(command)),
                    _ => Err(format!("step {i}: needs one of \"user\" or \"command\"")),
                }
            })
            .collect::<Result<_, _>>()?;
        let approvals = list("approvals")
            .iter()
            .enumerate()
            .map(|(i, approval)| {
                let text = |key| approval.get(key).and_then(Value::as_str);
                let answer = match text("answer") {
                    Some("y") => Answer::Yes,
                    Some("n") => Answer::No,
                    Some("a") => Answer::Always,
                    other => {
                        return Err(format!(
                            "approval {i}: answer {other:?} is not \"y\", \"n\" or \"a\""
                        ));
                    }
                };
                let question =
                    text("question").ok_or_else(|| format!("approval {i}: no \"question\""))?;
                Ok((question.to_string(), answer))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { steps, approvals })
    }
}

/// What a run saw: every event, in order, the questions asked with their
/// answers, and what went wrong.
#[derive(Debug, Default)]
pub struct Drive {
    pub events: Vec<Event>,
    pub asked: Vec<(String, Answer)>,
    pub problems: Vec<String>,
}

/// Runs `scenario`'s steps through `engine`, then shuts it down.
pub async fn drive(engine: &mut Engine, scenario: &Scenario) -> Drive {
    let mut run = Drive::default();
    let mut approvals = scenario.approvals.iter();
    for step in &scenario.steps {
        let op = match step {
            Step::User(text) => Op::UserTurn { text: text.clone() },
            Step::Command(text) => Op::Command { text: text.clone() },
        };
        let Ok(id) = engine.submit(op) else {
            run.problems
                .push("the engine stopped before the steps ended".into());
            return run;
        };
        if !follow(engine, &id, &mut approvals, &mut run).await {
            return run;
        }
    }
    run.problems.extend(
        approvals.map(|(question, _)| format!("scripted answer never asked for: {question:?}")),
    );
    match engine.submit(Op::Shutdown) {
        Ok(id) => {
            follow(engine, &id, &mut [].iter(), &mut run).await;
        }
        Err(_) => run
            .problems
            .push("the engine stopped before shutdown".into()),
    }
    run
}

/// Reads events until submission `id` has ended and its title, if it
/// started one, is done, answering its questions. False if the engine
/// stopped first.
async fn follow<'a>(
    engine: &mut Engine,
    id: &str,
    approvals: &mut impl Iterator<Item = &'a (String, Answer)>,
    run: &mut Drive,
) -> bool {
    let mut ended = false;
    let mut naming = false;
    loop {
        let Some(event) = engine.next_event().await else {
            run.problems
                .push(format!("the engine stopped during submission {id}"));
            return false;
        };
        if event.id == id {
            match &event.msg {
                EventMsg::TurnComplete(_)
                | EventMsg::TurnAborted(_)
                | EventMsg::TurnRefused(_)
                | EventMsg::CommandComplete(_)
                | EventMsg::ShutdownComplete => ended = true,
                EventMsg::Background(b) if b.task == BackgroundTask::Title => {
                    naming = matches!(b.state, BackgroundState::Started | BackgroundState::Running);
                }
                _ => {}
            }
        }
        if let EventMsg::QuestionRequest(request) = &event.msg {
            let prompt = &request.question.prompt;
            let answer = match approvals.next() {
                Some((part, answer)) if prompt.contains(part.as_str()) => *answer,
                expected => {
                    run.problems.push(format!(
                        "unscripted question {prompt:?} (script expected {:?})",
                        expected.map(|(part, _)| part)
                    ));
                    request.question.unattended
                }
            };
            run.asked.push((prompt.clone(), answer));
            let op = Op::QuestionAnswer {
                id: request.question_id.clone(),
                answer,
            };
            if engine.submit(op).is_err() {
                run.problems
                    .push("the engine stopped before an answer".into());
            }
        }
        run.events.push(event);
        if ended && !naming {
            return true;
        }
    }
}

#[cfg(test)]
#[path = "drive_tests.rs"]
mod tests;
