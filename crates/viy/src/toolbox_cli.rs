//! `viy toolbox`: the command-line tools the model keeps between chats
//! (rp's `cmd_toolbox`; the toolbox itself is `viy_agent::toolbox`).

use std::io::BufRead;
use std::io::Write;
use std::path::Path;

use viy_agent::toolbox;
use viy_agent::toolbox::Dirs;
use viy_agent::toolbox::Line;
use viy_agent::toolbox::Scope;
use viy_config::Paths;
use viy_config::Project;
use viy_protocol::NoticeLevel;

use crate::Failure;
use crate::cli::ToolboxAction;
use crate::cli::ToolboxArgs;

/// How `viy toolbox rm` asks before deleting.
pub struct Asking<'a> {
    pub input: &'a mut dyn BufRead,
    pub err: &'a mut dyn Write,
}

/// Runs `viy toolbox` in `cwd`'s project; `colour` when stdout is a
/// terminal.
pub fn cmd_toolbox(
    args: &ToolboxArgs,
    paths: &Paths,
    cwd: &Path,
    colour: bool,
    asking: Asking<'_>,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let project = Project::find(cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;
    let dirs = Dirs::new(paths, &project);
    let arg = args.arg.join(" ");
    let scope = if args.global {
        Scope::Global
    } else {
        Scope::Project
    };
    let (lines, code) = match args.action {
        ToolboxAction::List => (toolbox::cmd_list(&dirs), 0),
        ToolboxAction::Show => toolbox::cmd_show(&dirs, &arg),
        ToolboxAction::Find => (toolbox::cmd_find(&dirs, &arg), 0),
        ToolboxAction::Check => toolbox::cmd_check(&dirs),
        ToolboxAction::Path => (toolbox::cmd_path(&dirs), 0),
        ToolboxAction::Add if arg.is_empty() => {
            return Err("usage: viy toolbox add <dir> [--global]".into());
        }
        ToolboxAction::Add => {
            let src = match arg.strip_prefix('~') {
                Some("") => paths.home.clone(),
                Some(rest) if rest.starts_with('/') => paths.home.join(&rest[1..]),
                _ => cwd.join(&arg),
            };
            toolbox::cmd_add(&dirs, &src, scope, project.has_marker())
        }
        ToolboxAction::Rm if arg.is_empty() => {
            return Err("usage: viy toolbox rm <name> [--global]".into());
        }
        ToolboxAction::Rm => {
            let mut confirm = |question: &str| {
                let _ = asking.err.write_all(question.as_bytes());
                let _ = asking.err.flush();
                let mut answer = String::new();
                let _ = asking.input.read_line(&mut answer);
                matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
            };
            toolbox::cmd_rm(&dirs, &arg, scope, &mut confirm)
        }
    };
    say(out, &lines, colour).map_err(|e| Failure::from(e.to_string()))?;
    if code == 0 {
        Ok(())
    } else {
        Err(Failure::silent(code))
    }
}

/// The lines, in rp's colours on a terminal: dim, yellow, red.
fn say(out: &mut dyn Write, lines: &[Line], colour: bool) -> std::io::Result<()> {
    for l in lines {
        if colour {
            let c = match l.level {
                NoticeLevel::Info => "\x1b[2m",
                NoticeLevel::Warn => "\x1b[33m",
                NoticeLevel::Error => "\x1b[31m",
            };
            writeln!(out, "{c}{}\x1b[0m", l.text)?;
        } else {
            writeln!(out, "{}", l.text)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "toolbox_cli_tests.rs"]
mod tests;
