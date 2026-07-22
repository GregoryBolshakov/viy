//! The temp-XDG guard (docs/implementation-plan.md 3.6, kb d035).
//! `.cargo/config.toml` points `HOME` and the XDG base directories into
//! `target/test-home` for every process cargo starts, and viy code finds its
//! directories only through `viy_config::Paths`, which reads exactly those
//! variables. [`assert_isolated`] checks that the redirection is in force and
//! that nothing points at the real user's config, data or state.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;

use crate::repo::repo_root;

/// The variables the guard redirects.
pub const VARS: [&str; 5] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];

/// Where the redirected variables point.
pub fn test_home() -> PathBuf {
    let root = std::fs::canonicalize(repo_root()).unwrap_or_else(|_| repo_root());
    root.join("target").join("test-home")
}

/// Panics unless every variable in [`VARS`] is set inside [`test_home`] and
/// none is, or lies under, the real user's home, `~/.config`,
/// `~/.local/share` or `~/.local/state`.
pub fn assert_isolated() {
    let fake = test_home();
    let real = real_home();
    let forbidden: Vec<PathBuf> = real
        .iter()
        .flat_map(|h| [".config", ".local/share", ".local/state"].map(|d| h.join(d)))
        .collect();
    for name in VARS {
        let Some(value) = std::env::var_os(name) else {
            panic!("{name} is unset: run the tests through cargo (.cargo/config.toml sets it)");
        };
        let path = Path::new(&value);
        assert!(
            path.starts_with(&fake),
            "{name}={path:?} is outside {fake:?}: .cargo/config.toml's [env] did not apply"
        );
        assert!(
            real.as_deref() != Some(path) && !forbidden.iter().any(|f| path.starts_with(f)),
            "{name}={path:?} reaches the real user's files"
        );
    }
}

/// The home directory /etc/passwd gives this process's user, whatever `HOME`
/// says; `None` if the user is not in /etc/passwd.
pub fn real_home() -> Option<PathBuf> {
    let uid = std::fs::metadata("/proc/self").ok()?.uid().to_string();
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() == 7 && fields[2] == uid).then(|| PathBuf::from(fields[5]))
    })
}

#[cfg(test)]
#[path = "home_tests.rs"]
mod tests;
