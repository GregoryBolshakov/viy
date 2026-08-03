//! `viy model list|show|use|add`: the model profiles (rpctl's `cmd_model`).
//! The arguments come from the CLI parser (WP 1.6).

use std::io::Write;

use viy_config::Config;
use viy_config::Paths;
use viy_config::config::PROFILE_KEYS;
use viy_config::profiles;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAction {
    List,
    Show,
    Use,
    Add,
}

impl ModelAction {
    fn word(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Show => "show",
            Self::Use => "use",
            Self::Add => "add",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelArgs {
    pub action: ModelAction,
    /// The profile; required for `use` and `add`.
    pub name: Option<String>,
    /// `add`: overwrite an existing profile.
    pub force: bool,
}

/// Runs `viy model` on `cfg`, the loaded config. `Err` is the message to
/// exit with, status 1.
pub fn cmd_model(
    cfg: &Config,
    paths: &Paths,
    args: &ModelArgs,
    out: &mut dyn Write,
) -> Result<(), String> {
    macro_rules! say {
        ($($arg:tt)*) => {
            writeln!(out, $($arg)*).map_err(|e| e.to_string())?
        };
    }
    let name = args.name.as_deref();
    let needs_name = || format!("viy model {} needs a profile name", args.action.word());
    let io = |e: std::io::Error| e.to_string();
    match args.action {
        ModelAction::List => {
            let active = profiles::active_profile(cfg);
            let names = profiles::list_profiles(paths).map_err(io)?;
            if names.is_empty() {
                say!(
                    "no model profiles yet in {}/",
                    paths.profiles_dir().display()
                );
                say!("create one with:  viy model add <name>");
                return Ok(());
            }
            say!("model profiles (* = active):");
            for name in &names {
                let data = profiles::read_profile(paths, name).map_err(io)?;
                let model = ["SERVED_MODEL_NAME", "MODEL_REPO"]
                    .iter()
                    .filter_map(|key| data.get(*key))
                    .find(|value| !value.is_empty())
                    .map_or("?", String::as_str);
                let mark = if Some(name.as_str()) == active {
                    '*'
                } else {
                    ' '
                };
                say!("  {mark} {name}  ({model})");
            }
            if let Some(active) = active.filter(|a| !names.iter().any(|n| n == a)) {
                say!("  ! MODEL_PROFILE={active} is set but has no profile file");
            }
        }
        ModelAction::Show => {
            let Some(name) = name.or_else(|| profiles::active_profile(cfg)) else {
                say!("no active profile; pass a name:  viy model show <name>");
                return Ok(());
            };
            let path = profiles::profile_path(paths, name);
            let data = profiles::read_profile(paths, name).map_err(io)?;
            if data.is_empty() {
                say!("no such profile: {name}  ({})", path.display());
                return Ok(());
            }
            say!("profile {name}  ({}):", path.display());
            for key in PROFILE_KEYS {
                if let Some(value) = data.get(*key) {
                    say!("  {key}={value}");
                }
            }
            for (key, value) in &data {
                if !PROFILE_KEYS.contains(&key.as_str()) {
                    say!("  {key}={value}");
                }
            }
        }
        ModelAction::Use => {
            let name = name.ok_or_else(needs_name)?;
            if !profiles::list_profiles(paths)
                .map_err(io)?
                .iter()
                .any(|n| n == name)
            {
                return Err(format!("no such profile: {name}  (viy model list)"));
            }
            profiles::set_key(paths, "MODEL_PROFILE", name).map_err(io)?;
            say!("active model profile: {name}");
            say!(
                "Model/serving keys are vLLM launch flags, so recreate the pod for the switch to take effect."
            );
            say!(
                "If a pod is running under the current model, terminate it FIRST (viy down), then:  viy model use {name} && viy up"
            );
        }
        ModelAction::Add => {
            let name = name.ok_or_else(needs_name)?;
            let path = profiles::profile_path(paths, name);
            if path.exists() && !args.force {
                return Err(format!(
                    "{} already exists; pass --force to overwrite",
                    path.display()
                ));
            }
            // A new model wants its own volume.
            let seed = PROFILE_KEYS.iter().map(|&key| match key {
                "NETWORK_VOLUME_ID" => (key, ""),
                _ => (key, cfg.get(key).unwrap_or("")),
            });
            let path = profiles::write_profile(paths, name, seed).map_err(io)?;
            say!("wrote {}", path.display());
            say!("Edit it for the new model, then:  viy model use {name}");
            say!(
                "Give it a UNIQUE POD_NAME and NETWORK_VOLUME_NAME so it gets its own pod + weights (tracking keys on POD_NAME)."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
