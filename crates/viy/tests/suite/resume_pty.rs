//! `/resume`'s pick in the real chat under a pseudo-terminal: the chosen
//! chat is marked inside the question's frame, the arrows move it even
//! pressed quickly right after the box appears, and Enter resumes the one
//! chosen (kb d136). Three synthetic chats in a home of the test's own.

use std::process::Command;
use std::time::Duration;

use serde_json::json;
use viy_testkit::pty::Pty;
use viy_testkit::pty::wait_exit;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::screen::dump;

fn log(id: &str, project: &std::path::Path, text: &str, ts: f64) -> String {
    format!(
        "{}\n{}\n{}\n",
        json!({"t": "meta", "id": id, "created": ts, "project": project, "model": "m"}),
        json!({"t": "msg", "message": {"role": "user", "content": text}, "ts": ts + 1.0}),
        json!({"t": "msg", "message": {"role": "assistant", "content": format!("answer to {text}")}, "ts": ts + 2.0}),
    )
}

/// Whether some row holds `text`, drawn reversed where it starts.
fn chosen(s: &vt100::Screen, text: &str) -> bool {
    let rows: Vec<String> = s.rows(0, s.size().1).collect();
    rows.iter().enumerate().any(|(y, row)| {
        row.find(text).is_some_and(|at| {
            let x = row[..at].chars().count();
            s.cell(y as u16, x as u16).is_some_and(vt100::Cell::inverse)
        })
    })
}

#[test]
fn resume_marks_the_chosen_chat_and_the_arrows_move_it() {
    let scratch = ScratchDir::new("resume-pty").unwrap();
    let home = scratch.path().join("home");
    let project = scratch.path().join("project");
    std::fs::create_dir_all(project.join(".viy")).unwrap();
    let project = std::fs::canonicalize(&project).unwrap();
    let sessions = viy_config::Paths::under(&home).sessions_dir();
    std::fs::create_dir_all(&sessions).unwrap();
    // Newest first in the list: alpha, beta, gamma.
    for (i, text) in ["gamma", "beta", "alpha"].iter().enumerate() {
        let id = format!("2026110{}-100000-aaa{}", i + 1, i);
        let ts = 1_762_000_000.0 + 1000.0 * i as f64;
        std::fs::write(
            sessions.join(format!("{id}.jsonl")),
            log(&id, &project, text, ts),
        )
        .unwrap();
    }
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
                .env("CHAT_AUTO_TITLE", "0")
                .env("TOOLS_ENABLED", "0"),
        )
        .unwrap();
    pty.wait_for("the input box", |s| s.contents().contains("› "));
    pty.send(b"/resume").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.wait_for("the pick, the newest chosen", |s| chosen(s, "›  1. alpha"));
    // Two quick presses at once, as a held key sends them.
    pty.send(b"\x1b[B").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    pty.send(b"\x1b[B").unwrap();
    pty.wait_for("the third chosen", |s| chosen(s, "›  3. gamma"));
    pty.send(b"\x1b[A").unwrap();
    pty.wait_for("the second chosen", |s| chosen(s, "›  2. beta"));
    std::thread::sleep(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.wait_for("beta resumed", |s| s.contents().contains("answer to beta"));
    pty.with_screen(|p| {
        let text = p.screen().contents();
        assert!(!text.contains("answer to alpha"), "{}", dump(p.screen()));
    });
    pty.send(b"\x04").unwrap();
    let status = wait_exit(&mut child).unwrap();
    assert!(status.success(), "{status}");
}
