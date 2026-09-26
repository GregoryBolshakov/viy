//! The tunnel against an sshd of the tests' own (viy_testkit::sshd): real
//! ssh, ssh-keyscan and ssh-keygen, a forward to a stand-in for vLLM.

use std::io::Read;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_config::Config;
use viy_config::Paths;
use viy_testkit::http::FakeHttp;
use viy_testkit::http::Reply;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::sshd::LocalSshd;
use viy_testkit::sshd::SshdExec;
use viy_testkit::sshd::free_port;

use super::*;

const API_KEY: &str = "rpa_SYNTHETICKEY0000";

/// A pod behind a local sshd, with a stand-in for vLLM that answers every
/// connection with "vllm".
struct Bench {
    sshd: LocalSshd,
    exec: Arc<SshdExec>,
    _dir: ScratchDir,
    paths: Paths,
    cfg: Config,
    said: Arc<Mutex<Vec<Notice>>>,
}

impl Bench {
    fn new() -> Self {
        let sshd = LocalSshd::start().unwrap();
        let vllm = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let vllm_port = vllm.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in vllm.incoming() {
                let _ = std::io::Write::write_all(&mut stream.unwrap(), b"vllm");
            }
        });
        let dir = ScratchDir::new("tunnel").unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.pod_state_dir()).unwrap();
        std::fs::write(
            paths.pod_state_file(),
            json!({"pod_id": "pod111", "volume_id": "vol222", "tunnel_only": true}).to_string(),
        )
        .unwrap();
        let mut cfg = Config::defaults();
        cfg.set("POD_NAME", "qwen-pod");
        cfg.set("NETWORK_VOLUME_ID", "vol222");
        cfg.set("VLLM_PORT", &vllm_port.to_string());
        cfg.set("TUNNEL_LOCAL_PORT", &free_port().unwrap().to_string());
        Self {
            exec: sshd.exec(),
            sshd,
            _dir: dir,
            paths,
            cfg,
            said: Arc::default(),
        }
    }

    fn ssh(&self) -> Ssh<'_> {
        Ssh {
            paths: &self.paths,
            cfg: &self.cfg,
            exec: &*self.exec,
        }
    }

    fn say(&self) -> impl FnMut(Notice) + Send + 'static {
        let said = Arc::clone(&self.said);
        move |n| said.lock().unwrap_or_else(PoisonError::into_inner).push(n)
    }

    /// The notices so far, emptied.
    fn said(&self) -> Vec<(Tone, String)> {
        std::mem::take(&mut *self.said.lock().unwrap())
            .into_iter()
            .map(|n| (n.tone, n.text))
            .collect()
    }

    /// What the tunnel said and logged, for a failed assertion.
    fn why(&self) -> String {
        let said = self.said.lock().unwrap_or_else(PoisonError::into_inner);
        let said: Vec<&str> = said.iter().map(|n| n.text.as_str()).collect();
        let log = std::fs::read_to_string(self.paths.tunnel_log_file()).unwrap_or_default();
        let listening = std::process::Command::new("ss")
            .args(["-Hltnp", &format!("sport = :{}", self.local())])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        format!(
            "local port {}, sshd port {}\nsaid: {said:#?}\ntunnel.log:\n{log}\nlistening there: {listening}",
            self.local(),
            self.sshd.port
        )
    }

    fn local(&self) -> u16 {
        self.cfg.get("TUNNEL_LOCAL_PORT").unwrap().parse().unwrap()
    }

    /// What the forward's local end answers, if it accepts.
    fn through_the_tunnel(&self) -> Option<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.local())).ok()?;
        let mut text = String::new();
        stream.read_to_string(&mut text).ok()?;
        Some(text)
    }

    fn pin(&self) {
        std::fs::write(
            self.paths.known_hosts_file(),
            format!("rpctl-qwen-pod {}\n", self.sshd.host_key().unwrap()),
        )
        .unwrap();
    }

    /// The pod RunPod reports: this sshd's address.
    fn pod(&self) -> Reply {
        Reply::answer(
            200,
            json!({"id": "pod111", "publicIp": "127.0.0.1", "portMappings": {"22": self.sshd.port}})
                .to_string(),
        )
    }

    fn boot_log(&self, fingerprint: &str) -> Reply {
        let line = format!("sshd host key: 256 {fingerprint} root@pod (ED25519)");
        Reply::answer(200, format!("data: {}\n", json!({"line": line})))
    }

    /// How many forwards were started.
    fn forwards(&self) -> usize {
        self.exec
            .sent()
            .iter()
            .filter(|c| c.args.first().is_some_and(|a| a == "-N"))
            .count()
    }

    /// The ssh forwards for this bench's ports.
    fn forward_pids(&self) -> Vec<u32> {
        let spec = format!(
            "127.0.0.1:{}:127.0.0.1:{}",
            self.local(),
            self.cfg.get("VLLM_PORT").unwrap()
        );
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().to_str()?.parse().ok())
            .filter(|&pid| {
                let argv = process::cmdline(pid);
                argv.first().is_some_and(|a| a == b"ssh")
                    && argv.iter().any(|a| a == spec.as_bytes())
            })
            .collect()
    }

    fn tunnel_pid(&self) -> u32 {
        running(&self.ssh()).unwrap().pid.unwrap()
    }
}

