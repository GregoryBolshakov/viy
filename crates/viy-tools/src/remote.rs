//! Servers the user lends the model with `/ssh`: a VPS, a home server
//! (rpctl `remote.py`, rpctl kb d027, d029, d030, d031).
//!
//! Only the user grants this, by typing `/ssh <host>` at the prompt. There
//! is no config key for it, nothing the model writes can add a host, and
//! the grant lasts until `/unssh` or the end of the chat. While it lasts:
//!
//! - a run_shell command that is exactly `ssh <host> '<command>'` does not
//!   go to the sandbox: the client runs ssh itself ([`argv`]) with the key
//!   and settings fixed when the user typed `/ssh`. The model writes only
//!   the remote command; its ssh options, `~/.ssh/config` and the key never
//!   reach ssh or the sandbox;
//! - every such command is first rated by the reviewer (with the server
//!   described); `critical` refuses it in every mode. Otherwise it is a
//!   question of kind `remote` that the user or auto-yes answers, with no
//!   "always"; with nobody to ask the answer is no;
//! - the output counts as outside text, like a web page.
//!
//! By default the command runs on the server inside bubblewrap
//! ([`box_command`]): the server's files read-only, `~/rp-work` the only
//! writable place, no network, no other processes in view, `~/.ssh`,
//! `~/.config` and secret files hidden, viy's seccomp filter. `/ssh` checks
//! once that this works there ([`probe_box`]). Lent with `full`, the model
//! gets everything the remote user can do there, and the server's own
//! unfiltered internet.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use viy_config::vault::find_executable;
use viy_config::vault::wait_with_timeout;

use crate::secrets;

/// What starts every note from the client to the model about `/ssh`
/// (rpctl `sessions.SSH_NOTE_PREFIX`, renamed: flag F21, kb d131).
pub const NOTE_PREFIX: &str = "[viy chat: ssh: ";
/// rp's, in the chats rp saved.
pub const RP_NOTE_PREFIX: &str = "[rp chat: ssh: ";
/// Under the remote `$HOME`: the only place a boxed command writes, kept
/// between commands so the model can build something up there. It keeps
/// rp's name, as does `RPCTL_SANDBOX` in [`box_command`]: both are seen on
/// the server, where viy's name never goes (kb d131).
pub const BOX_DIR: &str = "rp-work";
/// Home folders hidden behind an empty tmpfs: keys, tokens, app config.
const BOX_HIDE_DIRS: [&str; 9] = [
    ".ssh",
    ".gnupg",
    ".config",
    ".local/share",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".password-store",
];
/// Home files masked with /dev/null: shell histories hold typed secrets.
const BOX_HIDE_FILES: [&str; 9] = [
    ".bash_history",
    ".zsh_history",
    ".python_history",
    ".psql_history",
    ".mysql_history",
    ".node_repl_history",
    ".lesshst",
    ".viminfo",
    ".vault-token",
];
/// Where secret-named files are looked for on the server.
const BOX_SCAN: [&str; 4] = ["\"$HOME\"", "/srv", "/opt", "/var/www"];
const BOX_SCAN_DEPTH: u32 = 6;
const BOX_SCAN_LIMIT: u32 = 200;
const BOX_OK: &str = "RP_BOX_OK";
/// What a probe or a command may take to connect and answer.
const SSH_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// The PATH ssh runs with.
const SSH_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
/// The ssh-keygen, ssh-add and `ssh -G` calls at grant time.
const TOOL_TIMEOUT: Duration = Duration::from_secs(15);

fn re(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => panic!("remote.rs pattern {pattern:?}: {e}"),
    }
}

/// Python's `\s` on str, which also counts \x1c-\x1f (Rust's `\s` does not).
use crate::egress::PY_SPACE;

static HOST_LINE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\A[\s\x1c-\x1f]*Host[\s\x1c-\x1f]+(.+?)[\s\x1c-\x1f]*\z"));
static REMOTE_TOOL: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"(^|[{PY_SPACE};&|(/])(ssh|scp|sftp|rsync|sshfs)([{PY_SPACE}]|$)"
    ))
});
static ALIAS: LazyLock<Regex> = LazyLock::new(|| re(r"\A[\w.@:\[\]-]+\z"));
/// A command word that runs something as root: sudo at the start of a line
/// or after an operator, maybe behind env/exec/nohup... Only for a warning.
static ROOT: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"(^|[;&|(`\n])[{PY_SPACE}]*(?:(?:env|exec|command|nohup|nice|time)[{PY_SPACE}]+)*(sudo|doas|pkexec|su)([{PY_SPACE}]|$)"
    ))
});

/// The command uses a lent host in a form the client does not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A server the user lent, its settings fixed at `/ssh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// What the user typed and the model writes.
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub key: PathBuf,
    /// The file that holds the host's key.
    pub known_hosts: PathBuf,
    /// SSH_AUTH_SOCK, when the key file needs a passphrase.
    pub agent: String,
    /// Commands run in bubblewrap there; `false`: `/ssh <host> full`.
    pub boxed: bool,
}

