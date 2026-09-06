use std::os::unix::process::ExitStatusExt;
use std::time::Instant;

use pretty_assertions::assert_eq;

use super::*;

fn sh(script: &str) -> Command {
    Command::new("sh").args(["-c", script])
}

#[tokio::test]
async fn run_captures_both_streams_and_the_status() {
    let out = SystemExec
        .run(sh("printf out; printf err >&2; exit 3"))
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(out.stdout, b"out");
    assert_eq!(out.stderr, b"err");
}

#[tokio::test]
async fn run_feeds_the_given_bytes_to_stdin() {
    let out = SystemExec
        .run(Command::new("cat").stdin("x key\n"))
        .await
        .unwrap();
    assert_eq!(out.stdout, b"x key\n");
}

#[tokio::test]
async fn without_bytes_stdin_is_dev_null() {
    let out = SystemExec
        .run(Command::new("readlink").arg("/proc/self/fd/0"))
        .await
        .unwrap();
    assert_eq!(out.stdout, b"/dev/null\n");
}

#[tokio::test]
async fn a_child_that_ignores_its_stdin_is_no_error() {
    let out = SystemExec
        .run(sh("exit 0").stdin(vec![b'x'; 1 << 20]))
        .await
        .unwrap();
    assert!(out.status.success());
}

#[tokio::test]
async fn run_past_its_timeout_fails_and_ends_the_child() {
    let started = Instant::now();
    let error = SystemExec
        .run(
            Command::new("sleep")
                .arg("30")
                .timeout(Duration::from_millis(200)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_missing_program_is_named() {
    let error = SystemExec
        .run(Command::new("viy-no-such-program"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(
        error.to_string().starts_with("viy-no-such-program: "),
        "{error}"
    );
}

#[tokio::test]
async fn detached_runs_in_its_own_session_with_stderr_to_the_file() {
    let dir = std::env::temp_dir().join(format!("viy-exec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("stderr.log");
    let mut child = SystemExec
        .spawn(
            sh("readlink /proc/self/fd/0 >&2; echo to-stdout; echo to-stderr >&2; sleep 30"),
            Spawn::Detached {
                stderr: File::create(&log).unwrap(),
            },
        )
        .unwrap();
    let pid = child.id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
    // state, ppid, pgrp, session: a new session is led by the child.
    assert_eq!(fields[3], pid.to_string());
    assert_eq!(child.try_wait().unwrap(), None);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::fs::read_to_string(&log).unwrap().contains("to-stderr") {
        assert!(Instant::now() < deadline, "nothing reached the log");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    child.terminate().unwrap();
    let status = child.wait().await.unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert_eq!(exit_text(status), "signal 15");
    // Once it is reaped there is nothing left to signal.
    child.terminate().unwrap();
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "/dev/null\nto-stderr\n"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn exit_text_is_the_code_or_the_signal() {
    assert_eq!(exit_text(ExitStatus::from_raw(255 << 8)), "255");
    assert_eq!(exit_text(ExitStatus::from_raw(9)), "signal 9");
}

#[test]
fn argv_is_the_program_then_its_arguments() {
    let command = Command::new("ssh").arg("-N").args(["-p", "22"]);
    assert_eq!(command.argv(), ["ssh", "-N", "-p", "22"]);
}

#[test]
fn spawn_refuses_what_only_run_can_honour() {
    for command in [
        Command::new("cat").stdin("x"),
        Command::new("sleep")
            .arg("1")
            .timeout(Duration::from_secs(1)),
    ] {
        let error = SystemExec.spawn(command, Spawn::Terminal).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
