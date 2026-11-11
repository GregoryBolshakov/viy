//! The egress proxy for sandboxed shell commands with network (rpctl
//! egress.py, netaccess Stage 6, netaccess kb d007).
//!
//! A network command runs in its own network namespace with nothing but a
//! loopback. Its only way out is 127.0.0.1:3128 there, a listening socket
//! the sandbox hands over ([`crate::sandbox::run`]), and a [`Proxy`] serves
//! it for exactly as long as the command runs. Everything the command sends
//! is hostile input: the model is untrusted, and so is whatever its install
//! scripts run.
//!
//! Accepted: CONNECT host:port (HTTPS; TLS stays end to end, the proxy never
//! sees plaintext) and absolute-form plain HTTP, rewritten to origin form
//! with Proxy-* headers dropped, Host taken from the URL and `Connection:
//! close` forced, so one connection cannot be reused for a second host.
//! Anything else is a 400.
//!
//! Per connection, in order; the first failure answers 403 with the reason
//! in X-Proxy-Blocked and the body (rpctl's words, viy's name in the body,
//! none in the header, kb d131: the model reads them):
//!   1. the target passes netguard's host rules: no userinfo, IDNA, no
//!      local names, IP literals public, and no IP spelled any other way
//!      (127.1, 2130706433, 0x7f.0.0.1) (T4)
//!   2. the port is 443 or 80, or one written next to the host in the
//!      command; never 22 (T4)
//!   3. the host is a package registry, written in the command, allowed by
//!      the user for this chat, or in SANDBOX_NET_ALLOW. Checked before any
//!      lookup, so a blocked name never reaches DNS (T6, T9)
//!   4. it is resolved once and every address must be public; the
//!      connection goes to a vetted address, never a second lookup (DNS
//!      rebinding, T4)
//!   5. bytes are relayed until both sides close, IDLE_TIMEOUT passes
//!      without traffic, or the command ends ([`Proxy`]'s `close`)

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::sync::LazyLock;

use regex::Regex;

use crate::approve::is_ascii_word;
use crate::approve::is_py_space;
use crate::netguard;

#[cfg(test)]
pub(crate) mod fakes;
mod head;
mod proxy;

pub use head::response;
pub use proxy::Connector;
pub use proxy::Event;
pub use proxy::Limits;
pub use proxy::Proxy;
pub use proxy::Report;
pub use proxy::Target;
pub use proxy::TcpConnector;

pub const PROXY_HOST: &str = "127.0.0.1";
pub const PROXY_PORT: u16 = 3128;

/// Hosts that serve packages and source and whose request logs the author
/// of a package cannot read, so an install script cannot use them to carry
/// data out. Exact names; the Hugging Face file CDN was read from live
/// redirects (rpctl netaccess kb f030), not guessed.
pub const REGISTRIES: [&str; 16] = [
    "pypi.org",
    "files.pythonhosted.org",
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "crates.io",
    "index.crates.io",
    "static.crates.io",
    "proxy.golang.org",
    "sum.golang.org",
    "github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "raw.githubusercontent.com",
    "huggingface.co",
    "us.aws.cdn.hf.co",
];
const DEFAULT_PORTS: [u16; 2] = [80, 443];
const NEVER_PORTS: [u16; 1] = [22];

/// The reason a host outside the policy is blocked.
pub const NOT_ALLOWED: &str = "not a registry and not in the command";

/// The request breaks a rule; the message says which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked(pub String);

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn rx(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => panic!("egress pattern {pattern:?}: {e}"),
    }
}

/// Python's `\s` (`str.isspace`), written out: Rust's `\s` lacks the
/// information separators `\x1c`-`\x1f` (flags F41).
/// Python's `\s` on str as a regex class's inside (shared with
/// [`crate::remote`]).
pub(crate) const PY_SPACE: &str = r"\t\n\x0B\x0C\r\x1C-\x1F \x{85}\x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}";

