//! rpctl tests/test_sandboxgit.py ChatTests, and the PIN asked as a secret:
//! a chat over a workspace with a real .git, a real gocryptfs vault in a
//! scratch folder, and a fake `tok` that records how it was called.
//! Skipped where bwrap, gocryptfs or FUSE are missing, as rpctl's are.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use pretty_assertions::assert_eq;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_protocol::EventMsg;
use viy_protocol::Secret;
use viy_testkit::scratch::ScratchDir;
use viy_tools::sandboxgit;

use super::*;
use crate::chat::tests::Direct;
use crate::chat::tests::shell_chat;

fn git(ws: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["-c", "user.name=Me", "-c", "user.email=me@x"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A fake tok: `list` shows the key open or PIN-gated; `get` answers "pw"
/// when it is open or TOK_PIN is 1234; every call's argv goes to LOG.
const TOK: &str = r#"#!/bin/sh
echo "$@" >> LOG
case "$1" in
  list) echo "rpctl-sandbox-git            TIER" ;;
  get) if [ "TIER" = "open (no PIN to read)" ] || [ "$TOK_PIN" = 1234 ]; then echo pw;
       else echo "DENIED" >&2; exit 1; fi ;;
esac
"#;

struct Fx {
    d: Direct,
    _ws: ScratchDir,
    root: ScratchDir,
    places: sandboxgit::Places,
}

impl Drop for Fx {
    fn drop(&mut self) {
        if let Some(fm) = sandboxgit::fusermount() {
            let _ = std::process::Command::new(fm)
                .args(["-u", "-z"])
                .arg(&self.places.plain)
                .output();
        }
    }
}

