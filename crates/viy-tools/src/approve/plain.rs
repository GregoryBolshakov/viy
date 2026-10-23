//! Plain commands, the "always" prefixes and the rules file (rpctl
//! approve.py `plain_pipeline`, `prefix_for`, `load_rules`, `add_rule`,
//! `rules_allow_net`).
//!
//! A command is plain, and so eligible for rule matching, when it is nothing
//! but words, quotes and the four list and pipe operators; anything else
//! (expansions, globs, redirects, subshells, backgrounding, newlines) makes
//! it opaque.

use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::pipe::basename;
use super::pipe::is_python;

/// Never offered as an "always" prefix: allowing `python` allows
/// `python -c <anything>`. rpctl's list plus viy (kb d014).
pub const NO_PREFIX: [&str; 41] = [
    "bash", "sh", "zsh", "dash", "env", "sudo", "xargs", "find", "make", "python", "python3",
    "node", "npx", "perl", "ruby", "php", "deno", "bun", "eval", "exec", "timeout", "nohup",
    "nice", "time", "watch", "uv", "uvx", "pipx", "curl", "wget", "git", "docker", "podman", "rp",
    "tok", "ssh", "scp", "rsync", "nc", "socat", "viy",
];

/// The operator between two segments of a plain command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    And,
    Or,
    Seq,
    Pipe,
}

/// A plain command split into argv lists and the operators between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub segments: Vec<Vec<String>>,
    pub ops: Vec<Op>,
}

fn plain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_./=:,@%+-'\" &|;".contains(c)
}

enum Tok {
    Word(String),
    Punct(String),
}

/// The tokens of a plain command: words (quotes removed) and runs of `&`,
/// `|`, `;`. `None` for an unclosed quote or a word that is empty or
/// nothing but quoted operator characters.
fn tokens(cmd: &str) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let mut chars = cmd.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == ' ' {
            chars.next();
        } else if "&|;".contains(c) {
            let mut run = String::new();
            while let Some(&p) = chars.peek().filter(|p| "&|;".contains(**p)) {
                run.push(p);
                chars.next();
            }
            out.push(Tok::Punct(run));
        } else {
            let mut word = String::new();
            let mut quoted_only = true;
            while let Some(&c) = chars.peek() {
                if c == ' ' || "&|;".contains(c) {
                    break;
                }
                chars.next();
                if c == '\'' || c == '"' {
                    loop {
                        match chars.next() {
                            None => return None,
                            Some(q) if q == c => break,
                            Some(q) => word.push(q),
                        }
                    }
                } else {
                    quoted_only = false;
                    word.push(c);
                }
            }
            // rpctl calls an empty word (`''`) opaque, and reads a quoted
            // `'&&'` as the operator; viy calls both opaque.
            if quoted_only && word.chars().all(|c| "&|;".contains(c)) {
                return None;
            }
            out.push(Tok::Word(word));
        }
    }
    Some(out)
}

/// `cmd` split into argv lists and the operators between them, or `None`
/// if it is opaque.
pub fn plain_pipeline(cmd: &str) -> Option<Pipeline> {
    if cmd.is_empty() || !cmd.chars().all(plain_char) {
        return None;
    }
    let mut segments = Vec::new();
    let mut ops = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for t in tokens(cmd)? {
        match t {
            Tok::Word(w) => cur.push(w),
            Tok::Punct(p) => {
                let op = match p.as_str() {
                    "&&" => Op::And,
                    "||" => Op::Or,
                    ";" => Op::Seq,
                    "|" => Op::Pipe,
                    _ => return None, // &, |&, ;; and friends
                };
                if cur.is_empty() {
                    return None;
                }
                segments.push(std::mem::take(&mut cur));
                ops.push(op);
            }
        }
    }
    if cur.is_empty() {
        return None;
    }
    segments.push(cur);
    Some(Pipeline { segments, ops })
}

/// A plain command's argv lists, or `None` if it is opaque.
pub fn plain_segments(cmd: &str) -> Option<Vec<Vec<String>>> {
    plain_pipeline(cmd).map(|p| p.segments)
}

/// The prefix "always" would save for argv: the program plus its
/// subcommand (`pip install`). `None` when a prefix would allow too much.
pub fn prefix_for(argv: &[String]) -> Option<Vec<String>> {
    let first = argv.first()?;
    let prog = basename(first);
    if NO_PREFIX.contains(&prog) || is_python_versioned(prog) {
        return None;
    }
    // A bare program would allow all of its subcommands (npm exec, cargo
    // run), so a rule always names one: `npm -g i` gets no rule.
    let sub = argv.get(1).filter(|s| !s.starts_with('-'))?;
    Some(vec![first.clone(), sub.clone()])
}

/// `python\d[\d.]*`: python3, python3.12.
fn is_python_versioned(prog: &str) -> bool {
    prog.strip_prefix("python")
        .and_then(|rest| rest.chars().next())
        .is_some_and(char::is_numeric)
        && is_python(prog)
}

// ------------------------------------------------------------------- rules

/// The header a new rules file starts with.
const RULES_HEADER: &str = "# viy chat approval rules. Only you edit this file; the model's\n\
# tools cannot reach it. One rule per line:\n\
#   net-allow <program> [subcommand]   network without review\n";

/// The `net-allow` prefixes in a rules file's text, as lists of words.
/// Lines whose words cannot be split, and prefixes `prefix_for` would not
/// offer, are skipped. So is a line holding `#`: the `shlex` crate reads a
/// word starting with one as a comment, which would leave a shorter, wider
/// rule than rpctl reads.
pub fn parse_rules(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
            if verb != "net-allow" || rest.contains('#') {
                return None;
            }
            shlex::split(rest).filter(|w| !w.is_empty() && prefix_for(w).is_some())
        })
        .collect()
}

/// The rules in `path`; none when it cannot be read.
pub fn load_rules(path: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(path)
        .map(|t| parse_rules(&t))
        .unwrap_or_default()
}

/// Appends `net-allow <words>` to `path`, creating it (mode 0600) with a
/// header first.
pub fn add_rule(path: &Path, words: &[String]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let line = shlex::try_join(words.iter().map(String::as_str))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut f = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    let mut text = String::new();
    if f.metadata()?.len() == 0 {
        text.push_str(RULES_HEADER);
    }
    text.push_str(&format!("net-allow {line}\n"));
    f.write_all(text.as_bytes())
}

/// Whether every segment of a plain command starts with an allowed prefix.
pub fn rules_allow_net(cmd: &str, rules: &[Vec<String>]) -> bool {
    let Some(segments) = plain_segments(cmd) else {
        return false;
    };
    !rules.is_empty()
        && segments
            .iter()
            .all(|seg| rules.iter().any(|r| seg.starts_with(r)))
}