impl Host {
    pub fn login(&self) -> String {
        let port = if self.port == 22 {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        format!("{}@{}{port}", self.user, self.hostname)
    }

    /// What the model may write after ssh to mean this host.
    pub fn targets(&self) -> Vec<String> {
        let mut names = vec![self.alias.to_lowercase()];
        if !names.contains(&self.hostname.to_lowercase()) {
            names.push(self.hostname.to_lowercase());
        }
        let user = self.user.to_lowercase();
        let with_user: Vec<String> = names.iter().map(|n| format!("{user}@{n}")).collect();
        names.extend(with_user);
        names
    }
}

/// A word as Python's `shlex.quote` writes it (as viy-pod's boot script's
/// `quote`): bare when it is only ASCII letters, digits and `_@%+=:,./-`,
/// else in single quotes with each `'` as `'"'"'`.
pub fn shell_quote(word: &str) -> String {
    let safe = |b: u8| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b);
    if word.is_empty() {
        "''".to_string()
    } else if word.bytes().all(safe) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r#"'"'"'"#))
    }
}

/// Python's `shlex.split(text)`: POSIX words, no comments; `None` where it
/// raises (an unclosed quote, a trailing `\`).
pub fn py_split(text: &str) -> Option<Vec<String>> {
    py_split_with(text, false)
}

/// [`py_split`], with `comments` as `shlex.split(text, comments=True)`: a
/// `#` outside quotes, in a word too, ends the line.
fn py_split_with(text: &str, comments: bool) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '#' if comments => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            ' ' | '\t' | '\r' | '\n' => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\\' => {
                word.push(chars.next()?);
                started = true;
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\') => word.push(c),
                            c => {
                                word.push('\\');
                                word.push(c);
                            }
                        },
                        c => word.push(c),
                    }
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    Some(words)
}