#[tokio::test]
async fn a_checked_first_contact_pins_the_key_and_the_tunnel_reaches_vllm() {
    let bench = Bench::new();
    let ssh = bench.ssh();
    let fingerprint = bench.sshd.host_fingerprint().unwrap();
    let rp = RunPod::new(
        API_KEY,
        Arc::new(FakeHttp::new([bench.boot_log(&fingerprint)])),
    );
    let port = bench.sshd.port;

    assert!(
        start(&ssh, &rp, "127.0.0.1", port, false, &mut bench.say())
            .await
            .unwrap()
    );
    let pid = bench.tunnel_pid();
    let local = bench.local();
    let vllm = bench.cfg.get("VLLM_PORT").unwrap();
    assert_eq!(
        bench.said(),
        [
            (
                Tone::Ok,
                "Pinned this pod's SSH host key; it matches the fingerprint in the pod's boot log:"
                    .to_string()
            ),
            (Tone::Dim, format!("  rpctl-qwen-pod ED25519 {fingerprint}")),
            (
                Tone::Dim,
                "The key lives on the network volume, so every later pod must present the same \
                 one or the connection is refused."
                    .to_string()
            ),
            (
                Tone::Ok,
                format!(
                    "Tunnel up (pid {pid}): 127.0.0.1:{local} -> 127.0.0.1:{port} -> 127.0.0.1:{vllm}"
                )
            ),
        ]
    );
    assert_eq!(bench.through_the_tunnel().as_deref(), Some("vllm"));
    assert_eq!(
        std::fs::read_to_string(bench.paths.known_hosts_file()).unwrap(),
        format!("rpctl-qwen-pod {}\n", bench.sshd.host_key().unwrap())
    );
    let state = state::load(&bench.paths).unwrap();
    assert_eq!(
        state.get(key::TUNNEL_LOCAL_PORT),
        Some(&json!(local.to_string()))
    );
    // The forward is an ssh in a session of its own.
    assert_eq!(
        process::cmdline(pid).first().map(Vec::as_slice),
        Some(&b"ssh"[..])
    );

    // Started again, it is the same tunnel.
    assert!(
        start(&ssh, &rp, "127.0.0.1", port, false, &mut bench.say())
            .await
            .unwrap()
    );
    assert_eq!(
        bench.said(),
        [(Tone::Dim, format!("Tunnel already running (pid {pid})."))]
    );
    assert_eq!(bench.forwards(), 1);

    assert!(status(&ssh, &mut bench.say()).await.unwrap());
    assert_eq!(
        bench.said(),
        [
            (
                Tone::Ok,
                format!("  tunnel:    running (pid {pid}) on 127.0.0.1:{local}")
            ),
            (
                Tone::Plain,
                format!("  host key:  rpctl-qwen-pod ED25519 {fingerprint}")
            ),
        ]
    );
    assert_eq!(session_warnings(&ssh).await.unwrap(), Vec::<String>::new());

    assert!(stop(&ssh, false, &mut bench.say()).await.unwrap());
    assert_eq!(
        bench.said(),
        [(Tone::Ok, format!("Stopped the tunnel (pid {pid})."))]
    );
    assert!(!process::pid_alive(pid));
    assert_eq!(
        state::load(&bench.paths).unwrap().get(key::TUNNEL_PID),
        Some(&Value::Null)
    );
    assert_eq!(bench.through_the_tunnel(), None);
    assert!(!stop(&ssh, false, &mut bench.say()).await.unwrap());
}

