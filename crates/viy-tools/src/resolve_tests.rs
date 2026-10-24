use std::os::unix::fs::symlink;
use std::path::Path;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::TooManyLinks;
use super::resolve;
use super::resolve_checked;

#[test]
fn lexical_parts_and_missing_rest() {
    assert_eq!(resolve(Path::new("/a/./b/../c")), Path::new("/a/c"));
    assert_eq!(resolve(Path::new("/..")), Path::new("/"));
    assert_eq!(resolve(Path::new("//x///y/")), Path::new("/x/y"));
}

#[test]
fn links_are_followed_before_dot_dot() {
    let scratch = ScratchDir::new("resolve").unwrap();
    let d = scratch.path();
    let real = d.join("real");
    std::fs::create_dir_all(real.join("sub")).unwrap();
    symlink(real.join("sub"), d.join("link")).unwrap();
    symlink("real", d.join("rel")).unwrap();
    assert_eq!(resolve(&d.join("link")), real.join("sub"));
    // link/.. is real, not d: the link is resolved first.
    assert_eq!(resolve(&d.join("link/..")), real);
    assert_eq!(
        resolve(&d.join("rel/sub/missing/x")),
        real.join("sub/missing/x")
    );
    // A loop stops after the limit instead of spinning; the checked form
    // refuses it.
    symlink(d.join("loop"), d.join("loop")).unwrap();
    assert!(resolve(&d.join("loop")).ends_with("loop"));
    assert_eq!(resolve_checked(&d.join("loop")), Err(TooManyLinks));
    assert_eq!(resolve_checked(&d.join("link")), Ok(real.join("sub")));
}

/// A chain longer than the limit is refused, not stopped on a link (the
/// review found a 41-link chain reaching a key outside the workspace).
#[test]
fn a_long_chain_is_refused() {
    let scratch = ScratchDir::new("resolve-chain").unwrap();
    let d = scratch.path();
    std::fs::write(d.join("end"), "x").unwrap();
    symlink(d.join("end"), d.join("l40")).unwrap();
    for i in (0..40).rev() {
        symlink(d.join(format!("l{}", i + 1)), d.join(format!("l{i}"))).unwrap();
    }
    assert_eq!(resolve_checked(&d.join("l1")), Ok(d.join("end")));
    assert_eq!(resolve_checked(&d.join("l0")), Err(TooManyLinks));
}
