//! Piping into an interpreter's stdin (rpctl approve.py,
//! `pipes_program_into_interpreter`): `curl .. | sh` runs downloaded code,
//! `curl .. | python3 -c "<code>"` only feeds it data. The question comes
//! when an interpreter after a pipe takes its *program* from stdin, and
//! anything this parser cannot follow counts as program-from-stdin.
//!
//! A port of rpctl's hand-written parser, which is already structural: the
//! command is split at unquoted operators, each piece into words with
//! redirections dropped, and the interpreter's options are walked. Where a
//! non-ASCII character decides how the words split (a digit before `>`, a
//! word character in `NAME=`), Python and Rust may class it differently, so
//! the command asks rather than guess (`chars`).

use std::path::Path;

use super::chars::all_digits;
use super::chars::assignment;
use super::chars::is_ascii_word;
use super::chars::is_py_space;
use super::chars::is_wide_digit;

pub(super) const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];
const STDIN_PATHS: [&str; 4] = ["-", "/dev/stdin", "/dev/fd/0", "/proc/self/fd/0"];
/// Programs that run their arguments as a command: `| env sh` is still `| sh`.
const WRAPPERS: [&str; 13] = [
    "env", "exec", "command", "nice", "nohup", "time", "timeout", "stdbuf", "setsid", "xargs",
    "builtin", "eval", "{",
];

/// Per interpreter: short options that give the program (`-c code`,
/// `-m module`), short options that take an argument (attached or as the
/// next word), long options that take the next word, and long options that
/// give the program.
struct Interp {
    program_letters: &'static str,
    arg_letters: &'static str,
    arg_longs: &'static [&'static str],
    program_longs: &'static [&'static str],
}

const SHELL: Interp = Interp {
    program_letters: "c",
    arg_letters: "oO",
    arg_longs: &["--rcfile", "--init-file"],
    program_longs: &[],
};
const PYTHON: Interp = Interp {
    program_letters: "cm",
    arg_letters: "WX",
    arg_longs: &["--check-hash-based-pycs"],
    program_longs: &[],
};
const NODE: Interp = Interp {
    program_letters: "ep",
    arg_letters: "rC",
    arg_longs: &[
        "--require",
        "--import",
        "--loader",
        "--input-type",
        "--conditions",
    ],
    program_longs: &["--eval", "--print"],
};
const PERL: Interp = Interp {
    program_letters: "eE",
    arg_letters: "IMmlx0",
    arg_longs: &[],
    program_longs: &[],
};
const RUBY: Interp = Interp {
    program_letters: "e",
    arg_letters: "IrCx",
    arg_longs: &[],
    program_longs: &[],
};

/// Whether an interpreter after a pipe in `cmd` reads its program from
/// stdin.
pub fn pipes_program_into_interpreter(cmd: &str) -> bool {
    match split_shell(cmd) {
        None => pipe_fallback(cmd),
        Some(parts) => parts
            .iter()
            .any(|(piped, text)| *piped && reads_program_from_stdin(text)),
    }
}

/// `[(after a pipe, segment text)]`, split at unquoted `|`, `||`, `&&`, `;`,
/// `&`, `(`, `)` and newlines, or `None` when quoting is unbalanced or `$(`
/// or a backtick appears. rpctl's `_split_shell`, which remote.py uses too.
pub fn split_shell(cmd: &str) -> Option<Vec<(bool, String)>> {
    let s: Vec<char> = cmd.chars().collect();
    let n = s.len();
    let at = |i: usize| s.get(i).copied();
    let subst = |i: usize| at(i) == Some('$') && at(i + 1) == Some('(');
    let mut parts = Vec::new();
    let mut cur: Vec<char> = Vec::new();
    let mut piped = false;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < n {
        let mut c = Some(s[i]);
        match quote {
            Some('\'') => {
                if c == Some('\'') {
                    quote = None;
                }
            }
            Some(_) => {
                if c == Some('\\') {
                    cur.push('\\');
                    i += 1;
                    c = at(i);
                } else if c == Some('"') {
                    quote = None;
                } else if c == Some('`') || subst(i) {
                    return None;
                }
            }
            None => {
                let ch = s[i];
                if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                } else if ch == '\\' {
                    cur.push('\\');
                    i += 1;
                    c = at(i);
                } else if ch == '`' || subst(i) {
                    return None;
                } else if ch == '&'
                    && (cur.last().is_some_and(|l| *l == '<' || *l == '>')
                        || at(i + 1) == Some('>'))
                {
                    // 2>&1, &>file
                } else if "|&;()\n".contains(ch) {
                    parts.push((piped, cur.drain(..).collect()));
                    let next = at(i + 1);
                    let two = (ch, next);
                    if matches!(two, ('|', Some('|')) | ('&', Some('&')) | (';', Some(';'))) {
                        piped = false;
                        i += 2;
                        continue;
                    }
                    if two == ('|', Some('&')) {
                        piped = true;
                        i += 2;
                        continue;
                    }
                    piped = ch == '|';
                    i += 1;
                    continue;
                }
            }
        }
        if let Some(c) = c {
            cur.push(c);
        }
        i += 1;
    }
    if quote.is_some() {
        return None;
    }
    parts.push((piped, cur.into_iter().collect()));
    Some(parts)
}

