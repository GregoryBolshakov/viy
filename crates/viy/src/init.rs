//! `viy init`: marks the working directory as a project (rpctl's
//! `cmd_init`).

use std::io::Write;
use std::path::Path;

use viy_agent::kb;
use viy_config::Config;
use viy_config::Paths;
use viy_config::Project;
use viy_config::project::MARKER;

use crate::cli::InitArgs;

/// Runs `viy init` in `cwd`. `Err` is the message to exit with, status 1.
pub fn cmd_init(paths: &Paths, cwd: &Path, out: &mut dyn Write) -> Result<(), String> {
    let (project, created) = Project::init(cwd).map_err(|e| e.to_string())?;
    let root = project.root().display();
    let mut text = format!(
        "{} project at {root}\n  {MARKER}/ holds this project's history\n  the agent's file + shell tools operate on {root}\n",
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

/// `viy init --kb`: the knowledge base after the marker ([`viy_agent::kb`],
/// kb d112), and the script's "Next:".
pub fn cmd_init_kb(
    args: &InitArgs,
    cfg: &Config,
    cwd: &Path,
    out: &mut dyn Write,
) -> Result<(), String> {
    let opts = kb::Options {
        project: args.project.clone(),
        context: args.context.clone(),
        day0: args.day0,
    };
    let report = kb::init(cwd, cfg, &opts, chrono::Local::now().date_naive())?;
    let mut text = String::new();
    for line in &report.lines {
        text += line;
        text.push('\n');
    }
    if let Some(why) = report.failed {
        out.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        return Err(why);
    }
    let commit = if report.hooked {
        "git add -A && git commit -m \"KB skeleton\""
    } else {
        "git add -A && git commit -m \"KB skeleton\"   (in the repository that holds it)"
    };
    text += &format!(
        "\nNext:\n  {commit}\n  viy chat\n\nThen open kb/00-index.md, write the project premise into {}, and delete\nkb/facts/f001-example.md once you have a real fact.\n",
        report.guide
    );
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "init_tests.rs"]
mod tests;
