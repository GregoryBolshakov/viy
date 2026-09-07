use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

fn scratch() -> (ScratchDir, Paths) {
    let dir = ScratchDir::new("pod-lock").unwrap();
    let paths = Paths::under(dir.path());
    (dir, paths)
}

fn lock_file(paths: &Paths) -> String {
    fs::read_to_string(paths.pod_lock_file()).unwrap()
}

/// Another process holding `pod.lock` as `<verb>`, the way `_exclusive`
/// does: python3 run as `program` (a link to it, so its command line says
/// what it is), with `extra` arguments. It runs in the lock's directory
/// and names the file without it: the path says `rpctl`, which rp's test
/// would take for rp.
pub(crate) fn holder(program: &Path, lock: &Path, verb: &str, extra: &[&str]) -> Child {
    let script = "import fcntl, os, sys\n\
                  f = open(sys.argv[1], 'a+')\n\
                  fcntl.flock(f, fcntl.LOCK_EX)\n\
                  f.seek(0); f.truncate(); f.write(f'{sys.argv[2]} {os.getpid()}'); f.flush()\n\
                  print('held', flush=True)\n\
                  sys.stdin.read()\n";
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let mut child = Command::new(program)
        .args(["-I", "-c", script])
        .arg(lock.file_name().unwrap())
        .arg(verb)
        .current_dir(lock.parent().unwrap())
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "held\n");
    child
}

/// python3, linked into `dir` as `name`.
pub(crate) fn python_as(dir: &Path, name: &str) -> std::path::PathBuf {
    let python = String::from_utf8(
        Command::new("python3")
            .args(["-I", "-c", "import sys; print(sys.executable)"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let link = dir.join(name);
    std::os::unix::fs::symlink(python.trim_end(), &link).unwrap();
    link
}

/// `f`'s first `Some` within two seconds. A lock just released can look
/// held for a moment: a child another test thread is spawning holds a copy
/// of every descriptor, the lock's included, until it execs.
pub(crate) fn soon<T>(mut f: impl FnMut() -> Option<T>) -> T {
    let end = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(std::time::Instant::now() < end, "still not so after 2 s");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_holder_writes_its_verb_and_pid() {
    let (_dir, paths) = scratch();
    let lock = PodLock::exclusive(&paths, "up").unwrap();
    assert_eq!(lock_file(&paths), format!("up {}", std::process::id()));
    assert!(!PodLock::is_free(&paths).unwrap());
    drop(lock);
    soon(|| PodLock::is_free(&paths).unwrap().then_some(()));
    let _again = soon(|| PodLock::exclusive(&paths, "stop").ok());
    assert_eq!(lock_file(&paths), format!("stop {}", std::process::id()));
}

#[test]
fn a_second_command_is_refused_and_told_who_holds_the_lock() {
    let (_dir, paths) = scratch();
    let _held = PodLock::exclusive(&paths, "up").unwrap();
    let err = PodLock::exclusive(&paths, "stop").unwrap_err();
    let pid = std::process::id();
    // This test binary is neither tool, so it is named as rp names it.
    assert_eq!(
        err.to_string(),
        format!(
            "Another rp up (pid {pid}) is already running. Let it finish, or Ctrl-C it, \
             then re-run viy stop."
        )
    );
}

#[test]
fn a_viy_holder_is_named_viy() {
    let (dir, paths) = scratch();
    let viy = python_as(dir.path(), "viy");
    let mut child = holder(&viy, &paths.pod_lock_file(), "up", &[]);
    let err = PodLock::exclusive(&paths, "up").unwrap_err();
    let LockError::Busy { holder, .. } = &err else {
        panic!("{err}");
    };
    assert_eq!(
        holder.as_ref().map(|h| (h.tool, h.verb.as_str(), h.pid)),
        Some((Some(Tool::Viy), "up", child.id()))
    );
    drop(child.stdin.take());
    child.wait().unwrap();
    soon(|| PodLock::exclusive(&paths, "up").ok());
}

#[test]
fn a_holder_line_that_is_not_verb_and_pid_names_no_one() {
    let (_dir, paths) = scratch();
    let held = PodLock::exclusive(&paths, "up").unwrap();
    for text in [
        "",
        "up",
        "up 12 x",
        "up -3",
        "up ١٢",
        "up 99999999999",
        "up 0",
    ] {
        fs::write(paths.pod_lock_file(), text).unwrap();
        let err = PodLock::exclusive(&paths, "stop").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Another rp or viy command is already running. Let it finish, or Ctrl-C it, \
             then re-run viy stop.",
            "{text:?}"
        );
    }
    drop(held);
}

#[test]
fn down_kills_an_rp_or_viy_holder_and_takes_the_lock() {
    let (dir, paths) = scratch();
    let viy = python_as(dir.path(), "viy");
    let python = python_as(dir.path(), "python3");
    for (program, extra, tool) in [
        (&python, &["rpctl"][..], Tool::Rp),
        (&viy, &[][..], Tool::Viy),
    ] {
        let mut child = holder(program, &paths.pod_lock_file(), "up", extra);
        let mut notices = Vec::new();
        let done = preempt_within(
            &paths,
            Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_secs(3),
            |n| notices.push(n),
        )
        .unwrap();
        child.wait().unwrap();
        assert_eq!(
            done.killed
                .iter()
                .map(|h| h.verb.as_str())
                .collect::<Vec<_>>(),
            ["up"]
        );
        assert!(done.lock.is_some());
        assert_eq!(lock_file(&paths), format!("down {}", std::process::id()));
        let killing = PreemptNotice::Killing(Holder {
            tool: Some(tool),
            verb: "up".to_string(),
            pid: child.id(),
        });
        assert_eq!(notices, std::slice::from_ref(&killing));
        assert_eq!(
            killing.to_string(),
            format!(
                "Killing the running {} up (pid {}) so it can't start or resume anything.",
                tool.name(),
                child.id()
            )
        );
    }
}

#[test]
fn down_leaves_a_holder_that_is_neither_tool_and_goes_on_without_the_lock() {
    let (dir, paths) = scratch();
    let python = python_as(dir.path(), "python3");
    let mut child = holder(&python, &paths.pod_lock_file(), "up", &[]);
    let mut notices = Vec::new();
    let done = preempt_within(
        &paths,
        Duration::from_millis(200),
        Duration::from_millis(20),
        Duration::from_secs(3),
        |n| notices.push(n),
    )
    .unwrap();
    assert!(done.killed.is_empty());
    assert!(done.lock.is_none());
    assert_eq!(notices, [PreemptNotice::GaveUp]);
    assert_eq!(
        PreemptNotice::GaveUp.to_string(),
        "Couldn't take the pod lock; tearing down without it."
    );
    assert!(process::pid_alive(child.id()));
    drop(child.stdin.take());
    child.wait().unwrap();
}

#[test]
fn down_never_kills_itself() {
    let (_dir, paths) = scratch();
    let _held = PodLock::exclusive(&paths, "up").unwrap();
    let mut notices = Vec::new();
    let done = preempt_within(
        &paths,
        Duration::from_millis(100),
        Duration::from_millis(20),
        Duration::from_secs(3),
        |n| notices.push(n),
    )
    .unwrap();
    assert!(done.killed.is_empty());
    assert_eq!(notices, [PreemptNotice::GaveUp]);
}
