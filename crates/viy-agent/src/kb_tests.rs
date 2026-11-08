use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

fn day(text: &str) -> NaiveDate {
    parse_day0(text).unwrap()
}

const TODAY: &str = "2026-07-26";

fn opts() -> Options {
    Options::default()
}

/// git in `dir` without the caller's GIT_* variables, as the module runs it.
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args).unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn have_python() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

#[test]
fn every_placeholder_is_filled_and_the_dates_are_right() {
    let o = Options {
        project: Some("Thing".into()),
        context: Some("bets on {today} staying literal".into()),
        day0: Some(day("2026-12-01")),
    };
    let files = files("dir", "QWEN.md", &o, day(TODAY));
    let keys = [
        "{today}",
        "{ttl_90}",
        "{ttl_120}",
        "{ttl_180}",
        "{day0}",
        "{day30}",
        "{day90}",
        "{stop}",
        "{project}",
        "{context}",
        "{guide_file}",
        "{guide_line}",
    ];
    // The checkers are written as they are (their f-strings hold `{today}`).
    for f in files.iter().filter(|f| !f.path.ends_with(".py")) {
        for key in keys {
            // The context is the user's text, filled once and kept.
            let left = f.text.replace("bets on {today} staying literal", "");
            assert!(!left.contains(key), "{} keeps {key}", f.path);
        }
    }
    let get = |p: &str| &files.iter().find(|f| f.path == p).unwrap().text;
    assert!(get("kb/facts/f001-example.md").contains("review_by: 2027-02-06\n"));
    assert!(get("kb/00-index.md").contains("review_by: 2027-03-08\n"));
    assert!(get("kb-system/schema.md").contains("review_by: 2027-05-07\n"));
    let cal = get("kb/strategy/s001-pinned-calendar.md");
    assert!(cal.contains("object: \"day0 2026-12-01; day90 2027-03-01; stop 2027-06-01\""));
    assert!(cal.contains("| Day 30 | 2026-12-31 |"));
    assert!(get("README.md").starts_with("# Thing\n\nbets on {today} staying literal\n"));
}

#[test]
fn the_calendar_only_with_day0_and_the_defaults_without_options() {
    let files = files("my-project", "QWEN.md", &opts(), day(TODAY));
    assert!(!files.iter().any(|f| f.path.contains("calendar")));
    let readme = &files.iter().find(|f| f.path == "README.md").unwrap().text;
    assert!(readme.starts_with(&format!("# my-project\n\n{NO_CONTEXT}\n")));
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "QWEN.md",
            "kb/00-index.md",
            "kb/facts/f001-example.md",
            "kb-system/schema.md",
            "tools/kb_lint.py",
            "tools/kb_facts.py",
            ".githooks/pre-commit",
            ".githooks/kb_lint.py",
            ".githooks/kb_facts.py",
            "README.md",
            ".gitignore",
        ]
    );
}

#[test]
fn the_guide_and_readme_name_no_model() {
    // The guide's text fits any model; only its file name is the model's.
    let files = files("p", "AGENTS.md", &opts(), day(TODAY));
    let models = [
        "qwen", "claude", "gpt", "gemini", "llama", "mistral", "deepseek", "codex",
    ];
    for path in ["AGENTS.md", "README.md"] {
        let text = files
            .iter()
            .find(|f| f.path == path)
            .unwrap()
            .text
            .to_lowercase();
        for m in models {
            assert!(!text.contains(m), "{path} names {m}");
        }
        assert!(
            !text.contains("rp chat") && !text.contains(".rpctl"),
            "{path}"
        );
    }
    let readme = &files.iter().find(|f| f.path == "README.md").unwrap().text;
    assert!(readme.contains(
        "AGENTS.md          standing instructions for the agent (auto-loaded by viy chat)\n"
    ));
    assert!(readme.contains("`viy chat` loads `AGENTS.md` into"));
}

#[test]
fn the_checkers_are_this_repositorys() {
    // The pre-commit hook compares them with tools/ too (the public copy has
    // no tools/kb_*.py, so this test reads them only when they are there).
    let written = files("p", "QWEN.md", &opts(), day(TODAY));
    let get = |p: &str| written.iter().find(|f| f.path == p).unwrap().text.clone();
    assert!(
        get("tools/kb_lint.py") == KB_LINT,
        "kb_lint.py is written as it is"
    );
    assert!(
        get("tools/kb_facts.py") == KB_FACTS,
        "kb_facts.py is written as it is"
    );
    assert!(KB_LINT.contains("(as of {today})"));
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (name, text) in [("kb_lint.py", KB_LINT), ("kb_facts.py", KB_FACTS)] {
        if let Ok(ours) = fs::read_to_string(repo.join("tools").join(name)) {
            assert!(ours == text, "assets/kb/{name} differs from tools/{name}");
        }
    }
}