#[tokio::test]
async fn a_key_the_boot_log_disowns_is_not_pinned_and_no_tunnel_starts() {
    let bench = Bench::new();
    let other = "SHA256:Synthetic9999SomeoneElsesKeyFingerprint99999";
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([bench.boot_log(other)])));
    let port = bench.sshd.port;
    assert!(
        !start(
            &bench.ssh(),
            &rp,
            "127.0.0.1",
            port,
            false,
            &mut bench.say()
        )
        .await
        .unwrap()
    );
    let fingerprint = bench.sshd.host_fingerprint().unwrap();
    assert_eq!(
        bench.said(),
        [(
            Tone::Error,
            format!(
                "SSH to 127.0.0.1:{port} presented host key {fingerprint}, but the pod's boot log \
                 (read over RunPod's HTTPS API) says {other}.\nTreat it as a machine-in-the-middle: \
                 nothing was pinned and no tunnel started."
            )
        )]
    );
    assert!(!bench.paths.known_hosts_file().exists());
    assert_eq!(bench.forwards(), 0);
}

#[tokio::test]
async fn a_changed_host_key_is_refused_and_explained() {
    let bench = Bench::new();
    std::fs::write(
        bench.paths.known_hosts_file(),
        "rpctl-qwen-pod ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl\n",
    )
    .unwrap();
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([])));
    let port = bench.sshd.port;
    assert!(
        !start(
            &bench.ssh(),
            &rp,
            "127.0.0.1",
            port,
            false,
            &mut bench.say()
        )
        .await
        .unwrap()
    );
    let said = bench.said();
    assert_eq!(
        said[0],
        (
            Tone::Error,
            format!("SSH to 127.0.0.1:{port} failed; no tunnel started.")
        )
    );
    assert!(
        said[1].1.contains("REMOTE HOST IDENTIFICATION HAS CHANGED"),
        "{said:?}"
    );
    assert_eq!(said[2].0, Tone::Warn);
    assert!(said[2].1.contains("viy tunnel --forget-key"), "{said:?}");
    assert_eq!(bench.forwards(), 0);

    bench.ssh().forget_host_key(&mut bench.say()).await.unwrap();
    assert_eq!(
        bench.said(),
        [(
            Tone::Warn,
            "Forgot the pinned host key for 'rpctl-qwen-pod'. The next `viy tunnel` trusts \
             whatever answers."
                .to_string()
        )]
    );
    assert!(bench.ssh().pinned_fingerprint().await.unwrap().is_none());
}

/// test_startup.py test_dead_tunnel_is_restarted_then_given_up: a forward
/// that dies mid-wait is restarted at the pod's current mapping, and one
/// that keeps dying ends the wait.
#[tokio::test]
async fn dead_tunnel_is_restarted_then_given_up() {
    let bench = Bench::new();
    bench.pin();
    let ssh = bench.ssh();
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([bench.pod(), bench.pod()])));
    let started = start(
        &ssh,
        &rp,
        "127.0.0.1",
        bench.sshd.port,
        true,
        &mut bench.say(),
    )
    .await
    .unwrap();
    assert!(started, "{}", bench.why());
    bench.said();
    // The guard starts from a mapping that has gone stale.
    let mut guard = Guard::with_max_restarts("pod111", "127.0.0.1", 1, 2);
    for attempt in 1..=2 {
        let dead = bench.tunnel_pid();
        process::kill(dead, Duration::from_secs(3));
        guard.check(&ssh, &rp, &mut bench.say()).await.unwrap();
        let said = bench.said();
        assert_eq!(
            said[0],
            (
                Tone::Error,
                format!("The SSH tunnel (pid {dead}) died while waiting for vLLM.")
            )
        );
        assert!(said.contains(&(
            Tone::Warn,
            format!("Restarting it (attempt {attempt} of 2)...")
        )));
        assert!(
            said.last().unwrap().1.starts_with("Tunnel up (pid "),
            "{said:?}"
        );
        assert_eq!(bench.through_the_tunnel().as_deref(), Some("vllm"));
    }
    assert_eq!(bench.forwards(), 3);

    let dead = bench.tunnel_pid();
    process::kill(dead, Duration::from_secs(3));
    let Err(GuardError::GaveUp { message, alert }) = guard.check(&ssh, &rp, &mut bench.say()).await
    else {
        panic!("the guard did not give up");
    };
    let log = bench.paths.tunnel_log_file();
    assert_eq!(
        message,
        format!(
            "The tunnel died 3 times; giving up. ssh's output is in {}. The pod is still billing: \
             fix the cause and run viy tunnel -b, or viy stop.",
            log.display()
        )
    );
    assert_eq!(
        alert,
        Alert {
            title: "viy up: SSH tunnel keeps dying".to_string(),
            body: format!("see {}", log.display()),
            urgent: true,
        }
    );
    assert_eq!(bench.forwards(), 3);
    assert_eq!(
        state::load(&bench.paths).unwrap().get(key::TUNNEL_PID),
        Some(&Value::Null)
    );
}

