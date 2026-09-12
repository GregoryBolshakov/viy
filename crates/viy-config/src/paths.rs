//! Where viy's and rp's files live: the home directory and the XDG base
//! directories, read once from the environment as rpctl's `config._xdg` does,
//! and the global scope's directories under them. Everything else receives a
//! [`Paths`] instead of reading `HOME` or `XDG_*` itself, so a test points
//! all of it at a scratch directory (kb d037). The project scope is
//! [`crate::Project`].

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

    /// `~/.config/viy`: the global config file and the model profiles.
    pub fn config_dir(&self) -> PathBuf {
        self.config_home.join(TOOL)
    }

    /// The global `KEY=value` config file.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config")
    }

    /// One `<name>.env` per model profile.
    pub fn profiles_dir(&self) -> PathBuf {
        self.config_dir().join("profiles")
    }

    /// viy's data directory under `XDG_DATA_HOME`: sessions and the global
    /// toolbox.
    pub fn data_dir(&self) -> PathBuf {
        self.data_home.join(TOOL)
    }

    /// Every chat's session log, from every project.
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir().join("sessions")
    }

    /// viy's state directory under `XDG_STATE_HOME`: its own state, apart
    /// from the pod's.
    pub fn state_dir(&self) -> PathBuf {
        self.state_home.join(TOOL)
    }

    /// rp's state directory: the pod state viy shares with rp until cutover
    /// (`pod.json`, `pod.lock`, `last_activity`, `known_hosts`, the boot
    /// records; kb d006).
    pub fn pod_state_dir(&self) -> PathBuf {
        self.state_home.join(RPCTL)
    }

    /// The one pod's state, `pod.json`.
    pub fn pod_state_file(&self) -> PathBuf {
        self.pod_state_dir().join("pod.json")
    }

    /// Held only around each read-modify-write of `pod.json`.
    pub fn pod_state_lock_file(&self) -> PathBuf {
        self.pod_state_dir().join("pod.json.lock")
    }

    /// Held for the whole of up, stop and down, by rp and viy alike.
    pub fn pod_lock_file(&self) -> PathBuf {
        self.pod_state_dir().join("pod.lock")
    }

    /// The pods' SSH host keys, pinned under an alias per volume, apart
    /// from `~/.ssh/known_hosts`.
    pub fn known_hosts_file(&self) -> PathBuf {
        self.pod_state_dir().join("known_hosts")
    }

    /// The background tunnel's stderr, so a forward that dies says why
    /// (shared with rp, kb d049).
    pub fn tunnel_log_file(&self) -> PathBuf {
        self.pod_state_dir().join("tunnel.log")
    }

    /// When this machine last streamed from the pod (`last_activity`):
    /// `viy reap` and rp's terminate a pod whose stamp is stale.
    pub fn activity_file(&self) -> PathBuf {
        self.pod_state_dir().join("last_activity")
    }

    /// One line per refused pod creation, `up_failures.jsonl`.
    pub fn up_failures_file(&self) -> PathBuf {
        self.pod_state_dir().join("up_failures.jsonl")
    }

    /// The backup volumes by id, each with its last use, `backups.json`.
    pub fn backups_file(&self) -> PathBuf {
        self.pod_state_dir().join("backups.json")
    }

    /// One line per A100 stock query, `stock.jsonl`.
    pub fn stock_file(&self) -> PathBuf {
        self.pod_state_dir().join("stock.jsonl")
    }

    /// One line per recorded boot, `boots.jsonl` (`viy boots`).
    pub fn boots_history_file(&self) -> PathBuf {
        self.pod_state_dir().join("boots.jsonl")
    }

    /// Each boot's record and logs, the newest 50 kept.
    pub fn boots_dir(&self) -> PathBuf {
        self.pod_state_dir().join("boots")
    }

    /// `~/.config/rpctl`: rp's global config, which `viy migrate` copies.
    pub fn rpctl_config_dir(&self) -> PathBuf {
        self.config_home.join(RPCTL)
    }

    /// rp's data directory: its sessions and toolbox, which `viy migrate`
    /// copies.
    pub fn rpctl_data_dir(&self) -> PathBuf {
        self.data_home.join(RPCTL)
    }
}

/// The directory name viy's own files go under.
const TOOL: &str = "viy";
/// rp's, for what viy shares with it or copies from it.
const RPCTL: &str = "rpctl";

#[cfg(test)]
#[path = "paths_tests.rs"]
mod tests;
