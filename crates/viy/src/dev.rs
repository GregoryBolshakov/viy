//! `viy dev`: developer tools, hidden from `--help`. `viy dev drive
//! SCENARIO` runs a scenario's steps through the engine
//! (`viy_agent::drive`) and prints every event as a JSON line; problems of
//! the run go to stderr with exit status 1. Until the chat's turn exists
//! (WP 3.2) each turn fails with a message saying so.

use std::io::Write;
use std::sync::Arc;

use viy_agent::BoxFuture;
use viy_agent::Engine;
use viy_agent::TurnContext;
use viy_agent::Turns;
use viy_agent::drive;
use viy_agent::drive::Scenario;
use viy_protocol::EventMsg;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::TurnOutcome;

use crate::Failure;
use crate::cli::DevArgs;
use crate::cli::DevCommand;

/// The engine's turns until WP 3.2 brings the chat's.
struct NotYet;

impl Turns for NotYet {
    fn turn(self: Arc<Self>, _: TurnContext, _: String) -> BoxFuture<'static, TurnOutcome> {
        Box::pin(async {
            TurnOutcome::Failed {
                message: "the chat's model turn comes with WP 3.2".into(),
            }
        })
    }

    fn command(self: Arc<Self>, ctx: TurnContext, text: String) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            ctx.emit(EventMsg::Notice(NoticeEvent {
                level: NoticeLevel::Warn,
                code: "not_ported".into(),
                message: format!("{text}: slash commands come with phase 3"),
            }));
        })
    }
}

pub fn run(args: &DevArgs, out: &mut (dyn Write + Send)) -> Result<(), Failure> {
    let DevCommand::Drive { scenario } = &args.command;
    let shown = scenario.display();
    let text = std::fs::read_to_string(scenario).map_err(|e| format!("{shown}: {e}"))?;
    let value = serde_json::from_str(&text).map_err(|e| format!("{shown}: {e}"))?;
    let scenario = Scenario::from_json(&value).map_err(|e| format!("{shown}: {e}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::from(e.to_string()))?;
    let run = runtime.block_on(async {
        let mut engine = Engine::spawn(Arc::new(NotYet));
        drive::drive(&mut engine, &scenario).await
    });
    for event in &run.events {
        let line = serde_json::to_string(event).map_err(|e| Failure::from(e.to_string()))?;
        writeln!(out, "{line}").map_err(|e| Failure::from(e.to_string()))?;
    }
    if run.problems.is_empty() {
        Ok(())
    } else {
        Err(Failure::from(run.problems.join("\n")))
    }
}

#[cfg(test)]
#[path = "dev_tests.rs"]
mod tests;