/// test_startup.py test_live_tunnel_is_left_alone.
#[tokio::test]
async fn live_tunnel_is_left_alone() {
    let bench = Bench::new();
    bench.pin();
    let ssh = bench.ssh();
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([])));
    let started = start(
        &ssh,
        &rp,
        "127.0.0.1",
        bench.sshd.port,
        true,
        &mut bench.say(),
    )
    .await
    .unwrap();
    assert!(started, "{}", bench.why());
    let commands = bench.exec.sent().len();
    Guard::new("pod111", "127.0.0.1", bench.sshd.port)
        .check(&ssh, &rp, &mut bench.say())
        .await
        .unwrap();
    assert_eq!(bench.said(), []);
    assert_eq!(bench.exec.sent().len(), commands);
    stop(&ssh, true, &mut bench.say()).await.unwrap();
}

#[tokio::test]
async fn a_restart_that_fails_ends_the_wait() {
    let bench = Bench::new();
    bench.pin();
    let ssh = bench.ssh();
    let rp = RunPod::new(
        API_KEY,
        Arc::new(FakeHttp::new([Reply::answer(503, "down")])),
    );
    // Recorded, but dead; and the local port taken by someone else.
    state::save(&bench.paths, [(key::TUNNEL_PID, json!(99_999_999))]).unwrap();
    let _taken = TcpListener::bind(("127.0.0.1", bench.local())).unwrap();
    let Err(GuardError::GaveUp { message, alert }) =
        Guard::new("pod111", "127.0.0.1", bench.sshd.port)
            .check(&ssh, &rp, &mut bench.say())
            .await
    else {
        panic!("the guard did not give up");
    };
    assert_eq!(
        message,
        "Could not restart the SSH tunnel. The pod is still billing: viy tunnel -b to retry, or viy stop."
    );
    assert_eq!(alert.title, "viy up: SSH tunnel failed");
    let said = bench.said();
    assert_eq!(
        said[..2],
        [
            (
                Tone::Error,
                "The SSH tunnel (pid 99999999) died while waiting for vLLM.".to_string()
            ),
            (Tone::Warn, "Restarting it (attempt 1 of 3)...".to_string()),
        ]
    );
    assert_eq!(said[2].0, Tone::Error);
    assert_eq!(bench.forwards(), 0);
}

#[tokio::test]
async fn stray_forwards_for_our_port_are_ended() {
    let bench = Bench::new();
    let ssh = bench.ssh();
    let spec = format!(
        "127.0.0.1:{}:127.0.0.1:{}",
        bench.local(),
        bench.cfg.get("VLLM_PORT").unwrap()
    );
    let mut ours = viy_testkit::exec::named_process("/usr/bin/ssh", &["-N", "-L", &spec]).unwrap();
    let other =
        viy_testkit::exec::named_process("ssh", &["-N", "-L", "127.0.0.1:1:127.0.0.1:2"]).unwrap();
    let not_ssh = viy_testkit::exec::named_process("sshd", &["-L", &spec]).unwrap();
    kill_strays(&ssh).await.unwrap();
    assert!(!ours.wait().unwrap().success());
    assert!(process::pid_alive(other.id()));
    assert!(process::pid_alive(not_ssh.id()));
}

