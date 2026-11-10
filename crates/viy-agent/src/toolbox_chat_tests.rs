//! rpctl tests/test_toolbox.py ChatTests and SandboxTests: the frozen
//! index, find_tool, `/toolbox`, and running a kept tool by name through
//! run_shell, sandboxed or not.

use pretty_assertions::assert_eq;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::EventMsg;
use viy_protocol::Question;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::chat::tests::Direct;
use crate::chat::tests::shell_chat;
use crate::toolbox::tests::Base;
use crate::toolbox::tests::make_tool;

struct Fx {
    d: Direct,
    _ws: ScratchDir,
    b: Base,
}

/// A chat with the build tools on over a toolbox of its own, frozen as a
/// chat starting now would freeze it.
fn chat(over: &[(&str, &str)]) -> Fx {
    let b = Base::new();
    fx_with(b, over)
}

fn fx_with(b: Base, over: &[(&str, &str)]) -> Fx {
    let (mut d, ws) = shell_chat(json!([]), over);
    d.chat.places.toolbox = b.dirs.clone();
    d.chat.snapshot_toolbox();
    Fx { d, _ws: ws, b }
}

fn names(c: &Chat) -> Vec<String> {
    c.tool_specs()
        .iter()
        .map(|s| s["function"]["name"].as_str().unwrap().to_string())
        .collect()
}

fn system(c: &Chat) -> String {
    c.system_prompt().unwrap_or_default()
}

