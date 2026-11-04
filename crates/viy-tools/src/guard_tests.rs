use std::path::Path;
use std::path::PathBuf;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// rpctl test_secret_files NameTests.test_file_tool_and_write_gates_use_the_same_set.
#[test]
fn file_tool_and_write_gates_use_the_same_set() {
    let ws = Path::new("/ws");
    for n in ["secrets.json", "id_rsa", "deploy.pem"] {
        assert!(secret_reason(&ws.join(n)).is_some(), "{n}");
        assert!(protected_reason(&ws.join(n), ws, ws, &[]).is_some(), "{n}");
    }
    assert_eq!(secret_reason(&ws.join("main.py")), None);
}

#[test]
fn protected_names_of_both_tools() {
    let ws = Path::new("/ws");
    let reason = |p: &str| protected_reason(&ws.join(p), ws, ws, &[]);
    assert_eq!(
        reason(".git/config").as_deref(),
        Some(".git runs or configures code outside the sandbox")
    );
    assert!(reason("vendor/lib/.git/hooks/pre-commit").is_some());
    assert!(reason(".rpctl/config").is_some());
    assert!(reason(".viy/config").is_some());
    assert_eq!(reason("rp").as_deref(), Some("rp runs outside the sandbox"));
    assert_eq!(
        reason("viy").as_deref(),
        Some("viy runs outside the sandbox")
    );
    assert_eq!(reason("sub/viy"), None);
    assert_eq!(
        reason("rpctl.egg-info/PKG-INFO"),
        None,
        "only the root's egg-info dir itself"
    );
    assert!(reason("rpctl.egg-info").is_some());
    assert_eq!(
        reason("sub/.envrc").as_deref(),
        Some("direnv runs it when you cd into the directory")
    );
    assert_eq!(
        protected_reason(Path::new("/elsewhere/x"), ws, ws, &[]).as_deref(),
        Some("it is outside the workspace")
    );
    assert_eq!(
        protected_reason(
            &ws.join("tool/bin/viy"),
            ws,
            ws,
            &[PathBuf::from("/ws/tool")]
        )
        .as_deref(),
        Some("it is the rp client's own code")
    );
    assert_eq!(reason("src/main.rs"), None);
}

#[test]
fn hooks_path_inside_the_project_is_protected() {
    let scratch = ScratchDir::new("guard-hooks").unwrap();
    let ws = scratch.path();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(ws)
            .args(args)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    assert_eq!(git_hooks_dir(ws), None);
    git(&["config", "core.hooksPath", ".githooks"]);
    assert_eq!(git_hooks_dir(ws), Some(ws.join(".githooks")));
    assert!(
        protected_reason(&ws.join(".githooks/pre-commit"), ws, ws, &[])
            .unwrap()
            .contains("hooks directory")
    );
    git(&["config", "core.hooksPath", "/elsewhere"]);
    assert_eq!(git_hooks_dir(ws), None);
}

#[test]
fn workspaces_that_cannot_be_confined() {
    let home = Path::new("/home/me");
    let rules = Path::new("/home/me/.config/viy/approval.rules");
    let problem = |ws: &str| workspace_problem(Path::new(ws), home, rules);
    assert!(problem("/").unwrap().contains("whole home directory"));
    assert!(problem("/home").unwrap().contains("whole home directory"));
    assert!(
        problem("/home/me")
            .unwrap()
            .contains("whole home directory")
    );
    assert!(
        problem("/home/me/.ssh/x")
            .unwrap()
            .contains("is inside /home/me/.ssh")
    );
    assert_eq!(problem("/home/me/project"), None);
    let inside = workspace_problem(
        Path::new("/srv/p"),
        home,
        Path::new("/srv/p/approval.rules"),
    );
    assert!(inside.unwrap().contains("approval rules file"));
}

/// rpctl's protected_reason against viy's on generated paths in a scratch
/// workspace (`cargo test -p viy-tools --features oracle`): viy protects
/// wherever rpctl does, with rpctl's reason, and more only for viy's names.
#[cfg(feature = "oracle")]
#[test]
fn protected_reason_never_less_strict() {
    use serde_json::Value;
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let scratch = ScratchDir::new("guard-oracle").unwrap();
    let ws = scratch.path().join("ws");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    let parts = [
        ".git",
        ".venv",
        "venv",
        ".rpctl",
        ".viy",
        ".vscode",
        ".idea",
        "rp",
        "viy",
        "pyproject.toml",
        "setup.py",
        "Makefile",
        ".envrc",
        "x.egg-info",
        "src",
        "a.py",
        ".env",
        ".env.example",
        "secrets.json",
        "id_rsa.pub",
        "..",
        ".",
        "PEM.KEY",
        "",
    ];
    let mut extra = 0;
    for case in 0..cases(3000) {
        let n = 1 + g.below(3);
        let rel: Vec<&str> = (0..n).map(|_| *g.pick(&parts)).collect();
        let path = ws.join(rel.join("/"));
        let theirs = oracle
            .call(
                "sandbox.protected_reason",
                json!([
                    path.to_str().unwrap(),
                    ws.to_str().unwrap(),
                    ws.to_str().unwrap()
                ]),
                json!({}),
            )
            .unwrap()
            .outcome;
        let Outcome::Returned(theirs) = theirs else {
            panic!("{path:?}: {theirs:?}");
        };
        let ours = protected_reason(&path, &ws, &ws, &[]);
        let at = format!("case {case}, seed {:#x}: {path:?}", seed());
        match (&ours, theirs) {
            (Some(o), Value::String(t)) if o.contains(".viy") || o.starts_with("viy ") => {
                // viy's own name found first; rpctl's reason is another.
                assert!(!t.is_empty(), "{at}");
            }
            (_, Value::String(t)) => assert_eq!(ours.as_deref(), Some(t.as_str()), "{at}"),
            (Some(o), Value::Null) => {
                assert!(o.contains(".viy") || o.starts_with("viy "), "{at}: {o}");
                extra += 1;
            }
            (None, Value::Null) => {}
            (_, other) => panic!("{at}: {other}"),
        }
    }
    eprintln!("protected_reason: {extra} paths only viy's names protect");
}
