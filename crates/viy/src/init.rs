//! `viy init`: marks the working directory as a project (rpctl's
//! `cmd_init`).

use std::io::Write;
use std::path::Path;

use viy_config::Paths;
use viy_config::Project;
use viy_config::project::MARKER;

/// Runs `viy init` in `cwd`. `Err` is the message to exit with, status 1.
pub fn cmd_init(paths: &Paths, cwd: &Path, out: &mut dyn Write) -> Result<(), String> {
    let (project, created) = Project::init(cwd).map_err(|e| e.to_string())?;
    let root = project.root().display();
    let mut text = format!(
        "{} project at {root}\n  {MARKER}/ holds this project's transcripts and history\n  the agent's file + shell tools operate on {root}\n",
        if created { "initialised" } else { "already a" }
    );
    if !paths.config_file().exists() {
        // rp's hint was for a config it would still read; viy reads only
        // its own, which `viy migrate` fills from rp's.
        if paths.rpctl_config_dir().join("config").exists() {
            text += &format!(
                "  next: viy migrate copies rp's config and profiles to {}\n",
                paths.config_dir().display()
            );
        } else {
            text += &format!(
                "  next: put RUNPOD_API_KEY in {}\n",
                paths.config_file().display()
            );
        }
    }
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "init_tests.rs"]
mod tests;