fn args(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn no_tools_no_index_and_no_find_tool() {
    let f = chat(&[]);
    assert!(!names(&f.d.chat).contains(&"find_tool".to_string()));
    assert!(!system(&f.d.chat).contains("# Toolbox"));
}

#[tokio::test]
async fn index_is_frozen_until_a_new_history() {
    let b = Base::new();
    make_tool(
        &b.dirs.global,
        "first-tool",
        &[("summary", "The first one")],
    );
    let mut f = fx_with(b, &[]);
    let before = system(&f.d.chat);
    assert!(before.contains("- first-tool: The first one"), "{before}");
    assert!(before.contains("check the toolbox"));
    assert!(names(&f.d.chat).contains(&"find_tool".to_string()));
    make_tool(
        &f.b.dirs.project,
        "second-tool",
        &[("summary", "Kept mid-chat")],
    );
    assert_eq!(system(&f.d.chat), before); // the prefix cache holds
    let ctx = f.d.ctx.clone();
    let call = crate::stream::ToolCall {
        id: "c1".into(),
        name: "find_tool".into(),
        arguments: json!({"name": "second-tool"}).to_string(),
    };
    let (r, _) = f.d.chat.dispatch(&ctx, &call).await;
    assert_eq!(r["tool"]["name"], "second-tool"); // but it is findable
    f.d.chat.new_session(&ctx, None).await;
    assert!(system(&f.d.chat).contains("- second-tool: Kept mid-chat"));
}

#[test]
fn summary_refreshes_the_index() {
    let b = Base::new();
    make_tool(&b.dirs.global, "first-tool", &[]);
    let mut f = fx_with(b, &[]);
    f.d.chat.messages = vec![
        json!({"role": "user", "content": "a"}),
        json!({"role": "assistant", "content": "b"}),
    ];
    make_tool(&f.b.dirs.global, "later-tool", &[]);
    let mut plan = f.d.chat.plan_summary(None, false).unwrap();
    plan.end = 2;
    plan.pinned.clear();
    f.d.chat.apply_summary(&plan, "summary text");
    assert!(system(&f.d.chat).contains("- later-tool:"));
}

#[tokio::test]
async fn switches() {
    let b = Base::new();
    make_tool(&b.dirs.global, "first-tool", &[]);
    let dirs = b.dirs.clone();
    let mut f = fx_with(b, &[("TOOLBOX_ENABLED", "0")]);
    assert!(!names(&f.d.chat).contains(&"find_tool".to_string()));
    assert!(!system(&f.d.chat).contains("# Toolbox"));
    let ctx = f.d.ctx.clone();
    f.d.chat.command(&ctx, "/toolbox on").await;
    assert!(system(&f.d.chat).contains("# Toolbox"));
    let (mut d, _ws) = shell_chat(json!([]), &[("TOOLS_ENABLED", "0")]);
    d.chat.places.toolbox = dirs;
    d.chat.snapshot_toolbox();
    assert_eq!(d.chat.tool_specs(), Vec::<Value>::new());
    assert!(!system(&d.chat).contains("# Toolbox"));
}

#[test]
fn find_tool_runs_in_parallel_batches() {
    assert!(super::tools::PARALLEL_SAFE.contains(&"find_tool"));
}

/// Runs `command` through run_shell without the sandbox, saying yes; the
/// result and the question asked.
async fn run_unsandboxed(f: &mut Fx, command: &str) -> (Value, Question) {
    f.d.chat.sandbox_check = Some(Err("no bwrap".into()));
    let ctx = f.d.ctx.clone();
    let old = std::mem::replace(&mut f.d.rx, tokio::sync::mpsc::unbounded_channel().1);
    let answering = tokio::spawn(async move {
        let mut old = old;
        loop {
            let e = old.recv().await.unwrap();
            if let EventMsg::QuestionRequest(q) = e.msg {
                ctx.answer(&q.question_id, Answer::Yes);
                return q.question;
            }
        }
    });
    let ctx = f.d.ctx.clone();
    let (r, _) =
        f.d.chat
            .run_shell(&ctx, "c1", &args(json!({ "command": command })))
            .await;
    (r, answering.await.unwrap())
}

#[tokio::test]
async fn run_shell_runs_a_tool_by_name_and_labels_it() {
    let b = Base::new();
    make_tool(
        &b.dirs.global,
        "say-hi",
        &[("author", "qwen"), ("status", "untested")],
    );
    let mut f = fx_with(b, &[]);
    let (r, asked) = run_unsandboxed(&mut f, "say-hi there").await;
    assert_eq!(r["exit_code"], 0, "{r}");
    assert!(
        r["stdout"]
            .as_str()
            .unwrap()
            .contains("say-hi ran with there")
    );
    let said: Vec<&str> = asked.reason.iter().map(|n| n.message.as_str()).collect();
    assert!(
        said.contains(&"  [toolbox] say-hi (by qwen, untested, global)"),
        "{said:?}"
    );
}

#[tokio::test]
async fn a_tool_kept_mid_chat_runs_at_once() {
    let mut f = chat(&[]);
    make_tool(&f.b.dirs.project, "late-tool", &[]);
    let (r, _) = run_unsandboxed(&mut f, "late-tool").await;
    assert!(
        r["stdout"].as_str().unwrap().contains("late-tool ran"),
        "{r}"
    );
}

// ---- SandboxTests -----------------------------------------------------------------

#[test]
fn argv_binds_toolbox_read_only_and_appends_path() {
    let b = Base::new();
    let ws = b.root.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let ro = [b.dirs.global.clone()];
    let extra = [b.dirs.global.join("x")];
    let jail = viy_tools::sandbox::Jail {
        ro_paths: &ro,
        path_extra: &extra,
        ..viy_tools::sandbox::Jail::new(&ws, &ws)
    };
    let host = viy_tools::sandbox::Host::new(b.root.path().join("home"), Vec::new());
    let a = viy_tools::sandbox::bwrap_argv(&host, &jail, 3);
    let i = a.iter().position(|x| *x == *b.dirs.global).unwrap();
    assert_eq!(a[i - 1], "--ro-bind-try");
    let path = a[a.iter().position(|x| x == "PATH").unwrap() + 1]
        .to_string_lossy()
        .into_owned();
    assert!(path.starts_with(&format!("{}:", viy_tools::sandbox::SANDBOX_PATH)));
    assert!(path.ends_with(&b.dirs.global.join("x").display().to_string()));
}

#[tokio::test]
async fn sandboxed_run_finds_the_tool_and_cannot_change_it() {
    let b = Base::new();
    let d = make_tool(&b.dirs.global, "say-hi", &[]);
    let mut f = fx_with(b, &[]);
    if let Err(why) = f.d.chat.auto_ready().await {
        eprintln!("skipped: {why}");
        return;
    }
    let ctx = f.d.ctx.clone();
    let command = format!("say-hi x; echo bad > {}/say-hi", d.display());
    let (r, card) =
        f.d.chat
            .run_shell(&ctx, "c1", &args(json!({ "command": command })))
            .await;
    assert!(card.sandboxed);
    assert!(
        r["stdout"].as_str().unwrap().contains("say-hi ran with x"),
        "{r}"
    );
    assert!(
        r["stderr"]
            .as_str()
            .unwrap()
            .contains("Read-only file system"),
        "{r}"
    );
    assert!(
        std::fs::read_to_string(d.join("say-hi"))
            .unwrap()
            .contains("say-hi ran")
    );
}
