//! The layered configuration, lowest to highest: DEFAULTS, the global file,
//! the selected model profile, the project's files and the environment
//! (rpctl's `config.load`). A config is an ordered string map; each call site
//! parses the keys it reads (kb d011). A project file may make the approval
//! mode, the sandbox network and ask_claude stricter, never looser.

use std::io;
use std::path::Path;
use std::path::PathBuf;

use indexmap::IndexMap;

use crate::Paths;
use crate::Project;
use crate::defaults::DEFAULTS;

/// The keys a profile clears when it sets them empty: a profile is a whole
/// model bundle, so its blank NETWORK_VOLUME_ID must not leave the new model
/// on the old model's volume.
pub const PROFILE_KEYS: &[&str] = &[
    "BACKEND",
    "MODEL_REPO",
    "SERVED_MODEL_NAME",
    "VLLM_IMAGE",
    "CUDA_VERSIONS",
    "TOOL_CALL_PARSER",
    "MAX_MODEL_LEN",
    "MAX_TOKENS",
    "POD_NAME",
    "NETWORK_VOLUME_NAME",
    "NETWORK_VOLUME_ID",
    "GPU_TYPE_IDS",
    "STARTUP_CACHE",
    "MODEL_CACHE_MODE",
    "SAFETENSORS_LOAD_STRATEGY",
    "WEIGHTS_PREWARM",
    "MM_WARMUP",
    "PREFIX_CACHING",
    "ENDPOINT_URL",
    "ENDPOINT_MODEL",
    "LOCAL_VLLM_PORT",
];

/// The ask_claude keys a project file may only make stricter.
pub const CONSULT_GUARDED: &[&str] = &[
    "CONSULT_CLAUDE_BIN",
    "CONSULT_PERMISSION_MODE",
    "CONSULT_ROOT",
    "CONSULT_ENABLED",
    "CONSULT_APPROVAL",
    "CONSULT_MODE",
];

/// A project file may never set these: the binary, the permission mode and
/// the folder ask_claude runs with.
const CONSULT_PINNED: &[&str] = &[
    "CONSULT_CLAUDE_BIN",
    "CONSULT_PERMISSION_MODE",
    "CONSULT_ROOT",
];

