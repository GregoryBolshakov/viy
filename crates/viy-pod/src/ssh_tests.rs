use std::fs;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_testkit::exec::FakeExec;
use viy_testkit::exec::NamedProcess;
use viy_testkit::exec::Reply as Run;
use viy_testkit::exec::named_process;
use viy_testkit::fixtures;
use viy_testkit::http::FakeHttp;
use viy_testkit::http::Reply;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::sshd::ClosedPort;
use viy_testkit::sshd::closed_port;

use super::*;
use crate::tunnel;
use crate::tunnel::TunnelError;

/// The fixture's synthetic key (tools/fixtures.py API_KEY).
const API_KEY: &str = "rpa_SYNTHETICKEY0000";
/// The pid the fixture calls a live ssh, and the local port it uses.
const FIXTURE_PID: &str = "4242";
const FIXTURE_PORT: &str = "18001";

fn http_replies(case: &Value) -> Vec<Reply> {
    case["replies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let status = r["status"].as_u64().map_or(200, |s| s as u16);
            Reply::answer(status, r["body"].as_str().unwrap_or(""))
        })
        .collect()
}

/// rpctl's text with viy's commands in it (notice.rs).
fn as_viy(text: &str) -> String {
    text.replace("rp down && TUNNEL=1 rp up", "viy down && TUNNEL=1 viy up")
        .replace("rp down && rp up", "viy down && viy up")
        .replace("rp tunnel", "viy tunnel")
        .replace("Run rp up", "Run viy up")
        .replace("not a tunnel rp started", "not a tunnel rp or viy started")
}

fn tone_name(tone: Tone) -> &'static str {
    match tone {
        Tone::Plain => "plain",
        Tone::Dim => "dim",
        Tone::Ok => "ok",
        Tone::Warn => "warn",
        Tone::Error => "error",
    }
}

/// The processes and listeners a case stands up, ended with it.
#[derive(Default)]
struct Stage {
    live: Option<NamedProcess>,
    listener: Option<TcpListener>,
    /// Held closed until the tunnel's ssh is to listen there.
    closed: Option<ClosedPort>,
}

