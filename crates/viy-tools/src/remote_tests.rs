//! rpctl tests/test_remote.py MatchTests, ArgvTests, BoxTests,
//! ResolveTests and NoteTests (the chat's are in viy-agent), and the
//! oracle fuzz of the decision and the text against rpctl's own functions.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

pub(crate) fn host() -> Host {
    Host {
        alias: "hetzner".into(),
        hostname: "203.0.113.7".into(),
        user: "user".into(),
        port: 22,
        key: PathBuf::from("/keys/hetzner"),
        known_hosts: PathBuf::from("/keys/known_hosts"),
        agent: String::new(),
        boxed: true,
    }
}

fn full() -> Host {
    Host {
        boxed: false,
        ..host()
    }
}

fn m(cmd: &str) -> Result<Option<(String, String)>, Refused> {
    let hosts = [host()];
    match_command(cmd, &hosts).map(|got| got.map(|(h, r)| (h.alias.clone(), r)))
}

fn run_on(r: &str) -> Result<Option<(String, String)>, Refused> {
    Ok(Some(("hetzner".into(), r.into())))
}

// ---- MatchTests ---------------------------------------------------------------------

#[test]
fn runs_on_the_server() {
    assert_eq!(m("ssh hetzner 'ls -la | head'"), run_on("ls -la | head"));
    assert_eq!(m("ssh hetzner uname -a"), run_on("uname -a"));
    assert_eq!(m("ssh user@hetzner -- ls"), run_on("ls"));
    assert_eq!(m("ssh 203.0.113.7 'df -h'"), run_on("df -h"));
    assert_eq!(m("ssh hetzner 'echo $(id)'"), run_on("echo $(id)"));
    assert_eq!(
        m("ssh hetzner 'cat > f <<\"EOF\"\nhi\nEOF'"),
        run_on("cat > f <<\"EOF\"\nhi\nEOF")
    );
}

#[test]
fn other_commands_are_not_its_business() {
    for cmd in ["ls", "ssh other ls", "echo hetzner", "git log"] {
        assert_eq!(m(cmd), Ok(None), "{cmd}");
    }
    assert_eq!(match_command("ssh hetzner ls", &[]), Ok(None));
}

#[test]
fn refuses_what_would_act_here_or_change_ssh() {
    for cmd in [
        "ssh hetzner",
        "ssh -i k hetzner ls",
        "ssh hetzner -t ls",
        "ssh -o ProxyCommand=sh hetzner ls",
        "ssh hetzner ls | grep x",
        "ssh hetzner ls > out",
        "ssh hetzner ls; rm x",
        "ssh hetzner \"echo $(id)\"",
        "ssh hetzner ls\nrm -rf x",
        "cd x && ssh hetzner ls",
        "scp a hetzner:/tmp",
        "rsync -a x user@203.0.113.7:/y",
        "/usr/bin/ssh hetzner ls",
    ] {
        assert!(m(cmd).is_err(), "{cmd}: {:?}", m(cmd));
    }
}

// ---- ArgvTests ----------------------------------------------------------------------

#[test]
fn model_text_is_only_the_last_argument() {
    let a = argv(&host(), "-oProxyCommand=sh x");
    assert_eq!(
        a[a.len() - 3..],
        ["--", "203.0.113.7", "-oProxyCommand=sh x"]
    );
    assert_eq!(a[1..3], ["-F", "/dev/null"]);
    for opt in [
        "IdentitiesOnly=yes",
        "IdentityAgent=none",
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "ProxyCommand=none",
        "ForwardAgent=no",
        "PermitLocalCommand=no",
        "ControlPath=none",
        "ClearAllForwardings=yes",
    ] {
        assert!(a.iter().any(|x| x == opt), "{opt}");
    }
    assert!(
        !env(&host(), Path::new("/h"))
            .iter()
            .any(|(k, _)| k == "SSH_AUTH_SOCK")
    );
}

