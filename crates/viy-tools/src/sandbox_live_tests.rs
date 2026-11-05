//! The sandbox run for real: bubblewrap, the seccomp filter, the launcher
//! and the handover (rpctl's SandboxTmpTests, SandboxReadOnlyTests,
//! SandboxMaskingTests, SandboxProxyTests; netaccess kb f010, f024-f027).
//! Skipped where bwrap cannot run, as rpctl's are.

use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

struct Box_ {
    _scratch: ScratchDir,
    ws: PathBuf,
    tmp: PathBuf,
    host: Host,
}

/// A workspace and a chat /tmp, or `None` (with the reason printed) where
/// bwrap cannot run.
async fn sandbox() -> Option<Box_> {
    let scratch = ScratchDir::new("sandbox").unwrap();
    let ws = scratch.path().join("ws");
    let tmp = scratch.path().join("tmp");
    let home = scratch.path().join("home");
    for d in [&ws, &tmp, &home] {
        fs::create_dir_all(d).unwrap();
    }
    let host = Host {
        home,
        user: "tester".into(),
        lang: "C.UTF-8".into(),
        clients: Vec::new(),
    };
    if let Err(why) = available(&host).await {
        eprintln!("skipped: {why}");
        return None;
    }
    Some(Box_ {
        _scratch: scratch,
        ws,
        tmp,
        host,
    })
}

impl Box_ {
    fn jail(&self) -> Jail<'_> {
        Jail {
            tmp_dir: Some(&self.tmp),
            ..Jail::new(&self.ws, &self.ws)
        }
    }

    async fn sh_in(&self, jail: &Jail<'_>, script: &str, proxy: Option<Arc<dyn Egress>>) -> Ran {
        run(
            &self.host,
            jail,
            &["bash".into(), "-c".into(), script.into()],
            Duration::from_secs(30),
            proxy,
        )
        .await
        .unwrap()
    }

    async fn sh(&self, script: &str) -> Ran {
        self.sh_in(&self.jail(), script, None).await
    }
}

#[tokio::test]
async fn tmp_persists_between_commands_of_a_chat() {
    let Some(b) = sandbox().await else { return };
    b.sh("echo kept > /tmp/f").await;
    assert_eq!(b.sh("cat /tmp/f").await.stdout.trim(), "kept");
    let without = b.sh_in(&Jail::new(&b.ws, &b.ws), "cat /tmp/f", None).await;
    assert_ne!(without.exit_code, Some(0));
}

#[tokio::test]
async fn a_readonly_sandbox_really_cannot_write() {
    let Some(b) = sandbox().await else { return };
    let jail = Jail {
        readonly: true,
        ..b.jail()
    };
    let r = b.sh_in(&jail, "echo x > f && echo wrote", None).await;
    assert_ne!(r.exit_code, Some(0));
    assert!(!b.ws.join("f").exists());
}

#[tokio::test]
async fn base64_of_a_secret_file_yields_nothing() {
    let Some(b) = sandbox().await else { return };
    fs::write(
        b.ws.join("secrets.json"),
        r#"{"api_key":"LEAKME-1234567890"}"#,
    )
    .unwrap();
    fs::write(b.ws.join("id_rsa"), "PRIVATEKEYBYTES-LEAKME\n").unwrap();
    let r = b.sh("base64 -w0 secrets.json; cat id_rsa").await;
    assert!(!r.stdout.contains("LEAKME"));
    assert_eq!(r.stdout.trim(), "");
}

#[tokio::test]
async fn protected_paths_are_read_only_and_the_rest_writable() {
    let Some(b) = sandbox().await else { return };
    fs::create_dir_all(b.ws.join(".git/hooks")).unwrap();
    fs::create_dir_all(b.ws.join(".viy")).unwrap();
    fs::write(b.ws.join("Makefile"), "all:\n").unwrap();
    let r = b
        .sh("touch .git/hooks/pre-commit; touch .viy/config; echo x >> Makefile; touch ok; ls -A ~; ls /run 2>&1")
        .await;
    assert!(b.ws.join("ok").exists(), "{r:?}");
    assert!(!b.ws.join(".git/hooks/pre-commit").exists());
    assert!(!b.ws.join(".viy/config").exists());
    assert_eq!(fs::read_to_string(b.ws.join("Makefile")).unwrap(), "all:\n");
    assert_eq!(
        r.stderr.matches("Read-only file system").count(),
        3,
        "{r:?}"
    );
    assert!(
        r.stdout.contains("No such file or directory"),
        "no /run: {r:?}"
    );
}