/// rpctl's `_URL_AUTH` without its leading `\b`, which [`url_hosts`]
/// decides itself.
static URL_AUTH: LazyLock<Regex> = LazyLock::new(|| {
    rx(&format!(
        r#"[A-Za-z][A-Za-z0-9+.-]*://([^/{PY_SPACE}?#'"`<>()\\;|&]+)"#
    ))
});
static BARE_ARG: LazyLock<Regex> = LazyLock::new(|| {
    rx(&format!(
        r"^((?:[A-Za-z0-9-]+\.)+[A-Za-z]{{2,63}}|\[[0-9A-Fa-f:.]+\])(?::([0-9]{{1,5}}))?(?:/[^{PY_SPACE}]*)?$"
    ))
});
static PORT: LazyLock<Regex> = LazyLock::new(|| rx(r"^[0-9]{1,5}$"));

/// Options of curl, wget and git clone whose next word is a value, not a
/// URL.
const CURL_VALUE_OPTS: &[&str] = &[
    "-o",
    "--output",
    "-H",
    "--header",
    "-d",
    "--data",
    "--data-raw",
    "--data-binary",
    "--data-urlencode",
    "-u",
    "--user",
    "-X",
    "--request",
    "-A",
    "--user-agent",
    "-e",
    "--referer",
    "-T",
    "--upload-file",
    "-F",
    "--form",
    "-b",
    "--cookie",
    "-c",
    "--cookie-jar",
    "-w",
    "--write-out",
    "-x",
    "--proxy",
    "-K",
    "--config",
    "-m",
    "--max-time",
    "--connect-timeout",
    "-r",
    "--range",
    "-C",
    "--continue-at",
    "-E",
    "--cert",
    "--cacert",
    "--key",
    "-U",
    "--proxy-user",
    "-Y",
    "-y",
    "-z",
    "--retry",
    "--resolve",
    "--connect-to",
    "--interface",
    "--output-dir",
    "-D",
    "--dump-header",
    "--limit-rate",
    "--noproxy",
];
const WGET_VALUE_OPTS: &[&str] = &[
    "-O",
    "--output-document",
    "-o",
    "--output-file",
    "-a",
    "--append-output",
    "-P",
    "--directory-prefix",
    "-U",
    "--user-agent",
    "-e",
    "--execute",
    "-t",
    "--tries",
    "-T",
    "--timeout",
    "-w",
    "--wait",
    "-Q",
    "--quota",
    "-l",
    "--level",
    "-i",
    "--input-file",
    "-B",
    "--base",
    "--header",
    "--user",
    "--password",
    "--post-data",
    "--post-file",
    "--referer",
    "-A",
    "--accept",
    "-R",
    "--reject",
    "-D",
    "--domains",
    "-X",
    "-I",
];
const GIT_CLONE_VALUE_OPTS: &[&str] = &[
    "-b",
    "--branch",
    "--depth",
    "-c",
    "--config",
    "-o",
    "--origin",
    "-j",
    "--jobs",
    "-u",
    "--upload-pack",
    "--template",
    "--reference",
    "--separate-git-dir",
    "--filter",
    "--shallow-since",
    "--shallow-exclude",
    "--server-option",
];
const GIT_GLOBAL_VALUE_OPTS: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace"];
const OPERATORS: &[&str] = &[";", "&", "&&", "|", "||", "(", ")", "\n", "|&", ";;"];

// ------------------------------------------------------------------ policy