/// The tokens of Python's `shlex.shlex(text, posix=True,
/// punctuation_chars=True)` iterated as rpctl's `match` does, with no
/// `whitespace_split`: a word is a run of shlex's word characters (or
/// quoted text joined to them), runs of `();<>|&` are tokens, any other
/// character is a token alone, an escaped character joins the word it is
/// in, and `#` ends the line (CPython 3.10's `read_token`). `None` where
/// shlex raises.
pub fn punct_tokens(text: &str) -> Option<Vec<String>> {
    const PUNCT: &str = "();<>|&";
    const WHITESPACE: &str = " \t\r\n";
    let word_char = |c: char| {
        c.is_ascii_alphanumeric()
            || "_~-./*?=".contains(c)
            || "ßàáâãäåæçèéêëìíîïðñòóôõöøùúûüýþÿÀÁÂÃÄÅÆÇÈÉÊËÌÍÎÏÐÑÒÓÔÕÖØÙÚÛÜÝÞ".contains(c)
    };
    #[derive(Clone, Copy, PartialEq)]
    enum S {
        Space,
        Word,
        Punct,
        Quote(char),
        Escape(char),
    }
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut pushback: Option<char> = None;
    let next = |i: &mut usize, pushback: &mut Option<char>| -> Option<char> {
        if let Some(c) = pushback.take() {
            return Some(c);
        }
        let c = chars.get(*i).copied();
        *i += 1;
        c
    };
    let skip_line = |i: &mut usize| {
        while *i < chars.len() && chars[*i] != '\n' {
            *i += 1;
        }
        *i += 1;
    };
    let mut tokens = Vec::new();
    let mut state = S::Space;
    loop {
        let mut token = String::new();
        let mut quoted = false;
        let mut escaped_state = S::Space;
        let mut at_end = false;
        loop {
            let c = next(&mut i, &mut pushback);
            match state {
                S::Space => {
                    let Some(c) = c else {
                        at_end = true;
                        break;
                    };
                    if WHITESPACE.contains(c) {
                        if !token.is_empty() || quoted {
                            break;
                        }
                    } else if c == '#' {
                        skip_line(&mut i);
                    } else if c == '\\' {
                        escaped_state = S::Word;
                        state = S::Escape('\\');
                    } else if word_char(c) {
                        token.push(c);
                        state = S::Word;
                    } else if PUNCT.contains(c) {
                        token.push(c);
                        state = S::Punct;
                    } else if c == '\'' || c == '"' {
                        state = S::Quote(c);
                    } else {
                        token.push(c);
                        break;
                    }
                }
                S::Quote(q) => {
                    quoted = true;
                    let c = c?;
                    if c == q {
                        state = S::Word;
                    } else if c == '\\' && q == '"' {
                        escaped_state = state;
                        state = S::Escape('\\');
                    } else {
                        token.push(c);
                    }
                }
                S::Escape(e) => {
                    let c = c?;
                    if let S::Quote(q) = escaped_state
                        && c != e
                        && c != q
                    {
                        token.push(e);
                    }
                    token.push(c);
                    state = escaped_state;
                }
                S::Word | S::Punct => {
                    let Some(c) = c else {
                        at_end = true;
                        break;
                    };
                    if WHITESPACE.contains(c) {
                        state = S::Space;
                        if !token.is_empty() || quoted {
                            break;
                        }
                    } else if c == '#' {
                        skip_line(&mut i);
                        state = S::Space;
                        if !token.is_empty() || quoted {
                            break;
                        }
                    } else if state == S::Punct {
                        if PUNCT.contains(c) {
                            token.push(c);
                        } else {
                            pushback = Some(c);
                            state = S::Space;
                            break;
                        }
                    } else if c == '\'' || c == '"' {
                        state = S::Quote(c);
                    } else if c == '\\' {
                        escaped_state = S::Word;
                        state = S::Escape('\\');
                    } else if word_char(c) {
                        token.push(c);
                    } else {
                        pushback = Some(c);
                        state = S::Space;
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                }
            }
        }
        if at_end && token.is_empty() && !quoted {
            return Some(tokens);
        }
        tokens.push(token);
        if at_end {
            return Some(tokens);
        }
    }
}

/// The Host names in an ssh config's text that name one host (no
/// wildcards), in order.
pub fn config_hosts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some(m) = HOST_LINE.captures(line) else {
            continue;
        };
        let names = py_split_with(&m[1], true).unwrap_or_default();
        for name in names {
            if !name.contains(['*', '?', '!']) && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// `'hetzner ~/.ssh/key full'` -> `("hetzner", Some("~/.ssh/key"), true)`;
/// the key and `full` (no sandbox on the server) are optional.
pub fn parse_arg(arg: &str) -> (String, Option<String>, bool) {
    // where shlex raises, rp splits at whitespace, Python's: \x1c-\x1f too
    let words = py_split(arg).unwrap_or_else(|| {
        arg.split(crate::approve::is_py_space)
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    });
    let Some((first, rest)) = words.split_first() else {
        return (String::new(), None, false);
    };
    let full = rest.iter().any(|w| w == "full");
    let key = rest.iter().find(|w| *w != "full").cloned();
    (first.clone(), key, full)
}

fn which(name: &str) -> Option<PathBuf> {
    find_executable(std::env::var_os("PATH").as_deref(), name)
}

/// A tool's run at grant time: (exit code, stdout, stderr), `None` when it
/// could not start or ran over.
fn tool(
    program: &str,
    args: &[&std::ffi::OsStr],
    env: Option<&[(&str, &str)]>,
) -> Option<(Option<i32>, String, String)> {
    let mut cmd = std::process::Command::new(which(program)?);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(env) = env {
        cmd.env_clear();
        cmd.envs(env.iter().copied());
    }
    let out = wait_with_timeout(cmd.spawn().ok()?, TOOL_TIMEOUT).ok()?;
    Some((
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

type Opts = std::collections::HashMap<String, Vec<String>>;

/// `ssh -G alias` as `{key: [values]}`.
fn ssh_g(alias: &str) -> Result<Opts, String> {
    if which("ssh").is_none() {
        return Err("ssh is not installed".into());
    }
    let Some((code, out, err)) = tool("ssh", &["-G".as_ref(), "--".as_ref(), alias.as_ref()], None)
    else {
        return Err(format!("ssh -G {alias} failed: it did not answer"));
    };
    if code != Some(0) {
        let last = err.trim().lines().last().unwrap_or("no output").to_string();
        return Err(format!("ssh -G {alias} failed: {last}"));
    }
    let mut opts = Opts::new();
    for line in out.lines() {
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        opts.entry(k.to_lowercase())
            .or_default()
            .push(v.trim().to_string());
    }
    Ok(opts)
}

/// The first of `files` that holds a key for the host.
fn known_in(files: &[String], hostname: &str, port: u16, home: &Path) -> Option<PathBuf> {
    let name = if port == 22 {
        hostname.to_string()
    } else {
        format!("[{hostname}]:{port}")
    };
    files.iter().map(|f| expand_home(f, home)).find(|f| {
        f.is_file()
            && tool(
                "ssh-keygen",
                &["-F".as_ref(), name.as_ref(), "-f".as_ref(), f.as_os_str()],
                None,
            )
            .is_some_and(|(code, out, _)| code == Some(0) && !out.trim().is_empty())
    })
}

fn needs_passphrase(key: &Path) -> bool {
    tool(
        "ssh-keygen",
        &[
            "-y".as_ref(),
            "-P".as_ref(),
            "".as_ref(),
            "-f".as_ref(),
            key.as_os_str(),
        ],
        None,
    )
    .is_none_or(|(code, _, _)| code != Some(0))
}

fn fingerprint(public: &Path) -> Option<String> {
    let (code, out, _) = tool(
        "ssh-keygen",
        &["-l".as_ref(), "-f".as_ref(), public.as_os_str()],
        None,
    )?;
    let f: Vec<&str> = out.split_whitespace().collect();
    (code == Some(0) && f.len() > 1).then(|| f[1].to_string())
}

fn agent_holds(sock: &str, fingerprint: Option<&str>) -> bool {
    let (Some(fp), false) = (fingerprint, sock.is_empty()) else {
        return false;
    };
    let path = std::env::var("PATH").unwrap_or_default();
    tool(
        "ssh-add",
        &["-l".as_ref()],
        Some(&[("SSH_AUTH_SOCK", sock), ("PATH", &path)]),
    )
    .is_some_and(|(_, out, _)| out.split_whitespace().any(|w| w == fp))
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// What resolving a host asks of the machine: rp's `ssh -G`, ssh-keygen and
/// ssh-add calls, behind a trait so the grant's rules are tested without a
/// server.
pub trait Machine: Send + Sync {
    fn ssh_g(&self, alias: &str) -> Result<Opts, String>;
    fn known_in(&self, files: &[String], hostname: &str, port: u16) -> Option<PathBuf>;
    fn needs_passphrase(&self, key: &Path) -> bool;
    fn fingerprint(&self, public: &Path) -> Option<String>;
    fn agent_holds(&self, sock: &str, fingerprint: Option<&str>) -> bool;
    fn agent_sock(&self) -> String;
    fn home(&self) -> PathBuf;
}

/// This machine's ssh tools.
pub struct SshTools {
    pub home: PathBuf,
}

impl Machine for SshTools {
    fn ssh_g(&self, alias: &str) -> Result<Opts, String> {
        ssh_g(alias)
    }
    fn known_in(&self, files: &[String], hostname: &str, port: u16) -> Option<PathBuf> {
        known_in(files, hostname, port, &self.home)
    }
    fn needs_passphrase(&self, key: &Path) -> bool {
        needs_passphrase(key)
    }
    fn fingerprint(&self, public: &Path) -> Option<String> {
        fingerprint(public)
    }
    fn agent_holds(&self, sock: &str, fingerprint: Option<&str>) -> bool {
        agent_holds(sock, fingerprint)
    }
    fn agent_sock(&self) -> String {
        std::env::var("SSH_AUTH_SOCK").unwrap_or_default()
    }
    fn home(&self) -> PathBuf {
        self.home.clone()
    }
}

/// A [`Host`] for `alias` (a `~/.ssh/config` name or a hostname), with its
/// settings fixed now; else what to do instead.
pub fn resolve(
    machine: &dyn Machine,
    alias: &str,
    key: Option<&str>,
    boxed: bool,
) -> Result<Host, String> {
    if alias.is_empty() || alias.starts_with('-') || !ALIAS.is_match(alias) {
        return Err(format!(
            "{} is not a host name",
            crate::netguard::url::py_repr(alias)
        ));
    }
    let opts = machine.ssh_g(alias)?;
    let one = |k: &str, default: &str| {
        opts.get(k)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_else(|| default.to_string())
    };
    for k in ["proxyjump", "proxycommand"] {
        if one(k, "none").to_lowercase() != "none" {
            return Err(format!(
                "{alias} goes through {k} {}, which /ssh does not support; lend the last hop \
                 directly",
                one(k, "")
            ));
        }
    }
    let (hostname, user) = (one("hostname", ""), one("user", ""));
    let port_text = one("port", "22");
    let Ok(port) = port_text.trim().parse::<u16>() else {
        return Err(format!(
            "ssh -G gives {alias} the port {}",
            crate::netguard::url::py_repr(&port_text)
        ));
    };
    if hostname.is_empty() || hostname.starts_with('-') || user.is_empty() || user.starts_with('-')
    {
        return Err(format!("ssh -G gives {alias} no usable host name or user"));
    }
    let home = machine.home();
    let keys: Vec<PathBuf> = match key {
        Some(k) => vec![expand_home(k, &home)],
        None => {
            let keys: Vec<PathBuf> = opts
                .get("identityfile")
                .into_iter()
                .flatten()
                .map(|k| expand_home(k, &home))
                .filter(|k| k.is_file())
                .collect();
            if keys.len() > 1 {
                let listed: Vec<String> = keys.iter().map(|k| k.display().to_string()).collect();
                return Err(format!(
                    "{alias} could use several keys ({}); name one: /ssh {alias} <key file>",
                    listed.join(", ")
                ));
            }
            keys
        }
    };
    let Some(key) = keys.first().filter(|k| k.is_file()) else {
        let at = keys
            .first()
            .map(|k| format!(" at {}", k.display()))
            .unwrap_or_default();
        return Err(format!(
            "no key file for {alias}{at}; name one: /ssh {alias} <key file>"
        ));
    };
    let key = std::fs::canonicalize(key).unwrap_or_else(|_| key.clone());
    // ssh -G lists several files on one line.
    let files: Vec<String> = ["userknownhostsfile", "globalknownhostsfile"]
        .iter()
        .flat_map(|k| opts.get(*k).into_iter().flatten())
        .flat_map(|v| v.split_whitespace().map(str::to_string))
        .collect();
    let files = if files.is_empty() {
        vec!["~/.ssh/known_hosts".to_string()]
    } else {
        files
    };
    let Some(known) = machine.known_in(&files, &hostname, port) else {
        return Err(format!(
            "{hostname} is not in your known_hosts: ssh {alias} once yourself to check and \
             accept its host key, then /ssh {alias}"
        ));
    };
    let mut agent = String::new();
    if machine.needs_passphrase(&key) {
        let sock = machine.agent_sock();
        let public = PathBuf::from(format!("{}.pub", key.display()));
        if !machine.agent_holds(&sock, machine.fingerprint(&public).as_deref()) {
            return Err(format!(
                "{} needs a passphrase and your ssh agent does not hold it; run ssh-add {} \
                 first (its .pub must sit next to it)",
                key.display(),
                key.display()
            ));
        }
        agent = sock;
    }
    Ok(Host {
        alias: alias.to_string(),
        hostname,
        user,
        port,
        key,
        known_hosts: known,
        agent,
        boxed,
    })
}

fn quote_opt(path: &str) -> String {
    format!("\"{}\"", path.replace('"', ""))
}

/// The ssh command line the client runs. Only `remote_cmd` comes from the
/// model, as the last argument after the host and `--`, so ssh never reads
/// it as an option.
pub fn argv(host: &Host, remote_cmd: &str) -> Vec<String> {
    let mut o: Vec<String> = [
        "-F",
        "/dev/null",
        "-i",
        &host.key.display().to_string(),
        "-p",
        &host.port.to_string(),
        "-l",
        &host.user,
        "-T",
        "-n",
        "-x",
        "-a",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    let agent = if host.agent.is_empty() {
        "none".to_string()
    } else {
        quote_opt(&host.agent)
    };
    let opts = [
        "IdentitiesOnly=yes".to_string(),
        format!("IdentityAgent={agent}"),
        "BatchMode=yes".into(),
        "PreferredAuthentications=publickey".into(),
        "StrictHostKeyChecking=yes".into(),
        "UpdateHostKeys=no".into(),
        format!(
            "UserKnownHostsFile={}",
            quote_opt(&host.known_hosts.display().to_string())
        ),
        "GlobalKnownHostsFile=/dev/null".into(),
        "ProxyCommand=none".into(),
        "ProxyJump=none".into(),
        "ControlMaster=no".into(),
        "ControlPath=none".into(),
        "ClearAllForwardings=yes".into(),
        "ForwardAgent=no".into(),
        "ForwardX11=no".into(),
        "PermitLocalCommand=no".into(),
        "Tunnel=no".into(),
        "EscapeChar=none".into(),
        "RequestTTY=no".into(),
        "ConnectTimeout=15".into(),
        "ServerAliveInterval=15".into(),
        "LogLevel=ERROR".into(),
    ];
    for opt in opts {
        o.push("-o".into());
        o.push(opt);
    }
    let mut out = vec!["ssh".to_string()];
    out.extend(o);
    out.extend([
        "--".to_string(),
        host.hostname.clone(),
        remote_cmd.to_string(),
    ]);
    out
}

/// `data` as a printf format of octal escapes: any POSIX sh, no NUL trouble.
fn octal(data: &[u8]) -> String {
    data.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "\\{b:03o}");
        s
    })
}

/// A GNU find expression for the names [`secrets::secret_name_reason`]
/// calls secret, so the server masks the same files this machine does.
pub fn secret_find() -> String {
    let q = shell_quote;
    let allow: Vec<String> = secrets::SECRET_ALLOW
        .iter()
        .map(|n| format!("! -name {}", q(n)))
        .collect();
    let mut files: Vec<&str> = secrets::SECRET_FILES.to_vec();
    files.sort_unstable();
    let mut names: Vec<String> = files.iter().map(|n| format!("-iname {}", q(n))).collect();
    names.extend(
        secrets::SECRET_SUFFIXES
            .iter()
            .map(|x| format!("-iname {}", q(&format!("*{x}")))),
    );
    let stem = format!(".*/{}", secrets::SECRET_STEM_PATTERN);
    let mut skip: Vec<&str> = crate::mounts::WALK_SKIP.to_vec();
    skip.push(".cache");
    skip.sort_unstable();
    skip.dedup();
    let mut skip: Vec<String> = skip.iter().map(|n| format!("-name {}", q(n))).collect();
    // The hidden folders are empty in the box: a file masked inside one
    // would show there by name, and count against the limit.
    skip.extend(
        BOX_HIDE_DIRS
            .iter()
            .map(|d| format!("-path \"$HOME\"/{}", q(d))),
    );
    format!(
        "find {} -maxdepth {BOX_SCAN_DEPTH} -regextype posix-extended \\( {} \\) -prune -o \\( \
         -type f -o -type l \\) ! -iname '*.pub' \\( -name .env -o \\( -name '.env.*' {} \\) -o \
         {} -o -iregex {} \\) -print 2>/dev/null",
        BOX_SCAN.join(" "),
        skip.join(" -o "),
        allow.join(" "),
        names.join(" -o "),
        q(&stem)
    )
}

/// The remote command line that runs `remote_cmd` in bubblewrap on the
/// server, for at most `timeout` seconds there (the client's own timeout
/// only ends ssh, which can leave the command running). POSIX sh, so the
/// login shell does not matter; the model's text is one quoted word given
/// to `bash -c` inside the sandbox.
pub fn box_command(remote_cmd: &str, timeout: u64) -> String {
    let q = shell_quote;
    let hide_dirs: Vec<String> = BOX_HIDE_DIRS
        .iter()
        .map(|d| format!("\"$HOME\"/{}", q(d)))
        .collect();
    let hide_files: Vec<String> = BOX_HIDE_FILES
        .iter()
        .map(|f| format!("\"$HOME\"/{}", q(f)))
        .collect();
    let work = format!("\"$HOME\"/{BOX_DIR}");
    let lines = [
        "set -- --ro-bind / / --proc /proc --dev /dev --tmpfs /tmp --tmpfs /var/tmp --tmpfs /run"
            .to_string(),
        format!("mkdir -p {work} || exit 125"),
        format!("set -- \"$@\" --bind {work} {work}"),
        format!(
            "for p in {}; do [ -e \"$p\" ] && set -- \"$@\" --ro-bind /dev/null \"$p\"; done",
            hide_files.join(" ")
        ),
        "n=0".into(),
        "while IFS= read -r p; do".into(),
        "  [ -n \"$p\" ] || continue".into(),
        format!("  n=$((n + 1)); [ $n -le {BOX_SCAN_LIMIT} ] || break"),
        "  set -- \"$@\" --ro-bind /dev/null \"$p\"".into(),
        "done <<RPCTL_EOF".into(),
        format!("$({})", secret_find()),
        "RPCTL_EOF".into(),
        // After the masks, so a tmpfs covers any made inside a hidden folder
        // (a $HOME the find's -path cannot match): bwrap mounts in order.
        format!(
            "for p in {}; do [ -d \"$p\" ] && set -- \"$@\" --tmpfs \"$p\"; done",
            hide_dirs.join(" ")
        ),
        // The filter goes in on fd 3 through a pipe, as here; nothing is
        // stored on the server for the model to change.
        format!(
            "printf '{}' | {{ exec 3<&0 </dev/null; exec timeout -k 5 {timeout} bwrap \"$@\" \
             --unshare-all --die-with-parent --new-session --cap-drop ALL --seccomp 3 --chdir \
             {work} --clearenv --setenv HOME \"$HOME\" --setenv USER \"$(id -un)\" --setenv PATH \
             {} --setenv LANG C.UTF-8 --setenv TERM dumb --setenv TMPDIR /tmp --setenv \
             RPCTL_SANDBOX 1 -- bash -c {}; }}",
            octal(&crate::sandbox::seccomp::program()),
            crate::sandbox::SANDBOX_PATH,
            q(remote_cmd)
        ),
    ];
    lines.join("\n")
}

/// What ssh asks the server to run for the model's `remote_cmd`.
pub fn command(host: &Host, remote_cmd: &str, timeout: u64) -> String {
    if host.boxed {
        box_command(remote_cmd, timeout)
    } else {
        remote_cmd.to_string()
    }
}

/// The environment ssh runs with: nothing of the client's but what ssh
/// needs; the agent socket only when the key lives in the agent.
pub fn env(host: &Host, home: &Path) -> Vec<(OsString, OsString)> {
    let mut e: Vec<(OsString, OsString)> = vec![
        ("PATH".into(), SSH_PATH.into()),
        ("HOME".into(), home.as_os_str().to_os_string()),
        (
            "LANG".into(),
            std::env::var_os("LANG").unwrap_or_else(|| "C.UTF-8".into()),
        ),
    ];
    if !host.agent.is_empty() {
        e.push(("SSH_AUTH_SOCK".into(), host.agent.clone().into()));
    }
    e
}

pub use crate::approve::Pending;

/// Running ssh for a lent host: [`run`] and [`probe_box`], behind a trait
/// so the chat's rules are tested without a server.
pub trait Ssh: Send + Sync {
    fn run<'a>(
        &'a self,
        host: &'a Host,
        remote: &'a str,
        cwd: &'a Path,
        home: &'a Path,
        timeout: Duration,
    ) -> Pending<'a, std::io::Result<Ran>>;
    fn probe<'a>(&'a self, host: &'a Host, home: &'a Path) -> Pending<'a, Option<String>>;
}

/// This machine's ssh.
pub struct SystemSsh;

impl Ssh for SystemSsh {
    fn run<'a>(
        &'a self,
        host: &'a Host,
        remote: &'a str,
        cwd: &'a Path,
        home: &'a Path,
        timeout: Duration,
    ) -> Pending<'a, std::io::Result<Ran>> {
        Box::pin(run(host, remote, cwd, home, timeout))
    }
    fn probe<'a>(&'a self, host: &'a Host, home: &'a Path) -> Pending<'a, Option<String>> {
        Box::pin(probe_box(host, home))
    }
}

