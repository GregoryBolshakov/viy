//! rpctl tests/test_consult.py, against a fake `claude` that plays one
//! scenario and records its argv, stdin, working folder and the CLAUDE.md
//! switch. Each test bakes its scenario and log path into its own script,
//! so tests running side by side share no environment variable.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::EventMsg;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::chat::tests::Direct;
use crate::chat::tests::shell_chat;

const FAKE: &str = r#"
import json, os, signal, sys, time
args = sys.argv[1:]
mode = MODE
if mode == "chatty":
    # writes more than a pipe holds before it reads the prompt
    sys.stdout.write("x" * 300000 + "\n")
    sys.stdout.flush()
prompt = sys.stdin.read()
ws = args[args.index("--add-dir") + 1]
with open(LOG, "a") as f:
    f.write(json.dumps({"argv": args, "prompt": prompt, "cwd": os.getcwd(), "pid": os.getpid(),
                        "md": os.environ.get("CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD")}) + "\n")
def emit(d):
    print(json.dumps(d), flush=True)
sid = "11111111-2222-3333-4444-555555555555"
emit({"type": "system", "subtype": "init", "session_id": sid, "model": "claude-opus-5-5"})
emit({"type": "assistant", "message": {"content": [
    {"type": "tool_use", "name": "Edit", "input": {"file_path": ws + "/app.py"}}]}})
def result(report, **kw):
    emit(dict({"type": "result", "subtype": "success", "is_error": False, "session_id": sid,
               "structured_output": report, "result": json.dumps(report),
               "total_cost_usd": 0.5, "num_turns": 3}, **kw))
if mode == "done":
    with open(os.path.join(ws, "app.py"), "w") as f:
        f.write("print('fixed')\n")
    result({"status": "done", "summary": "Fixed the bug in app.py.",
            "changes": [{"path": "app.py", "what": "fixed it"}], "verify": "python app.py"})
elif mode == "declined":
    result({"status": "declined", "summary": "You should not be doing this at all. " * 20,
            "decline_reason": "not something I will help with, and here is a long lecture " * 5,
            "open": "Consider stopping your work."})
elif mode == "partial":
    result({"status": "partial", "summary": "Did the first half.",
            "decline_reason": "second half: " + "x" * 200})
elif mode == "advise":
    os.makedirs(os.path.join(ws, "__pycache__"), exist_ok=True)
    open(os.path.join(ws, "__pycache__", "app.pyc"), "w").close()
    result({"status": "done", "summary": "Apply the patch.",
            "patch": "--- a/app.py\n+++ b/app.py\n",
            "changes": [{"path": ws + "/app.py", "what": "proposed"}]})
elif mode == "chatty":
    result({"status": "done", "summary": "Read it all."})
elif mode == "badline":
    sys.stdout.buffer.write(b"\xff\xfe not utf-8\n")
    sys.stdout.flush()
    result({"status": "done", "summary": "Fixed it."})
elif mode == "killed":
    os.kill(os.getpid(), signal.SIGKILL)
elif mode == "sleep":
    time.sleep(60)
elif mode == "error":
    sys.stderr.write("boom: something broke\n")
    sys.exit(1)
elif mode == "noreport":
    pass
"#;

struct Fx {
    d: Direct,
    root: ScratchDir,
    _shell: ScratchDir,
    ws: PathBuf,
}