/// The host as the policy compares it, or why it is refused.
pub fn canonical_host(raw: &str) -> Result<String, Blocked> {
    let mut host = raw.trim_matches(is_py_space).trim_end_matches('.');
    if host.len() >= 2 && host.starts_with('[') && host.ends_with(']') {
        host = &host[1..host.len() - 1];
    }
    if host.is_empty() {
        return Err(Blocked("the request names no host".into()));
    }
    if host.contains('@') {
        return Err(Blocked(
            "user names and passwords in the target are refused".into(),
        ));
    }
    let host = netguard::check_host(host).map_err(|e| Blocked(e.0))?;
    if netguard::parse_ip(&host).is_none() {
        // inet_aton spellings (127.1, 2130706433, 0x7f.0.0.1, 0177.0.0.1)
        // and anything ending in a numeric label: no real domain looks
        // like that.
        let last = host.rsplit('.').next().unwrap_or_default();
        let numeric =
            inet_aton(&host) || !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit());
        if numeric {
            return Err(Blocked(format!(
                "{host} is an IP address in a non-standard spelling"
            )));
        }
        return Ok(host);
    }
    // IDNA can turn digits into an IP.
    match netguard::address_problem(&host) {
        Some(why) => Err(Blocked(why)),
        None => Ok(host),
    }
}

/// Whether the C library's `inet_aton` reads `text` as an IPv4 address,
/// as Python's `socket.inet_aton` does.
fn inet_aton(text: &str) -> bool {
    unsafe extern "C" {
        fn inet_aton(cp: *const libc::c_char, inp: *mut libc::in_addr) -> libc::c_int;
    }
    let Ok(c) = CString::new(text) else {
        return false;
    };
    let mut out = libc::in_addr { s_addr: 0 };
    // SAFETY: `c` is a NUL-terminated string and `out` a valid in_addr.
    unsafe { inet_aton(c.as_ptr(), &mut out) != 0 }
}

/// `(host, port)` of `host`, `host:port`, `[v6]` or `[v6]:port`.
pub fn split_authority(auth: &str) -> Result<(&str, Option<u16>), Blocked> {
    let bad = || Blocked("the target is not a valid host:port".into());
    let (host, port) = if let Some(inner) = auth.strip_prefix('[') {
        let end = inner.find(']').ok_or_else(bad)?;
        let (host, rest) = (&inner[..end], &inner[end + 1..]);
        if rest.is_empty() {
            return Ok((host, None));
        }
        (host, rest.strip_prefix(':').ok_or_else(bad)?)
    } else {
        match auth.rsplit_once(':') {
            Some(split) => split,
            None => return Ok((auth, None)),
        }
    };
    match port.parse::<u16>() {
        Ok(p) if PORT.is_match(port) && p > 0 => Ok((host, Some(p))),
        _ => Err(Blocked(
            "the target's port is not a number from 1 to 65535".into(),
        )),
    }
}

/// The hosts a command names, with the ports written next to them.
pub type CommandHosts = BTreeMap<String, BTreeSet<u16>>;

fn note(found: &mut CommandHosts, auth: &str) {
    let Ok((host, port)) = split_authority(auth) else {
        return;
    };
    let Ok(host) = canonical_host(host) else {
        return;
    };
    let ports = found.entry(host).or_default();
    ports.extend(port);
}

/// Scheme starts whose word boundary a non-ASCII character decides that
/// [`url_hosts`] follows both ways; past them it counts no more URLs.
const MAX_UNSURE: usize = 8;

/// Whether rpctl's `\b` holds before the ASCII letter at `at`: `Some` when
/// Python and Rust agree, `None` when a non-ASCII character before it
/// decides (Python's `\w` is `str.isalnum`, Unicode 13; Rust's is not).
fn boundary(cmd: &str, at: usize) -> Option<bool> {
    match cmd[..at].chars().next_back() {
        None => Some(true),
        Some(c) if c.is_ascii() => Some(!is_ascii_word(c)),
        Some(c) if is_py_space(c) => Some(true),
        Some(_) => None,
    }
}