/// How a remote run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs ssh for `host` with `remote` (from [`command`]) in `cwd`, outside
/// the sandbox, its process group killed when it runs over.
pub async fn run(
    host: &Host,
    remote: &str,
    cwd: &Path,
    home: &Path,
    timeout: Duration,
) -> std::io::Result<Ran> {
    let argv = argv(host, remote);
    // looked up in the PATH it runs with, as Python's subprocess does
    let program = find_executable(Some(SSH_PATH.as_ref()), "ssh")
        .map_or_else(|| "ssh".into(), PathBuf::into_os_string);
    let running = viy_exec::sandboxed::spawn(viy_exec::sandboxed::Spec {
        program,
        args: argv[1..].iter().map(OsString::from).collect(),
        cwd: cwd.to_path_buf(),
        env: env(host, home),
        pass_fds: Vec::new(),
    })?;
    let done = running.finish(timeout, true).await;
    Ok(Ran {
        exit_code: done.exit_code,
        timed_out: done.timed_out,
        stdout: done.stdout,
        stderr: done.stderr,
    })
}

/// Why boxed commands do not run on `host`, or `None` when they do. One
/// fixed command of the client's, run at `/ssh` time.
pub async fn probe_box(host: &Host, home: &Path) -> Option<String> {
    let checks = "[ \"$(uname -m)\" = x86_64 ] || { echo \"the seccomp filter is x86_64-only \
        (this server is $(uname -m))\" >&2; exit 3; }\ncommand -v bwrap >/dev/null || { echo \
        \"bubblewrap (bwrap) is not installed there\" >&2; exit 3; }\nfind --version 2>/dev/null \
        | grep -q GNU || { echo \"GNU find is not installed there\" >&2; exit 3; }\n";
    let script = format!("{checks}{}", box_command(&format!("echo {BOX_OK}"), 20));
    let res = match run(host, &script, home, home, SSH_PROBE_TIMEOUT).await {
        Ok(r) => r,
        Err(e) => return Some(format!("ssh did not start: {e}")),
    };
    // A login shell may print a banner first.
    if res.exit_code == Some(0) && res.stdout.split_whitespace().any(|w| w == BOX_OK) {
        return None;
    }
    if res.timed_out {
        return Some(format!("no answer within {}s", SSH_PROBE_TIMEOUT.as_secs()));
    }
    probe_reason(&res)
}

