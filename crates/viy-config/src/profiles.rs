//! Model profiles and writing config keys back (rpctl's `config.py` from
//! `_write_kv` to `persist`, and `ensure_vllm_api_key`). A profile bundles the model and serving keys
//! for one model at `~/.config/viy/profiles/<name>.env`; MODEL_PROFILE in
//! the environment or else the global file selects it, and
//! [`crate::Layers`] layers it between the global and the project files.
//!
//! Every write replaces the file atomically, through a symlink to its
//! target, as Codex's `write_atomically` does, and leaves it readable by its
//! owner only (kb d040).

use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use indexmap::IndexMap;
use rand::TryRngCore;
use rand::rngs::OsRng;

use crate::Config;
use crate::Paths;
use crate::config::assignment;
use crate::config::parse_env_file;

/// `profiles/<name>.env`.
pub fn profile_path(paths: &Paths, name: &str) -> PathBuf {
    paths.profiles_dir().join(format!("{name}.env"))
}

/// The profile MODEL_PROFILE names in `cfg`, trimmed, if any.
pub fn active_profile(cfg: &Config) -> Option<&str> {
    cfg.get("MODEL_PROFILE")
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// The names of the `*.env` files in the profiles directory, sorted.
pub fn list_profiles(paths: &Paths) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(paths.profiles_dir()) {
        Ok(entries) => entries,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e),
    };
    let mut names = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "env")
            && let Some(name) = path.file_stem().and_then(|stem| stem.to_str())
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// A profile's keys in file order; empty when it has no file.
pub fn read_profile(paths: &Paths, name: &str) -> io::Result<IndexMap<String, String>> {
    parse_env_file(&profile_path(paths, name))
}

/// Writes profile `name` from `mapping`, one `KEY=value` line each, over any
/// file already there. Returns its path.
pub fn write_profile<'a>(
    paths: &Paths,
    name: &str,
    mapping: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> io::Result<PathBuf> {
    let path = profile_path(paths, name);
    let text: String = mapping
        .into_iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect();
    write_atomically(&path, &text)?;
    Ok(path)
}

/// Sets `key` in the global config file.
pub fn set_key(paths: &Paths, key: &str, value: &str) -> io::Result<()> {
    write_kv(&paths.config_file(), key, value)
}

/// Remembers a value found at run time, such as NETWORK_VOLUME_ID: in the
/// active profile when there is one, so models don't overwrite each other's
/// settings, else in the global file. Sets it in `cfg` as well.
pub fn persist(cfg: &mut Config, paths: &Paths, key: &str, value: &str) -> io::Result<()> {
    let path = match active_profile(cfg) {
        Some(name) => profile_path(paths, name),
        None => paths.config_file(),
    };
    write_kv(&path, key, value)?;
    cfg.set(key, value);
    Ok(())
}

/// The endpoint's bearer token, VLLM_API_KEY: viy's own, never rp's (kb
/// d048), made once and kept in viy's global config file. `sk-` and 32
/// random bytes in unpadded URL-safe base64, the shape of rpctl's
/// `secrets.token_urlsafe(32)`, from the system's random source as Codex
/// mints its secrets.
pub fn ensure_vllm_api_key(cfg: &mut Config, paths: &Paths) -> io::Result<String> {
    if let Some(key) = cfg.get("VLLM_API_KEY").filter(|k| !k.is_empty()) {
        return Ok(key.to_string());
    }
    let mut bytes = [0_u8; 32];
    OsRng.try_fill_bytes(&mut bytes).map_err(io::Error::other)?;
    let key = format!("sk-{}", URL_SAFE_NO_PAD.encode(bytes));
    set_key(paths, "VLLM_API_KEY", &key)?;
    cfg.set("VLLM_API_KEY", &key);
    Ok(key)
}

/// Sets `key=value` in a `KEY=value` file, keeping its comments and order.
/// The first line that sets the key, as [`parse_env_file`] reads it,
/// becomes `KEY=value` and later ones go, since the last would win on load;
/// with none, the first `#KEY=` line is uncommented, else the line goes last.
fn write_kv(path: &Path, key: &str, value: &str) -> io::Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    let commented = format!("#{key}=");
    let sets = |raw: &str| assignment(raw).is_some_and(|(k, _)| k == key);
    let lines: Vec<&str> = text.lines().collect();
    let target = lines.iter().position(|raw| sets(raw)).or_else(|| {
        lines
            .iter()
            .position(|raw| raw.trim().starts_with(&commented))
    });
    let line = format!("{key}={value}");
    let mut out = String::new();
    for (i, raw) in lines.iter().enumerate() {
        if Some(i) == target {
            out.push_str(&line);
        } else if sets(raw) {
            continue;
        } else {
            out.push_str(raw);
        }
        out.push('\n');
    }
    if target.is_none() {
        out.push_str(&line);
        out.push('\n');
    }
    write_atomically(path, &out)
}

/// Replaces `path` with `contents` in one step: written to a new file beside
/// the target, mode 0600, then renamed over it. A symlink is followed to the
/// file it points to, which is replaced, so the link stays.
fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    let target = symlink_target(path);
    let with_path = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", target.display()));
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", target.display()),
        )
    })?;
    fs::create_dir_all(parent).map_err(with_path)?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{name}.tmp.{}", std::process::id()));
    // Left behind by an earlier run with the same process ID.
    let _ = fs::remove_file(&tmp);
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .and_then(|()| fs::rename(&tmp, &target));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.map_err(with_path)
}

/// Where a chain of symlinks starting at `path` ends; `path` itself when it
/// is no symlink or the chain loops.
fn symlink_target(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    let mut seen = HashSet::new();
    while let Ok(link) = fs::read_link(&current) {
        if !seen.insert(current.clone()) {
            return path.to_path_buf();
        }
        current = match current.parent() {
            Some(dir) => dir.join(link),
            None => link,
        };
    }
    current
}

#[cfg(test)]
#[path = "profiles_tests.rs"]
mod tests;
