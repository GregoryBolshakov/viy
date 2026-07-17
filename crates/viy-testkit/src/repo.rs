use std::path::PathBuf;

/// The viy repository's root, from this crate's manifest directory.
pub(crate) fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// The full rpctl commit pinned in `tests/rpctl-ref.txt`.
pub(crate) fn pinned_commit() -> Option<String> {
    let pin = std::fs::read_to_string(repo_root().join("tests").join("rpctl-ref.txt")).ok()?;
    // A file viy writes itself, ASCII only: no Python semantics needed.
    pin.split('\n')
        .find_map(|l| l.strip_prefix("commit:"))
        .map(|c| c.chars().filter(char::is_ascii_hexdigit).collect())
}