/// What a failed probe says (rp's last stderr line, `ssh failed:` before it
/// for exit 255).
pub fn probe_reason(res: &Ran) -> Option<String> {
    let code = res
        .exit_code
        .map_or_else(|| "None".to_string(), |c| c.to_string());
    let why = res
        .stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map_or_else(|| format!("exit {code}"), str::to_string);
    Some(if res.exit_code == Some(255) {
        format!("ssh failed: {why}")
    } else {
        why
    })
}

fn usage(host: &Host) -> String {
    format!(
        "write it as a command on its own: ssh {} '<command>', with pipes, redirections and \
         heredocs inside the quotes (they run on the server)",
        host.alias
    )
}

/// `(host, remote command)` when `cmd` is `ssh <lent host> <command>` and
/// nothing else; `None` when it is not about a lent host; [`Refused`] for
/// other uses of a lent host (options, local pipes, scp...), saying how to
/// write it (rp's `match`).
pub fn match_command<'a>(
    cmd: &str,
    hosts: &'a [Host],
) -> Result<Option<(&'a Host, String)>, Refused> {
    if hosts.is_empty() {
        return Ok(None);
    }
    // the last host of a target wins, as a dict built in order keeps it
    let by_target = |t: &str| -> Option<&'a Host> {
        hosts
            .iter()
            .rev()
            .find(|h| h.targets().iter().any(|x| x == t))
    };
    let words = py_split(cmd);
    if let Some(words) = words.as_ref().filter(|w| w.len() > 1 && w[0] == "ssh") {
        let mut rest: &[String] = &words[1..];
        let host = if let Some(h) = by_target(&rest[0].to_lowercase()) {
            rest = &rest[1..];
            h
        } else if rest[0].starts_with('-')
            && let Some(h) = rest.iter().find_map(|w| by_target(&w.to_lowercase()))
        {
            return Err(Refused(format!(
                "ssh options are not allowed (the client sets the key and options); {}",
                usage(h)
            )));
        } else {
            return Ok(None);
        };
        let parts = crate::approve::split_shell(cmd);
        if parts.as_ref().is_none_or(|p| p.len() != 1) {
            return Err(Refused(format!(
                "only a single ssh command runs on {}, and $( ) or backticks must be inside \
                 single quotes; {}",
                host.alias,
                usage(host)
            )));
        }
        if rest.first().is_some_and(|w| w == "--") {
            rest = &rest[1..];
        }
        if rest.first().is_some_and(|w| w.starts_with('-')) {
            return Err(Refused(format!(
                "ssh options are not allowed (the client sets the key and options); {}",
                usage(host)
            )));
        }
        let operator = |w: &str| !w.is_empty() && w.chars().all(|c| "|&;()<>".contains(c));
        if punct_tokens(cmd)
            .unwrap_or_default()
            .iter()
            .any(|w| operator(w))
        {
            return Err(Refused(format!(
                "a redirection outside the quotes would act on this machine; {}",
                usage(host)
            )));
        }
        let joined = rest.join(" ");
        // Python's strip(): \x1c-\x1f are whitespace to it too
        if rest.is_empty() || joined.trim_matches(crate::approve::is_py_space).is_empty() {
            return Err(Refused(format!(
                "there is no interactive login; {}",
                usage(host)
            )));
        }
        return Ok(Some((host, joined)));
    }
    if REMOTE_TOOL.is_match(cmd) {
        let low = cmd.to_lowercase();
        let mut seen: Vec<String> = Vec::new();
        for h in hosts {
            for t in h.targets() {
                if seen.contains(&t) {
                    continue;
                }
                seen.push(t.clone());
                let pattern = format!(
                    r#"(^|[{PY_SPACE}@'"]){}([{PY_SPACE}:'"]|$)"#,
                    regex::escape(&t)
                );
                if re(&pattern).is_match(&low) {
                    let host = by_target(&t).unwrap_or(h);
                    return Err(Refused(format!(
                        "{} is reached only by ssh run by the client, not from the sandbox (no \
                         scp, rsync or sshfs; copy a file with ssh {} 'cat > path <<\"EOF\" ... \
                         EOF'); {}",
                        host.alias,
                        host.alias,
                        usage(host)
                    )));
                }
            }
        }
    }
    Ok(None)
}

