//! `viy migrate`: copies rp's global config and model profiles to viy's,
//! leaving rp's in place (`viy_config::migrate`). rpctl's `cmd_migrate`
//! copied an old in-repo `.env` instead; viy keeps its message shape.

use std::io::Write;

use viy_config::Paths;
use viy_config::migrate;

/// Runs `viy migrate`. `Err` is the message to exit with, status 1.
pub fn cmd_migrate(paths: &Paths, out: &mut dyn Write) -> Result<(), String> {
    let report = migrate::migrate(paths).map_err(|e| e.to_string())?;
    let mut text = String::new();
    if report.copied.is_empty() {
        text += &if report.kept.is_empty() {
            format!(
                "nothing to migrate; no rp config in {}\n",
                paths.rpctl_config_dir().display()
            )
        } else {
            format!(
                "nothing to migrate; viy already has rp's config and profiles in {}\n",
                paths.config_dir().display()
            )
        };
    } else {
        for (src, dst) in &report.copied {
            text += &format!("copied {}\n    -> {}\n", src.display(), dst.display());
        }
        for (src, dst) in &report.kept {
            text += &format!(
                "kept {} (already there; {} not copied)\n",
                dst.display(),
                src.display()
            );
        }
        text += "Originals left in place: rp keeps using them until cutover.\n";
    }
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