#[tokio::test]
async fn unix_and_vsock_sockets_and_user_namespaces_are_refused() {
    let Some(b) = sandbox().await else { return };
    for (family, refused) in [("AF_UNIX", true), ("AF_VSOCK", true), ("AF_INET", false)] {
        let r = b
            .sh(&format!(
                "python3 -c 'import socket; socket.socket(socket.{family})' && echo made"
            ))
            .await;
        assert_eq!(
            r.stderr.contains("Address family not supported"),
            refused,
            "{family}: {r:?}"
        );
        assert_eq!(r.stdout.contains("made"), !refused, "{family}: {r:?}");
    }
    let r = b.sh("unshare -U true && echo nested").await;
    assert!(!r.stdout.contains("nested"), "{r:?}");
    let r = b
        .sh("python3 -c 'import socket; a, b = socket.socketpair(); print(\"pair\")'")
        .await;
    assert_eq!(r.stdout.trim(), "pair", "socketpair stays allowed: {r:?}");
}

/// A descriptor of `path` without CLOEXEC, as a careless parent leaves one.
fn leaked(path: &Path) -> std::os::fd::OwnedFd {
    let file = fs::File::open(path).unwrap();
    // SAFETY: dup of a valid descriptor; dup clears CLOEXEC on the copy.
    let fd = unsafe { libc::dup(file.as_raw_fd()) };
    assert!(fd > 2);
    // SAFETY: a fresh descriptor this test owns.
    unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }
}

#[tokio::test]
async fn a_descriptor_leaked_in_the_parent_stays_out() {
    let Some(b) = sandbox().await else { return };
    fs::write(b.ws.join("marker"), "x").unwrap();
    let stray = leaked(&b.ws.join("marker"));
    let r = b
        .sh("for f in /proc/$$/fd/*; do echo \"${f##*/} $(readlink $f)\"; done")
        .await;
    assert!(!r.stdout.contains("marker"), "{}", r.stdout);
    let fds: Vec<&str> = r
        .stdout
        .lines()
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    // 0-2 and the folder the glob reads.
    assert_eq!(fds.len(), 4, "{}", r.stdout);
    assert!(!fds.contains(&stray.as_raw_fd().to_string().as_str()));
}

#[tokio::test]
async fn host_loopback_is_unreachable_without_the_host_network() {
    let Some(b) = sandbox().await else { return };
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let port = server.local_addr().unwrap().port();
    let r = b
        .sh(&format!(
            "curl -s -m 3 http://127.0.0.1:{port}/a; echo direct=$?"
        ))
        .await;
    assert_eq!(r.stdout.trim(), "direct=7", "{r:?}");
    assert!(server.accept().is_err(), "nothing reached the host");
    // SANDBOX_NET=host, the escape hatch (netaccess kb f025).
    let jail = Jail {
        net: Net::Host,
        ..b.jail()
    };
    let reply = std::thread::spawn(move || {
        server.set_nonblocking(false).unwrap();
        let (mut c, _) = server.accept().unwrap();
        let mut buf = [0u8; 512];
        let _ = c.read(&mut buf).unwrap();
        c.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 11\r\n\r\nhost secret")
            .unwrap();
    });
    let r = b
        .sh_in(
            &jail,
            &format!("curl -s -m 5 http://127.0.0.1:{port}/e"),
            None,
        )
        .await;
    reply.join().unwrap();
    assert_eq!(r.stdout, "host secret");
}

#[tokio::test]
async fn background_processes_die_with_the_command() {
    let Some(b) = sandbox().await else { return };
    let r = b
        .sh("(sleep 1; touch /tmp/late) >/dev/null 2>&1 & echo started")
        .await;
    assert_eq!(r.stdout.trim(), "started");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!b.tmp.join("late").exists());
}

