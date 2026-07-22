//! Where viy's and rp's files live: the home directory and the XDG base
//! directories, read once from the environment as rpctl's `config._xdg` does.
//! Everything else receives a [`Paths`] instead of reading `HOME` or `XDG_*`
//! itself, so a test points all of it at a scratch directory (kb d037).
//! The scopes and the `.viy/` marker on top of these roots come in WP 1.1.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, else `~/.config`.
    pub config_home: PathBuf,
    /// `$XDG_DATA_HOME`, else `~/.local/share`.
    pub data_home: PathBuf,
    /// `$XDG_STATE_HOME`, else `~/.local/state`.
    pub state_home: PathBuf,
}

impl Paths {
    /// From this process's environment.
    pub fn from_env() -> io::Result<Self> {
        // The one place viy asks for the home directory (clippy.toml).
        #[allow(clippy::disallowed_methods)]
        let home = std::env::home_dir().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not determine the home directory",
            )
        })?;
        Ok(Self::from_vars(home, |name| std::env::var_os(name)))
    }

    /// From `home` and the variables `var` returns. An unset or empty
    /// variable falls back to its default under `home`.
    pub fn from_vars(home: PathBuf, var: impl Fn(&str) -> Option<OsString>) -> Self {
        let xdg = |name: &str, default: &str| match var(name) {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => home.join(default),
        };
        Self {
            config_home: xdg("XDG_CONFIG_HOME", ".config"),
            data_home: xdg("XDG_DATA_HOME", ".local/share"),
            state_home: xdg("XDG_STATE_HOME", ".local/state"),
            home,
        }
    }

    /// Everything under `home`, as with no XDG variables set: for tests.
    pub fn under(home: impl Into<PathBuf>) -> Self {
        Self::from_vars(home.into(), |_| None)
    }
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod tests;