#[test]
fn the_guide_file_comes_from_the_profile_and_must_be_loaded() {
    let mut cfg = Config::defaults();
    assert_eq!(guide_file(&cfg).unwrap(), "QWEN.md");
    cfg.set("KB_GUIDE_FILE", "AGENTS.md");
    assert_eq!(guide_file(&cfg).unwrap(), "AGENTS.md");
    cfg.set("KB_GUIDE_FILE", "GEMINI.md");
    let e = guide_file(&cfg).unwrap_err();
    assert!(
        e.contains("not in INSTRUCTION_FILES (AGENTS.md,QWEN.md,CLAUDE.md)"),
        "{e}"
    );
    cfg.set("INSTRUCTION_FILES", "AGENTS.md,GEMINI.md");
    assert_eq!(guide_file(&cfg).unwrap(), "GEMINI.md");
    cfg.set("KB_GUIDE_FILE", "docs/GEMINI.md");
    assert!(guide_file(&cfg).unwrap_err().contains("without a folder"));
    cfg.set("KB_GUIDE_FILE", "");
    cfg.set("INSTRUCTION_FILES", "");
    assert_eq!(guide_file(&cfg).unwrap(), "QWEN.md");
}

#[test]
fn kb_arguments_parse_as_a_shell_would() {
    assert_eq!(Options::parse("").unwrap(), opts());
    assert_eq!(
        Options::parse("--project 'New Thing' --context=\"one line\" --day0 2026-12-01").unwrap(),
        Options {
            project: Some("New Thing".into()),
            context: Some("one line".into()),
            day0: Some(day("2026-12-01")),
        }
    );
    assert!(
        Options::parse("--day0 12/01/2026")
            .unwrap_err()
            .contains("ISO YYYY-MM-DD")
    );
    assert!(
        Options::parse("--project")
            .unwrap_err()
            .contains("needs a value")
    );
    assert!(
        Options::parse("--force")
            .unwrap_err()
            .contains("unknown argument")
    );
    assert!(
        Options::parse("--context 'open")
            .unwrap_err()
            .contains("unbalanced")
    );
}

#[test]
fn a_fresh_folder_gets_the_whole_kb_and_it_checks_clean() {
    let dir = ScratchDir::new("kb-fresh").unwrap();
    let root = dir.path().join("new-thing");
    fs::create_dir(&root).unwrap();
    let report = init(&root, &Config::defaults(), &opts(), day(TODAY)).unwrap();
    for f in files("new-thing", "QWEN.md", &opts(), day(TODAY)) {
        let path = root.join(&f.path);
        assert_eq!(fs::read_to_string(&path).unwrap(), f.text, "{}", f.path);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111 != 0, f.executable, "{}", f.path);
    }
    for d in ["kb/strategy", "kb/decisions", ".viy"] {
        assert!(root.join(d).is_dir(), "{d}");
    }
    assert!(report.lines.contains(&"  wrote QWEN.md".to_string()));
    assert!(report.lines.contains(&"  wrote .viy/".to_string()));
    assert!(report.hooked, "{:#?}", report.lines);
    assert_eq!(git_in(&root, &["config", "core.hooksPath"]), ".githooks");
    if have_python() {
        assert_eq!(report.failed, None, "{:#?}", report.lines);
        assert!(
            report.lines.iter().any(|l| l.contains("0 errors")),
            "{:#?}",
            report.lines
        );
    }
}

#[test]
fn an_existing_project_keeps_its_readme_and_gets_gitignore_lines() {
    let dir = ScratchDir::new("kb-existing").unwrap();
    let root = dir.path();
    fs::write(root.join("README.md"), "mine\n").unwrap();
    fs::write(root.join(".gitignore"), "target/\n*.pyc").unwrap();
    fs::create_dir(root.join("tools")).unwrap();
    fs::write(root.join("tools/other.sh"), "x").unwrap();
    let report = init(root, &Config::defaults(), &opts(), day(TODAY)).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("README.md")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        fs::read_to_string(root.join(".gitignore")).unwrap(),
        "target/\n*.pyc\n__pycache__/\n.DS_Store\n"
    );
    assert!(
        report
            .lines
            .contains(&"  kept README.md (there is one)".to_string())
    );
    assert!(root.join("tools/kb_lint.py").is_file() && root.join("tools/other.sh").is_file());
}

