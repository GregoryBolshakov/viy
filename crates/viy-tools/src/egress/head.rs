//! Request heads: what the proxy reads from a command and what it answers
//! (rpctl egress.py `_parse_head`, `_response`, `_origin_form`, `_safe`).

use std::sync::LazyLock;

use regex::Regex;

use super::rx;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| rx(r"^[!#$%&'*+.^_`|~0-9A-Za-z-]+$"));
static IPV_FUTURE: LazyLock<Regex> = LazyLock::new(|| rx(r"^v[a-fA-F0-9]+\..+$"));

/// An answer to the command instead of a connection: the status, the
/// reason the command and the model see, and whether it is a block (the
/// policy) or a failure (the lookup or the connection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Answer {
    pub status: u16,
    pub reason: String,
    pub failed: bool,
}

impl Answer {
    pub fn blocked(status: u16, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            failed: false,
        }
    }

    pub fn failed(status: u16, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            failed: true,
        }
    }
}

/// Printable ASCII only, at most `n` characters: it goes into a header,
/// the terminal and the model.
pub(super) fn safe(text: &str, n: usize) -> String {
    text.chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .take(n)
        .collect()
}

/// The proxy's answer for `status`, rpctl's words and header.
pub fn response(status: u16, reason: &str) -> Vec<u8> {
    let text = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let why = safe(reason, 200);
    let body = format!("rpctl egress proxy: {why}\n");
    format!(
        "HTTP/1.1 {status} {text}\r\nX-Rpctl-Blocked: {why}\r\nContent-Type: text/plain\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A request head, split.
#[derive(Debug)]
pub(super) struct Head {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
}

fn is_ctl_or_space(c: char) -> bool {
    c <= ' ' || c == '\x7f'
}

/// The request line and headers of `head` (without its blank line), or a
/// 400.
pub(super) fn parse_head(head: &[u8]) -> Result<Head, Answer> {
    let not_proxy = || Answer::blocked(400, "the request is not an HTTP proxy request");
    let text = std::str::from_utf8(head)
        .ok()
        .filter(|t| t.is_ascii())
        .ok_or_else(not_proxy)?;
    let mut lines = text.split("\r\n");
    let parts: Vec<&str> = lines.next().unwrap_or_default().split(' ').collect();
    let [method, target, version] = parts[..] else {
        return Err(not_proxy());
    };
    if !TOKEN.is_match(method)
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || target.chars().any(is_ctl_or_space)
    {
        return Err(not_proxy());
    }
    let mut headers = Vec::new();
    for line in lines {
        let malformed = || Answer::blocked(400, "the request has a malformed header");
        let (name, value) = line.split_once(':').ok_or_else(malformed)?;
        if !TOKEN.is_match(name) || value.chars().any(|c| c < ' ' && c != '\t') {
            return Err(malformed());
        }
        headers.push((
            name.to_string(),
            value.trim_matches([' ', '\t']).to_string(),
        ));
    }
    Ok(Head {
        method: method.into(),
        target: target.into(),
        version: version.into(),
        headers,
    })
}

/// An absolute `http://` URL as Python 3.10.12's `urlsplit` reads it
/// (Ubuntu's, with the bracketed-host checks), for ASCII without spaces or
/// control characters, which is all a parsed head can hold.
#[derive(Debug)]
pub(crate) struct HttpUrl {
    pub netloc: String,
    /// `hostname`: lower-case, without brackets; empty for none.
    pub hostname: String,
    /// `port or 80`.
    pub port: u16,
    pub path: String,
    pub query: String,
}

/// `None` where `urlsplit` or its `port` raises.
pub(super) fn split_http_url(url: &str) -> Option<HttpUrl> {
    // The caller checked the scheme: `http:` and then `//`.
    let rest = &url[5..];
    let (netloc, mut tail) = match rest.strip_prefix("//") {
        Some(after) => {
            let end = after.find(['/', '?', '#']).unwrap_or(after.len());
            (&after[..end], &after[end..])
        }
        None => ("", rest),
    };
    let (open, close) = (netloc.contains('['), netloc.contains(']'));
    if open != close {
        return None;
    }
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let (hostname, port) = match hostinfo.split_once('[') {
        Some((before, bracketed)) => {
            let (host, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            if open && close && (!before.is_empty() || !after.is_empty() && !after.starts_with(':'))
            {
                return None;
            }
            (host, after.split_once(':').map_or("", |(_, p)| p))
        }
        None => hostinfo.split_once(':').unwrap_or((hostinfo, "")),
    };
    if open && close {
        if hostname.starts_with('v') {
            if !IPV_FUTURE.is_match(hostname) {
                return None;
            }
        } else {
            // An address, never a bracketed IPv4 one. Python reads a
            // scope id here too; no public address has one.
            let ip = crate::netguard::parse_ip(hostname)?;
            if ip.is_ipv4() {
                return None;
            }
        }
    }
    let port = if port.is_empty() {
        80
    } else {
        if !port.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        match port.parse::<u32>() {
            Ok(0) => 80,
            Ok(p) => u16::try_from(p).ok()?,
            // All digits and too long for u32: out of range too.
            Err(_) => return None,
        }
    };
    if let Some((before, _fragment)) = tail.split_once('#') {
        tail = before;
    }
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    Some(HttpUrl {
        netloc: netloc.into(),
        hostname: hostname.to_ascii_lowercase(),
        port,
        path: path.into(),
        query: query.into(),
    })
}

/// The request head sent upstream for a plain HTTP request: origin form,
/// Proxy-* and hop-by-hop headers dropped, Host from the URL, one
/// `Connection: close`.
pub(super) fn origin_form(
    head: &Head,
    url: &HttpUrl,
    host: &str,
    port: u16,
) -> Result<Vec<u8>, Answer> {
    let mut path = if url.path.is_empty() {
        "/".to_string()
    } else {
        url.path.clone()
    };
    if !url.query.is_empty() {
        path.push('?');
        path.push_str(&url.query);
    }
    if path.chars().any(is_ctl_or_space) {
        return Err(Answer::blocked(
            400,
            "the request URL contains spaces or control characters",
        ));
    }
    let names: Vec<String> = head
        .headers
        .iter()
        .map(|(n, _)| n.to_ascii_lowercase())
        .collect();
    if names.iter().any(|n| n == "transfer-encoding") && names.iter().any(|n| n == "content-length")
    {
        return Err(Answer::blocked(
            400,
            "the request has both Transfer-Encoding and Content-Length",
        ));
    }
    const HOP: [&str; 6] = [
        "host",
        "connection",
        "keep-alive",
        "te",
        "upgrade",
        "trailer",
    ];
    let mut lines = vec![format!("{} {path} {}", head.method, head.version)];
    for ((name, value), lower) in head.headers.iter().zip(&names) {
        if !lower.starts_with("proxy-") && !HOP.contains(&lower.as_str()) {
            lines.push(format!("{name}: {value}"));
        }
    }
    let shown = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    lines.push(if port == 80 {
        format!("Host: {shown}")
    } else {
        format!("Host: {shown}:{port}")
    });
    lines.push("Connection: close".into());
    Ok(format!("{}\r\n\r\n", lines.join("\r\n")).into_bytes())
}