/// rpctl's ssh and tunnel functions, case by case: the commands, the
/// requests, the notices, the result and the files after.
async fn replay(only: impl Fn(&str) -> bool) {
    let fixture = fixtures::load("pod/ssh").unwrap();
    let mut played = 0;
    for case in fixture
        .cases
        .iter()
        .filter(|c| only(c["name"].as_str().unwrap()))
    {
        played += 1;
        let name = case["name"].as_str().unwrap();
        let dir = ScratchDir::new("pod-ssh").unwrap();
        let paths = Paths::under(dir.path());
        let mut stage = Stage::default();
        let mut cfg = Config::defaults();
        cfg.set("POD_NAME", "qwen-pod");
        cfg.set("RUNPOD_API_KEY", API_KEY);
        for (k, v) in [
            ("NETWORK_VOLUME_ID", "vol222"),
            ("TUNNEL_LOCAL_PORT", FIXTURE_PORT),
            ("VLLM_PORT", "8000"),
        ] {
            cfg.set(k, v);
        }
        for (k, v) in case["config"].as_object().into_iter().flatten() {
            cfg.set(k, v.as_str().unwrap());
        }
        // The fixture's port and pid, made real here.
        // A port that should answer is held from the start, so no other
        // test takes it meanwhile.
        if case["listening"] == true {
            stage.listener = Some(TcpListener::bind(("127.0.0.1", 0)).unwrap());
        }
        let port = if cfg.get("TUNNEL_LOCAL_PORT") == Some(FIXTURE_PORT) {
            let port = match &stage.listener {
                Some(listener) => listener.local_addr().unwrap().port(),
                None => {
                    let closed = closed_port().unwrap();
                    let port = closed.port;
                    stage.closed = Some(closed);
                    port
                }
            }
            .to_string();
            cfg.set("TUNNEL_LOCAL_PORT", &port);
            port
        } else {
            FIXTURE_PORT.to_string()
        };
        if case["live"] == true {
            stage.live = Some(named_process("ssh", &["-N"]).unwrap());
        }
        let mut pid = stage
            .live
            .as_ref()
            .map_or(FIXTURE_PID.to_string(), |c| c.id().to_string());
        let state_dir = paths.pod_state_dir().display().to_string();
        let real =
            |text: &str, pid: &str| text.replace(FIXTURE_PID, pid).replace(FIXTURE_PORT, &port);
        fs::create_dir_all(paths.pod_state_dir()).unwrap();
        for (key, path) in [
            ("state", paths.pod_state_file()),
            ("known_hosts", paths.known_hosts_file()),
            ("tunnel_log", paths.tunnel_log_file()),
        ] {
            if let Some(text) = case[key].as_str() {
                fs::write(path, real(text, &pid)).unwrap();
            }
        }
        let mut runs: Vec<Run> = case["runs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| match r["raise_"].as_str() {
                Some("timeout") => Run::Fail(io::ErrorKind::TimedOut),
                Some("missing") => Run::Fail(io::ErrorKind::NotFound),
                _ => Run::exit(
                    r["returncode"].as_i64().unwrap_or(0) as i32,
                    r["stdout"].as_str().unwrap_or(""),
                    r["stderr"].as_str().unwrap_or(""),
                ),
            })
            .collect();
        if let Some(popen) = case["popen"].as_object() {
            runs.push(Run::StandIn(match popen.get("exits") {
                Some(code) => Command::new("sh")
                    .arg("-c")
                    .arg(format!("printf %s \"$1\" >&2; exit {code}"))
                    .arg("sh")
                    .arg(real(
                        popen.get("stderr").and_then(Value::as_str).unwrap_or(""),
                        &pid,
                    )),
                None if case["listening"].is_array() => Command::new("python3").args([
                    "-I",
                    "-c",
                    "import socket, sys, time\n\
                     s = socket.socket()\n\
                     s.bind(('127.0.0.1', int(sys.argv[1])))\n\
                     s.listen()\n\
                     time.sleep(600)",
                    &port,
                ]),
                None => Command::new("sleep").arg("600"),
            }));
        }
        if case["listening"].is_array() {
            stage.closed = None;
        }
        let exec = FakeExec::new(runs);
        let http = Arc::new(FakeHttp::new(http_replies(case)));
        let rp = RunPod::new(API_KEY, http.clone());
        let ssh = Ssh {
            paths: &paths,
            cfg: &cfg,
            exec: &exec,
        };
        let said = Mutex::new(Vec::new());
        let mut say = |n: Notice| {
            said.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(json!([tone_name(n.tone), n.text]));
        };
        let args = &case["args"];
        let (ip, ssh_port) = (
            args[0].as_str().unwrap_or(""),
            args[1].as_u64().unwrap_or(0) as u16,
        );
        let argv = |c: Command| -> Value {
            json!(
                c.argv()
                    .iter()
                    .map(|a| a.replace(&state_dir, "<STATE>"))
                    .collect::<Vec<_>>()
            )
        };
        let outcome: Result<Value, TunnelError> = match case["call"].as_str().unwrap() {
            "alias" => Ok(json!(ssh.alias().unwrap())),
            "ssh_opts" => Ok(
                argv(Command::new("x").args(ssh.opts("accept-new").unwrap()))
                    .as_array()
                    .map(|a| json!(a[1..]))
                    .unwrap(),
            ),
            "tunnel_cmd" => Ok(argv(tunnel::tunnel_cmd(&ssh, ip, ssh_port).unwrap())),
            "ssh_endpoint" => Ok(json!(
                ssh_endpoint(&args[0]).map(|(ip, port)| [ip, port.to_string()])
            )),
            "pinned_fingerprint" => Ok(json!(ssh.pinned_fingerprint().await.unwrap())),
            "forget_host_key" => ssh
                .forget_host_key(&mut say)
                .await
                .map(|()| Value::Null)
                .map_err(Into::into),
            "scan_host_key" => Ok(json!(ssh.scan_host_key(ip, ssh_port).await.unwrap())),
            "pin_checked_key" => Ok(json!(match ssh
                .pin_checked_key(&rp, ip, ssh_port, &mut say)
                .await
                .unwrap()
            {
                Checked::Pinned => "pinned",
                Checked::Mismatch => "mismatch",
                Checked::Unchecked => "unchecked",
            })),
            "preflight_ssh" => Ok(json!(
                ssh.preflight(&rp, ip, ssh_port, &mut say).await.unwrap()
            )),
            "tunnel_log_tail" => Ok(json!(tunnel::tunnel_log_tail(&ssh, 5))),
            "tunnel_status" => Ok(json!(tunnel::status(&ssh, &mut say).await.unwrap())),
            "preflight" => Ok(json!(tunnel::session_warnings(&ssh).await.unwrap())),
            "start_tunnel" => Ok(json!(
                tunnel::start(&ssh, &rp, ip, ssh_port, false, &mut say)
                    .await
                    .unwrap()
            )),
            "stop_tunnel" => Ok(json!(tunnel::stop(&ssh, false, &mut say).await.unwrap())),
            "resolve_ssh_target" => tunnel::resolve_target(&ssh, &rp, &mut say)
                .await
                .map(|(id, ip, port)| json!([id, ip, port])),
            call => panic!("{name}: no call {call}"),
        };
        // A stand-in the tunnel left running is pod.json's tunnel_pid.
        let after = fs::read_to_string(paths.pod_state_file()).ok();
        if case["popen"].is_object()
            && let Some(state) = &after
            && let Some(started) =
                serde_json::from_str::<Value>(state).unwrap()["tunnel_pid"].as_u64()
        {
            pid = started.to_string();
            crate::process::kill(started as u32, std::time::Duration::from_secs(1));
        }

        let ran: Vec<Value> = exec
            .sent()
            .into_iter()
            .map(|c| {
                json!({
                    "stdin": c.stdin.as_ref().map(|s| String::from_utf8_lossy(s).into_owned()),
                    "timeout": c.timeout.map(|t| t.as_secs()),
                    "argv": argv(c),
                })
            })
            .collect();
        let want_ran: Vec<Value> = case["ran"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                json!({
                    "stdin": r["input"],
                    "timeout": r["timeout"],
                    "argv": json!(r["argv"].as_array().unwrap().iter()
                        .map(|a| real(a.as_str().unwrap(), &pid)).collect::<Vec<_>>()),
                })
            })
            .collect();
        assert_eq!(ran, want_ran, "{name}: commands");
        let sent: Vec<String> = http
            .sent()
            .iter()
            .map(|r| format!("{} {}", r.method, r.url))
            .collect();
        assert_eq!(json!(sent), case["sent"], "{name}: requests");
        let want_said: Vec<Value> = case["said"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| json!([s[0], as_viy(&real(s[1].as_str().unwrap(), &pid))]))
            .collect();
        assert_eq!(*said.lock().unwrap(), want_said, "{name}: notices");
        match outcome {
            Ok(value) => {
                let want =
                    serde_json::from_str::<Value>(&as_viy(&real(&case["ok"].to_string(), &pid)))
                        .unwrap();
                assert_eq!(value, want, "{name}");
            }
            Err(TunnelError::Failed) => assert_eq!(case["exit"], 1, "{name}"),
            Err(TunnelError::Exit(message)) => {
                assert_eq!(
                    json!(message),
                    json!(as_viy(case["exit"].as_str().unwrap())),
                    "{name}"
                );
            }
            Err(TunnelError::Io(e)) => panic!("{name}: {e}"),
        }
        let want_after = case["state_after"].as_str().map(|s| real(s, &pid));
        assert_eq!(after, want_after, "{name}: pod.json");
        let known_hosts = fs::read_to_string(paths.known_hosts_file()).ok();
        assert_eq!(
            json!(known_hosts),
            case["known_hosts_after"],
            "{name}: known_hosts"
        );
        // rpctl signalled the ssh: the live one or the stand-in is gone.
        if case["signals"].as_array().is_some_and(|s| !s.is_empty())
            && let Some(child) = &mut stage.live
        {
            let status = child.wait().unwrap();
            assert!(!status.success(), "{name}: the ssh was not ended");
        }
    }
    assert!(played > 0);
}