impl Fx {
    /// A chat whose claude plays `mode`; `over` on top of rpctl's test
    /// config (approval auto).
    fn new(mode: &str, over: &[(&str, &str)]) -> Self {
        let root = ScratchDir::new("consult").unwrap();
        let fake = root.path().join("claude");
        let script = FAKE.replace("MODE", &format!("{mode:?}")).replace(
            "LOG",
            &format!("{:?}", root.path().join("fake.log").display().to_string()),
        );
        std::fs::write(&fake, format!("#!/usr/bin/env python3\n{script}")).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = fake.display().to_string();
        let consults = root.path().join("consults").display().to_string();
        let mut cfg: Vec<(&str, &str)> = vec![
            ("CONSULT_CLAUDE_BIN", bin.as_str()),
            ("CONSULT_ROOT", consults.as_str()),
            ("CONSULT_APPROVAL", "auto"),
            ("CONSULT_ENABLED", "1"),
        ];
        cfg.extend_from_slice(over);
        let (mut d, shell) = shell_chat(json!([]), &cfg);
        let ws = shell.path().join("ws");
        std::fs::write(ws.join("app.py"), "print('bug')\n").unwrap();
        d.chat.turn_user = Some(json!("please fix app.py, it prints bug"));
        d.chat.consult.projects_dir = root.path().join("claudecfg").join("projects");
        Self {
            d,
            root,
            _shell: shell,
            ws,
        }
    }

    /// Rewrites the fake to play `mode` next.
    fn play(&self, mode: &str) {
        let script = FAKE.replace("MODE", &format!("{mode:?}")).replace(
            "LOG",
            &format!(
                "{:?}",
                self.root.path().join("fake.log").display().to_string()
            ),
        );
        let fake = self.root.path().join("claude");
        let tmp = self.root.path().join("claude.new");
        std::fs::write(&tmp, format!("#!/usr/bin/env python3\n{script}")).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(tmp, fake).unwrap();
    }

    async fn ask(&mut self, args: Value) -> Value {
        let mut a = json!({"task": "fix app.py"});
        for (k, v) in args.as_object().into_iter().flatten() {
            a[k] = v.clone();
        }
        let ctx = self.d.ctx.clone();
        let call = crate::stream::ToolCall {
            id: "c1".into(),
            name: "ask_claude".into(),
            arguments: a.to_string(),
        };
        let (out, _) = self.d.chat.dispatch(&ctx, &call).await;
        out
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.path().join("fake.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn folder(&self, cid: &Value) -> PathBuf {
        self.d.chat.find_consult(cid.as_str().unwrap()).unwrap()
    }
}

fn arg_after(argv: &Value, flag: &str) -> String {
    let a = argv.as_array().unwrap();
    let i = a.iter().position(|x| x == flag).unwrap();
    a[i + 1].as_str().unwrap().to_string()
}

// ---- ActTests ---------------------------------------------------------------

#[tokio::test]
async fn done_reports_real_changes_and_builds_the_folder() {
    let mut f = Fx::new("done", &[]);
    let r = f
        .ask(json!({"context": "I tried print('ok')", "paths": ["app.py"]}))
        .await;
    assert_eq!(r["status"], "done");
    assert_eq!(r["files_changed"], json!(["app.py"]));
    assert_eq!(r["summary"], "Fixed the bug in app.py.");
    assert_eq!(r["mode"], "act");
    assert_eq!(r["cost_usd"], 0.5);
    assert!(r["note"].as_str().unwrap().contains("Read a file again"));
    let folder = f.folder(&r["consult_id"]);
    for name in [
        "task.md",
        "request.md",
        "claude.jsonl",
        "meta.json",
        "scratch",
        "out/report.md",
    ] {
        assert!(folder.join(name).exists(), "{name}");
    }
    let mode = std::fs::metadata(&folder).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert!(
        std::fs::read_to_string(folder.join("request.md"))
            .unwrap()
            .contains("please fix app.py")
    );
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(folder.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["session_id"], "11111111-2222-3333-4444-555555555555");
    assert_eq!(meta["runs"][0]["files_changed"], json!(["app.py"]));
}

#[tokio::test]
async fn claude_gets_the_defaults_workspace_and_claude_md() {
    let mut f = Fx::new("done", &[]);
    f.ask(json!({"context": "ctx here"})).await;
    let call = &f.calls()[0];
    let argv = &call["argv"];
    assert_eq!(arg_after(argv, "--model"), "claude-opus-5-5");
    assert_eq!(arg_after(argv, "--effort"), "xhigh");
    let ws = std::fs::canonicalize(&f.ws).unwrap();
    assert_eq!(arg_after(argv, "--add-dir"), ws.display().to_string());
    assert_eq!(arg_after(argv, "--permission-mode"), "bypassPermissions");
    let prompt = call["prompt"].as_str().unwrap();
    assert!(prompt.starts_with("Mode: act"));
    for flag in [
        "--safe-mode",
        "--restricted",
        "--bare",
        "--tools",
        "--allowedTools",
    ] {
        assert!(
            !argv.as_array().unwrap().iter().any(|a| a == flag),
            "{flag}"
        );
    }
    assert_eq!(call["md"], "1");
    assert!(prompt.contains("fix app.py"));
    assert!(prompt.contains("ctx here"));
    assert!(prompt.contains("please fix app.py, it prints bug"));
    let consults = f.root.path().join("consults").display().to_string();
    assert!(call["cwd"].as_str().unwrap().starts_with(&consults));
}

#[tokio::test]
async fn changes_seen_in_a_git_workspace() {
    let mut f = Fx::new("done", &[]);
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&f.ws)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-qm",
        "init",
    ]);
    let r = f.ask(json!({})).await;
    assert_eq!(r["files_changed"], json!(["app.py"]));
}

