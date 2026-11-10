//! A URL's parts as rpctl reads them: `fetch_url` splits a URL, takes its
//! host and port, and joins a redirect's Location onto it with Python
//! 3.10.12's `urllib.parse` (`urlsplit`, `hostname`, `port`, `urljoin`).
//! The safeguard decides on these parts, so they are rpctl's, not WHATWG's
//! (the `url` crate reads `http://0x7f.1/` as an address and `\` as `/`,
//! where rpctl reads a name and refuses the backslash). Only what
//! `fetch_url` uses is here: str input, fragments split off.

/// `urlsplit`'s five parts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parts {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
}

/// What may lead and is removed (WHATWG's C0 controls and space).
fn c0_or_space(c: char) -> bool {
    c <= ' '
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// `urlsplit(url, scheme)`, or its ValueError's message.
pub fn split(url: &str, default_scheme: &str) -> Result<Parts, String> {
    let url: String = url
        .trim_start_matches(c0_or_space)
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut scheme: String = default_scheme
        .trim_matches(c0_or_space)
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut rest = url.as_str();
    if let Some(i) = rest.find(':')
        && i > 0
        && rest[..i].chars().all(is_scheme_char)
    {
        scheme = rest[..i].to_ascii_lowercase();
        rest = &rest[i + 1..];
    }
    let mut netloc = "";
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = &after[..end];
        rest = &after[end..];
        let (open, close) = (netloc.contains('['), netloc.contains(']'));
        if open != close {
            return Err("Invalid IPv6 URL".into());
        }
        if open {
            check_bracketed_netloc(netloc)?;
        }
    }
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    check_netloc(netloc)?;
    Ok(Parts {
        scheme,
        netloc: netloc.into(),
        path: path.into(),
        query: query.into(),
        fragment: fragment.into(),
    })
}

/// `_check_bracketed_netloc`: a bracket opens the host, and what is in it
/// is an IPv6 address or an IPvFuture.
fn check_bracketed_netloc(netloc: &str) -> Result<(), String> {
    let hostport = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let host = match hostport.split_once('[') {
        Some((before, bracketed)) => {
            if !before.is_empty() {
                return Err("Invalid IPv6 URL".into());
            }
            let (host, port) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            if !port.is_empty() && !port.starts_with(':') {
                return Err("Invalid IPv6 URL".into());
            }
            host
        }
        None => hostport.split_once(':').map_or(hostport, |(h, _)| h),
    };
    if let Some(future) = host.strip_prefix('v') {
        let ok = future.split_once('.').is_some_and(|(hex, tail)| {
            !hex.is_empty()
                && hex.chars().all(|c| c.is_ascii_hexdigit())
                && !tail.is_empty()
                && !tail.contains('\n')
        });
        return if ok {
            Ok(())
        } else {
            Err("IPvFuture address is invalid".into())
        };
    }
    match super::parse_ip(host) {
        Some(std::net::IpAddr::V6(_)) => Ok(()),
        Some(std::net::IpAddr::V4(_)) => Err("An IPv4 address cannot be in brackets".into()),
        None => Err(format!(
            "{} does not appear to be an IPv4 or IPv6 address",
            py_repr(host)
        )),
    }
}

/// `_checknetloc`: a non-ASCII netloc whose NFKC form gains a delimiter.
fn check_netloc(netloc: &str) -> Result<(), String> {
    if netloc.is_ascii() {
        return Ok(());
    }
    let n: String = netloc
        .chars()
        .filter(|c| !matches!(c, '@' | ':' | '#' | '?'))
        .collect();
    let normalized = nfkc_of(&n);
    if normalized == n {
        return Ok(());
    }
    if normalized.contains(['/', '?', '#', '@', ':']) {
        return Err(format!(
            "netloc '{netloc}' contains invalid characters under NFKC normalization"
        ));
    }
    Ok(())
}

/// The netloc's host and port text, as `_hostinfo` splits them.
fn host_info(netloc: &str) -> (&str, Option<&str>) {
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let (host, port) = match hostinfo.split_once('[') {
        Some((_, bracketed)) => {
            let (host, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            (host, after.split_once(':').map_or("", |(_, p)| p))
        }
        None => hostinfo.split_once(':').unwrap_or((hostinfo, "")),
    };
    (host, (!port.is_empty()).then_some(port))
}

impl Parts {
    /// `.hostname`: lower-case but for a zone id, `None` when empty.
    pub fn hostname(&self) -> Option<String> {
        let (host, _) = host_info(&self.netloc);
        if host.is_empty() {
            return None;
        }
        Some(match host.split_once('%') {
            Some((h, zone)) => format!("{}%{zone}", py_lower(h)),
            None => py_lower(host),
        })
    }

    /// `.port`, or its ValueError's message.
    pub fn port(&self) -> Result<Option<u16>, String> {
        let Some(port) = host_info(&self.netloc).1 else {
            return Ok(None);
        };
        if !port.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!(
                "Port could not be cast to integer value as {}",
                py_repr(port)
            ));
        }
        // Digits only, so the only failure is size.
        let value = port.trim_start_matches('0');
        if value.len() > 5 {
            return Err("Port out of range 0-65535".into());
        }
        let n: u32 = if value.is_empty() {
            0
        } else {
            value.parse().unwrap_or(u32::MAX)
        };
        u16::try_from(n)
            .map(Some)
            .map_err(|_| "Port out of range 0-65535".into())
    }
}