#[tokio::test]
async fn ssh_and_tunnel_do_what_rpctls_do() {
    replay(|name| name != "start_never_listens").await;
}

/// The one case that waits out rpctl's 60 checks, 15 s of them.
#[tokio::test(start_paused = true)]
async fn a_tunnel_that_never_listens_is_ended() {
    replay(|name| name == "start_never_listens").await;
}

fn scratch_ssh_state(dir: &ScratchDir, state: Value) -> Paths {
    let paths = Paths::under(dir.path());
    fs::create_dir_all(paths.pod_state_dir()).unwrap();
    fs::write(paths.pod_state_file(), state.to_string()).unwrap();
    paths
}

// test_regions.py HostKeyAliasTests: the key lives on the volume, so each
// volume is its own identity.

fn alias_for(state: Value) -> String {
    let dir = ScratchDir::new("ssh-alias").unwrap();
    let paths = scratch_ssh_state(&dir, state);
    let mut cfg = Config::defaults();
    cfg.set("POD_NAME", "p");
    cfg.set("NETWORK_VOLUME_ID", "prim");
    Ssh {
        paths: &paths,
        cfg: &cfg,
        exec: &FakeExec::default(),
    }
    .alias()
    .unwrap()
}

#[test]
fn primary_volume_keeps_the_plain_alias() {
    assert_eq!(alias_for(json!({"volume_id": "prim"})), "rpctl-p");
    assert_eq!(alias_for(json!({})), "rpctl-p");
}