/// The sudo/doas/pkexec/su that `remote_cmd` runs, or `None`.
pub fn root_word(remote_cmd: &str) -> Option<String> {
    ROOT.captures(remote_cmd).map(|c| c[2].to_string())
}

/// `text` for a question the user answers: every control character but
/// newline and tab written out, so nothing in it can hide the rest.
pub fn shown(text: &str) -> String {
    static UNPRINTABLE: LazyLock<Regex> =
        LazyLock::new(|| re(r"[\p{Cc}\p{Cf}\p{Co}\p{Cn}\p{Zl}\p{Zp}\p{Zs}]"));
    let mut out = String::new();
    for c in text.chars() {
        let printable = c == ' ' || !UNPRINTABLE.is_match(c.encode_utf8(&mut [0; 4]));
        if c == '\n' || c == '\t' || printable {
            out.push(c);
        } else if (c as u32) < 0x100 {
            let _ = write!(out, "\\x{:02x}", c as u32);
        } else {
            let _ = write!(out, "\\u{:04x}", c as u32);
        }
    }
    out
}

/// What the model is told, as the first part of the next user message (not
/// the system prompt: every change there re-prefills the history).
pub fn note(hosts: &[Host], gone: &[String]) -> String {
    let text = if let Some(first) = hosts.first() {
        let listed: Vec<String> = hosts
            .iter()
            .map(|h| {
                format!(
                    "{} ({}, {})",
                    h.alias,
                    h.login(),
                    if h.boxed { "sandboxed" } else { "full access" }
                )
            })
            .collect();
        let mut text = format!(
            "the user lent you ssh access to {}. Run a command there with run_shell, as a \
             command on its own: ssh {} '<command>'. The client runs it on the server with the \
             user's key, which you never see, and the user approves each one. No ssh options, \
             no interactive login, no stdin, no scp or rsync: pipes, redirections and heredocs \
             go inside the quotes and run on the server. Leave needs_network unset.",
            listed.join("; "),
            first.alias
        );
        if hosts.iter().any(|h| h.boxed) {
            text += &format!(
                " On a sandboxed server the command runs in bash in ~/{BOX_DIR}, the only \
                 writable directory, kept between commands; the rest of the server's files are \
                 read-only, ~/.ssh, ~/.config, ~/.local/share and secret files are hidden, \
                 there is no network and no other process is visible, so systemctl, docker, \
                 sudo and package installs do not work. If a task needs them, say so: only the \
                 user can lend the server with full access."
            );
        }
        if !gone.is_empty() {
            text += &format!(" No longer lent: {}.", gone.join(", "));
        }
        text
    } else {
        let mut text = if gone.is_empty() {
            "no ssh hosts are lent to you now".to_string()
        } else {
            format!("the user took back ssh access to {}", gone.join(", "))
        };
        text += "; ssh commands no longer run anywhere but in the sandbox.";
        text
    };
    format!("{NOTE_PREFIX}{text}]")
}

/// Whether `text` is an ssh note, viy's or one rp wrote.
pub fn is_note(text: &str) -> bool {
    text.starts_with(NOTE_PREFIX) || text.starts_with(RP_NOTE_PREFIX)
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
