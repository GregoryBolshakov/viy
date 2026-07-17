//! Expected values printed by CPython 3.10.12 on 2026-07-12; the oracle
//! tests compare realpath and resolve on generated symlink trees.

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

fn p(s: &str) -> &Path {
    Path::new(s)
}

#[test]
fn normpath_and_pure_path_match_python() {
    let cases = [
        ("", ".", "."),
        (".", ".", "."),
        ("/", "/", "/"),
        ("//", "//", "//"),
        ("///", "/", "/"),
        ("//a//b/", "//a/b", "//a/b"),
        ("a/./b/../c", "a/c", "a/b/../c"),
        ("../../a", "../../a", "../../a"),
        ("/../a", "/a", "/../a"),
        ("a/..", ".", "a/.."),
        ("a/../..", "..", "a/../.."),
        ("/a/b/../../..", "/", "/a/b/../../.."),
        ("./", ".", "."),
        ("a//b", "a/b", "a/b"),
        ("//a/../b", "//b", "//a/../b"),
    ];
    for (input, norm, pure) in cases {
        assert_eq!(normpath(p(input)), p(norm), "normpath({input:?})");
        assert_eq!(pure_path(p(input)), p(pure), "PurePosixPath({input:?})");
    }
}

#[test]
fn split_and_join_match_python() {
    let cases = [
        ("", "", ""),
        ("/", "/", ""),
        ("a", "", "a"),
        ("a/", "a", ""),
        ("/a/b", "/a", "b"),
        ("//a", "//", "a"),
        ("a//b//", "a//b", ""),
        ("///", "///", ""),
    ];
    for (input, head, tail) in cases {
        assert_eq!(
            split(p(input)),
            (p(head).into(), p(tail).into()),
            "split({input:?})"
        );
    }
    let joins = [
        ("a", "b", "a/b"),
        ("a/", "b", "a/b"),
        ("", "b", "b"),
        ("a", "/b", "/b"),
        ("a", "", "a/"),
        ("/", "..", "/.."),
    ];
    for (a, b, want) in joins {
        assert_eq!(join(p(a), p(b)), p(want), "join({a:?}, {b:?})");
    }
}

#[test]
fn resolve_keeps_the_missing_rest_and_names_loops() {
    let dir = ScratchDir::new("posixpath").unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("d")).unwrap();
    std::os::unix::fs::symlink("d", root.join("to-d")).unwrap();
    std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
    assert_eq!(
        resolve_from(p("to-d/missing/../x"), root).unwrap(),
        root.join("d/x")
    );
    let err = resolve_from(p("loop/x"), root).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("Symlink loop from '{}/loop/x'", root.display())
    );
}