#[test]
fn backup_volume_gets_its_own_alias() {
    assert_eq!(alias_for(json!({"volume_id": "bk"})), "rpctl-p-bk");
}

// test_regions.py FirstContactTests: a new volume's host key is pinned only
// if it matches the fingerprint the pod printed in its boot log.

const LINE: &str =
    "sshd host key: 256 SHA256:4K6Kzir7pOvKk/6zcIBgF6hYkPIkyH5L0GTIdscicOo root@f7fa (ED25519)";

fn sse(lines: &[&str]) -> Reply {
    Reply::answer(
        200,
        lines
            .iter()
            .map(|l| format!("data: {}\n", json!({"line": l})))
            .collect::<String>(),
    )
}

/// `_pin_checked_key` with `logged` in the boot log and `seen` presented.
async fn first_contact(logged: &[&str], seen: &str) -> (Checked, Option<String>) {
    let scan = [
        Run::stdout("[1.2.3.4]:22 ssh-ed25519 AAAAkey\n"),
        Run::stdout(format!("256 {seen} x (ED25519)\n")),
    ];
    let (checked, known_hosts, _) = first_contact_scanning(logged, scan).await;
    (checked, known_hosts)
}

/// `_pin_checked_key` with `logged` in the boot log and `scan` the runs of
/// ssh-keyscan and ssh-keygen; the notices it gave too.
async fn first_contact_scanning(
    logged: &[&str],
    scan: impl IntoIterator<Item = Run>,
) -> (Checked, Option<String>, Vec<String>) {
    let dir = ScratchDir::new("ssh-first-contact").unwrap();
    let paths = scratch_ssh_state(&dir, json!({"pod_id": "x", "volume_id": "bk"}));
    let mut cfg = Config::defaults();
    cfg.set("POD_NAME", "p");
    cfg.set("NETWORK_VOLUME_ID", "prim");
    let lines: Vec<String> = logged
        .iter()
        .map(|fp| format!("sshd host key: 256 {fp} root@x (ED25519)"))
        .collect();
    let http = Arc::new(FakeHttp::new([sse(&lines
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>())]));
    let exec = FakeExec::new(scan);
    let ssh = Ssh {
        paths: &paths,
        cfg: &cfg,
        exec: &exec,
    };
    let mut said = Vec::new();
    let checked = ssh
        .pin_checked_key(&RunPod::new(API_KEY, http), "1.2.3.4", 22, &mut |n| {
            said.push(n.text)
        })
        .await
        .unwrap();
    let known_hosts = fs::read_to_string(paths.known_hosts_file()).ok();
    (checked, known_hosts, said)
}

#[tokio::test]
async fn the_boot_log_line_is_read() {
    let http = Arc::new(FakeHttp::new([sse(&["noise", LINE])]));
    let keys = logged_host_keys(&RunPod::new(API_KEY, http), "x")
        .await
        .unwrap();
    assert_eq!(
        keys,
        BTreeSet::from(["SHA256:4K6Kzir7pOvKk/6zcIBgF6hYkPIkyH5L0GTIdscicOo".to_string()])
    );
}

#[tokio::test]
async fn matching_key_is_pinned_under_the_volume_alias() {
    assert_eq!(
        first_contact(&["SHA256:a"], "SHA256:a").await,
        (
            Checked::Pinned,
            Some("rpctl-p-bk ssh-ed25519 AAAAkey\n".to_string())
        )
    );
}

#[tokio::test]
async fn mismatch_pins_nothing() {
    assert_eq!(
        first_contact(&["SHA256:a"], "SHA256:evil").await,
        (Checked::Mismatch, None)
    );
}

/// rpctl 5a76e0e: an endpoint offering only RSA or ECDSA, or a scan that
/// timed out, would otherwise have ssh pin whatever it is shown next.
#[tokio::test]
async fn no_ed25519_key_against_a_logged_fingerprint_pins_nothing() {
    let rsa_only = [Run::stdout("[1.2.3.4]:22 ssh-rsa AAAArsa\n")];
    let (checked, known_hosts, said) = first_contact_scanning(&["SHA256:a"], rsa_only).await;
    assert_eq!((checked, known_hosts), (Checked::Mismatch, None));
    assert!(said[0].contains("no ed25519 host key"), "{said:?}");
}