#[tokio::test]
async fn the_foreground_tunnel_reconnects_and_closes_on_interrupt() {
    let bench = Bench::new();
    bench.pin();
    let ssh = bench.ssh();
    let rp = RunPod::new(API_KEY, Arc::new(FakeHttp::new([bench.pod()])));
    let (interrupt, interrupted) = tokio::sync::oneshot::channel::<()>();
    let mut say = bench.say();
    let tunnel = foreground(
        &ssh,
        &rp,
        "pod111",
        "127.0.0.1".to_string(),
        bench.sshd.port,
        async {
            let _ = interrupted.await;
        },
        &mut say,
    );
    let drive = async {
        let open = || async {
            for _ in 0..200 {
                if bench.through_the_tunnel().as_deref() == Some("vllm") {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!(
                "the forward never came up: {:?}\n{}",
                bench.said.lock().unwrap(),
                bench.sshd.log()
            );
        };
        open().await;
        // A dropped connection; SIGTERM would make ssh exit 0, which ends
        // the tunnel as rpctl's does.
        let forward = bench.forward_pids();
        assert_eq!(forward.len(), 1);
        // SAFETY: a plain signal to the forward's ssh.
        unsafe { libc::kill(forward[0] as i32, libc::SIGKILL) };
        // The backoff is 2 s; then the same endpoint again.
        tokio::time::sleep(Duration::from_millis(500)).await;
        open().await;
        interrupt.send(()).unwrap();
    };
    let (ended, ()) = tokio::join!(tunnel, drive);
    ended.unwrap();
    let said = bench.said();
    let local = bench.local();
    let port = bench.sshd.port;
    let vllm = bench.cfg.get("VLLM_PORT").unwrap();
    assert_eq!(
        said,
        [
            (
                Tone::Ok,
                format!("Forwarding 127.0.0.1:{local} -> 127.0.0.1:{port} -> 127.0.0.1:{vllm}")
            ),
            (
                Tone::Dim,
                "Leave this running; use `viy chat` in another terminal. Ctrl-C to stop."
                    .to_string()
            ),
            (
                Tone::Warn,
                "Tunnel dropped (ssh exit signal 9); reconnecting in 2s...".to_string()
            ),
            (
                Tone::Dim,
                "\nTunnel closed. vLLM is no longer reachable from this machine.".to_string()
            ),
        ]
    );
    // The interrupt ended the ssh, not just the wait for it.
    assert_eq!(bench.forward_pids(), Vec::<u32>::new());
    assert_eq!(bench.through_the_tunnel(), None);
    assert_eq!(bench.forwards(), 2);
}

#[tokio::test]
async fn the_command_stops_reports_and_refuses_a_second_tunnel() {
    let bench = Bench::new();
    bench.pin();
    let ssh = bench.ssh();
    let never = || -> Result<RunPod, String> { panic!("no RunPod client needed") };
    command(
        &ssh,
        Action::Stop,
        never,
        std::future::pending(),
        &mut bench.say(),
    )
    .await
    .unwrap();
    assert_eq!(
        bench.said(),
        [(Tone::Dim, "No tunnel was running.".to_string())]
    );

    let pods = [bench.pod(), bench.pod()];
    let connect = || Ok(RunPod::new(API_KEY, Arc::new(FakeHttp::new(pods))));
    command(
        &ssh,
        Action::Background,
        connect,
        std::future::pending(),
        &mut bench.say(),
    )
    .await
    .unwrap();
    let said = bench.said();
    assert!(said[0].1.starts_with("Tunnel up (pid "), "{said:?}");
    assert_eq!(
        said[1],
        (Tone::Dim, "Stop it with:  viy tunnel --stop".to_string())
    );
    let pid = bench.tunnel_pid();

    command(
        &ssh,
        Action::Foreground,
        never,
        std::future::pending(),
        &mut bench.say(),
    )
    .await
    .unwrap();
    assert_eq!(
        bench.said(),
        [
            (
                Tone::Warn,
                format!("A tunnel is already running in the background (pid {pid}).")
            ),
            (
                Tone::Dim,
                "Stop it first with:  viy tunnel --stop".to_string()
            ),
        ]
    );
    command(
        &ssh,
        Action::Stop,
        never,
        std::future::pending(),
        &mut bench.say(),
    )
    .await
    .unwrap();
    assert_eq!(
        bench.said(),
        [(Tone::Ok, format!("Stopped the tunnel (pid {pid})."))]
    );

    let refused = command(
        &ssh,
        Action::Background,
        || Err("RUNPOD_API_KEY is not set.".to_string()),
        std::future::pending(),
        &mut bench.say(),
    )
    .await;
    assert!(matches!(refused, Err(TunnelError::Exit(m)) if m == "RUNPOD_API_KEY is not set."));
}