#[tokio::test]
async fn a_timeout_ends_the_sandbox() {
    let Some(b) = sandbox().await else { return };
    let t0 = Instant::now();
    let r = run(
        &b.host,
        &b.jail(),
        &["sleep".into(), "30".into()],
        Duration::from_millis(500),
        None,
    )
    .await
    .unwrap();
    assert!(r.timed_out);
    assert_eq!(r.exit_code, None);
    assert!(t0.elapsed() < Duration::from_secs(10));
}

// ------------------------------------------------------------- proxied

/// A stand-in for the egress proxy (WP 4.8): answers every connection
/// with "ok", and counts them.
#[derive(Default)]
struct StandIn {
    served: Mutex<Option<TcpListener>>,
    stop: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    closed: AtomicBool,
}

impl Egress for StandIn {
    fn serve(&self, listener: TcpListener) {
        listener.set_nonblocking(true).unwrap();
        let mine = listener.try_clone().unwrap();
        *self.served.lock().unwrap() = Some(listener);
        let (stop, count) = (Arc::clone(&self.stop), Arc::clone(&self.connections));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match mine.accept() {
                    Ok((mut c, _)) => {
                        count.fetch_add(1, Ordering::SeqCst);
                        let _ = c.write_all(b"ok\n");
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
    }

    fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.served.lock().unwrap().take();
        self.closed.store(true, Ordering::SeqCst);
    }
}

fn proxied<'a>(b: &'a Box_) -> Jail<'a> {
    Jail {
        net: Net::Proxy,
        ..b.jail()
    }
}

#[tokio::test]
async fn the_proxy_port_is_handed_over_and_served() {
    let Some(b) = sandbox().await else { return };
    let proxy = Arc::new(StandIn::default());
    let r = b
        .sh_in(
            &proxied(&b),
            "python3 -c \"import socket; s = socket.create_connection(('127.0.0.1', 3128)); \
             print(s.recv(3).decode().strip())\"; printenv HTTPS_PROXY",
            Some(proxy.clone()),
        )
        .await;
    assert_eq!(r.net_error, None, "{r:?}");
    assert_eq!(
        r.stdout.split_whitespace().collect::<Vec<_>>(),
        ["ok", PROXY_URL],
        "{r:?}"
    );
    assert_eq!(proxy.connections.load(Ordering::SeqCst), 1);
    assert!(
        proxy.closed.load(Ordering::SeqCst),
        "closed when the command ended"
    );
}

#[tokio::test]
async fn a_proxied_command_holds_no_descriptor_of_the_pair() {
    let Some(b) = sandbox().await else { return };
    let r = b
        .sh_in(
            &proxied(&b),
            "for f in /proc/$$/fd/* /proc/self/fd/*; do echo \"$f $(readlink $f)\"; done",
            Some(Arc::new(StandIn::default())),
        )
        .await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    assert!(!r.stdout.contains("socket:"), "{}", r.stdout);
    assert!(r.stdout.contains("/proc/self/fd/0"));
}

#[tokio::test]
async fn unix_sockets_stay_refused_with_the_proxy() {
    let Some(b) = sandbox().await else { return };
    let r = b
        .sh_in(
            &proxied(&b),
            "python3 -c 'import socket; socket.socket(socket.AF_UNIX)'",
            Some(Arc::new(StandIn::default())),
        )
        .await;
    assert_ne!(r.exit_code, Some(0));
    assert!(r.stderr.contains("Address family not supported"), "{r:?}");
}

#[tokio::test]
async fn direct_addresses_and_dns_fail_with_the_proxy() {
    let Some(b) = sandbox().await else { return };
    let r = b
        .sh_in(
            &proxied(&b),
            "curl -sS --noproxy '*' -m 3 http://1.1.1.1/; echo rc=$?; \
             getent hosts pypi.org; echo dns=$?",
            Some(Arc::new(StandIn::default())),
        )
        .await;
    assert!(r.stdout.contains("rc=7"), "{r:?}");
    assert!(r.stdout.contains("dns=2"), "{r:?}");
}