/// Why `words` gave no words.
enum Unread {
    /// Expansions, input redirects or heredocs this check cannot follow:
    /// rpctl's fallback pattern decides.
    Opaque,
    /// A non-ASCII character decides how the text splits into words, and
    /// Python and Rust may class it differently (a digit before `>`): the
    /// command asks, as rpctl may.
    Unsure,
}

/// The words of one simple command with redirections dropped.
fn words(text: &str) -> Result<Vec<String>, Unread> {
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut quote: Option<char> = None;
    let mut skip_next = false;
    let flush = |words: &mut Vec<String>, cur: &mut String, quoted: &mut bool| {
        if !cur.is_empty() || *quoted {
            words.push(std::mem::take(cur));
        }
        *quoted = false;
    };
    let mut i = 0;
    while i < n {
        let c = s[i];
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            Some(_) => {
                if c == '"' {
                    quote = None;
                } else if c == '$' {
                    return Err(Unread::Opaque);
                } else if c == '\\' && i + 1 < n && "\"\\$`\n".contains(s[i + 1]) {
                    i += 1;
                    cur.push(s[i]);
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    quoted = true;
                } else if c == '\\' {
                    i += 1;
                    if i < n {
                        cur.push(s[i]);
                    }
                    quoted = true;
                } else if c == '$' {
                    return Err(Unread::Opaque);
                } else if c == '&' && s.get(i + 1) == Some(&'>') {
                    flush(&mut words, &mut cur, &mut quoted); // &>file
                    i += 1;
                    continue;
                } else if c == '<' {
                    return Err(Unread::Opaque);
                } else if c == '>' {
                    let fd = if cur.is_empty() || quoted {
                        Some(false)
                    } else {
                        all_digits(&cur)
                    };
                    if fd.is_none() {
                        return Err(Unread::Unsure);
                    }
                    if fd == Some(true) {
                        cur.clear(); // 2>file
                    } else {
                        flush(&mut words, &mut cur, &mut quoted);
                    }
                    if skip_next {
                        return Err(Unread::Opaque);
                    }
                    let mut j = i;
                    while j < n && (s[j] == '>' || s[j] == '&') {
                        j += 1;
                    }
                    let mut k = j;
                    while k < n && (s[k].is_ascii_digit() || s[k] == '-') {
                        k += 1;
                    }
                    if s.get(k)
                        .is_some_and(|c| all_digits(&c.to_string()).is_none())
                    {
                        return Err(Unread::Unsure);
                    }
                    if s[j - 1] == '&' && k > j {
                        i = k; // >&2, 2>&1
                        continue;
                    }
                    i = j;
                    skip_next = true; // the next word is the target
                    while i < n && (s[i] == ' ' || s[i] == '\t') {
                        i += 1;
                    }
                    continue;
                } else if c == ' ' || c == '\t' {
                    if skip_next && (!cur.is_empty() || quoted) {
                        cur.clear();
                        quoted = false;
                        skip_next = false;
                    } else {
                        flush(&mut words, &mut cur, &mut quoted);
                    }
                } else {
                    cur.push(c);
                }
            }
        }
        i += 1;
    }
    if quote.is_some() {
        return Err(Unread::Opaque);
    }
    if skip_next {
        if cur.is_empty() && !quoted {
            return Err(Unread::Opaque);
        }
        cur.clear();
        quoted = false;
    }
    flush(&mut words, &mut cur, &mut quoted);
    Ok(words)
}