/// rpctl 5a76e0e: the pod's sshd has only an ed25519 key (boot.py).
#[test]
fn ssh_accepts_only_an_ed25519_host_key() {
    let dir = ScratchDir::new("ssh-key-type").unwrap();
    let paths = Paths::under(dir.path());
    let cfg = Config::defaults();
    let exec = FakeExec::default();
    let ssh = Ssh {
        paths: &paths,
        cfg: &cfg,
        exec: &exec,
    };
    let opts = ssh.opts("accept-new").unwrap();
    let at = opts
        .iter()
        .position(|o| o == "HostKeyAlgorithms=ssh-ed25519")
        .expect("the option");
    assert_eq!(opts[at - 1], "-o");
}

#[tokio::test]
async fn no_logged_fingerprint_falls_back_to_first_use() {
    let dir = ScratchDir::new("ssh-first-use").unwrap();
    let paths = scratch_ssh_state(&dir, json!({"pod_id": "x", "volume_id": "bk"}));
    let cfg = Config::defaults();
    let exec = FakeExec::default();
    let ssh = Ssh {
        paths: &paths,
        cfg: &cfg,
        exec: &exec,
    };
    let http = Arc::new(FakeHttp::new([sse(&[])]));
    let checked = ssh
        .pin_checked_key(&RunPod::new(API_KEY, http), "1.2.3.4", 22, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(checked, Checked::Unchecked);
    assert!(
        exec.sent().is_empty(),
        "nothing to compare with, nothing scanned"
    );
    assert!(!paths.known_hosts_file().exists());
}

/// test_startup.py test_ssh_refreshes_mapping: a starting pod can change
/// its mapping, and the probe never sticks to the first endpoint.
#[tokio::test]
async fn ssh_refreshes_mapping() {
    let open = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let new_port = open.local_addr().unwrap().port();
    let closed = closed_port().unwrap();
    let old_port = closed.port;
    let pod = |port: u16| {
        Reply::answer(
            200,
            json!({"id": "id", "publicIp": "127.0.0.1", "portMappings": {"22": port}}).to_string(),
        )
    };
    let http = Arc::new(FakeHttp::new([pod(old_port), pod(new_port)]));
    let rp = RunPod::new(API_KEY, http);
    let mut watch = SshWatch::new(&rp, "id");
    assert_eq!(
        watch.probe().await,
        Err(format!("127.0.0.1:{old_port} not accepting"))
    );
    assert_eq!(watch.probe().await, Ok(("127.0.0.1".to_string(), new_port)));
}

#[tokio::test]
async fn a_failed_call_keeps_the_last_endpoint_and_none_is_no_mapping() {
    let http = Arc::new(FakeHttp::new([
        Reply::answer(200, json!({"id": "id"}).to_string()),
        Reply::answer(
            200,
            json!({"id": "id", "publicIp": "127.0.0.1", "portMappings": {"22": 1}}).to_string(),
        ),
        Reply::answer(503, "upstream unavailable"),
    ]));
    let rp = RunPod::new(API_KEY, http);
    let mut watch = SshWatch::new(&rp, "id");
    assert_eq!(watch.probe().await, Err("no port mapping yet".to_string()));
    assert_eq!(
        watch.probe().await,
        Err("127.0.0.1:1 not accepting".to_string())
    );
    assert_eq!(
        watch.probe().await,
        Err("127.0.0.1:1 not accepting".to_string())
    );
}

#[test]
fn the_public_key_is_found_ed25519_first() {
    let dir = ScratchDir::new("ssh-pubkey").unwrap();
    let paths = Paths::under(dir.path());
    assert_eq!(find_ssh_pubkey(&paths).unwrap(), None);
    fs::create_dir_all(dir.path().join(".ssh")).unwrap();
    fs::write(dir.path().join(".ssh/id_rsa.pub"), "ssh-rsa AAAArsa me\n").unwrap();
    assert_eq!(
        find_ssh_pubkey(&paths).unwrap().as_deref(),
        Some("ssh-rsa AAAArsa me")
    );
    fs::write(
        dir.path().join(".ssh/id_ed25519.pub"),
        "  ssh-ed25519 AAAAed me \n",
    )
    .unwrap();
    assert_eq!(
        find_ssh_pubkey(&paths).unwrap().as_deref(),
        Some("ssh-ed25519 AAAAed me")
    );
}