#[tokio::test]
async fn model_and_effort_are_configurable() {
    let mut f = Fx::new(
        "done",
        &[("CONSULT_MODEL", "sonnet"), ("CONSULT_EFFORT", "high")],
    );
    f.ask(json!({})).await;
    let argv = f.calls()[0]["argv"].clone();
    assert_eq!(arg_after(&argv, "--model"), "sonnet");
    assert_eq!(arg_after(&argv, "--effort"), "high");
    let ctx = f.d.ctx.clone();
    f.d.chat.consult_command(&ctx, "model claude-fable-5-1");
    f.d.chat.consult_command(&ctx, "effort max");
    f.ask(json!({})).await;
    let argv = f.calls()[1]["argv"].clone();
    assert_eq!(arg_after(&argv, "--model"), "claude-fable-5-1");
    assert_eq!(arg_after(&argv, "--effort"), "max");
}

#[tokio::test]
async fn follow_up_resumes_the_session_in_the_same_folder() {
    let mut f = Fx::new("done", &[]);
    let first = f.ask(json!({})).await;
    let second = f
        .ask(json!({"follow_up_of": first["consult_id"], "task": "also add a test"}))
        .await;
    assert_eq!(second["consult_id"], first["consult_id"]);
    let argv = f.calls()[1]["argv"].clone();
    assert_eq!(
        arg_after(&argv, "--resume"),
        "11111111-2222-3333-4444-555555555555"
    );
    let folder = f.folder(&first["consult_id"]);
    assert!(folder.join("task.2.md").exists());
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(folder.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["runs"].as_array().unwrap().len(), 2);
    for bad in ["20990101T000000-abcd", "../../etc"] {
        assert!(
            f.ask(json!({"follow_up_of": bad}))
                .await
                .get("error")
                .is_some(),
            "{bad}"
        );
    }
}

// ---- DeclineTests ------------------------------------------------------------

#[tokio::test]
async fn decline_is_a_status_and_a_few_words() {
    let mut f = Fx::new("declined", &[]);
    let r = f.ask(json!({})).await;
    let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["consult_id", "status", "reason", "note"]);
    assert_eq!(r["status"], "declined");
    assert!(r["reason"].as_str().unwrap().chars().count() <= DECLINE_REASON_CHARS);
    let text = r.to_string();
    assert!(!text.contains(&"lecture".repeat(2)) && !text.contains("stopping"));
    assert!(
        r["note"]
            .as_str()
            .unwrap()
            .contains("Continue with the rest")
    );
}

