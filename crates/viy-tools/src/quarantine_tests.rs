use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

#[test]
fn a_bare_repository_built_file_by_file_is_moved_out() {
    let scratch = ScratchDir::new("quarantine").unwrap();
    let ws = scratch.path().join("ws");
    let repo = ws.join("vendor/evil");
    std::fs::create_dir_all(repo.join("objects")).unwrap();
    std::fs::create_dir_all(repo.join("refs")).unwrap();
    assert!(bare_repo_ancestors(&repo.join("HEAD"), &ws).is_empty());
    std::fs::write(repo.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(repo.join("config"), "[core]\n").unwrap();
    let found = bare_repo_ancestors(&repo.join("config"), &ws);
    assert_eq!(found, std::slice::from_ref(&repo));
    let dest = scratch.path().join("quarantine/20260721-120000");
    let (moved, failed) = quarantine(&found, &ws, &dest);
    assert!(failed.is_none());
    assert_eq!(
        moved,
        [("vendor/evil".to_string(), dest.join("vendor/evil"))]
    );
    assert!(!repo.exists());
    assert!(dest.join("vendor/evil/HEAD").is_file());
    // The workspace itself is never moved.
    assert!(bare_repo_ancestors(&ws.join("x"), &ws).is_empty());
}
