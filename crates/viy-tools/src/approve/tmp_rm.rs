//! Deletes in the sandbox's `/tmp` (rpctl approve.py `sandbox_tmp_rm`).
//!
//! Every sandboxed command gets fresh, empty tmpfs mounts at `/tmp` and
//! `/var/tmp`, so deleting there loses nothing real. Only the simplest shape
//! is exempt: an optional leading `cd /tmp &&`, then `rm` of entries sitting
//! directly in `/tmp` or `/var/tmp`. Because the parent is the mount itself,
//! no path component can be a symlink into the workspace; a trailing `/`
//! (which follows a symlink) and `..` are refused too. Only the leading part
//! up to the delete has to be that simple: what follows cannot change where
//! the rm ran, but it must not hold another recursive or forced rm.
//!
//! viy exempts no command rpctl would ask about: the "another rm" check is
//! the union of both views (a superset of rpctl's pattern), and paths that
//! Python's `normpath` would rewrite (`//tmp`, `/tmp/x/..`) are refused
//! rather than normalized.

use std::path::Path;

use super::argv;
use super::literal::Literal;
use super::plain::Op;
use super::plain::plain_pipeline;
use crate::resolve::resolve;

const SANDBOX_TMPS: [&str; 2] = ["/tmp", "/var/tmp"];

/// Whether the delete in `cmd` stays in the sandbox's own `/tmp`, for a
/// sandbox around workspace `ws`.
pub fn sandbox_tmp_rm(cmd: &str, ws: &Path) -> bool {
    let ws = resolve(ws);
    for t in SANDBOX_TMPS {
        let tp = resolve(Path::new(t));
        // A workspace inside /tmp is bind-mounted over the tmpfs, and one
        // that contains /tmp (say /var) puts the real directory back. Both
        // are real.
        if ws.starts_with(&tp) || tp.starts_with(&ws) {
            return false;
        }
    }
    let chars: Vec<char> = cmd.chars().collect();
    let cuts = (0..chars.len())
        .filter(|&i| "&|;\n".contains(chars[i]))
        .chain([chars.len()]);
    for i in cuts {
        let head: String = chars[..i].iter().collect();
        let tail: String = chars[i..].iter().collect();
        if tmp_rm_only(&head) && !any_rm(&tail) {
            return true;
        }
    }
    false
}

/// A recursive or forced rm anywhere in `text`, in either view.
pub(super) fn any_rm(text: &str) -> bool {
    Literal::new(text).rm() || argv::commands(text).iter().any(|a| argv::rm(a))
}

fn tmp_rm_only(cmd: &str) -> bool {
    let Some(p) = plain_pipeline(cmd) else {
        return false; // refuses $, `, globs, ~, redirects
    };
    let mut cwd: Option<String> = None; // the workspace: relative paths are real
    let mut deleted = false;
    for (i, argv) in p.segments.iter().enumerate() {
        match argv[0].as_str() {
            "cd" | "pushd" | "popd" => {
                if i != 0 || argv.len() != 2 || !matches!(p.ops.first(), Some(Op::And | Op::Seq)) {
                    return false;
                }
                match normpath(&argv[1]) {
                    Some(dir) if SANDBOX_TMPS.contains(&dir.as_str()) => cwd = Some(dir),
                    _ => return false,
                }
            }
            "rm" | "/bin/rm" | "/usr/bin/rm" => {
                let mut targets = Vec::new();
                let mut opts = true;
                for a in &argv[1..] {
                    if opts && a == "--" {
                        opts = false;
                    } else if opts && a.starts_with('-') && a != "-" {
                        continue;
                    } else {
                        targets.push(a);
                    }
                }
                if targets.is_empty() || !targets.iter().all(|t| tmp_entry(t, cwd.as_deref())) {
                    return false;
                }
                deleted = true;
            }
            _ => {
                if any_rm(&argv.join(" ")) {
                    return false; // sudo rm, xargs rm, bash -c 'rm ..'
                }
            }
        }
    }
    deleted
}

fn tmp_entry(target: &str, cwd: Option<&str>) -> bool {
    if target.is_empty() || target.ends_with('/') || target.split('/').any(|p| p == "..") {
        return false;
    }
    let joined;
    let target = if target.starts_with('/') {
        target
    } else {
        let Some(cwd) = cwd else {
            return false;
        };
        joined = format!("{cwd}/{target}");
        &joined
    };
    let Some(full) = normpath(target) else {
        return false;
    };
    let Some((dir, base)) = full.rsplit_once('/') else {
        return false;
    };
    SANDBOX_TMPS.contains(&dir) && !base.is_empty() && base != "."
}

/// `path` with repeated slashes and `.` removed, or `None` for what
/// Python's `normpath` would treat differently or rewrite through `..`: a
/// leading `//` or any `..`. The callers then refuse the delete.
fn normpath(path: &str) -> Option<String> {
    if path.starts_with("//") || path.split('/').any(|p| p == "..") {
        return None;
    }
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let body = parts.join("/");
    Some(if path.starts_with('/') {
        format!("/{body}")
    } else if body.is_empty() {
        ".".to_string()
    } else {
        body
    })
}
