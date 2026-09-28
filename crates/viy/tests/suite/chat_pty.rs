//! `viy chat` itself under a pseudo-terminal (WP 3.13, kb d041): it opens
//! with no pod and no RUNPOD_API_KEY, in a home of its own; Enter keeps
//! the draft and says why; an rp up in another terminal holding the pod
//! lock shows the pod booting; a pod brought up by another terminal (its
//! pod.json, reached through the tunnel's local port, here the scripted
//! model server) is noticed and the same line goes to it; on quit, where
//! the chat was saved and rp's billing hint.

use std::process::Command;

use serde_json::Value;
use serde_json::json;
use viy_testkit::mock_llm::MockLlm;
use viy_testkit::pty::Pty;
use viy_testkit::pty::wait_exit;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::screen::dump;

#[test]
fn the_chat_opens_without_a_pod_keeps_the_draft_and_notices_one_brought_up() {
    let mock = MockLlm::start(&json!({
        "name": "chat-without-pod",
        "max_model_len": 65536,
        "responses": [{"stream": [{"content": "the answer is 4"}]}],
    }))
    .unwrap();
    let port = mock.uri().rsplit(':').next().unwrap().to_string();
    let scratch = ScratchDir::new("chat-pty").unwrap();
    let home = scratch.path().join("home");
    let project = scratch.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let pty = Pty::open(30, 100).unwrap();
    let mut child = pty
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_viy"))
                .arg("chat")
                .current_dir(&project)
                .env("HOME", &home)
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_DATA_HOME")
                .env_remove("XDG_STATE_HOME")
                .env_remove("RUNPOD_API_KEY")
                .env_remove("VLLM_BASE_URL")
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY")
                .env("VLLM_API_KEY", "fake-key")
                .env("TUNNEL", "1")
                .env("TUNNEL_LOCAL_PORT", &port)
                .env("CHAT_AUTO_TITLE", "0"),
        )
        .unwrap();
    pty.wait_for("the input box and no pod", |s| {
        s.alternate_screen() && s.contents().contains("› ") && s.contents().contains("no pod (/up)")
    });
    pty.send(b"what is 2+2").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.wait_for("the reason, the draft kept", |s| {
        let text = s.contents();
        text.contains("no pod is running: /up starts one.") && text.contains("› what is 2+2")
    });
    pty.with_screen(|p| {
        let text = p.screen().contents();
        assert!(!text.contains("you ›"), "{}", dump(p.screen()));
    });
    assert!(mock.requests().is_empty());

    // rp up in another terminal, holding the pod lock: the footer says so
    // and a line is refused as the pod boots.
    let state = home.join(".local/state/rpctl");
    std::fs::create_dir_all(&state).unwrap();
    let rp = scratch.path().join("rp");
    std::os::unix::fs::symlink("/bin/sleep", &rp).unwrap();
    let mut other = Command::new(&rp).arg("60").spawn().unwrap();
    std::fs::write(state.join("pod.lock"), format!("up {}", other.id())).unwrap();
    pty.wait_for("the pod booting elsewhere", |s| {
        s.contents()
            .contains("pod booting: rp up in another terminal")
    });
    pty.send(b"\r").unwrap();
    pty.wait_for("the line refused while it boots", |s| {
        s.contents()
            .contains("the pod is still booting (rp up in another terminal)")
    });
    other.kill().unwrap();
    other.wait().unwrap();

    // Its up done: the pod is tracked, the tunnel's local end answers.
    std::fs::write(
        state.join("pod.json"),
        json!({"pod_id": "pod111", "tunnel_only": true, "cost_per_hr": 1.89}).to_string(),
    )
    .unwrap();
    pty.send(b"\r").unwrap();
    pty.wait_for("the answer", |s| s.contents().contains("the answer is 4"));
    let user = mock
        .requests()
        .iter()
        .filter_map(|r| r.body.as_ref())
        .find(|b| b["stream"] == Value::Bool(true))
        .and_then(|b| b["messages"].as_array().cloned())
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "user").cloned());
    assert_eq!(
        user.map(|m| m["content"].clone()),
        Some(json!("what is 2+2"))
    );
    pty.wait_for("the pod in the footer", |s| {
        s.contents().contains("1.89$/h")
    });

    pty.send(b"\x04").unwrap();
    let status = wait_exit(&mut child).unwrap();
    assert!(status.success(), "{status}");
    pty.wait_for("where it was saved and the billing hint", |s| {
        let text = s.contents();
        !s.alternate_screen()
            && text.contains("chat saved as 'what is 2+2'. Resume it with viy resume --last")
            && text
                .contains("Pod is still running. viy stop to pause billing, viy down to terminate.")
    });
    assert!(pty.cooked());
}

/// What `viy shutdown` and `/shutdown` do to an open chat when they fire:
/// SIGTERM. The chat saves, ends its session log and gives the terminal
/// back (rp chat's `_exit_on_signal`). Its log is in the test's own home.
#[test]
fn sigterm_saves_and_closes_the_chat() {
    let mock = MockLlm::start(&json!({
        "name": "chat-sigterm",
        "max_model_len": 65536,
        "responses": [{"stream": [{"content": "the answer is 4"}]}],
    }))
    .unwrap();
    let port = mock.uri().rsplit(':').next().unwrap().to_string();
    let scratch = ScratchDir::new("chat-sigterm").unwrap();
    let home = scratch.path().join("home");
    let project = scratch.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let paths = viy_config::Paths::under(&home);
    std::fs::create_dir_all(paths.pod_state_dir()).unwrap();
    std::fs::write(
        paths.pod_state_file(),
        json!({"pod_id": "pod111", "tunnel_only": true}).to_string(),
    )
    .unwrap();
    let pty = Pty::open(30, 100).unwrap();
    let mut child = pty
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_viy"))
                .current_dir(&project)
                .env("HOME", &home)
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_DATA_HOME")
                .env_remove("XDG_STATE_HOME")
                .env_remove("RUNPOD_API_KEY")
                .env_remove("VLLM_BASE_URL")
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY")
                .env("VLLM_API_KEY", "fake-key")
                .env("TUNNEL", "1")
                .env("TUNNEL_LOCAL_PORT", &port)
                .env("CHAT_AUTO_TITLE", "0"),
        )
        .unwrap();
    pty.wait_for("the input box", |s| {
        s.alternate_screen() && s.contents().contains("› ")
    });
    pty.send(b"what is 2+2").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.wait_for("the answer", |s| s.contents().contains("the answer is 4"));
    let pid = libc::pid_t::try_from(child.id()).unwrap();
    // SAFETY: a plain signal to our own child.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let status = wait_exit(&mut child).unwrap();
    assert!(status.success(), "{status}");
    pty.wait_for("the terminal given back", |s| !s.alternate_screen());
    assert!(pty.cooked());
    let log = std::fs::read_dir(paths.sessions_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .unwrap();
    let last = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .last()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .unwrap();
    assert_eq!(last["t"], "end", "{last}");
}