#[tokio::test]
async fn partial_keeps_the_work_and_clips_the_declined_part() {
    let mut f = Fx::new("partial", &[]);
    let r = f.ask(json!({})).await;
    assert_eq!(r["status"], "partial");
    assert_eq!(r["summary"], "Did the first half.");
    assert!(r["declined_part"].as_str().unwrap().chars().count() <= DECLINE_REASON_CHARS);
}

// ---- AdviseTests -------------------------------------------------------------

#[tokio::test]
async fn advise_returns_the_patch() {
    let mut f = Fx::new("advise", &[]);
    let r = f.ask(json!({"mode": "advise"})).await;
    assert_eq!(r["mode"], "advise");
    assert!(r["patch"].as_str().unwrap().contains("+++ b/app.py"));
    assert_eq!(r["files_changed"], json!([])); // __pycache__ is not a change
    assert_eq!(r["changes"][0]["path"], "app.py"); // made workspace-relative
    assert!(
        f.calls()[0]["prompt"]
            .as_str()
            .unwrap()
            .starts_with("Mode: advise")
    );
    let folder = f.folder(&r["consult_id"]);
    assert!(
        std::fs::read_to_string(folder.join("out").join("patch.diff"))
            .unwrap()
            .contains("+++ b/app.py")
    );
}

#[tokio::test]
async fn follow_up_carries_its_own_mode() {
    let mut f = Fx::new("done", &[]);
    let first = f.ask(json!({})).await;
    f.play("advise");
    f.ask(json!({"mode": "advise", "follow_up_of": first["consult_id"]}))
        .await;
    assert!(
        f.calls()[0]["prompt"]
            .as_str()
            .unwrap()
            .starts_with("Mode: act")
    );
    assert!(
        f.calls()[1]["prompt"]
            .as_str()
            .unwrap()
            .starts_with("Mode: advise")
    );
}

#[tokio::test]
async fn qwen_cannot_widen_advise_to_act() {
    let mut f = Fx::new("advise", &[("CONSULT_MODE", "advise")]);
    let r = f.ask(json!({"task": "t", "mode": "act"})).await;
    assert_eq!(r["mode"], "advise");
}

// ---- FailureTests ------------------------------------------------------------

