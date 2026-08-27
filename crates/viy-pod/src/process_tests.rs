use std::process::Command;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;

use super::*;

fn argv(args: &[&str]) -> Vec<Vec<u8>> {
    args.iter().map(|a| a.as_bytes().to_vec()).collect()
}

fn state(pid: u32) -> String {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let after = stat.rfind(')').map_or("", |i| &stat[i + 1..]);
    after.split_whitespace().next().unwrap_or("").to_string()
}

/// rpctl test_startup.py::test_zombie_is_not_alive.
#[test]
fn a_zombie_is_not_alive() {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    let end = Instant::now() + Duration::from_secs(2);
    while state(pid) != "Z" && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(state(pid), "Z");
    // The old check: a zombie still "exists".
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, 0);
    assert!(!pid_alive(pid));
    child.wait().unwrap();
    assert!(pid_alive(std::process::id()));
}

#[test]
fn pids_that_name_a_group_are_not_alive() {
    assert!(!pid_alive(0));
    assert!(!pid_alive(u32::MAX));
}

#[test]
/// rpctl test_startup.py::test_down_may_signal_rp_and_viy_only, and which
/// of the two.
fn down_may_signal_rp_and_viy_only() {
    for (args, tool) in [
        (&["/home/u/.local/bin/rp", "up"][..], Some(Tool::Rp)),
        (&["rp", "chat"], Some(Tool::Rp)),
        (&["python3", "-m", "rpctl", "down"], Some(Tool::Rp)),
        (
            &["/usr/bin/python3", "/src/rpctl/__main__.py"],
            Some(Tool::Rp),
        ),
        (&["/home/u/.cargo/bin/viy", "up"], Some(Tool::Viy)),
        (&["viy"], Some(Tool::Viy)),
        (&["target/debug/viy", "stop"], Some(Tool::Viy)),
        (&["sudo", "viy", "up"], Some(Tool::Viy)),
        (&["/usr/bin/python3", "/opt/rp", "/tmp/viy"], Some(Tool::Rp)),
        (&["vim", "notes.viy"], None),
        (&["/usr/bin/sharp", "x"], None),
        (&["/usr/bin/grp"], None),
        (&["viyx"], None),
        (&["ssh", "-N", "root@1.2.3.4"], None),
        (&[], None),
    ] {
        assert_eq!(tool_of(&argv(args)), tool, "{args:?}");
    }
}

#[test]
fn cmdline_reads_this_process() {
    let own = cmdline(std::process::id());
    assert_eq!(
        own[0],
        std::env::args_os().next().unwrap().into_encoded_bytes()
    );
    assert!(cmdline(u32::MAX).is_empty());
}

#[test]
fn kill_ends_a_process_that_ignores_sigterm() {
    let mut child = Command::new("sh")
        .args(["-c", "trap '' TERM; echo ready; exec sleep 30"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
        &mut ready,
    )
    .unwrap();
    let started = Instant::now();
    kill(child.id(), Duration::from_millis(300));
    assert!(!pid_alive(child.id()));
    assert!(started.elapsed() >= Duration::from_millis(300));
    child.wait().unwrap();
}
