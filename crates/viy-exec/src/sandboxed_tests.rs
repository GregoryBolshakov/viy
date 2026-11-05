use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;

use super::*;

fn sh(script: &str, cwd: &std::path::Path) -> Spec {
    Spec {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: cwd.to_path_buf(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        pass_fds: Vec::new(),
    }
}

/// A descriptor of `path` opened without CLOEXEC, as one inherited from a
/// careless parent would be.
fn leaked(path: &std::path::Path) -> OwnedFd {
    let file = std::fs::File::open(path).unwrap();
    // SAFETY: dup of a valid descriptor; dup clears CLOEXEC on the copy.
    let fd = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(&file)) };
    assert!(fd > 2);
    // SAFETY: `fd` is a fresh descriptor this test owns.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn fds_of(listing: &str) -> Vec<String> {
    let mut fds: Vec<String> = listing.split_whitespace().map(str::to_string).collect();
    fds.sort();
    fds
}

#[tokio::test]
async fn only_the_passed_descriptors_reach_the_child() {
    let dir = std::env::temp_dir();
    let stray = leaked(Path::new("/etc/hostname"));
    let passed = leaked(Path::new("/etc/passwd"));
    let (stray_no, passed_no) = (stray.as_raw_fd(), passed.as_raw_fd());
    let mut spec = sh("for f in /proc/$$/fd/*; do echo ${f##*/}; done", &dir);
    spec.pass_fds.push(passed);
    let done = spawn(spec)
        .unwrap()
        .finish(Duration::from_secs(10), true)
        .await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    // 0-2, the one passed, and the folder the glob is reading.
    let fds = fds_of(&done.stdout);
    assert!(fds.contains(&passed_no.to_string()), "{fds:?}");
    assert!(!fds.contains(&stray_no.to_string()), "{fds:?}");
    assert_eq!(fds.len(), 5, "{fds:?}");
    drop(stray);
}

#[tokio::test]
async fn a_timeout_kills_the_whole_group() {
    let t0 = Instant::now();
    let done = spawn(sh("sleep 30 & sleep 30", &std::env::temp_dir()))
        .unwrap()
        .finish(Duration::from_millis(300), false)
        .await;
    assert!(done.timed_out);
    assert_eq!(done.exit_code, None);
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn reap_group_kills_what_was_left_in_the_background() {
    let scratch = std::env::temp_dir().join(format!("viy-reap-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    for (reap, survives) in [(true, false), (false, true)] {
        let _ = std::fs::remove_file(scratch.join("late"));
        let done = spawn(sh(
            "(sleep 1; touch late) >/dev/null 2>&1 & echo started",
            &scratch,
        ))
        .unwrap()
        .finish(Duration::from_secs(10), reap)
        .await;
        assert_eq!(done.stdout.trim(), "started");
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert_eq!(scratch.join("late").exists(), survives, "reap {reap}");
    }
    std::fs::remove_dir_all(&scratch).unwrap();
}

#[tokio::test]
async fn dropping_it_kills_the_group() {
    let scratch = std::env::temp_dir().join(format!("viy-drop-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let running = spawn(sh("sleep 1; touch late", &scratch)).unwrap();
    drop(running);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!scratch.join("late").exists());
    std::fs::remove_dir_all(&scratch).unwrap();
}

#[tokio::test]
async fn exit_codes_and_signals_as_python_reports_them() {
    let dir = std::env::temp_dir();
    let run = |s: &'static str| async move {
        spawn(sh(s, &std::env::temp_dir()))
            .unwrap()
            .finish(Duration::from_secs(10), true)
            .await
    };
    assert_eq!(run("exit 3").await.exit_code, Some(3));
    assert_eq!(run("kill -9 $$").await.exit_code, Some(-9));
    let done = run("printf 'a\\377b'; echo err >&2").await;
    assert_eq!(done.stdout, "a\u{FFFD}b");
    assert_eq!(done.stderr, "err\n");
    let _ = dir;
}

#[tokio::test]
async fn the_environment_is_only_what_was_given() {
    let done = spawn(sh("env | sort", &std::env::temp_dir()))
        .unwrap()
        .finish(Duration::from_secs(10), true)
        .await;
    let names: Vec<&str> = done
        .stdout
        .lines()
        .filter_map(|l| l.split('=').next())
        .filter(|n| !["PWD", "SHLVL", "_", "OLDPWD"].contains(n))
        .collect();
    assert_eq!(names, ["PATH"]);
}

// From the WP 4.7 review: a timeout that lands after the exit and stdout's
// end but before stderr's must not poll the finished stdout reader again.
#[tokio::test]
async fn a_timeout_between_the_two_streams_ends_cleanly() {
    let done = spawn(sh(
        "echo out; (exec >&-; sleep 30) & exit 0",
        &std::env::temp_dir(),
    ))
    .unwrap()
    .finish(Duration::from_millis(500), false)
    .await;
    assert!(done.timed_out);
    assert_eq!(done.stdout, "out\n");
}