/// The hosts of URLs rpctl's `_URL_AUTH.finditer` finds from `pos`, and
/// only those: where a non-ASCII character decides whether a scheme starts
/// there, both readings are followed (matches do not overlap, so one start
/// moves every later match) and a host counts only if both find it. On
/// ASCII text this is rpctl's scan exactly; elsewhere it finds no host
/// rpctl's does not.
fn url_hosts(cmd: &str, mut pos: usize, budget: &mut usize) -> CommandHosts {
    let mut found = CommandHosts::new();
    while let Some(m) = URL_AUTH.captures_at(cmd, pos) {
        let (start, end) = (
            m.get(0).map_or(pos, |g| g.start()),
            m.get(0).map_or(pos, |g| g.end()),
        );
        let next = start + cmd[start..].chars().next().map_or(1, char::len_utf8);
        let auth = &m[1];
        let auth = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
        match boundary(cmd, start) {
            Some(true) => {
                note(&mut found, auth);
                pos = end;
            }
            Some(false) => pos = next,
            None => {
                if *budget == 0 {
                    return found;
                }
                *budget -= 1;
                let mut here = url_hosts(cmd, end, budget);
                note(&mut here, auth);
                let skipped = url_hosts(cmd, next, budget);
                for (host, ports) in here {
                    if let Some(theirs) = skipped.get(&host) {
                        let both: BTreeSet<u16> = ports.intersection(theirs).copied().collect();
                        found.entry(host).or_default().extend(both);
                    }
                }
                return found;
            }
        }
    }
    found
}

/// The last component of a path as `PurePosixPath(word).name` gives it.
fn path_name(word: &str) -> &str {
    word.split('/')
        .rfind(|p| !p.is_empty() && *p != ".")
        .unwrap_or_default()
}

/// `{host: {ports written next to it}}` for the hosts a command names:
/// hosts of URLs anywhere in its text, and the bare host of curl, wget and
/// git clone arguments. Local names and non-public addresses are left out
/// (they are refused at connect time anyway).
pub fn hosts_in_command(cmd: &str) -> CommandHosts {
    let mut found = CommandHosts::new();
    let mut budget = MAX_UNSURE;
    for (host, ports) in url_hosts(cmd, 0, &mut budget) {
        found.entry(host).or_default().extend(ports);
    }
    let words = shlex_words(cmd).unwrap_or_else(|| {
        cmd.split(is_py_space)
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    });
    let (mut i, n) = (0, words.len());
    while i < n {
        let prog = path_name(&words[i]);
        i += 1;
        let takes_value = match prog {
            "git" => {
                while i < n && words[i].starts_with('-') {
                    i += if GIT_GLOBAL_VALUE_OPTS.contains(&words[i].as_str()) {
                        2
                    } else {
                        1
                    };
                }
                if i >= n || words[i] != "clone" {
                    continue;
                }
                i += 1;
                GIT_CLONE_VALUE_OPTS
            }
            "curl" => CURL_VALUE_OPTS,
            "wget" => WGET_VALUE_OPTS,
            _ => continue,
        };
        while i < n && !OPERATORS.contains(&words[i].as_str()) {
            let w = &words[i];
            i += 1;
            if w.starts_with('-') {
                if takes_value.contains(&w.as_str()) {
                    i += 1;
                }
                continue;
            }
            if let Some(m) = BARE_ARG.captures(w)
                && !w.contains("://")
            {
                let auth = match m.get(2) {
                    Some(p) => format!("{}:{}", &m[1], p.as_str()),
                    None => m[1].to_string(),
                };
                note(&mut found, &auth);
            }
        }
    }
    found
}

