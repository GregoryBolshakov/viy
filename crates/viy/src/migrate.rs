//! `viy migrate`: copies rp's global config and model profiles to viy's,
//! leaving rp's in place (`viy_config::migrate`); with `--finish`, the
//! cutover from rp ([`crate::finish`], kb d135). rpctl's `cmd_migrate`
//! copied an old in-repo `.env` instead; viy keeps its message shape.

use std::io::Write;
use std::path::Path;

use viy_config::Paths;
use viy_config::migrate;
use viy_exec::SystemExec;
use viy_pod::shutdown::SystemChats;

use crate::finish;

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

/// Runs `viy migrate --finish` on this machine ([`crate::finish`]),
/// printing each line as it happens.
pub fn cmd_finish(
    paths: &Paths,
    cwd: &Path,
    dry_run: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let me = std::env::current_exe().map_err(|e| format!("viy itself: {e}"))?;
    let path = std::env::var_os("PATH");
    let on_path = viy_config::vault::find_executable(path.as_deref(), "viy");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let machine = finish::Machine {
        paths,
        cwd,
        me: &me,
        on_path: on_path.as_deref(),
        chats: &SystemChats,
        exec: &SystemExec,
        now,
    };
    let mut failed = None;
    let mut say = |line: String| {
        if failed.is_none()
            && let Err(e) = writeln!(out, "{line}").and_then(|()| out.flush())
        {
            failed = Some(e.to_string());
        }
    };
    runtime.block_on(finish::run(&machine, dry_run, &mut say))?;
    failed.map_or(Ok(()), Err)
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