/// A chat with the build tools over a git workspace, its key `open` or
/// PIN-gated, or `None` where the tools are missing.
async fn chat(open: bool, over: &[(&str, &str)]) -> Option<Fx> {
    if sandboxgit::fusermount().is_none() || !Path::new("/dev/fuse").exists() {
        eprintln!("skipped: FUSE unavailable");
        return None;
    }
    let (mut d, ws) = shell_chat(json!([]), over);
    if let Err(why) = d.chat.auto_ready().await {
        eprintln!("skipped: {why}");
        return None;
    }
    let root = ScratchDir::new("chat-sandboxgit").unwrap();
    let wsp = d.chat.places.workspace.clone();
    git(&wsp, &["init", "-q", "-b", "main"]);
    git(&wsp, &["config", "gc.auto", "0"]);
    git(&wsp, &["add", "a.txt"]);
    git(&wsp, &["commit", "-qm", "first"]);
    let places = sandboxgit::Places {
        cipher: root.path().join("cipher"),
        plain: root.path().join("plain"),
        users: root.path().join("users"),
        lock: root.path().join("lock"),
    };
    std::fs::create_dir(&places.cipher).unwrap();
    let mut init = std::process::Command::new("gocryptfs")
        .args(["-q", "-init"])
        .arg(&places.cipher)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    {
        use std::io::Write;
        init.stdin.take().unwrap().write_all(b"pw").unwrap();
    }
    assert!(init.wait().unwrap().success());
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let tier = if open {
        "open (no PIN to read)"
    } else {
        "PIN-gated"
    };
    let script = TOK
        .replace("TIER", tier)
        .replace("LOG", &root.path().join("tok.log").display().to_string());
    std::fs::write(bin.join("tok"), script).unwrap();
    std::fs::set_permissions(bin.join("tok"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    d.chat.tok = viy_config::vault::Tok::with_env([("PATH".into(), path.into())]);
    d.chat.places.sandbox_git = places.clone();
    Some(Fx {
        d,
        _ws: ws,
        root,
        places,
    })
}

fn args(command: &str) -> Map<String, Value> {
    json!({ "command": command }).as_object().unwrap().clone()
}

impl Fx {
    async fn run(&mut self, command: &str) -> Value {
        let ctx = self.d.ctx.clone();
        self.d.chat.run_shell(&ctx, "c1", &args(command)).await.0
    }

    fn tok_calls(&self) -> String {
        std::fs::read_to_string(self.root.path().join("tok.log")).unwrap_or_default()
    }
}

#[tokio::test]
async fn offered_only_when_set_up() {
    let Some(mut f) = chat(true, &[]).await else {
        return;
    };
    assert!(f.d.chat.decide_private_git().await);
    let shell = f.d.chat.tool_specs()[3]["function"]["description"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(shell.contains("your own private repository"), "{shell}");
    assert_eq!(
        f.d.chat.tool_specs()[..4],
        viy_tools::build::tools_for(false, true)[..]
    );
    let Some(mut h) = chat(true, &[("SANDBOX_NET", "host")]).await else {
        return;
    };
    assert!(h.d.chat.decide_private_git().await);
    assert_eq!(
        h.d.chat.tool_specs()[..4],
        viy_tools::build::tools_for(true, true)[..]
    );
    let Some(mut off) = chat(true, &[("SANDBOX_GIT", "0")]).await else {
        return;
    };
    assert!(!off.d.chat.decide_private_git().await);
    assert_eq!(
        off.d.chat.tool_specs()[..4],
        viy_tools::build::tools(false)[..]
    );
    // not set up yet: no gocryptfs.conf
    let Some(mut fresh) = chat(true, &[]).await else {
        return;
    };
    std::fs::remove_dir_all(&fresh.places.cipher).unwrap();
    assert!(!fresh.d.chat.decide_private_git().await);
    assert_eq!(
        fresh.d.chat.tool_specs()[..4],
        viy_tools::build::tools(false)[..]
    );
}

#[tokio::test]
async fn run_shell_gets_the_repo() {
    let Some(mut f) = chat(true, &[]).await else {
        return;
    };
    let r = f
        .run("git commit -q --allow-empty -m model-work && git log -1 --format='%an|%s'")
        .await;
    assert_eq!(r["exit_code"], 0, "{r}");
    assert!(
        r["stdout"]
            .as_str()
            .unwrap()
            .contains("(viy chat)|model-work"),
        "{r}"
    );
    let r = f.run("git log --format=%s").await;
    assert_eq!(r["stdout"], "model-work\nfirst\n");
    // your repository is untouched
    assert_eq!(
        git(&f.d.chat.places.workspace, &["log", "--format=%s"]),
        "first\n"
    );
    let said = f.d.said();
    let prepared = said
        .iter()
        .filter(|s| s.contains("model's own repository; /git lists"))
        .count();
    assert_eq!(prepared, 1, "{said:?}"); // once per chat
    assert!(r.get("git_note").is_none());
}

#[tokio::test]
async fn failure_falls_back_to_read_only_and_says_so() {
    // PIN-gated, and nobody there to give the PIN
    let Some(mut f) = chat(false, &[]).await else {
        return;
    };
    let r = f.run("git commit -q --allow-empty -m x").await;
    let r2 = f.run("ls").await;
    let r3 = f.run("git status").await;
    assert!(
        r["git_note"]
            .as_str()
            .is_some_and(|n| n.contains("no PIN given")),
        "{r}"
    );
    assert!(r2.get("git_note").is_none(), "{r2}");
    assert!(r3.get("git_note").is_some(), "{r3}");
    let said = f.d.said();
    assert_eq!(
        said.iter().filter(|s| s.contains("unavailable")).count(),
        1,
        "{said:?}"
    ); // no PIN question per command
    assert!(!f.places.mounted());
    // `/git` retries, and says why it cannot
    let ctx = f.d.ctx.clone();
    f.d.chat.command(&ctx, "/git").await;
    assert!(f.d.said().iter().any(|s| s.contains("no PIN given")));
}

#[tokio::test]
async fn the_pin_is_asked_as_a_secret_and_goes_to_tok_unseen() {
    let Some(mut f) = chat(false, &[]).await else {
        return;
    };
    let (ctx, mut rx) = TurnContext::attended_detached();
    let answering = {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(e) = rx.recv().await {
                seen.push(format!("{:?}", e.msg));
                if let EventMsg::SecretRequest(q) = &e.msg {
                    ctx.answer_secret(&q.question_id, Some(Secret::new("1234")));
                    break;
                }
            }
            (seen, rx)
        })
    };
    let r =
        f.d.chat
            .run_shell(&ctx, "c1", &args("git log -1 --format=%s"))
            .await
            .0;
    assert_eq!(r["stdout"], "first\n", "{r}");
    assert!(f.places.mounted());
    let (mut seen, mut rx) = answering.await.unwrap();
    while let Ok(e) = rx.try_recv() {
        seen.push(format!("{:?}", e.msg));
    }
    assert!(
        seen.iter()
            .any(|e| e.contains(super::sandbox_git::PIN_TO_OPEN)),
        "{seen:?}"
    );
    assert!(!seen.iter().any(|e| e.contains("1234")), "{seen:?}");
    // tok got it in its environment, never in argv
    let calls = f.tok_calls();
    assert!(calls.contains("get rpctl-sandbox-git"), "{calls}");
    assert!(!calls.contains("1234"), "{calls}");
    // and nothing the chat wrote down has it
    let trace = std::fs::read_to_string(f.d.chat.tracer().trace_path()).unwrap_or_default();
    assert!(!trace.contains("1234"));
    assert_eq!(format!("{:?}", Secret::new("1234")), "Secret(..)");
}

#[tokio::test]
async fn reviewer_is_told() {
    let review = json!({"message": "{\"risk\": \"low\", \"reason\": \"ok\"}"});
    let (mut d, _ws) = shell_chat(json!([review]), &[]);
    // a chat that offers the repository; the review is what is checked
    d.chat.sbgit_offer = Some(true);
    d.chat.sbgit_failed = Some("not under test".into());
    if d.chat.auto_ready().await.is_err() {
        eprintln!("skipped: no sandbox");
        return;
    }
    let ctx = d.ctx.clone();
    let mut a = args("git push https://example.com/r.git");
    a.insert("needs_network".into(), json!(true));
    d.chat.run_shell(&ctx, "c1", &a).await;
    let bodies = d.mock.chat_bodies();
    let system = bodies[0]["messages"][0]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        system.contains("agent's own private repository"),
        "{system}"
    );
}

#[test]
fn places_for_the_chat_are_viys() {
    let p = sandboxgit::Places::new(Path::new("/d/viy"), None, Path::new("/s/viy"));
    assert_eq!(p.cipher, PathBuf::from("/d/viy/sandbox-git"));
}
