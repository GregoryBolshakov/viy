use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use pretty_assertions::assert_eq;
use serde_json::Value;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// How many arguments each bwrap option takes.
fn arity(flag: &str) -> usize {
    match flag {
        "--ro-bind" | "--bind" | "--ro-bind-try" | "--symlink" | "--setenv" => 2,
        "--seccomp" | "--proc" | "--dev" | "--tmpfs" | "--dir" | "--chdir" => 1,
        _ => 0,
    }
}

/// `argv` as options, each run of self-binds (`--ro-bind p p`) and of masks
/// (`--ro-bind /dev/null p`) in (depth, path) order: rpctl's protected
/// paths come out of a set, its masks in the walk's order, and neither
/// order means anything beyond parents before children.
fn normalized(argv: &[String]) -> Vec<Vec<String>> {
    let mut opts: Vec<Vec<String>> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let n = arity(&argv[i]);
        opts.push(argv[i..=i + n].to_vec());
        i += n + 1;
    }
    let kind = |o: &Vec<String>| match o.as_slice() {
        [f, a, b] if f == "--ro-bind" && a == b => 1,
        [f, a, _] if f == "--ro-bind" && a == "/dev/null" => 2,
        _ => 0,
    };
    let depth = |o: &Vec<String>| Path::new(&o[2]).components().count();
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut run: Vec<Vec<String>> = Vec::new();
    for o in opts {
        if !run.is_empty() && kind(&run[0]) != kind(&o) {
            run.sort_by_key(|o| (depth(o), o[2].clone()));
            out.append(&mut run);
        }
        if kind(&o) == 0 {
            out.push(o);
        } else {
            run.push(o);
        }
    }
    run.sort_by_key(|o| (depth(o), o[2].clone()));
    out.append(&mut run);
    out
}

/// The tree a fixture case lays out under `root`, as the generator does.
fn lay_out(root: &Path, case: &Value) {
    let t = |s: &str| s.replace("$T", &root.display().to_string());
    for d in case["dirs"].as_array().into_iter().flatten() {
        std::fs::create_dir_all(root.join(d.as_str().unwrap())).unwrap();
    }
    for (rel, text) in case["files"].as_object().into_iter().flatten() {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text.as_str().unwrap()).unwrap();
    }
    for (rel, target) in case["links"].as_object().into_iter().flatten() {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(t(target.as_str().unwrap()), p).unwrap();
    }
    for (rel, hooks) in case["git"].as_object().into_iter().flatten() {
        let repo = root.join(rel);
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("HOME", root.join("home"))
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", repo.to_str().unwrap()]);
        if let Some(hooks) = hooks.as_str() {
            let dir = repo.join(".git");
            git(&[
                "--git-dir",
                dir.to_str().unwrap(),
                "config",
                "core.hooksPath",
                hooks,
            ]);
        }
    }
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("ws")).unwrap();
}

fn host(root: &Path) -> Host {
    Host {
        home: root.join("home"),
        user: "tester".into(),
        lang: "C.UTF-8".into(),
        clients: Vec::new(),
    }
}

fn strings(argv: &[OsString], root: &Path) -> Vec<String> {
    let root = root.display().to_string();
    argv.iter()
        .map(|a| a.to_string_lossy().replace(&root, "$T"))
        .collect()
}