#[test]
fn agent_key() {
    let h = Host {
        alias: "h".into(),
        user: "u".into(),
        port: 2222,
        key: "/k".into(),
        known_hosts: "/kh".into(),
        agent: "/run/agent.sock".into(),
        ..host()
    };
    let a = argv(&h, "ls");
    assert!(a.iter().any(|x| x == "IdentityAgent=\"/run/agent.sock\""));
    let p = a.iter().position(|x| x == "-p").unwrap();
    assert_eq!(a[p + 1], "2222");
    assert!(
        env(&h, Path::new("/h"))
            .iter()
            .any(|(k, v)| k == "SSH_AUTH_SOCK" && v == "/run/agent.sock")
    );
    assert_eq!(h.login(), "u@203.0.113.7:2222");
}

#[test]
fn parse_arg_takes_a_key_and_full() {
    assert_eq!(parse_arg(""), (String::new(), None, false));
    assert_eq!(parse_arg("hetzner"), ("hetzner".into(), None, false));
    assert_eq!(parse_arg("hetzner full"), ("hetzner".into(), None, true));
    assert_eq!(
        parse_arg("hetzner ~/.ssh/k full"),
        ("hetzner".into(), Some("~/.ssh/k".into()), true)
    );
    assert_eq!(
        parse_arg("hetzner full ~/.ssh/k"),
        ("hetzner".into(), Some("~/.ssh/k".into()), true)
    );
}

#[test]
fn root_words() {
    for (cmd, word) in [
        ("sudo systemctl restart app", Some("sudo")),
        ("cd /srv && sudo -n true", Some("sudo")),
        ("ls\n  nohup sudo reboot", Some("sudo")),
        ("echo x | doas tee f", Some("doas")),
        ("su - -c id", Some("su")),
        ("(exec sudo id)", Some("sudo")),
        ("grep sudo /var/log/auth.log", None),
        ("sudoku", None),
        ("ls ~/sudo", None),
        ("summary", None),
    ] {
        assert_eq!(root_word(cmd).as_deref(), word, "{cmd}");
    }
}

#[test]
fn shown_writes_out_controls() {
    assert_eq!(
        shown("ls\x1b[2K\u{202e} x\n\tok"),
        "ls\\x1b[2K\\u202e x\n\tok"
    );
}

// ---- BoxTests -----------------------------------------------------------------------

#[test]
fn full_host_gets_the_command_as_written() {
    assert_eq!(command(&full(), "ls", 60), "ls");
    assert!(host().boxed);
}

#[test]
fn model_text_is_one_word_for_bash_in_bwrap() {
    let cmd = "echo 'a'\"b\" $HOME; rm -rf / # x\nexit 3";
    let boxed = command(&host(), cmd, 60);
    assert!(boxed.ends_with(&format!("bash -c {}; }}", shell_quote(cmd))));
    let last = &boxed[boxed.find("printf '").unwrap()..];
    for flag in [
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--seccomp 3",
        "timeout -k 5 60 bwrap",
    ] {
        assert!(last.contains(flag), "{flag}");
    }
    assert!(boxed.starts_with("set -- --ro-bind / / --proc /proc"));
    // The filter is sent with the command, byte for byte.
    let octal = last
        .split_once("printf '")
        .unwrap()
        .1
        .split_once('\'')
        .unwrap()
        .0;
    let bytes: Vec<u8> = octal
        .split('\\')
        .skip(1)
        .map(|o| u8::from_str_radix(o, 8).unwrap())
        .collect();
    assert_eq!(bytes, crate::sandbox::seccomp::program());
}

