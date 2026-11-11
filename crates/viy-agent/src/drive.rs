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
//! all three, as it gives them to rp. A scenario's `web` section is served
//! by the same server: [`scenario_web`] points the chat's web tools there,
//! as drive_rp points rp's.

use std::collections::HashMap;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use viy_http::BoxFuture;
use viy_http::HttpClient;
use viy_http::HttpError;
use viy_http::Request;
use viy_http::Response;
use viy_protocol::Answer;
use viy_protocol::BackgroundState;
use viy_protocol::BackgroundTask;
use viy_protocol::Event;
use viy_protocol::EventMsg;
use viy_protocol::Op;
use viy_tools::approve::Pending;
use viy_tools::netguard::Connect;
use viy_tools::netguard::ConnectError;
use viy_tools::netguard::Io;
use viy_tools::netguard::Resolver;

use crate::chat::web::WebNet;
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
                    Some("q") => Answer::Cancel,
                    other => {
                        return Err(format!(
                            "approval {i}: answer {other:?} is not \"y\", \"n\", \"a\" or \"q\""
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
        if let EventMsg::SecretRequest(request) = &event.msg {
            // A script has no secrets to give: the asker goes on without.
            let op = Op::SecretAnswer {
                id: request.question_id.clone(),
                secret: None,
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

/// The chat's web tools pointed at the scenario server `url` when the
/// scenario has a `web` section: every name resolves to a public address
/// (or the one its `resolve` gives), every page connection reaches the
/// server in plain HTTP with the page's Host, and Tavily's endpoints are
/// the server's `/tavily/`. An error when the server's URL is not
/// `http://<ip>:<port>`: the scenario must not reach the internet.
pub fn scenario_web(
    url: &str,
    scenario: &Value,
    http: Arc<dyn HttpClient>,
) -> Result<Option<WebNet>, String> {
    let Some(web) = scenario.get("web").filter(|w| !w.is_null()) else {
        return Ok(None);
    };
    let base = url.trim_end_matches('/').to_string();
    let addr: SocketAddr = base
        .strip_prefix("http://")
        .and_then(|a| a.parse().ok())
        .ok_or_else(|| {
            format!(
                "a scenario with a web section needs the server as http://<ip>:<port>, not {url}"
            )
        })?;
    let names = web["resolve"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(host, ip)| Some((host.clone(), ip.as_str()?.parse().ok()?)))
        .collect();
    Ok(Some(WebNet {
        resolver: Arc::new(Names(names)),
        connect: Arc::new(ToServer(addr)),
        http: Arc::new(TavilyAt { base, http }),
    }))
}

/// A scenario's names, each else a public address.
struct Names(HashMap<String, IpAddr>);

impl Resolver for Names {
    fn resolve<'a>(&'a self, host: &'a str, _: u16) -> Pending<'a, Result<Vec<IpAddr>, String>> {
        let ip = self
            .0
            .get(host)
            .copied()
            .unwrap_or(IpAddr::from([93, 184, 216, 34]));
        Box::pin(async move { Ok(vec![ip]) })
    }
}

/// Every page connection to the scenario server, in plain HTTP.
struct ToServer(SocketAddr);

impl Connect for ToServer {
    fn connect<'a>(
        &'a self,
        _: IpAddr,
        _: u16,
        _: &'a str,
        _: &'a str,
        timeout: Duration,
    ) -> Pending<'a, Result<Box<dyn Io>, ConnectError>> {
        Box::pin(async move {
            match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(self.0)).await {
                Ok(Ok(s)) => Ok(Box::new(s) as Box<dyn Io>),
                Ok(Err(e)) => Err(ConnectError::Other(e.to_string())),
                Err(_) => Err(ConnectError::Other("timed out".into())),
            }
        })
    }
}

/// Tavily's requests sent to the scenario server instead.
struct TavilyAt {
    base: String,
    http: Arc<dyn HttpClient>,
}

impl HttpClient for TavilyAt {
    fn request(&self, mut request: Request) -> BoxFuture<'_, Result<Response, HttpError>> {
        if request.url == viy_tools::search::TAVILY_URL {
            request.url = format!("{}/tavily/search", self.base);
        } else if request.url == viy_tools::webfetch::TAVILY_EXTRACT_URL {
            request.url = format!("{}/tavily/extract", self.base);
        }
        self.http.request(request)
    }
}