fn reads_program_from_stdin(text: &str) -> bool {
    let words = match words(text) {
        Ok(words) => words,
        Err(Unread::Opaque) => return pipe_fallback(&format!("|{text}")),
        Err(Unread::Unsure) => return true,
    };
    let mut words: &[String] = &words;
    while let Some(w) = words.first() {
        match assignment(w) {
            Some(true) => words = &words[1..], // FOO=1 python3
            Some(false) => break,
            None => return true,
        }
    }
    let Some(first) = words.first() else {
        return true; // `| (sh)`: a subshell follows the pipe
    };
    if first == "sudo" {
        words = &words[1..];
        if words.first().is_none_or(|w| w.starts_with('-')) {
            return true;
        }
    }
    let prog = basename(&words[0]);
    let args = &words[1..];
    if WRAPPERS.contains(&prog) || prog == "!" {
        return true;
    }
    if SHELLS.contains(&prog) {
        return interp_reads_stdin(args, &SHELL, true);
    }
    if is_python(prog) {
        return interp_reads_stdin(args, &PYTHON, false);
    }
    match prog {
        "node" | "nodejs" => interp_reads_stdin(args, &NODE, false),
        "perl" => interp_reads_stdin(args, &PERL, false),
        "ruby" => interp_reads_stdin(args, &RUBY, false),
        _ => false,
    }
}

fn interp_reads_stdin(args: &[String], kind: &Interp, shell: bool) -> bool {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            break;
        }
        if kind.program_longs.contains(&a.as_str()) {
            return false;
        }
        if a.starts_with("--") {
            i += usize::from(kind.arg_longs.contains(&a.as_str()));
            continue;
        }
        let chars: Vec<char> = a.chars().collect();
        if chars.len() < 2 || !(chars[0] == '-' || (shell && chars[0] == '+')) {
            i -= 1; // the script path
            break;
        }
        for (j, f) in chars.iter().enumerate().skip(1) {
            if shell && *f == 's' {
                return true; // sh -s: program from stdin
            }
            if kind.program_letters.contains(*f) {
                if !shell {
                    return false;
                }
                // sh -c '<program>': the program may itself pipe into a shell.
                let program = args.get(i).map_or("", String::as_str);
                return pipes_program_into_interpreter(program);
            }
            if kind.arg_letters.contains(*f) {
                i += usize::from(j == chars.len() - 1); // -W ignore vs -Wignore
                break;
            }
        }
    }
    i >= args.len() || STDIN_PATHS.contains(&args[i].as_str())
}

/// rpctl's `_PIPE_FALLBACK` for text the parser cannot follow: a `|`, then
/// optional whitespace and `sudo `, then a shell name ending at a word
/// boundary, or `python[digits.]`, `node`, `perl`, `ruby` followed by
/// whitespace or the end.
fn pipe_fallback(text: &str) -> bool {
    let s: Vec<char> = text.chars().collect();
    let skip_space = |mut i: usize| {
        while i < s.len() && is_py_space(s[i]) {
            i += 1;
        }
        i
    };
    let starts = |i: usize, w: &str| {
        let w: Vec<char> = w.chars().collect();
        s[i..].starts_with(&w).then_some(i + w.len())
    };
    // (ba|z|da|k)?sh\b
    let shell_at = |i: usize| {
        ["bash", "zsh", "dash", "ksh", "sh"]
            .iter()
            .any(|w| starts(i, w).is_some_and(|end| s.get(end).is_none_or(|c| !is_ascii_word(*c))))
    };
    // (python[\d.]*|node|perl|ruby)(\s|$)
    let interpreter_at = |i: usize| {
        let end = if let Some(end) = starts(i, "python") {
            Some(
                end + s[end..]
                    .iter()
                    .take_while(|c| is_wide_digit(**c) || **c == '.')
                    .count(),
            )
        } else {
            ["node", "perl", "ruby"].iter().find_map(|w| starts(i, w))
        };
        end.is_some_and(|end| s.get(end).is_none_or(|c| is_py_space(*c)))
    };
    (0..s.len()).filter(|&i| s[i] == '|').any(|bar| {
        let i = skip_space(bar + 1);
        if shell_at(i) || interpreter_at(i) {
            return true;
        }
        // (sudo\s+)? before a shell only
        starts(i, "sudo").is_some_and(|j| {
            let k = skip_space(j);
            k > j && shell_at(k)
        })
    })
}

/// `python[\d.]*`, with any Unicode digit.
pub(super) fn is_python(prog: &str) -> bool {
    prog.strip_prefix("python")
        .is_some_and(|rest| rest.chars().all(|c| is_wide_digit(c) || c == '.'))
}

/// The last component of a program path, as pathlib's `.name` (empty for
/// `/` or `..`).
pub(super) fn basename(word: &str) -> &str {
    Path::new(word)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
}
