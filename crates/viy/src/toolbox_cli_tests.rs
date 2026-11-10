//! rpctl tests/test_toolbox.py CliTests::test_cli_entry_joins_words, and
//! `viy toolbox`'s other ways in.

use std::os::unix::fs::PermissionsExt;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::cli::Command;

fn run(root: &Path, words: &[&str], answer: &str) -> (String, String, Result<(), Failure>) {
    let mut argv = vec!["toolbox"];
    argv.extend_from_slice(words);
    let cli = crate::cli::parse(argv.iter().map(std::ffi::OsString::from)).unwrap();
    let Command::Toolbox(args) = &cli.command else {
        panic!("not toolbox");
    };
    let paths = Paths::under(root.join("home"));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut input = answer.as_bytes();
    let r = cmd_toolbox(
        args,
        &paths,
        &root.join("proj"),
        false,
        Asking {
            input: &mut input,
            err: &mut err,
        },
        &mut out,
    );
    (
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
        r,
    )
}

fn tree() -> ScratchDir {
    let root = ScratchDir::new("toolbox-cli").unwrap();
    std::fs::create_dir_all(root.path().join("proj").join(".viy")).unwrap();
    let paths = Paths::under(root.path().join("home"));
    let d = paths.data_dir().join("toolbox").join("pdf-tables");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("TOOL.md"),
        "---\nname: pdf-tables\nsummary: Extract tables from a text PDF\nusage: pdf-tables F\n---\n",
    )
    .unwrap();
    std::fs::write(d.join("pdf-tables"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(d.join("pdf-tables"), std::fs::Permissions::from_mode(0o755)).unwrap();
    root
}

#[test]
fn cli_entry_joins_words() {
    let root = tree();
    let (out, _, r) = run(root.path(), &["find", "tables", "pdf"], "");
    assert!(r.is_ok());
    assert!(out.starts_with("  pdf-tables  "), "{out}");
    assert!(out.ends_with("  Extract tables from a text PDF\n"), "{out}");
}

#[test]
fn show_and_check_exit_with_their_codes() {
    let root = tree();
    let (out, _, r) = run(root.path(), &["show", "pdf-tables"], "");
    assert!(r.is_ok());
    assert!(out.contains("usage: pdf-tables F"), "{out}");
    let (out, _, r) = run(root.path(), &["show", "pdf-table"], "");
    assert_eq!(r.err().map(|f| f.code), Some(1));
    assert_eq!(out, "no tool named pdf-table; did you mean pdf-tables?\n");
    let (out, _, r) = run(root.path(), &["check"], "");
    assert!(r.is_ok());
    assert_eq!(out, "1 tool(s) ok, 0 with problems\n");
}

#[test]
fn add_and_rm_need_an_argument_and_rm_asks() {
    let root = tree();
    let (_, _, r) = run(root.path(), &["add"], "");
    assert_eq!(
        r.err().and_then(|f| f.message),
        Some("usage: viy toolbox add <dir> [--global]".into())
    );
    let (_, _, r) = run(root.path(), &["rm"], "");
    assert_eq!(
        r.err().and_then(|f| f.message),
        Some("usage: viy toolbox rm <name> [--global]".into())
    );
    let (out, err, r) = run(root.path(), &["rm", "pdf-tables", "--global"], "n\n");
    assert_eq!(r.err().map(|f| f.code), Some(1));
    assert!(
        err.starts_with("delete ") && err.ends_with("? [y/N] "),
        "{err}"
    );
    assert_eq!(out, "aborted\n");
    let (out, _, r) = run(root.path(), &["rm", "pdf-tables", "--global"], "yes\n");
    assert!(r.is_ok());
    assert!(out.starts_with("deleted "), "{out}");
}
