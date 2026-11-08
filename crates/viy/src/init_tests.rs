use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use pretty_assertions::assert_eq;
use viy_testkit::fixtures;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// rp's names as viy's: the marker and the config directory.
fn as_viy(text: &str) -> String {
    text.replace(".rpctl", ".viy")
        .replace("$T/config/rpctl/", "$T/config/viy/")
}

/// rp's transcripts folder, which viy does not make (kb d082).
fn is_rps_transcripts(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    parts.windows(2).any(|w| w == [".rpctl", "transcripts"])
}

/// rp's `.gitignore` without its transcripts line (kb d082).
fn without_transcripts_line(text: &str) -> String {
    text.replace("transcripts/\n", "")
}

fn paths(root: &Path) -> Paths {
    let config_home = root.join("config");
    Paths::from_vars(root.join("home"), move |name| {
        (name == "XDG_CONFIG_HOME").then(|| config_home.clone().into_os_string())
    })
}

/// Every path under `dir`, directories with a trailing `/`, and the files'
/// text, as the fixture lists them.
fn tree(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.insert(format!("{rel}/"), String::new());
                stack.push(path);
            } else {
                out.insert(rel, fs::read_to_string(&path).unwrap());
            }
        }
    }
    out
}

#[test]
fn viy_init_does_what_rp_init_does() {
    let fixture = fixtures::load("config/init").unwrap();
    assert_eq!(fixture.cases.len(), 7);
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let scratch = ScratchDir::new("init").unwrap();
        let root = scratch.path();
        let paths = paths(root);
        if let Some(text) = case["global_config"].as_str() {
            fs::create_dir_all(paths.config_dir()).unwrap();
            fs::write(paths.config_file(), text).unwrap();
        }
        let project = root.join("project");
        fs::create_dir(&project).unwrap();
        for (rel, text) in case["project_files"].as_object().unwrap() {
            let path = project.join(as_viy(rel));
            if rel.ends_with('/') {
                fs::create_dir_all(path).unwrap();
            } else {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, text.as_str().unwrap()).unwrap();
            }
        }
        let mut out = Vec::new();
        let result = cmd_init(
            &paths,
            &project.join(case["cwd"].as_str().unwrap()),
            &mut out,
        );
        let before = case["project_files"].as_object().unwrap();
        let expected_files: BTreeMap<String, String> = case["project_files_after"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(k, _)| before.contains_key(*k) || !is_rps_transcripts(k))
            .map(|(k, v)| {
                let text = v.as_str().unwrap();
                let text = if before.contains_key(k) {
                    text.to_string()
                } else {
                    without_transcripts_line(text)
                };
                (as_viy(k), text)
            })
            .collect();
        assert_eq!(tree(&project), expected_files, "{name}");
        if case["exit"] == 0 {
            assert_eq!(result, Ok(()), "{name}");
            let expected = as_viy(case["stdout"].as_str().unwrap())
                .replace("$T", &root.to_string_lossy())
                .replace(
                    "this project's transcripts and history",
                    "this project's history",
                );
            assert_eq!(String::from_utf8(out).unwrap(), expected, "{name}");
        } else {
            // rp fails with a Python traceback; viy with the path and the
            // OS error, nothing written.
            let err = result.unwrap_err();
            assert!(err.contains("Not a directory"), "{name}: {err}");
            assert_eq!(out, b"", "{name}");
        }
    }
}

/// Not in rp: a folder rp marked gets viy's own marker beside it, and
/// rp's is left as it was (kb d039).
#[test]
fn an_rp_project_gets_viys_marker_and_rps_is_untouched() {
    let scratch = ScratchDir::new("init").unwrap();
    let paths = paths(scratch.path());
    let project = scratch.path().join("project");
    fs::create_dir_all(project.join(".rpctl")).unwrap();
    fs::write(project.join(".rpctl").join("config"), "CHAT_MODE=normal\n").unwrap();
    let mut out = Vec::new();
    cmd_init(&paths, &project, &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.starts_with("initialised project at "), "{out}");
    let files = tree(&project);
    assert_eq!(files[".rpctl/config"], "CHAT_MODE=normal\n");
    assert_eq!(
        files.keys().filter(|k| k.starts_with(".rpctl")).count(),
        2,
        "{files:?}"
    );
    assert_eq!(files[".viy/.gitignore"], "history\n");
}

