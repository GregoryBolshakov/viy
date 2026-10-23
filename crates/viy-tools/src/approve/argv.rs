//! The argv view of a command, as Codex's `is_dangerous_command` reads one
//! (kb d033): the command is split into simple commands at unquoted
//! operators, each into words with the `shlex` crate, and wrappers are
//! unwrapped (`sudo`, `env VAR=x`, `timeout 5`, `xargs`, `sh -c '...'`,
//! `eval`, `find -exec ... ;`), so the program and its options are seen as
//! the shell will run them: `"rm" -rf x`, `/bin/rm x -rf` and
//! `git -C dir clean -fd` ask here, where rpctl's patterns miss them.
//!
//! What this view cannot follow (`$(...)`, backticks, unbalanced quotes) it
//! skips: the literal view still checks that text, so skipping only ever
//! means asking as rpctl does.

use super::chars::assignment;
use super::pipe::SHELLS;
use super::pipe::basename;
use super::pipe::split_shell;

/// How deep wrappers and `sh -c` scripts are followed (Codex's limit).
const MAX_DEPTH: usize = 8;

/// The simple commands `cmd` runs, wrappers removed.
pub(super) fn commands(cmd: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    collect(cmd, 0, &mut out);
    out
}

fn collect(cmd: &str, depth: usize, out: &mut Vec<Vec<String>>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some(parts) = split_shell(cmd) else {
        return;
    };
    for (_, text) in parts {
        if let Some(words) = shlex::split(&text) {
            unwrap(&words, depth, out);
        }
    }
}

/// Options that take the next word as their value, per wrapper.
fn option_values(prog: &str) -> &'static [&'static str] {
    match prog {
        "sudo" | "doas" => &[
            "-u",
            "-g",
            "-h",
            "-p",
            "-C",
            "-D",
            "-r",
            "-t",
            "-U",
            "-T",
            "--user",
            "--group",
            "--host",
            "--prompt",
            "--chdir",
            "--role",
            "--type",
            "--other-user",
            "--command-timeout",
            "--close-from",
        ],
        "env" => &["-u", "-C", "--unset", "--chdir"],
        "timeout" => &["-s", "-k", "--signal", "--kill-after"],
        "nice" => &["-n", "--adjustment"],
        "ionice" => &["-c", "-n", "-p", "-P", "-u", "--class", "--classdata"],
        "stdbuf" => &["-i", "-o", "-e", "--input", "--output", "--error"],
        "xargs" => &[
            "-a",
            "-d",
            "-E",
            "-e",
            "-I",
            "-i",
            "-L",
            "-l",
            "-n",
            "-P",
            "-s",
            "--arg-file",
            "--delimiter",
            "--eof",
            "--replace",
            "--max-lines",
            "--max-args",
            "--max-procs",
            "--max-chars",
            "--process-slot-var",
        ],
        "time" => &["-f", "-o", "--format", "--output"],
        "watch" => &["-n", "-d", "--interval", "--differences"],
        _ => &[],
    }
}

/// Wrappers whose operands, after their options, are the command they run.
const WRAPPERS: [&str; 15] = [
    "sudo", "doas", "env", "timeout", "nice", "ionice", "nohup", "setsid", "stdbuf", "xargs",
    "time", "exec", "command", "builtin", "!",
];

fn unwrap(words: &[String], depth: usize, out: &mut Vec<Vec<String>>) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut w = words;
    while w.first().is_some_and(|x| is_assignment(x)) {
        w = &w[1..];
    }
    let Some(first) = w.first() else {
        return;
    };
    let prog = basename(first);
    if WRAPPERS.contains(&prog) || prog == "watch" {
        let values = option_values(prog);
        let mut i = 1;
        while let Some(a) = w.get(i) {
            if a == "--" {
                i += 1;
                break;
            }
            if prog == "env" && is_assignment(a) {
                i += 1;
                continue;
            }
            if prog == "env" && (a == "-S" || a == "--split-string") {
                // env -S 'cmd args': the string is a command line.
                if let Some(line) = w.get(i + 1) {
                    collect(line, depth + 1, out);
                }
                return;
            }
            if !a.starts_with('-') || a == "-" {
                break;
            }
            i += 1 + usize::from(values.contains(&a.as_str()));
        }
        if prog == "timeout" {
            i += 1; // the duration
        }
        let rest = w.get(i..).unwrap_or_default();
        if prog == "watch" {
            collect(&rest.join(" "), depth + 1, out);
        } else {
            unwrap(rest, depth + 1, out);
        }
        return;
    }
    if prog == "eval" {
        collect(&w[1..].join(" "), depth + 1, out);
        return;
    }
    if SHELLS.contains(&prog) {
        // sh -c '<script>', -lc, -ec ...: the first operand is the script.
        let mut script = false;
        for a in &w[1..] {
            if script && !a.starts_with('-') && !a.starts_with('+') {
                collect(a, depth + 1, out);
                break;
            }
            if a.starts_with('-') && !a.starts_with("--") && a.contains('c') {
                script = true;
            }
        }
    }
    if prog == "find" {
        // find ... -exec cmd {} ; / -execdir / -ok / -okdir
        let mut i = 1;
        while i < w.len() {
            if matches!(w[i].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                let end = w[i + 1..]
                    .iter()
                    .position(|a| a == ";" || a == "+")
                    .map_or(w.len(), |p| i + 1 + p);
                unwrap(&w[i + 1..end], depth + 1, out);
                i = end;
            }
            i += 1;
        }
    }
    out.push(w.to_vec());
}

