use std::net::TcpListener;

use super::*;

/// Ports below the ephemeral range, each a different one, each bindable
/// when handed out: the kernel never gives them to a bind to port 0 or to
/// an outgoing connection. A port someone holds is skipped. (One test:
/// the walk through the range is the process's.)
#[test]
fn free_ports_are_distinct_and_never_ephemeral() {
    let ceiling = ephemeral_start();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..200 {
        let port = free_port().unwrap();
        assert!((FLOOR..ceiling).contains(&port), "{port}");
        assert!(seen.insert(port), "{port} twice");
        drop(TcpListener::bind(("127.0.0.1", port)).unwrap());
    }
    // Ports the kernel picks itself are all at or above the range's start.
    for _ in 0..50 {
        let any = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        assert!(any.local_addr().unwrap().port() >= ceiling);
    }
    // The port after the next one handed out, taken the way a test takes
    // one: its lock first, so no other test process is using it, then
    // bound. (Bound without the lock, this test once held a port another
    // process's tunnel test was about to listen on.)
    let first = free_port().unwrap();
    let held = first + 1;
    let _bound = (reserve(held).unwrap())
        .then(|| TcpListener::bind(("127.0.0.1", held)).ok())
        .flatten();
    let next = free_port().unwrap();
    assert_ne!(next, first);
    assert_ne!(next, held);
    // The check connects: a listener is seen, a free port is not.
    if _bound.is_some() {
        assert!(accepts(held));
    }
    assert!(!accepts(next));
}

/// A port this process handed out is held against other processes while
/// it lives: `flock -n` from a child is refused it.
#[test]
fn a_port_handed_out_is_held_against_other_processes() {
    let mine = free_port().unwrap();
    let lock = std::env::temp_dir()
        .join("viy-test-ports")
        .join(mine.to_string());
    let status = std::process::Command::new("flock")
        .args(["-n", "-E", "9"])
        .arg(&lock)
        .arg("true")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(9), "another process took {mine}");
}