#[tokio::test]
async fn timeout_kills_claude() {
    let mut f = Fx::new("sleep", &[("CONSULT_TIMEOUT", "1")]);
    let t = std::time::Instant::now();
    let r = f.ask(json!({})).await;
    assert!(t.elapsed().as_secs() < 20);
    assert_eq!(r["status"], "failed");
    assert!(r["error"].as_str().unwrap().contains("timed out"), "{r}");
    // What the stream said before the stop is kept, as rpctl's reader
    // keeps it: the session a follow-up resumes, and SIGTERM's code.
    let meta: Value = serde_json::from_str(
        &std::fs::read_to_string(f.folder(&r["consult_id"]).join("meta.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["session_id"], "11111111-2222-3333-4444-555555555555");
    let run = &meta["runs"][0];
    assert_eq!(
        (&run["how"], &run["rc"], &run["model"]),
        (&json!("timeout"), &json!(-15), &json!("claude-opus-5-5"))
    );
}

// viy's review of the port: a claude killed by a signal reports its code
// (-N, as Python's returncode) and stderr, as rpctl does.
#[tokio::test]
async fn a_signal_exit_is_a_negative_code() {
    let mut f = Fx::new("killed", &[]);
    let r = f.ask(json!({})).await;
    assert_eq!(r["status"], "failed");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("claude exited with code -9"),
        "{r}"
    );
}

// viy's review of the port: a stream line that is not UTF-8 does not stop
// the reading.
#[tokio::test]
async fn a_line_not_utf8_is_skipped() {
    let mut f = Fx::new("badline", &[]);
    let r = f.ask(json!({})).await;
    assert_eq!(r["status"], "done", "{r}");
}

// viy's review of the port: a claude that writes before reading a prompt
// larger than a pipe holds does not deadlock with viy writing it.
#[tokio::test]
async fn a_large_prompt_and_early_output_do_not_deadlock() {
    let mut f = Fx::new("chatty", &[("CONSULT_TIMEOUT", "30")]);
    f.d.chat.turn_user = Some(json!("y".repeat(300_000)));
    let r = tokio::time::timeout(std::time::Duration::from_secs(20), f.ask(json!({})))
        .await
        .expect("deadlocked");
    assert_eq!(r["status"], "done", "{r}");
    assert_eq!(
        f.calls()[0]["prompt"]
            .as_str()
            .unwrap()
            .matches('y')
            .count()
            >= 300_000,
        true
    );
}

// rpctl's snapshot stays in git mode in a repository with no commits yet
// (HEAD unresolved): ignored build output is not a change.
#[test]
fn a_repository_without_commits_is_still_git() {
    let d = ScratchDir::new("consult-git").unwrap();
    let ws = d.path();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(ws)
        .status()
        .unwrap();
    assert!(ok.success());
    std::fs::write(ws.join(".gitignore"), "build/\n").unwrap();
    std::fs::create_dir(ws.join("build")).unwrap();
    std::fs::write(ws.join("build").join("out.o"), "x").unwrap();
    std::fs::write(ws.join("new.py"), "x").unwrap();
    let snap = snapshot(ws);
    assert!(snap.git);
    assert_eq!(snap.head, "HEAD"); // what `git rev-parse HEAD` prints, rpctl's too
    let names: Vec<&String> = snap.files.keys().collect();
    assert_eq!(names, [".gitignore", "new.py"]);
}

#[tokio::test]
async fn error_exit_reports_stderr_and_alarms() {
    let mut f = Fx::new("error", &[]);
    let echo: crate::trace::Echo = Arc::new(|_, _| {});
    f.d.chat.monitor = Some(crate::trace::TurnMonitor::new(
        f.d.chat.tracer().clone(),
        "main",
        Some(1),
        echo,
    ));
    let r = f.ask(json!({})).await;
    assert_eq!(r["status"], "failed");
    assert!(r["error"].as_str().unwrap().contains("boom"), "{r}");
    assert!(
        f.d.chat
            .monitor
            .as_ref()
            .unwrap()
            .alarms
            .contains(&"consult_failed".to_string())
    );
}

#[tokio::test]
async fn no_report() {
    let mut f = Fx::new("noreport", &[]);
    let r = f.ask(json!({})).await;
    assert_eq!(r["status"], "failed");
    assert!(
        r["error"].as_str().unwrap().contains("without a report"),
        "{r}"
    );
}

/// Answers every consult question with `answer`; the questions asked.
fn answering(f: &mut Fx, answer: Answer) -> (tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let ctx = f.d.ctx.clone();
    let asked = Arc::new(AtomicUsize::new(0));
    let n = Arc::clone(&asked);
    let mut old = std::mem::replace(&mut f.d.rx, tokio::sync::mpsc::unbounded_channel().1);
    let task = tokio::spawn(async move {
        while let Some(e) = old.recv().await {
            if let EventMsg::QuestionRequest(q) = e.msg {
                assert_eq!(q.question.kind, Some(QuestionKind::Consult));
                n.fetch_add(1, Ordering::SeqCst);
                ctx.answer(&q.question_id, answer);
            }
        }
    });
    (task, asked)
}

#[tokio::test]
async fn user_declines_approval() {
    let mut f = Fx::new("done", &[("CONSULT_APPROVAL", "ask")]);
    let (task, _) = answering(&mut f, Answer::No);
    let r = f.ask(json!({})).await;
    task.abort();
    assert_eq!(r["status"], "not_run");
    assert!(!f.root.path().join("fake.log").exists());
}

#[tokio::test]
async fn always_switches_approval_off_for_the_chat() {
    let mut f = Fx::new("done", &[("CONSULT_APPROVAL", "ask")]);
    let (task, asked) = answering(&mut f, Answer::Always);
    f.ask(json!({})).await;
    f.ask(json!({})).await;
    task.abort();
    assert_eq!(asked.load(Ordering::SeqCst), 1);
}

// ---- AvailabilityTests, PruneTests ----------------------------------------------

#[tokio::test]
async fn offered_only_with_tools_and_the_binary() {
    let names = |c: &Chat| -> Vec<String> {
        c.tool_specs()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect()
    };
    let mut f = Fx::new("done", &[]);
    assert!(names(&f.d.chat).contains(&"ask_claude".to_string()));
    assert!(
        f.d.chat
            .system_prompt()
            .unwrap()
            .contains(crate::prompt::CONSULT_GUIDE)
    );
    let ctx = f.d.ctx.clone();
    f.d.chat.consult_command(&ctx, "off");
    assert!(!names(&f.d.chat).contains(&"ask_claude".to_string()));
    let r = f.ask(json!({"task": "x"})).await;
    assert!(r["error"].as_str().unwrap().contains("off"), "{r}");
    let off = Fx::new("done", &[("TOOLS_ENABLED", "0")]);
    assert!(!names(&off.d.chat).contains(&"ask_claude".to_string()));
    let missing = Fx::new("done", &[("CONSULT_CLAUDE_BIN", "/nonexistent/claude")]);
    assert!(!names(&missing.d.chat).contains(&"ask_claude".to_string()));
}

#[tokio::test]
async fn old_folders_and_their_claude_sessions_go() {
    let mut f = Fx::new("done", &[("CONSULT_KEEP_DAYS", "1")]);
    let r = f.ask(json!({})).await;
    let folder = f.folder(&r["consult_id"]);
    let entry =
        f.d.chat
            .consult
            .projects_dir
            .join(slug(&folder.display().to_string()));
    std::fs::create_dir_all(&entry).unwrap();
    assert_eq!(f.d.chat.prune_consults(), 0);
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86400);
    let fh = std::fs::File::open(&folder).unwrap();
    fh.set_times(
        std::fs::FileTimes::new()
            .set_modified(old)
            .set_accessed(old),
    )
    .unwrap();
    assert_eq!(f.d.chat.prune_consults(), 1);
    assert!(!folder.exists());
    assert!(!entry.exists());
}

// New in viy's port: a turn interrupted drops the consult, and Claude's
// process group is killed with it (rpctl kills it on KeyboardInterrupt).
#[tokio::test]
async fn an_interrupted_consult_kills_claude() {
    let mut f = Fx::new("sleep", &[]);
    let ran = tokio::time::timeout(std::time::Duration::from_secs(2), f.ask(json!({}))).await;
    assert!(ran.is_err(), "the consult should still have been running");
    let pid = f.calls()[0]["pid"].as_i64().unwrap();
    let mut gone = false;
    for _ in 0..100 {
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // gone, or a zombie waiting to be reaped
        if state.is_empty() || state.split(") ").nth(1).is_some_and(|r| r.starts_with('Z')) {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(gone, "claude {pid} still runs");
    // and the run is recorded as cancelled, as rpctl records it, so
    // /consult lists it and a follow-up can resume the session
    let consults = f.root.path().join("consults");
    let mut meta = Value::Null;
    for _ in 0..100 {
        let found = walk_meta(&consults);
        if let Some(m) = found {
            meta = m;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let run = &meta["runs"][0];
    assert_eq!(
        (&run["how"], &run["error"], &run["status"]),
        (
            &json!("cancelled"),
            &json!("cancelled by the user"),
            &json!("failed")
        )
    );
    assert_eq!(meta["session_id"], "11111111-2222-3333-4444-555555555555");
}

/// The one meta.json under `root`'s session folders, once written.
fn walk_meta(root: &Path) -> Option<Value> {
    for s in std::fs::read_dir(root).ok()?.flatten() {
        for c in std::fs::read_dir(s.path()).ok()?.flatten() {
            if let Ok(t) = std::fs::read_to_string(c.path().join("meta.json")) {
                return serde_json::from_str(&t).ok();
            }
        }
    }
    None
}