/// The words rpctl's `shlex.shlex(cmd, posix=True, punctuation_chars=True)`
/// with `whitespace_split` reads: whitespace is space, tab, CR and LF; runs
/// of `();<>|&` are words of their own; `#` starts a comment to the end of
/// the line, inside a word too; quotes and `\` as POSIX shlex. `None` where
/// shlex raises (an unclosed quote, a trailing `\`), and rpctl falls back
/// to splitting at whitespace.
pub(crate) fn shlex_words(cmd: &str) -> Option<Vec<String>> {
    const PUNCT: &str = "();<>|&";
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Space,
        Word,
        Punct,
        Quote(char),
        Escape(char),
    }
    let chars: Vec<char> = cmd.chars().collect();
    let mut words = Vec::new();
    let (mut i, n) = (0, chars.len());
    let mut state = State::Space;
    let mut token = String::new();
    let mut quoted = false;
    // The state an escape returns to.
    let mut escaped_from = State::Word;
    let skip_line = |i: &mut usize| {
        while *i < n && chars[*i] != '\n' {
            *i += 1;
        }
        if *i < n {
            *i += 1;
        }
    };
    let emit = |words: &mut Vec<String>, token: &mut String, quoted: &mut bool| {
        if !token.is_empty() || *quoted {
            words.push(std::mem::take(token));
        }
        *quoted = false;
    };
    loop {
        let c = chars.get(i).copied();
        match state {
            State::Space => {
                let Some(c) = c else { break };
                i += 1;
                match c {
                    ' ' | '\t' | '\r' | '\n' => {}
                    '#' => skip_line(&mut i),
                    '\\' => {
                        escaped_from = State::Word;
                        state = State::Escape('\\');
                    }
                    '\'' | '"' => state = State::Quote(c),
                    _ if PUNCT.contains(c) => {
                        token.push(c);
                        state = State::Punct;
                    }
                    _ => {
                        token.push(c);
                        state = State::Word;
                    }
                }
            }
            State::Word | State::Punct => {
                let Some(c) = c else {
                    emit(&mut words, &mut token, &mut quoted);
                    break;
                };
                match c {
                    ' ' | '\t' | '\r' | '\n' => {
                        i += 1;
                        state = State::Space;
                        emit(&mut words, &mut token, &mut quoted);
                    }
                    '#' => {
                        i += 1;
                        skip_line(&mut i);
                        state = State::Space;
                        emit(&mut words, &mut token, &mut quoted);
                    }
                    _ if state == State::Punct => {
                        if PUNCT.contains(c) {
                            i += 1;
                            token.push(c);
                        } else {
                            // Left for the next word.
                            state = State::Space;
                            emit(&mut words, &mut token, &mut quoted);
                        }
                    }
                    '\'' | '"' => {
                        i += 1;
                        state = State::Quote(c);
                    }
                    '\\' => {
                        i += 1;
                        escaped_from = State::Word;
                        state = State::Escape('\\');
                    }
                    _ if PUNCT.contains(c) => {
                        state = State::Space;
                        emit(&mut words, &mut token, &mut quoted);
                    }
                    _ => {
                        i += 1;
                        token.push(c);
                    }
                }
            }
            State::Quote(q) => {
                quoted = true;
                let c = c?;
                i += 1;
                if c == q {
                    state = State::Word;
                } else if c == '\\' && q == '"' {
                    escaped_from = state;
                    state = State::Escape('\\');
                } else {
                    token.push(c);
                }
            }
            State::Escape(e) => {
                let c = c?;
                i += 1;
                // Inside quotes only the quote itself or the escape may be
                // escaped.
                if let State::Quote(q) = escaped_from
                    && c != e
                    && c != q
                {
                    token.push(e);
                }
                token.push(c);
                state = escaped_from;
            }
        }
    }
    Some(words)
}

/// SANDBOX_NET_ALLOW: `(exact hosts, suffixes)`. `*.example.com` allows the
/// subdomains of example.com, not example.com itself; no other wildcard.
pub fn parse_allow(text: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let (mut exact, mut suffixes) = (BTreeSet::new(), BTreeSet::new());
    for item in text.replace(' ', ",").split(',') {
        let item = item.trim_matches(is_py_space).to_lowercase();
        if item.is_empty() {
            continue;
        }
        let wild = item.strip_prefix("*.");
        let Ok(host) = canonical_host(wild.unwrap_or(&item)) else {
            continue;
        };
        if wild.is_some() {
            // *.local would only match local names.
            if canonical_host(&format!("x.{host}")).is_err() {
                continue;
            }
            suffixes.insert(format!(".{host}"));
        } else {
            exact.insert(host);
        }
    }
    (exact, suffixes)
}