/// A project file may change these only to the stricter value.
const CONSULT_STRICTER: &[(&str, &str)] = &[
    ("CONSULT_ENABLED", "0"),
    ("CONSULT_APPROVAL", "ask"),
    ("CONSULT_MODE", "advise"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    values: IndexMap<String, String>,
}

impl Config {
    /// DEFAULTS alone.
    pub fn defaults() -> Self {
        Self {
            values: DEFAULTS
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Sets `key`; a new key goes last.
    pub fn set(&mut self, key: &str, value: &str) {
        self.values.insert(key.to_string(), value.to_string());
    }

    /// Every key and value, defaults first in their order, then keys the
    /// files added, in the order they first appeared.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Overlays a file's non-empty values; keys in `clearable` apply even
    /// when empty.
    fn apply(&mut self, path: &Path, clearable: &[&str]) -> io::Result<()> {
        for (key, value) in parse_env_file(path)? {
            if !value.is_empty() || clearable.contains(&key.as_str()) {
                self.values.insert(key, value);
            }
        }
        Ok(())
    }

    /// Overlays one project file, then undoes what it loosened. A project
    /// folder can come from anywhere (a cloned repo, or files the model
    /// wrote), so it must not answer the approver's questions, widen the
    /// network or change how ask_claude runs.
    fn apply_project(&mut self, path: &Path) -> io::Result<()> {
        let before = self.clone();
        self.apply(path, &[])?;
        let restore = |cfg: &mut Self, key: &str| match before.get(key) {
            Some(value) => cfg.set(key, value),
            None => {
                cfg.values.shift_remove(key);
            }
        };
        for key in CONSULT_PINNED {
            restore(self, key);
        }
        for (key, stricter) in CONSULT_STRICTER {
            let now = self.get(key);
            if now != before.get(key) && now != Some(stricter) {
                restore(self, key);
            }
        }
        let mode = self.get("CHAT_MODE");
        if mode != before.get("CHAT_MODE") && mode != Some("normal") {
            restore(self, "CHAT_MODE");
        }
        if sandbox_net_is_host(self.get("SANDBOX_NET")) {
            restore(self, "SANDBOX_NET");
        }
        restore(self, "SANDBOX_NET_ALLOW");
        Ok(())
    }
}

/// The files the layers come from. [`Layers::new`] gives viy's; the parity
/// check (`tools/parity/config_diff.py`) points them at rp's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layers {
    pub global: PathBuf,
    pub profiles_dir: PathBuf,
    /// In layering order, each guarded on its own (kb d039).
    pub project: Vec<PathBuf>,
}

impl Layers {
    pub fn new(paths: &Paths, project: &Project) -> Self {
        Self {
            global: paths.config_file(),
            profiles_dir: paths.profiles_dir(),
            project: project.config_files().to_vec(),
        }
    }

    /// Merges the layers. `env` is the environment, which wins over every
    /// file for the keys they know, when set and not empty.
    pub fn load(&self, env: impl Fn(&str) -> Option<String>) -> io::Result<Config> {
        let mut cfg = Config::defaults();
        cfg.apply(&self.global, &[])?;
        // The environment may pick the profile, a project may not: the
        // profile sits below the project's files. cfg keeps the one layered
        // here, so persist writes to the file this load read.
        let profile = env("MODEL_PROFILE")
            .filter(|p| !p.is_empty())
            .or_else(|| cfg.get("MODEL_PROFILE").map(str::to_string))
            .unwrap_or_default();
        if !profile.is_empty() {
            let path = self.profiles_dir.join(format!("{profile}.env"));
            cfg.apply(&path, PROFILE_KEYS)?;
        }
        for path in &self.project {
            cfg.apply_project(path)?;
        }
        for (key, value) in cfg.values.iter_mut() {
            if let Some(set) = env(key).filter(|v| !v.is_empty()) {
                *value = set;
            }
        }
        cfg.set("MODEL_PROFILE", &profile);
        Ok(cfg)
    }
}

/// Loads viy's config for `project`, with this process's environment on top.
pub fn load(paths: &Paths, project: &Project) -> io::Result<Config> {
    Layers::new(paths, project).load(|key| std::env::var(key).ok())
}

/// A `KEY=value` file: blank lines, `#` comments and lines without `=` are
/// skipped; key and value are trimmed and the value loses its surrounding
/// quotes. A later line wins. A missing file is empty.
pub fn parse_env_file(path: &Path) -> io::Result<IndexMap<String, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(IndexMap::new()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    let mut out = IndexMap::new();
    for (key, value) in text.lines().filter_map(assignment) {
        let value = value.trim().trim_matches('"').trim_matches('\'');
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

/// The trimmed key and the raw value a line of a `KEY=value` file sets, as
/// [`parse_env_file`] reads it; the writer matches lines with it too.
pub(crate) fn assignment(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    Some((key.trim(), value))
}

/// Whether SANDBOX_NET asks for the host's network. The project guard and
/// the chat's sandbox use this one test, so a value the guard lets through
/// never means the host network.
pub fn sandbox_net_is_host(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("host"))
}

/// Whether new pods are created tunnel-only. Whether an existing pod is
/// reachable that way depends on how it was created, which the pod module
/// answers.
pub fn tunnel_enabled(cfg: &Config) -> bool {
    !matches!(
        cfg.get("TUNNEL").unwrap_or("").trim(),
        "" | "0" | "false" | "no"
    )
}

/// The local end of the tunnel: TUNNEL_LOCAL_PORT, else VLLM_PORT.
pub fn tunnel_local_port(cfg: &Config) -> &str {
    match cfg.get("TUNNEL_LOCAL_PORT").unwrap_or("").trim() {
        "" => cfg.get("VLLM_PORT").unwrap_or(""),
        port => port,
    }
}

/// GPU_TYPE_IDS as a list, in its order.
pub fn gpu_type_list(cfg: &Config) -> Vec<&str> {
    cfg.get("GPU_TYPE_IDS")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .collect()
}

/// RUNPOD_API_KEY, or the message to exit with: where to put it and where
/// to get one.
pub fn require_api_key<'a>(cfg: &'a Config, paths: &Paths) -> Result<&'a str, String> {
    match cfg.get("RUNPOD_API_KEY") {
        Some(key) if !key.is_empty() => Ok(key),
        _ => Err(format!(
            "RUNPOD_API_KEY is not set.\n  Add it to {}\n  Get one at https://console.runpod.io/user/settings -> API Keys",
            paths.config_file().display()
        )),
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