/// Not in rp: without viy's config but with rp's, the hint is migrate.
#[test]
fn the_hint_names_migrate_when_rp_has_a_config() {
    let scratch = ScratchDir::new("init").unwrap();
    let paths = paths(scratch.path());
    fs::create_dir_all(paths.rpctl_config_dir()).unwrap();
    fs::write(paths.rpctl_config_dir().join("config"), "POD_NAME=p\n").unwrap();
    let project = scratch.path().join("project");
    fs::create_dir(&project).unwrap();
    let mut out = Vec::new();
    cmd_init(&paths, &project, &mut out).unwrap();
    let last = String::from_utf8(out)
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .to_string();
    assert_eq!(
        last,
        format!(
            "  next: viy migrate copies rp's config and profiles to {}",
            paths.config_dir().display()
        )
    );
}

// `viy init --kb` (kb d112): the marker, then the knowledge base and the
// script's "Next:"; a second run writes nothing and fails.
#[test]
fn viy_init_kb_writes_the_knowledge_base() {
    let scratch = ScratchDir::new("init-kb").unwrap();
    let dir = scratch.path().join("new-thing");
    fs::create_dir(&dir).unwrap();
    let cli = crate::cli::parse(
        [
            "init",
            "--kb",
            "--context",
            "Bets on X.",
            "--day0",
            "2026-07-28",
        ]
        .map(Into::into),
    )
    .unwrap();
    let crate::cli::Command::Init(args) = &cli.command else {
        panic!("{cli:?}")
    };
    let mut out = Vec::new();
    cmd_init(&paths(scratch.path()), &dir, &mut out).unwrap();
    let cfg = Config::defaults();
    let done = cmd_init_kb(args, &cfg, &dir, &mut out);
    let text = String::from_utf8(out).unwrap();
    if text.contains("python3 not found") {
        return;
    }
    done.unwrap();
    assert!(
        text.contains("  wrote QWEN.md\n  wrote kb/00-index.md\n"),
        "{text}"
    );
    assert!(
        text.contains("  wrote kb/strategy/s001-pinned-calendar.md\n"),
        "{text}"
    );
    assert!(text.ends_with(
        "Next:\n  git add -A && git commit -m \"KB skeleton\"\n  viy chat\n\nThen open kb/00-index.md, write the project premise into QWEN.md, and delete\nkb/facts/f001-example.md once you have a real fact.\n"
    ), "{text}");
    assert!(
        fs::read_to_string(dir.join("README.md"))
            .unwrap()
            .starts_with("# new-thing\n\nBets on X.\n")
    );
    let e = cmd_init_kb(args, &cfg, &dir, &mut Vec::new()).unwrap_err();
    assert!(e.contains("viy overwrites nothing"), "{e}");
}

#[test]
fn the_kb_flags_need_kb_and_an_iso_day0() {
    let parse = |args: &[&str]| crate::cli::parse(args.iter().map(Into::into));
    for args in [
        &["init", "--project", "X"][..],
        &["init", "--context", "X"],
        &["init", "--day0", "2026-07-28"],
    ] {
        let e = parse(args).unwrap_err().to_string();
        assert!(e.contains("--kb"), "{args:?}: {e}");
    }
    let e = parse(&["init", "--kb", "--day0", "11/10"])
        .unwrap_err()
        .to_string();
    assert!(e.contains("--day0 must be ISO YYYY-MM-DD"), "{e}");
    let cli = parse(&["init"]).unwrap();
    assert!(matches!(cli.command, crate::cli::Command::Init(ref a) if !a.kb));
}