/// `urlunsplit` for a scheme that uses a netloc (http, https).
pub fn unsplit(parts: &Parts) -> String {
    let mut url = parts.path.clone();
    if !parts.netloc.is_empty()
        || (!parts.scheme.is_empty() && uses_netloc(&parts.scheme) && !url.starts_with("//"))
    {
        if !url.is_empty() && !url.starts_with('/') {
            url.insert(0, '/');
        }
        url = format!("//{}{url}", parts.netloc);
    }
    if !parts.scheme.is_empty() {
        url = format!("{}:{url}", parts.scheme);
    }
    if !parts.query.is_empty() {
        url = format!("{url}?{}", parts.query);
    }
    if !parts.fragment.is_empty() {
        url = format!("{url}#{}", parts.fragment);
    }
    url
}

const USES_RELATIVE: [&str; 19] = [
    "", "ftp", "http", "gopher", "nntp", "imap", "wais", "file", "https", "shttp", "mms",
    "prospero", "rtsp", "rtspu", "sftp", "svn", "svn+ssh", "ws", "wss",
];
const USES_NETLOC: [&str; 25] = [
    "", "ftp", "http", "gopher", "nntp", "telnet", "imap", "wais", "file", "mms", "https", "shttp",
    "snews", "prospero", "rtsp", "rtspu", "rsync", "svn", "svn+ssh", "sftp", "nfs", "git",
    "git+ssh", "ws", "wss",
];
const USES_PARAMS: [&str; 15] = [
    "", "ftp", "hdl", "prospero", "http", "imap", "https", "shttp", "rtsp", "rtspu", "sip", "sips",
    "mms", "sftp", "tel",
];

fn uses_netloc(scheme: &str) -> bool {
    USES_NETLOC.contains(&scheme)
}

/// `urlparse`: `urlsplit` with the last segment's `;params` split off.
fn parse6(url: &str, default_scheme: &str) -> Result<(Parts, String), String> {
    let mut parts = split(url, default_scheme)?;
    let mut params = String::new();
    if USES_PARAMS.contains(&parts.scheme.as_str()) && parts.path.contains(';') {
        let path = parts.path.clone();
        let from = path.rfind('/').unwrap_or(0);
        let at = if path.contains('/') {
            path[from..].find(';').map(|i| from + i)
        } else {
            path.find(';')
        };
        if let Some(i) = at {
            params = path[i + 1..].to_string();
            parts.path = path[..i].to_string();
        }
    }
    Ok((parts, params))
}

/// `urlunparse`.
fn unparse6(mut parts: Parts, params: &str) -> String {
    if !params.is_empty() {
        parts.path = format!("{};{params}", parts.path);
    }
    unsplit(&parts)
}

/// `urljoin(base, url)`, or the ValueError's message a part raised.
pub fn join(base: &str, url: &str) -> Result<String, String> {
    if base.is_empty() {
        return Ok(url.into());
    }
    if url.is_empty() {
        return Ok(base.into());
    }
    let (b, bparams) = parse6(base, "")?;
    let (mut u, mut params) = parse6(url, &b.scheme)?;
    if u.scheme != b.scheme || !USES_RELATIVE.contains(&u.scheme.as_str()) {
        return Ok(url.into());
    }
    if uses_netloc(&u.scheme) {
        if !u.netloc.is_empty() {
            return Ok(unparse6(u, &params));
        }
        u.netloc = b.netloc.clone();
    }
    if u.path.is_empty() && params.is_empty() {
        u.path = b.path.clone();
        params = bparams;
        if u.query.is_empty() {
            u.query = b.query;
        }
        return Ok(unparse6(u, &params));
    }
    let mut base_parts: Vec<&str> = b.path.split('/').collect();
    if base_parts.last() != Some(&"") {
        base_parts.pop();
    }
    let path = u.path.clone();
    let segments: Vec<&str> = if path.starts_with('/') {
        path.split('/').collect()
    } else {
        let mut s: Vec<&str> = base_parts;
        s.extend(path.split('/'));
        // Drop empty segments between the first and the last.
        if s.len() > 2 {
            let last = s[s.len() - 1];
            let mut kept = vec![s[0]];
            kept.extend(s[1..s.len() - 1].iter().filter(|x| !x.is_empty()));
            kept.push(last);
            s = kept;
        }
        s
    };
    let mut resolved: Vec<&str> = Vec::new();
    for seg in &segments {
        match *seg {
            ".." => {
                resolved.pop();
            }
            "." => {}
            s => resolved.push(s),
        }
    }
    if matches!(segments.last(), Some(&".") | Some(&"..")) {
        resolved.push("");
    }
    let joined = resolved.join("/");
    u.path = if joined.is_empty() {
        "/".into()
    } else {
        joined
    };
    Ok(unparse6(u, &params))
}

/// `urldefrag(url)`'s URL: a URL with a `#` goes through `urlparse` and
/// `urlunparse` without its fragment (so `http://a/b?#f` loses its `?`
/// too), one without is kept as it is.
pub fn defrag(url: &str) -> Result<String, String> {
    if !url.contains('#') {
        return Ok(url.into());
    }
    let (mut parts, params) = parse6(url, "")?;
    parts.fragment.clear();
    Ok(unparse6(parts, &params))
}

/// `str.lower()`.
fn py_lower(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// `repr()` of a str, for error messages.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// NFKC, for `_checknetloc`: ICU's tables (already in the tree through
/// idna), where Python 3.10 has Unicode 13's.
fn nfkc_of(s: &str) -> String {
    icu_normalizer::ComposingNormalizerBorrowed::new_nfkc()
        .normalize(s)
        .into_owned()
}