#[test]
fn secret_names_match_this_machines() {
    let names = [
        ".env",
        ".env.prod",
        ".env.example",
        "credentials.json",
        "id_ed25519",
        "id_ed25519.pub",
        "server.KEY",
        "app-secrets.yml",
        "secrets",
        ".netrc",
        "notes.txt",
        "secretary.json",
        "main.py",
    ];
    let d = ScratchDir::new("remote-find").unwrap();
    let dir = d.path().join("a").join("b");
    std::fs::create_dir_all(&dir).unwrap();
    for n in names {
        std::fs::write(dir.join(n), "x").unwrap();
    }
    let out = std::process::Command::new("sh")
        .args(["-c", &secret_find()])
        .env_clear()
        .env("HOME", d.path())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let mut found: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|p| p.starts_with(&d.path().display().to_string()))
        .map(|p| {
            Path::new(p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    found.sort();
    let mut want: Vec<String> = names
        .iter()
        .filter(|n| crate::secrets::is_secret_name(n))
        .map(|n| (*n).to_string())
        .collect();
    want.sort();
    assert_eq!(found, want);
    assert!(found.contains(&".env.prod".to_string()));
    assert!(!found.contains(&".env.example".to_string()));
}

/// The wrapper itself, run with this machine's bwrap against a fake home
/// (outside /tmp, which the sandbox replaces).
#[test]
fn runs_sandboxed_here() {
    let Some(cache) = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")) else {
        return;
    };
    if std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--unshare-all", "true"])
        .output()
        .map_or(true, |o| !o.status.success())
    {
        eprintln!("skipped: bwrap cannot run here");
        return;
    }
    std::fs::create_dir_all(&cache).unwrap();
    let home = cache.join(format!("viy-remote-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh").join("id_x"), "LEAKME-KEY").unwrap();
    std::fs::create_dir_all(home.join("app")).unwrap();
    std::fs::write(home.join("app").join(".env"), "TOKEN=LEAKME-ENV").unwrap();
    std::fs::write(
        home.join("app").join("prod-secrets.json"),
        "{\"k\": \"LEAKME-JSON\"}",
    )
    .unwrap();
    std::fs::write(home.join("app").join("id_x.pub"), "public").unwrap();
    std::fs::write(home.join("app").join("notes"), "readable").unwrap();
    let script = "cat ~/.ssh/id_x ~/app/.env ~/app/prod-secrets.json 2>/dev/null; \
        cat ~/app/id_x.pub ~/app/notes; echo; touch ~/app/new 2>/dev/null || echo ro; \
        echo kept > made; pwd; \
        python3 -c 'import socket; socket.socket(socket.AF_UNIX)' 2>/dev/null || echo nounix; \
        exit 7";
    let r = std::process::Command::new("sh")
        .args(["-c", &box_command(script, 20)])
        .current_dir(&home)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&r.stdout).into_owned();
    let made = std::fs::read_to_string(home.join(BOX_DIR).join("made"));
    let new = home.join("app").join("new").exists();
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(
        r.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert!(!stdout.contains("LEAKME"), "{stdout}");
    assert_eq!(
        stdout.split_whitespace().collect::<Vec<_>>(),
        [
            "publicreadable",
            "ro",
            &home.join(BOX_DIR).display().to_string(),
            "nounix"
        ]
    );
    assert_eq!(made.unwrap(), "kept\n");
    assert!(!new);
}

#[test]
fn probe_reasons() {
    let ran = |stderr: &str, code: Option<i32>| Ran {
        exit_code: code,
        timed_out: false,
        stdout: String::new(),
        stderr: stderr.into(),
    };
    assert_eq!(
        probe_reason(&ran("bubblewrap (bwrap) is not installed there\n", Some(3))).as_deref(),
        Some("bubblewrap (bwrap) is not installed there")
    );
    assert!(
        probe_reason(&ran(
            "bwrap: setting up uid map: Permission denied\n",
            Some(1)
        ))
        .is_some_and(|w| w.contains("uid map"))
    );
    assert_eq!(
        probe_reason(&ran("Connection refused\n", Some(255))).as_deref(),
        Some("ssh failed: Connection refused")
    );
}

// ---- ResolveTests -------------------------------------------------------------------

struct Fake {
    opts: Mutex<HashMap<String, Vec<String>>>,
    known_files: Mutex<Option<Vec<String>>>,
    passphrase: bool,
    holds: bool,
    home: PathBuf,
}

impl Machine for Fake {
    fn ssh_g(&self, _alias: &str) -> Result<Opts, String> {
        Ok(self.opts.lock().unwrap().clone())
    }
    fn known_in(&self, files: &[String], hostname: &str, _port: u16) -> Option<PathBuf> {
        *self.known_files.lock().unwrap() = Some(files.to_vec());
        (hostname == "203.0.113.7").then(|| PathBuf::from("/kh2"))
    }
    fn needs_passphrase(&self, _key: &Path) -> bool {
        self.passphrase
    }
    fn fingerprint(&self, _public: &Path) -> Option<String> {
        Some("SHA256:x".into())
    }
    fn agent_holds(&self, _sock: &str, _fingerprint: Option<&str>) -> bool {
        self.holds
    }
    fn agent_sock(&self) -> String {
        "/run/a".into()
    }
    fn home(&self) -> PathBuf {
        self.home.clone()
    }
}

fn fake(d: &ScratchDir) -> (Fake, PathBuf) {
    let key = d.path().join("hetzner");
    std::fs::write(&key, "key").unwrap();
    let opts: HashMap<String, Vec<String>> = [
        ("hostname", vec!["203.0.113.7".to_string()]),
        ("user", vec!["user".into()]),
        ("port", vec!["22".into()]),
        ("identityfile", vec![key.display().to_string()]),
        ("userknownhostsfile", vec!["/kh /kh2".into()]),
        ("globalknownhostsfile", vec!["/g".into()]),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    (
        Fake {
            opts: Mutex::new(opts),
            known_files: Mutex::new(None),
            passphrase: false,
            holds: false,
            home: d.path().to_path_buf(),
        },
        key,
    )
}

#[test]
fn settings_are_fixed_at_grant() {
    let d = ScratchDir::new("remote-resolve").unwrap();
    let (f, key) = fake(&d);
    let h = resolve(&f, "hetzner", None, true).unwrap();
    assert_eq!(
        (
            h.hostname.as_str(),
            h.user.as_str(),
            h.port,
            h.key.clone(),
            h.known_hosts.clone(),
            h.agent.as_str()
        ),
        (
            "203.0.113.7",
            "user",
            22,
            std::fs::canonicalize(&key).unwrap(),
            PathBuf::from("/kh2"),
            ""
        )
    );
    assert_eq!(
        f.known_files.lock().unwrap().clone().unwrap(),
        ["/kh", "/kh2", "/g"]
    );
}

#[test]
fn refusals() {
    let d = ScratchDir::new("remote-resolve").unwrap();
    let (f, _key) = fake(&d);
    for bad in ["-oProxyCommand=x", "a;id", ""] {
        assert!(resolve(&f, bad, None, true).is_err(), "{bad}");
    }
    f.opts
        .lock()
        .unwrap()
        .insert("proxyjump".into(), vec!["bastion".into()]);
    assert!(resolve(&f, "hetzner", None, true).is_err_and(|e| e.contains("proxyjump")));
    f.opts.lock().unwrap().remove("proxyjump");
    f.opts
        .lock()
        .unwrap()
        .insert("hostname".into(), vec!["198.51.100.1".into()]);
    assert!(resolve(&f, "hetzner", None, true).is_err_and(|e| e.contains("known_hosts")));
    f.opts
        .lock()
        .unwrap()
        .insert("hostname".into(), vec!["203.0.113.7".into()]);
    assert!(resolve(&f, "hetzner", Some("/nope"), true).is_err_and(|e| e.contains("no key file")));
    let other = d.path().join("other");
    std::fs::write(&other, "key").unwrap();
    f.opts
        .lock()
        .unwrap()
        .get_mut("identityfile")
        .unwrap()
        .push(other.display().to_string());
    assert!(resolve(&f, "hetzner", None, true).is_err_and(|e| e.contains("several keys")));
    let named = resolve(&f, "hetzner", Some(&other.display().to_string()), true).unwrap();
    assert_eq!(named.key, std::fs::canonicalize(&other).unwrap());
}

#[test]
fn passphrase_key_needs_the_agent_to_hold_it() {
    let d = ScratchDir::new("remote-resolve").unwrap();
    let (mut f, _key) = fake(&d);
    f.passphrase = true;
    assert!(resolve(&f, "hetzner", None, true).is_err_and(|e| e.contains("ssh-add")));
    f.holds = true;
    assert_eq!(resolve(&f, "hetzner", None, true).unwrap().agent, "/run/a");
}

#[test]
fn config_hosts_skip_wildcards() {
    let text = "Host *\n  User x\nHost hetzner box # two\n  HostName h\nHost *.lan !a\nhost Home\n";
    assert_eq!(config_hosts(text), ["hetzner", "box", "Home"]);
}

// ---- NoteTests ----------------------------------------------------------------------

#[test]
fn notes_are_told_apart() {
    let n = note(&[host()], &[]);
    assert!(n.contains("sandboxed"));
    assert!(n.contains(&format!("~/{BOX_DIR}")));
    assert!(!note(&[full()], &[]).contains("writable"));
    assert!(is_note(&n));
    assert!(!crate::mounts::is_note(&n));
    assert!(!is_note(&crate::mounts::note(&[], &[])));
    assert!(n.contains("ssh hetzner '<command>'"));
    assert!(note(&[], &["hetzner".into()]).contains("took back ssh access to hetzner"));
}

// ---- the oracle ---------------------------------------------------------------------

#[cfg(feature = "oracle")]
fn host_json(h: &Host) -> serde_json::Value {
    serde_json::json!({"alias": h.alias, "hostname": h.hostname, "user": h.user,
                       "port": h.port, "key": h.key, "known_hosts": h.known_hosts,
                       "agent": h.agent, "boxed": h.boxed})
}

#[cfg(feature = "oracle")]
#[test]
fn remote_matches_rpctl() {
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;
    use viy_testkit::oracle::tuple;

    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let mut call =
        |f: &str, args: serde_json::Value| oracle.call(f, args, json!({})).unwrap().outcome;
    let box_host = Host {
        alias: "box".into(),
        hostname: "192.0.2.5".into(),
        user: "root".into(),
        port: 2222,
        agent: "/run/agent \"x\".sock".into(),
        boxed: false,
        ..host()
    };
    let hosts_sets = [
        vec![host()],
        vec![host(), box_host.clone()],
        vec![box_host.clone()],
    ];

    // the fixed pieces
    for h in [host(), box_host] {
        let cmd = "ls -la; echo 'x'";
        assert_eq!(
            call("remote.argv", json!([host_json(&h), cmd])),
            Outcome::Returned(json!(argv(&h, cmd)))
        );
        let ours: serde_json::Map<String, serde_json::Value> = env(&h, Path::new("/h"))
            .into_iter()
            .filter(|(k, _)| k != "HOME" && k != "LANG")
            .map(|(k, v)| (k.to_string_lossy().into_owned(), json!(v.to_string_lossy())))
            .collect();
        assert_eq!(
            call("remote.env", json!([host_json(&h)])),
            Outcome::Returned(json!(ours))
        );
    }
    assert_eq!(
        call("remote.secret_find", json!([])),
        Outcome::Returned(json!(secret_find()))
    );

    let pieces = [
        "ssh",
        "ssh ",
        " ",
        "  ",
        "hetzner",
        "HETZNER",
        "user@hetzner",
        "203.0.113.7",
        "box",
        "root@192.0.2.5",
        "other",
        "-i k ",
        "-o ProxyCommand=sh ",
        "-t",
        "--",
        "ls",
        "-la",
        "'",
        "\"",
        "|",
        ";",
        "&&",
        "||",
        "&",
        ">",
        ">>",
        "<",
        "<<",
        "$(",
        ")",
        "(",
        "`",
        "\n",
        "\t",
        "#",
        "\\",
        "scp ",
        "rsync ",
        "sftp ",
        "sshfs ",
        "/usr/bin/",
        ":",
        "@",
        "x",
        "echo",
        "$HOME",
        "cd",
        "sudo",
        "{",
        "}",
        "é",
        "\u{1c}",
        "=",
        "*",
    ];
    // match's outcomes: not about a lent host, run there, refused
    let mut seen = [0usize; 3];
    for case in 0..cases(20000) {
        let at = format!("case {case}, seed {:#x}", seed());
        let hosts = g.pick(&hosts_sets).clone();
        let wire: Vec<serde_json::Value> = hosts.iter().map(host_json).collect();
        let mut cmd = g.string(&pieces, 12);
        let target = *g.pick(&["hetzner", "user@hetzner", "box", "203.0.113.7", "HeTzNeR"]);
        match g.below(3) {
            0 => cmd = format!("ssh {target} {cmd}"),
            // the form a model writes: the remote command in single quotes
            1 => {
                cmd = format!(
                    "ssh {target} '{}'{}",
                    cmd.replace('\'', ""),
                    g.pick(&["", " ", "\n", " x", " | y"])
                )
            }
            _ => {}
        }
        let ours = match match_command(&cmd, &hosts) {
            Ok(None) => {
                seen[0] += 1;
                Outcome::Returned(json!(null))
            }
            Ok(Some((h, r))) => {
                seen[1] += 1;
                Outcome::Returned(json!([h.alias, r]))
            }
            Err(Refused(why)) => {
                seen[2] += 1;
                Outcome::Raised {
                    kind: "Refused".into(),
                    message: why,
                }
            }
        };
        assert_eq!(
            call("remote.match", json!([cmd, wire])),
            ours,
            "match {at}: {cmd:?}"
        );

        let text = g.string(&pieces, 8);
        assert_eq!(
            call("remote.root_word", json!([text])),
            Outcome::Returned(json!(root_word(&text))),
            "root_word {at}: {text:?}"
        );
        assert_eq!(
            call("remote.shown", json!([text])),
            Outcome::Returned(json!(shown(&text))),
            "shown {at}: {text:?}"
        );
        let (a, k, f) = parse_arg(&text);
        assert_eq!(
            call("remote.parse_arg", json!([text])),
            Outcome::Returned(tuple(vec![json!(a), json!(k), json!(f)])),
            "parse_arg {at}: {text:?}"
        );
        // \x1c-\x1e end a line to Python's splitlines, not to Rust's lines:
        // an implementation detail of the listing, left out here
        let cfg = format!(
            "Host {text}\n{}Host {}\n",
            g.pick(&["", "  User x\n", "HOST a b\n"]),
            g.string(&pieces, 4)
        )
        .replace('\u{1c}', " ");
        // rpctl raises on a line shlex cannot split (an unclosed quote),
        // and its /ssh listing with it; viy skips the line (kb d129)
        match call("remote.config_hosts", json!([cfg])) {
            Outcome::Raised { kind, .. } if kind == "ValueError" => {}
            theirs => assert_eq!(
                theirs,
                Outcome::Returned(json!(config_hosts(&cfg))),
                "config_hosts {at}: {cfg:?}"
            ),
        }
        let gone: Vec<String> = (0..g.below(3))
            .map(|_| (*g.pick(&["hetzner", "box", "a b"])).to_string())
            .collect();
        assert_eq!(
            call("remote.note", json!([wire, gone])),
            Outcome::Returned(json!(note(&hosts, &gone))),
            "note {at}"
        );
        if case % 50 == 0 {
            let timeout = 1 + g.below(300) as u64;
            let boxed = box_command(&text, timeout);
            let octal = octal(&crate::sandbox::seccomp::program());
            let (before, after) = boxed.split_once(&octal).unwrap();
            assert_eq!(
                call("remote.box_command", json!([text, timeout])),
                Outcome::Returned(json!([before, after])),
                "box_command {at}: {text:?}"
            );
        }
    }
    eprintln!("match: none, run, refused: {seen:?}");
}