/// Which hosts and ports one command may reach.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub command_hosts: CommandHosts,
    pub user_hosts: BTreeSet<String>,
    pub allow_exact: BTreeSet<String>,
    pub allow_suffixes: BTreeSet<String>,
}

impl Policy {
    /// The policy of `command`, with the hosts the user allowed for this
    /// chat and SANDBOX_NET_ALLOW.
    pub fn new<'a>(
        command: &str,
        user_hosts: impl IntoIterator<Item = &'a str>,
        allow: &str,
    ) -> Self {
        let (allow_exact, allow_suffixes) = parse_allow(allow);
        Self {
            command_hosts: hosts_in_command(command),
            user_hosts: user_hosts
                .into_iter()
                .filter_map(|h| canonical_host(h).ok())
                .collect(),
            allow_exact,
            allow_suffixes,
        }
    }

    pub fn host_allowed(&self, host: &str) -> bool {
        REGISTRIES.contains(&host)
            || self.command_hosts.contains_key(host)
            || self.user_hosts.contains(host)
            || self.allow_exact.contains(host)
            || self
                .allow_suffixes
                .iter()
                .any(|s| host.ends_with(s.as_str()))
    }

    /// `None` when `host:port` may be reached, else the reason.
    pub fn why_not(&self, host: &str, port: u16) -> Option<String> {
        if NEVER_PORTS.contains(&port) {
            return Some(format!("port {port} (ssh) is never allowed"));
        }
        let written = self
            .command_hosts
            .get(host)
            .is_some_and(|ports| ports.contains(&port));
        if !DEFAULT_PORTS.contains(&port) && !written {
            return Some(format!(
                "port {port} is not allowed (only 80, 443 and a port written with the host in \
                 the command)"
            ));
        }
        if !self.host_allowed(host) {
            return Some(NOT_ALLOWED.into());
        }
        None
    }

    /// For the approval prompt and the reviewer, e.g. `registries +
    /// example.com (from the command)`.
    pub fn describe(&self) -> String {
        fn listed<'a>(hosts: impl IntoIterator<Item = &'a str>) -> String {
            let mut hosts: Vec<&str> = hosts.into_iter().collect();
            hosts.sort_unstable();
            let mut out = hosts.iter().take(8).copied().collect::<Vec<_>>().join(", ");
            if hosts.len() > 8 {
                out.push_str(&format!(" and {} more", hosts.len() - 8));
            }
            out
        }
        let registry = |h: &str| REGISTRIES.contains(&h);
        let mut parts = vec!["registries".to_string()];
        let cmd: Vec<&str> = self
            .command_hosts
            .keys()
            .map(String::as_str)
            .filter(|h| !registry(h))
            .collect();
        if !cmd.is_empty() {
            parts.push(format!("{} (from the command)", listed(cmd)));
        }
        let user: Vec<&str> = self
            .user_hosts
            .iter()
            .map(String::as_str)
            .filter(|h| !registry(h) && !self.command_hosts.contains_key(*h))
            .collect();
        if !user.is_empty() {
            parts.push(format!("{} (you allowed)", listed(user)));
        }
        let wild: Vec<String> = self
            .allow_suffixes
            .iter()
            .map(|s| format!("*{s}"))
            .collect();
        let extra: Vec<&str> = self
            .allow_exact
            .iter()
            .map(String::as_str)
            .chain(wild.iter().map(String::as_str))
            .collect();
        if !extra.is_empty() {
            parts.push(format!("{} (SANDBOX_NET_ALLOW)", listed(extra)));
        }
        parts.join(" + ")
    }
}

#[cfg(test)]
#[path = "egress_tests.rs"]
mod tests;
