//! posixpath against CPython 3.10.12 through the oracle, on generated trees
//! of directories, files and symlinks (relative, absolute, dangling, loops):
//! realpath, abspath and Path.resolve from several working directories.
//! Zero differences is the gate (S2, F08).

use std::path::Path;
use std::path::PathBuf;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_pycompat::posixpath;
use viy_testkit::fuzz;
use viy_testkit::fuzz::Gen;
use viy_testkit::oracle::Oracle;
use viy_testkit::oracle::Outcome;
use viy_testkit::scratch::ScratchDir;

const TREES: usize = 60;
const QUERIES: usize = 50;
const DIRS: &[&str] = &["d0", "d0/d1", "d2", "d2/d3"];
const FILES: &[&str] = &["f", "d0/f", "d2/d3/g"];
const LINK_DIRS: &[&str] = &["", "d0", "d0/d1", "d2"];
const NAMES: &[&str] = &[
    "d0", "d1", "d2", "d3", "f", "g", "l0", "l1", "l2", "l3", "l4", "l5", "..", ".", "", "missing",
    "x",
];

/// A symlink target, relative to the link's directory or absolute.
fn target(g: &mut Gen, root: &Path) -> String {
    match g.below(4) {
        0 => format!(
            "{}/{}",
            root.display(),
            g.pick(&["d0", "d2/d3", "l1", "missing"])
        ),
        1 => (*g.pick(&["/", "/nonexistent", "..", ".", "../..", "/tmp/.."])).to_string(),
        _ => {
            let n = 1 + g.below(3);
            (0..n).map(|_| *g.pick(NAMES)).collect::<Vec<_>>().join("/")
        }
    }
}

fn build(g: &mut Gen, root: &Path) {
    for d in DIRS {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    for f in FILES {
        std::fs::write(root.join(f), "x").unwrap();
    }
    for i in 0..6 {
        let at = root.join(g.pick(LINK_DIRS)).join(format!("l{i}"));
        let to = target(g, root);
        let _ = std::os::unix::fs::symlink(to, at);
    }
}

fn query(g: &mut Gen, root: &Path) -> String {
    let n = g.below(6);
    let rel = (0..n).map(|_| *g.pick(NAMES)).collect::<Vec<_>>().join("/");
    match g.below(4) {
        0 => format!("{}/{rel}", root.display()),
        1 => format!("/{}/{rel}", root.display()),
        _ => rel,
    }
}

fn python(oracle: &mut Oracle, func: &str, path: &str, cwd: &Path) -> Result<String, String> {
    let args = json!([func, path, cwd.to_str().unwrap()]);
    match oracle
        .call("oracle.path_call", args, json!({}))
        .unwrap()
        .outcome
    {
        Outcome::Returned(v) => Ok(v.as_str().unwrap().to_string()),
        Outcome::Raised { kind, message } => Err(format!("{kind}: {message}")),
    }
}

fn shown(p: PathBuf) -> String {
    p.to_str().unwrap().to_string()
}

#[test]
fn realpath_abspath_resolve_equal_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    let mut loops = 0;
    for t in 0..fuzz::cases(TREES) {
        let dir = ScratchDir::new("posixpath-oracle").unwrap();
        let root = dir.path();
        build(&mut g, root);
        for _ in 0..QUERIES {
            let q = query(&mut g, root);
            // A cwd as getcwd() reports it: the physical directory.
            let cwd =
                std::fs::canonicalize(root.join(g.pick(&["", "d0", "d0/d1", "d2/d3"]))).unwrap();
            let ctx = format!(
                "seed {seed:#x}, tree {t}, path {q:?}, cwd {}",
                cwd.display()
            );
            let qp = Path::new(&q);
            let rust = posixpath::realpath_from(qp, &cwd)
                .map(shown)
                .map_err(|e| e.to_string());
            assert_eq!(
                rust,
                python(&mut oracle, "realpath", &q, &cwd),
                "realpath, {ctx}"
            );
            let rust = Ok(shown(posixpath::abspath_from(qp, &cwd)));
            assert_eq!(
                rust,
                python(&mut oracle, "abspath", &q, &cwd),
                "abspath, {ctx}"
            );
            let rust = posixpath::resolve_from(qp, &cwd)
                .map(shown)
                .map_err(|e| format!("RuntimeError: {e}"));
            let want = python(&mut oracle, "resolve", &q, &cwd);
            loops += usize::from(want.is_err());
            assert_eq!(rust, want, "resolve, {ctx}");
        }
    }
    // Not vacuous: some queries must have run into a symlink loop.
    assert!(loops > 0, "no loops met");
    eprintln!("posixpath: {loops} loops among the resolve queries");
}
