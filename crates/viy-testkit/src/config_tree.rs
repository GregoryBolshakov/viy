//! A case from the `config/model` and `config/persist` fixtures laid out at
//! viy's paths in a scratch directory T: the global file and the profiles
//! under `T/config/viy/`, the project at `T/project` with its marker, HOME
//! at `T/home`. The fixtures recorded rpctl in the same layout under
//! `T/config/rpctl/` and `.rpctl/`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

use crate::scratch::ScratchDir;

#[derive(Debug)]
pub struct ConfigTree {
    scratch: ScratchDir,
}

impl ConfigTree {
    /// Lays out `case`'s `global_config`, `profiles`, `profiles_dir`,
    /// `other_files` and `project_config`, the project marked with `marker`.
    pub fn new(case: &Value, marker: &str) -> io::Result<Self> {
        let tree = Self {
            scratch: ScratchDir::new("config-tree")?,
        };
        fs::create_dir(tree.home())?;
        fs::create_dir_all(tree.project().join(marker))?;
        let dir = tree.config_dir();
        let mut files = Vec::new();
        if let Some(text) = case["global_config"].as_str() {
            files.push(("config".to_string(), text));
        }
        for (key, prefix, suffix) in [("profiles", "profiles/", ".env"), ("other_files", "", "")] {
            for (name, text) in case[key].as_object().into_iter().flatten() {
                files.push((
                    format!("{prefix}{name}{suffix}"),
                    text.as_str().unwrap_or(""),
                ));
            }
        }
        for (rel, text) in files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, text)?;
        }
        if case["profiles_dir"].as_bool() == Some(true) {
            fs::create_dir_all(dir.join("profiles"))?;
        }
        if let Some(text) = case["project_config"].as_str() {
            fs::write(tree.project().join(marker).join("config"), text)?;
        }
        Ok(tree)
    }

    /// T.
    pub fn root(&self) -> &Path {
        self.scratch.path()
    }

    pub fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    /// What XDG_CONFIG_HOME points at.
    pub fn config_home(&self) -> PathBuf {
        self.root().join("config")
    }

    pub fn config_dir(&self) -> PathBuf {
        self.config_home().join("viy")
    }

    pub fn project(&self) -> PathBuf {
        self.root().join("project")
    }

    /// Every file under [`Self::config_dir`], by path relative to it.
    pub fn files(&self) -> io::Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        let dir = self.config_dir();
        let mut pending = vec![dir.clone()];
        while let Some(next) = pending.pop() {
            let entries = match fs::read_dir(&next) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            for entry in entries {
                let path = entry?.path();
                if path.is_dir() {
                    pending.push(path);
                } else if let Ok(rel) = path.strip_prefix(&dir) {
                    out.insert(
                        rel.to_string_lossy().into_owned(),
                        fs::read_to_string(&path)?,
                    );
                }
            }
        }
        Ok(out)
    }
}

/// A case's `env` object.
pub fn case_env(case: &Value) -> HashMap<String, String> {
    case["env"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
        .collect()
}

/// A case's `files` object, as [`ConfigTree::files`] returns it.
pub fn case_files(case: &Value) -> BTreeMap<String, String> {
    case["files"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
        .collect()
}