#[tokio::test]
async fn a_background_process_cannot_use_the_proxy_afterwards() {
    let Some(b) = sandbox().await else { return };
    let proxy = Arc::new(StandIn::default());
    let r = b
        .sh_in(
            &proxied(&b),
            "(sleep 1; python3 -c \"import socket; socket.create_connection(('127.0.0.1', 3128))\" \
             && touch /tmp/late) >/dev/null 2>&1 & echo started",
            Some(proxy.clone()),
        )
        .await;
    assert_eq!(r.stdout.trim(), "started");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!b.tmp.join("late").exists());
    assert_eq!(proxy.connections.load(Ordering::SeqCst), 0);
    assert!(
        proxy.served.lock().unwrap().is_none(),
        "the listener is closed"
    );
}

// rpctl's test_launcher_failure_fails_closed (netaccess kb f027).
#[tokio::test]
async fn a_launcher_that_fails_runs_nothing() {
    let Some(b) = sandbox().await else { return };
    let launchers: [(&str, Launcher<'_>); 3] = [
        (
            "missing",
            Launcher {
                argv: &["python3-not-installed"],
                code: launch::LAUNCHER,
            },
        ),
        (
            "exits",
            Launcher {
                argv: &launch::LAUNCHER_ARGV,
                code: "import sys; sys.exit(3)",
            },
        ),
        (
            "wrong socket",
            Launcher {
                argv: &launch::LAUNCHER_ARGV,
                code: "import socket, sys; p = socket.socket(fileno=int(sys.argv[1])); \
                       s = socket.socket(); socket.send_fds(p, [b'L'], [s.fileno()]); \
                       p.recv(1)",
            },
        ),
    ];
    for (name, launcher) in launchers {
        let proxy = Arc::new(StandIn::default());
        let r = run_with(
            &b.host,
            &proxied(&b),
            &["touch".into(), "ran".into()],
            Duration::from_secs(30),
            Some(proxy.clone()),
            launcher,
        )
        .await
        .unwrap();
        let why = r.net_error.clone().unwrap_or_default();
        assert!(why.contains("did not run"), "{name}: {r:?}");
        assert!(why.contains("no fallback"), "{name}: {r:?}");
        assert_eq!(r.exit_code, None, "{name}");
        assert!(!b.ws.join("ran").exists(), "{name}");
        assert!(proxy.closed.load(Ordering::SeqCst), "{name}");
    }
}

#[tokio::test]
async fn a_proxied_jail_without_a_proxy_is_refused() {
    let Some(b) = sandbox().await else { return };
    let e = run(
        &b.host,
        &proxied(&b),
        &["true".into()],
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn sigpipe_is_back_to_default_after_the_launcher() {
    let Some(b) = sandbox().await else { return };
    let r = b
        .sh_in(
            &proxied(&b),
            "yes | head -1; echo \"${PIPESTATUS[0]}\"",
            Some(Arc::new(StandIn::default())),
        )
        .await;
    assert_eq!(
        r.stdout.split_whitespace().collect::<Vec<_>>(),
        ["y", "141"],
        "{r:?}"
    );
}

#[test]
fn the_chat_tmp_is_made_once_and_removed() {
    let scratch = ScratchDir::new("chat-tmp").unwrap();
    let root = scratch.path().join("sandbox-tmp");
    fs::create_dir_all(root.join("old")).unwrap();
    // A day and a bit old: what a crashed chat left.
    let old = std::time::SystemTime::now() - Duration::from_secs(90000);
    fs::File::open(root.join("old"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    fs::create_dir_all(root.join("recent")).unwrap();
    let mut tmp = ChatTmp::new(root.clone());
    let dir = tmp.path().unwrap().to_path_buf();
    assert_eq!(tmp.path().unwrap(), dir, "made once");
    assert!(dir.is_dir());
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with(&format!("{}-", std::process::id())),
        "{name}"
    );
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(!root.join("old").exists());
    assert!(root.join("recent").exists());
    drop(tmp);
    assert!(!dir.exists());
}

// rpctl's test_mounts ChatTests.test_sandboxed_commands_see_the_mount, at
// the sandbox's level (run_shell itself is WP 4.10): a read-only drive is
// readable but its env file is not and nothing can be written; a rw one
// takes writes, and a .git a command makes there is found and moved out.
#[tokio::test]
async fn sandboxed_commands_see_the_mount() {
    let Some(b) = sandbox().await else { return };
    let drive = b._scratch.path().join("drive");
    fs::create_dir_all(drive.join("docs")).unwrap();
    fs::write(drive.join("docs/a.txt"), "hello from the drive\n").unwrap();
    fs::write(drive.join(".env"), "KEY=secret\n").unwrap();
    let stop = AtomicBool::new(false);
    let ro = [mounts::scan(&drive, false, &stop).unwrap()];
    let d = drive.display();
    let jail = Jail {
        mounts: &ro,
        ..b.jail()
    };
    let r = b
        .sh_in(
            &jail,
            &format!("cat {d}/docs/a.txt; cat {d}/.env; touch {d}/x"),
            None,
        )
        .await;
    assert!(r.stdout.contains("hello from the drive"), "{r:?}");
    assert!(!r.stdout.contains("secret"), "{r:?}");
    assert!(r.stderr.contains("Read-only file system"), "{r:?}");
    assert!(!drive.join("x").exists());
    let rw = [mounts::scan(&drive, true, &stop).unwrap()];
    let jail = Jail {
        mounts: &rw,
        ..b.jail()
    };
    let before = crate::quarantine::autorun_snapshot(&drive);
    let r = b
        .sh_in(&jail, &format!("touch {d}/x && mkdir {d}/docs/.git"), None)
        .await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    assert!(drive.join("x").exists());
    let new = crate::quarantine::autorun_new(&before, &crate::quarantine::autorun_snapshot(&drive));
    assert_eq!(new, [drive.join("docs/.git")]);
    let dest = b._scratch.path().join("quarantine/20260722-000000");
    let (moved, failed) = crate::quarantine::quarantine(&new, &drive, &dest);
    assert!(failed.is_none());
    assert_eq!(moved, [("docs/.git".to_string(), dest.join("docs/.git"))]);
    assert!(!drive.join("docs/.git").exists());
}

// From the WP 4.7 review: Ctrl-C (the run's future dropped) still closes
// the proxy, as rpctl's `finally`.
#[tokio::test]
async fn a_cancelled_proxied_run_still_closes_the_proxy() {
    let Some(b) = sandbox().await else { return };
    let proxy = Arc::new(StandIn::default());
    let jail = proxied(&b);
    let argv: [OsString; 3] = ["bash".into(), "-c".into(), "sleep 20; touch late".into()];
    let cut = tokio::time::timeout(
        Duration::from_millis(1500),
        run(
            &b.host,
            &jail,
            &argv,
            Duration::from_secs(30),
            Some(proxy.clone()),
        ),
    )
    .await;
    assert!(cut.is_err(), "still running when dropped");
    assert!(proxy.closed.load(Ordering::SeqCst));
    assert!(proxy.served.lock().unwrap().is_none());
    assert!(!b.ws.join("late").exists());
}

// rpctl's SandboxLinkTests (test_egress.py, 8258e5c and e386614; flags F52).

#[tokio::test]
async fn a_linked_git_replaced_by_a_real_one_is_new() {
    use crate::quarantine::{autorun_new, autorun_snapshot};
    let Some(b) = sandbox().await else { return };
    let real = b._scratch.path().join("real.git");
    fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, b.ws.join(".git")).unwrap();
    let before = autorun_snapshot(&b.ws);
    let r = b
        .sh("rm .git && mkdir -p .git/hooks && echo x > .git/hooks/pre-commit")
        .await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    assert_eq!(
        autorun_new(&before, &autorun_snapshot(&b.ws)),
        [b.ws.join(".git")]
    );
    assert_eq!(autorun_new(&before, &before), Vec::<PathBuf>::new());
}

#[test]
fn a_link_pointed_elsewhere_is_new() {
    use crate::quarantine::{autorun_new, autorun_snapshot};
    let scratch = ScratchDir::new("autorun-link").unwrap();
    let ws = scratch.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    std::os::unix::fs::symlink(scratch.path().join("a"), ws.join(".vscode")).unwrap();
    let before = autorun_snapshot(&ws);
    fs::remove_file(ws.join(".vscode")).unwrap();
    std::os::unix::fs::symlink(scratch.path().join("b"), ws.join(".vscode")).unwrap();
    assert_eq!(
        autorun_new(&before, &autorun_snapshot(&ws)),
        [ws.join(".vscode")]
    );
}

#[tokio::test]
async fn a_dangling_link_whose_target_appears_is_new() {
    use crate::quarantine::{autorun_new, autorun_snapshot};
    let Some(b) = sandbox().await else { return };
    std::os::unix::fs::symlink(b.ws.join("later"), b.ws.join(".git")).unwrap();
    fs::create_dir_all(b.ws.join("sub")).unwrap();
    std::os::unix::fs::symlink(b._scratch.path().join("nowhere"), b.ws.join("sub/.idea")).unwrap();
    let before = autorun_snapshot(&b.ws);
    let r = b
        .sh("mkdir -p later/hooks && echo x > later/hooks/pre-commit")
        .await;
    assert_eq!(
        r.exit_code,
        Some(0),
        "a broken nested link stops nothing: {r:?}"
    );
    assert_eq!(
        autorun_new(&before, &autorun_snapshot(&b.ws)),
        [b.ws.join(".git")]
    );
}

#[tokio::test]
async fn a_gitdir_target_in_the_workspace_is_read_only() {
    let Some(b) = sandbox().await else { return };
    fs::create_dir_all(b.ws.join("real.git/hooks")).unwrap();
    fs::write(b.ws.join(".git"), "gitdir: real.git\n").unwrap();
    fs::create_dir_all(b.ws.join("sub")).unwrap();
    fs::write(b.ws.join("sub/.git"), "gitdir: ../real.git\n").unwrap();
    let a = bwrap_argv(&b.host, &b.jail(), 3);
    let w: Vec<Vec<OsString>> = a.windows(3).map(<[OsString]>::to_vec).collect();
    for p in [b.ws.join("real.git"), b.ws.join("sub/.git")] {
        let p = p.into_os_string();
        assert!(w.contains(&vec!["--ro-bind".into(), p.clone(), p]));
    }
    let r = b
        .sh("echo x > real.git/hooks/pre-commit; echo y > sub/.git")
        .await;
    assert!(!b.ws.join("real.git/hooks/pre-commit").exists());
    assert_eq!(
        fs::read_to_string(b.ws.join("sub/.git")).unwrap(),
        "gitdir: ../real.git\n"
    );
    assert_eq!(
        r.stderr.matches("Read-only file system").count(),
        2,
        "{r:?}"
    );
}

#[test]
fn gitdir_targets_outside_or_odd_are_not_bound() {
    let scratch = ScratchDir::new("gitdir").unwrap();
    let ws = scratch.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let git = ws.join(".git");
    for text in [
        "gitdir: /nonexistent/x\n",
        "not a gitdir line\n",
        "",
        "gitdir:\n",
    ] {
        fs::write(&git, text).unwrap();
        assert!(
            crate::guard::gitdir_target(&git).is_none_or(|t| !t.exists()),
            "{text:?}"
        );
    }
    let outside = scratch.path().join("outside.git");
    fs::create_dir_all(&outside).unwrap();
    fs::write(&git, format!("gitdir: {}\n", outside.display())).unwrap();
    let host = Host {
        home: scratch.path().join("home"),
        user: "tester".into(),
        lang: "C.UTF-8".into(),
        clients: Vec::new(),
    };
    let a = bwrap_argv(&host, &Jail::new(&ws, &ws), 3);
    assert!(!a.contains(&outside.into_os_string()));
    fs::remove_file(&git).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&git)
        .status()
        .unwrap();
    assert!(made.success());
    assert_eq!(
        crate::guard::gitdir_target(&git),
        None,
        "a FIFO is not read: no hang"
    );
}

#[tokio::test]
async fn a_broken_egg_info_link_does_not_stop_commands() {
    let Some(b) = sandbox().await else { return };
    let gone = b._scratch.path().join("gone");
    std::os::unix::fs::symlink(&gone, b.ws.join("x.egg-info")).unwrap();
    fs::create_dir_all(b.ws.join("y.egg-info")).unwrap();
    let a = bwrap_argv(&b.host, &b.jail(), 3);
    assert!(!a.contains(&gone.into_os_string()));
    assert!(a.contains(&b.ws.join("y.egg-info").into_os_string()));
    assert_eq!(b.sh("true").await.exit_code, Some(0));
}