/// `NAME=value`; one whose name holds a non-ASCII letter counts too (the
/// argv view only adds questions, so unwrapping more is safe).
fn is_assignment(word: &str) -> bool {
    assignment(word) != Some(false)
}

/// A cluster of short options (`-rf`, `-qf`), not a long one.
fn short_cluster(a: &str, letters: &[char]) -> bool {
    a.len() > 1
        && a.starts_with('-')
        && !a.starts_with("--")
        && a.chars().skip(1).any(|c| letters.contains(&c))
}

/// The options before `--`.
fn options(args: &[String]) -> impl Iterator<Item = &str> {
    args.iter()
        .map(String::as_str)
        .take_while(|a| *a != "--")
        .filter(|a| a.starts_with('-'))
}

// ------------------------------------------------------------- DESTRUCTIVE

/// `rm` with `-r`, `-R`, `-f` in any cluster or position, or `--recursive`,
/// `--force` and their abbreviations.
pub(super) fn rm(argv: &[String]) -> bool {
    basename(&argv[0]) == "rm"
        && options(&argv[1..]).any(|a| {
            short_cluster(a, &['r', 'R', 'f'])
                || a.strip_prefix("--").is_some_and(|long| {
                    !long.is_empty() && ("recursive".starts_with(long) || "force".starts_with(long))
                })
        })
}

/// git's working-tree rewrites, past its global options (`-C dir`, `-c k=v`).
pub(super) fn git(argv: &[String]) -> bool {
    if basename(&argv[0]) != "git" {
        return false;
    }
    let mut i = 1;
    while let Some(a) = argv.get(i) {
        if !a.starts_with('-') {
            break;
        }
        let takes_value = matches!(
            a.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
        );
        i += 1 + usize::from(takes_value);
    }
    let Some(sub) = argv.get(i) else {
        return false;
    };
    let rest = &argv[i + 1..];
    let has = |words: &[&str]| rest.iter().any(|a| words.contains(&a.as_str()));
    match sub.as_str() {
        "clean" | "restore" => true,
        "reset" => has(&["--hard"]),
        "stash" => rest
            .iter()
            .find(|a| !a.starts_with('-'))
            .is_some_and(|a| a == "drop" || a == "clear"),
        "checkout" => {
            has(&["--", "-f", "--force", "."]) || rest.iter().any(|a| short_cluster(a, &['f']))
        }
        "switch" => {
            has(&["-f", "--force", "--discard-changes"])
                || rest.iter().any(|a| short_cluster(a, &['f']))
        }
        _ => false,
    }
}

pub(super) fn find_delete(argv: &[String]) -> bool {
    basename(&argv[0]) == "find" && argv[1..].iter().any(|a| a == "-delete")
}

pub(super) fn overwrite(argv: &[String]) -> bool {
    let prog = basename(&argv[0]);
    matches!(prog, "shred" | "truncate" | "dd") || prog.starts_with("mkfs")
}

pub(super) fn bulk_permissions(argv: &[String]) -> bool {
    match basename(&argv[0]) {
        "chown" => true,
        "chmod" => options(&argv[1..]).any(|a| a == "--recursive" || short_cluster(a, &['R'])),
        _ => false,
    }
}

// ---------------------------------------------------------- NET_ALWAYS_ASK

pub(super) fn program(argv: &[String], names: &[&str]) -> bool {
    names.contains(&basename(&argv[0]))
}