#[test]
fn bwrap_argv_equals_rpctls() {
    let fixture = viy_testkit::fixtures::load("sandbox/bwrap").unwrap();
    assert!(fixture.cases.len() >= 20);
    // The fixture was made with no global git config; git_hooks_dir reads
    // this machine's, as rpctl's does. Say so rather than compare wrongly.
    let global = Command::new("git")
        .args(["config", "--global", "--get", "core.hooksPath"])
        .output()
        .unwrap();
    assert!(
        !global.status.success(),
        "~/.gitconfig sets core.hooksPath; the sandbox/bwrap cases assume it does not"
    );
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let scratch = ScratchDir::new("bwrap").unwrap();
        let root = scratch.path().join(name);
        lay_out(&root, case);
        let t =
            |v: &Value| PathBuf::from(v.as_str().unwrap().replace("$T", root.to_str().unwrap()));
        let args = &case["args"];
        let list = |key: &str| -> Vec<PathBuf> {
            args[key].as_array().into_iter().flatten().map(t).collect()
        };
        let mounts: Vec<Mount> = args["mounts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| Mount {
                secrets: m["secrets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(t)
                    .collect(),
                ..Mount::new(t(&m["path"]), m["rw"].as_bool().unwrap())
            })
            .collect();
        let net = match &args["network"] {
            Value::Null | Value::Bool(false) => Net::Off,
            Value::Bool(true) => Net::Proxy,
            v => net_mode(v.as_str().unwrap()),
        };
        let (ro_paths, path_extra) = (list("ro_paths"), list("path_extra"));
        let tmp = args.get("tmp_dir").map(t);
        let git = args.get("git_dir").map(t);
        let project = root.join(args["project"].as_str().unwrap_or("ws"));
        let ws = root.join("ws");
        let jail = Jail {
            net,
            readonly: args["readonly"].as_bool().unwrap_or(false),
            tmp_dir: tmp.as_deref(),
            ro_paths: &ro_paths,
            path_extra: &path_extra,
            mounts: &mounts,
            git_dir: git.as_deref(),
            ..Jail::new(&ws, &project)
        };
        let ours = strings(&bwrap_argv(&host(&root), &jail, 3), &root);
        let theirs: Vec<String> = case["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        // viy also protects its own folder (kb d014).
        let ours: Vec<Vec<String>> = normalized(&ours)
            .into_iter()
            .filter(|o| !(o[0] == "--ro-bind" && o[1] == o[2] && o[1].ends_with("/.viy")))
            .collect();
        assert_eq!(ours, normalized(&theirs), "case {name}");
        // viy also counts its own folder (kb d014).
        let ours: Vec<String> = crate::quarantine::autorun_paths(&ws)
            .iter()
            .map(|p| {
                p.display()
                    .to_string()
                    .replace(root.to_str().unwrap(), "$T")
            })
            .filter(|p| !p.ends_with("/.viy"))
            .collect();
        let theirs: Vec<String> = case["autorun"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert_eq!(ours, theirs, "autorun {name}");
        if name == "autorun" {
            assert!(crate::quarantine::autorun_paths(&ws).contains(&ws.join(".viy")));
        }
    }
}

#[test]
fn both_tools_names_are_protected() {
    let scratch = ScratchDir::new("bwrap-names").unwrap();
    let ws = scratch.path().join("ws");
    for d in [".viy", ".rpctl", "sub/.viy"] {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    for f in ["viy", "rp"] {
        std::fs::write(ws.join(f), "").unwrap();
    }
    let got = protected_existing(&ws, &ws, &[]);
    let want: Vec<PathBuf> = [".rpctl", ".viy", "rp", "viy", "sub/.viy"]
        .iter()
        .map(|p| ws.join(p))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn the_clients_code_in_the_workspace_is_read_only() {
    let scratch = ScratchDir::new("bwrap-clients").unwrap();
    let ws = scratch.path().join("ws");
    std::fs::create_dir_all(ws.join("crates")).unwrap();
    let clients = vec![
        ws.join("crates"),
        ws.join("target/debug/viy"),
        PathBuf::from("/elsewhere/rpctl"),
    ];
    assert_eq!(
        protected_existing(&ws, &ws, &clients),
        [ws.join("crates")],
        "only those that exist: bwrap refuses a missing source"
    );
}

// rpctl's SandboxProxyTests.test_argv_shares_the_network_only_in_host_mode
// (test_egress.py), SandboxTmpTests.test_argv_binds_the_chat_tmp
// (test_netaccess.py) and SandboxReadOnlyTests.
// test_readonly_mounts_the_workspace_read_only (test_workflows.py).

fn windows(a: &[OsString], n: usize) -> Vec<Vec<String>> {
    let a: Vec<String> = a.iter().map(|s| s.to_string_lossy().into_owned()).collect();
    a.windows(n).map(<[String]>::to_vec).collect()
}

#[test]
fn argv_shares_the_network_only_in_host_mode() {
    let scratch = ScratchDir::new("bwrap-net").unwrap();
    let ws = scratch.path();
    let h = host(ws);
    for (value, shared, proxied) in [
        ("", false, false),
        ("proxy", false, true),
        ("host", true, false),
        ("bogus", false, true),
    ] {
        let jail = Jail {
            net: net_mode(value),
            ..Jail::new(ws, ws)
        };
        let a = bwrap_argv(&h, &jail, 3);
        assert_eq!(a.contains(&"--share-net".into()), shared, "{value}");
        assert_eq!(a.contains(&"HTTPS_PROXY".into()), proxied, "{value}");
    }
}

#[test]
fn argv_binds_the_chat_tmp() {
    let scratch = ScratchDir::new("bwrap-tmp").unwrap();
    let ws = scratch.path();
    let jail = Jail {
        tmp_dir: Some(Path::new("/x/chat-tmp")),
        ..Jail::new(ws, ws)
    };
    let a = bwrap_argv(&host(ws), &jail, 3);
    let w = windows(&a, 3);
    assert!(w.contains(&vec!["--bind".into(), "/x/chat-tmp".into(), "/tmp".into()]));
    assert!(!windows(&a, 2).contains(&vec!["--tmpfs".into(), "/tmp".into()]));
}

#[test]
fn readonly_binds_the_workspace_read_only() {
    let scratch = ScratchDir::new("bwrap-ro").unwrap();
    let ws = scratch.path();
    let s = ws.display().to_string();
    let ro = bwrap_argv(
        &host(ws),
        &Jail {
            readonly: true,
            ..Jail::new(ws, ws)
        },
        3,
    );
    let rw = bwrap_argv(&host(ws), &Jail::new(ws, ws), 3);
    assert!(windows(&ro, 3).contains(&vec!["--ro-bind".into(), s.clone(), s.clone()]));
    assert!(windows(&rw, 3).contains(&vec!["--bind".into(), s.clone(), s]));
}

// rpctl's test_mounts ChatTests.test_argv_binds_mounts.
#[test]
fn argv_binds_mounts() {
    let scratch = ScratchDir::new("bwrap-mounts").unwrap();
    let proj = scratch.path().join("proj");
    let drive = scratch.path().join("drive");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(drive.join("docs")).unwrap();
    std::fs::write(drive.join(".env"), "KEY=secret\n").unwrap();
    let ro = Mount {
        secrets: vec![drive.join(".env")],
        ..Mount::new(&drive, false)
    };
    let index = |a: &[OsString], s: &Path| a.iter().position(|x| x == s.as_os_str()).unwrap();
    let mounts = [ro];
    let a = bwrap_argv(
        &host(scratch.path()),
        &Jail {
            mounts: &mounts,
            ..Jail::new(&proj, &proj)
        },
        3,
    );
    assert_eq!(a[index(&a, &drive) - 1], "--ro-bind");
    assert!(a.contains(&drive.join(".env").into_os_string()));
    let rw = [Mount::new(&drive, true)];
    let a = bwrap_argv(
        &host(scratch.path()),
        &Jail {
            mounts: &rw,
            ..Jail::new(&proj, &proj)
        },
        3,
    );
    assert_eq!(a[index(&a, &drive) - 1], "--bind");
    let a = bwrap_argv(
        &host(scratch.path()),
        &Jail {
            mounts: &rw,
            readonly: true,
            ..Jail::new(&proj, &proj)
        },
        3,
    );
    assert_eq!(a[index(&a, &drive) - 1], "--ro-bind");
}