#[test]
fn one_existing_kb_file_means_nothing_is_written() {
    for existing in [
        "QWEN.md",
        "kb/notes.md",
        "tools/kb_lint.py",
        ".githooks/pre-commit",
    ] {
        let dir = ScratchDir::new("kb-conflict").unwrap();
        let root = dir.path();
        let path = root.join(existing);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "mine").unwrap();
        let before = fs::read_dir(root).unwrap().count();
        let e = init(root, &Config::defaults(), &opts(), day(TODAY)).unwrap_err();
        assert!(e.contains("viy overwrites nothing. Nothing written"), "{e}");
        let named = existing.split_once("kb/").map_or(existing, |_| "kb/");
        assert!(e.contains(named), "{existing}: {e}");
        assert_eq!(fs::read_dir(root).unwrap().count(), before, "{existing}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "mine");
        assert!(!root.join(".viy").exists());
    }
}

#[test]
fn a_kb_under_another_models_guide_names_the_rename() {
    let dir = ScratchDir::new("kb-model").unwrap();
    let root = dir.path();
    init(root, &Config::defaults(), &opts(), day(TODAY)).unwrap();
    let mut cfg = Config::defaults();
    cfg.set("KB_GUIDE_FILE", "AGENTS.md");
    let e = init(root, &cfg, &opts(), day(TODAY)).unwrap_err();
    assert!(e.contains("has a KB whose guide is QWEN.md"), "{e}");
    assert!(e.contains("git mv QWEN.md AGENTS.md"), "{e}");
    assert!(!root.join("AGENTS.md").exists());
}

#[test]
fn no_write_goes_through_a_link() {
    let dir = ScratchDir::new("kb-link").unwrap();
    let root = dir.path().join("p");
    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join("tools")).unwrap();
    let e = init(&root, &Config::defaults(), &opts(), day(TODAY)).unwrap_err();
    assert!(e.starts_with("tools is a link"), "{e}");
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn inside_another_repository_or_over_its_hooks_no_hook() {
    let dir = ScratchDir::new("kb-nested").unwrap();
    let outer = dir.path();
    git_in(outer, &["init", "-q"]);
    let inner = outer.join("kb-here");
    fs::create_dir(&inner).unwrap();
    let report = init(&inner, &Config::defaults(), &opts(), day(TODAY)).unwrap();
    assert!(!report.hooked);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.starts_with("  inside the git repo at "))
    );
    assert_eq!(git_in(outer, &["config", "core.hooksPath"]), "");

    let own = ScratchDir::new("kb-hooks").unwrap();
    git_in(own.path(), &["init", "-q"]);
    git_in(own.path(), &["config", "core.hooksPath", "hooks"]);
    let report = init(own.path(), &Config::defaults(), &opts(), day(TODAY)).unwrap();
    assert!(!report.hooked);
    assert!(
        report.lines.contains(
            &"  repo already has its own hooks - pre-commit hook NOT installed".to_string()
        )
    );
    assert_eq!(git_in(own.path(), &["config", "core.hooksPath"]), "hooks");
}

#[test]
fn the_hook_runs_its_own_checkers_which_the_model_cannot_write() {
    let dir = ScratchDir::new("kb-hook").unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let report = init(&root, &Config::defaults(), &opts(), day(TODAY)).unwrap();
    assert!(report.hooked);
    // git runs .githooks/ outside the sandbox: write_file refuses it, and the
    // sandbox binds it read-only (both through guard's core.hooksPath).
    let reason =
        viy_tools::guard::protected_reason(&root.join(".githooks/kb_lint.py"), &root, &root, &[]);
    assert!(reason.unwrap().contains("core.hooksPath"));
    if !have_python() {
        return;
    }
    let commit = |msg: &str| {
        git_in(&root, &["add", "-A"]);
        git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                msg,
            ],
        )
        .unwrap()
    };
    let out = commit("KB skeleton");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A checker the model changed in tools/ stops the next commit.
    fs::write(root.join("tools/kb_lint.py"), "print('0 errors')\n").unwrap();
    let out = commit("tampered");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("tools/kb_lint.py differs from .githooks/kb_lint.py")
    );
}
