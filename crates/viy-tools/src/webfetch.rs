//! fetch_url's pages (rpctl webfetch.py): a web page as text the model can
//! use.
//!
//! The direct read comes first: [`netguard::get`] on this machine, then
//! the page as the text a reader sees ([`html`]), with its links numbered
//! so the model can follow them and the chat knows which URLs a page
//! offered (the provenance rule, netaccess kb d005). Tavily's extract
//! endpoint is the fallback when the direct read fails or is blocked (401,
//! 403, 406, 429, 451, 5xx, a JavaScript-only page, a PDF), if a key is
//! set (netaccess kb d001). Pages are kept a while per chat ([`Cache`]),
//! so the next part ([`window`]) or the passages about something
//! ([`passages`]) come without a second read.
//!
//! Rust's way where Python's is only an implementation detail: the
//! charset is a WHATWG label decoded as browsers decode it (encoding_rs),
//! where Python's codecs know other names (an unknown one is UTF-8 in
//! both), and a JSON body is laid out as `json.dumps(indent=1)` lays it
//! out with its numbers and key order as the server wrote them.

mod entities;
pub mod html;
mod pyhtml;

use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::Instant;

use encoding_rs::Encoding;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;
use serde_json::json;
use viy_http::HttpClient;
use viy_http::HttpError;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;

use crate::approve::chars::py_strip;
use crate::netguard;
use crate::netguard::Connect;
use crate::netguard::FetchError;
use crate::netguard::Options;
use crate::netguard::Resolver;
use crate::netguard::Response;
use crate::untrusted;

pub const TAVILY_EXTRACT_URL: &str = "https://api.tavily.com/extract";

/// The types read as text; any other `text/*` is too.
const TEXT_TYPES: [&str; 12] = [
    "text/html",
    "application/xhtml+xml",
    "text/plain",
    "text/markdown",
    "text/x-markdown",
    "application/json",
    "application/ld+json",
    "text/xml",
    "application/xml",
    "application/rss+xml",
    "application/atom+xml",
    "text/csv",
];
/// Direct reads worth retrying through Tavily: bot walls and outages.
const RETRY_STATUS: [u16; 9] = [401, 403, 406, 429, 451, 500, 502, 503, 504];
/// Less text than this from HTML: probably a JavaScript app.
pub const THIN_PAGE_CHARS: usize = 300;

/// What a page's text was made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Html,
    Text,
    Json,
    Markdown,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Text => "text",
            Self::Json => "json",
            Self::Markdown => "markdown",
        }
    }
}

/// Who read the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Direct,
    Tavily,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Tavily => "tavily",
        }
    }
}

/// A page read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    /// The URL asked for.
    pub url: String,
    /// After redirects.
    pub final_url: String,
    pub title: String,
    pub text: String,
    /// Absolute URLs: `[n]` is `links[n - 1]`; `None` where the text has
    /// no `[n]`.
    pub links: Vec<Option<String>>,
    pub kind: Kind,
    pub backend: Backend,
    pub status: u16,
    /// The body hit the byte cap.
    pub truncated: bool,
    /// How it was read, when that is worth saying.
    pub note: Option<String>,
    pub fetched: Instant,
}

impl Doc {
    pub fn new(
        url: impl Into<String>,
        final_url: impl Into<String>,
        text: impl Into<String>,
        kind: Kind,
        backend: Backend,
    ) -> Self {
        Self {
            url: url.into(),
            final_url: final_url.into(),
            title: String::new(),
            text: text.into(),
            links: Vec::new(),
            kind,
            backend,
            status: 200,
            truncated: false,
            note: None,
            fetched: Instant::now(),
        }
    }
}

/// What a read goes through: netguard's lookup and connection for the
/// page, the HTTP client and key for Tavily.
#[derive(Clone, Copy)]
pub struct Net<'a> {
    pub resolver: &'a dyn Resolver,
    pub connect: &'a dyn Connect,
    pub http: &'a dyn HttpClient,
    pub tavily_key: Option<&'a str>,
}

/// The page at `url`: direct, then Tavily when that fails or is blocked.
/// A refusal is never retried.
pub async fn fetch(url: &str, net: &Net<'_>, opts: &Options) -> Result<Doc, FetchError> {
    // Refuse a bad URL before any backend sees it.
    let target = netguard::parse(url).map_err(|r| FetchError::Refused(r.0))?;
    let key = net.tavily_key.filter(|k| !k.is_empty());
    let (doc, mut err) = match direct(&target.url, net, opts).await {
        Ok(doc) => {
            let thin = doc.kind == Kind::Html && doc.text.chars().count() < THIN_PAGE_CHARS;
            if !thin && (doc.status < 400 || !RETRY_STATUS.contains(&doc.status)) {
                return Ok(doc);
            }
            let err = if doc.status >= 400 {
                format!("HTTP {}", doc.status)
            } else {
                "the page has almost no text without JavaScript".to_string()
            };
            (Some(doc), err)
        }
        Err(FetchError::Failed(why)) => (None, why),
        // A refusal, or what rpctl raises: never retried.
        Err(other) => return Err(other),
    };
    if let Some(key) = key {
        match tavily_extract(&target.url, key, net.http).await {
            Ok(mut doc) => {
                doc.note = Some(format!(
                    "read via Tavily because the direct read failed: {err}"
                ));
                return Ok(doc);
            }
            Err(why) => err = format!("{err}; {why}"),
        }
    }
    match doc {
        Some(mut doc) => {
            let retry = if key.is_some() {
                ""
            } else {
                " (no Tavily key for a retry)"
            };
            doc.note = Some(format!("direct read: {err}{retry}"));
            Ok(doc)
        }
        None => Err(FetchError::Failed(err)),
    }
}

/// `url` read with netguard and made text.
pub async fn direct(url: &str, net: &Net<'_>, opts: &Options) -> Result<Doc, FetchError> {
    let resp = netguard::get(url, opts, net.resolver, net.connect).await?;
    page(url, &resp)
}

/// The text of a page netguard read for `url`: HTML as a reader sees it,
/// JSON laid out, other text cleaned; an error for a type that is not
/// text.
pub fn page(url: &str, resp: &Response) -> Result<Doc, FetchError> {
    let mut ctype = resp.content_type();
    if ctype.is_empty() {
        ctype = "text/html".into();
    }
    if !ctype.starts_with("text/") && !TEXT_TYPES.contains(&ctype.as_str()) {
        return Err(FetchError::Failed(format!(
            "the URL serves {ctype}, not text"
        )));
    }
    let (body, _, _) = charset(resp.charset(), &resp.body).decode(&resp.body);
    let looks_html = || {
        let head: String = body.chars().take(2000).collect();
        head.to_ascii_lowercase().contains("<html")
    };
    let mut doc = if ctype == "text/html"
        || ctype == "application/xhtml+xml"
        || (ctype.starts_with("text/") && looks_html())
    {
        let page =
            html::extract(&body, &resp.url).map_err(|e| FetchError::Raised("ValueError", e))?;
        let mut doc = Doc::new(url, &resp.url, page.text, Kind::Html, Backend::Direct);
        doc.title = page.title;
        doc.links = page.links;
        doc
    } else if ctype.contains("json") {
        let text = json_text(&body).unwrap_or_else(|| body.to_string());
        Doc::new(url, &resp.url, text, Kind::Json, Backend::Direct)
    } else {
        let text = untrusted::clean(&body);
        Doc::new(url, &resp.url, text, Kind::Text, Backend::Direct)
    };
    doc.status = resp.status;
    doc.truncated = resp.truncated;
    Ok(doc)
}

static META_CHARSET: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r#"(?i-u)<meta[^>]+charset\s*=\s*["']?([A-Za-z0-9_\-]+)"#)
        .unwrap_or_else(|e| panic!("bad pattern: {e}"))
});

/// `_charset`: the Content-Type's, else a `<meta>` one in the first 4 KB,
/// else UTF-8, as is a label nobody knows.
fn charset(declared: Option<String>, body: &[u8]) -> &'static Encoding {
    let label = declared.filter(|cs| !cs.is_empty()).or_else(|| {
        let head = &body[..body.len().min(4096)];
        META_CHARSET
            .captures(head)
            .map(|c| String::from_utf8_lossy(&c[1]).to_lowercase())
    });
    label
        .and_then(|l| {
            Encoding::for_label(l.as_bytes())
                .or_else(|| Encoding::for_label(l.replace('_', "-").as_bytes()))
        })
        .unwrap_or(encoding_rs::UTF_8)
}

/// A JSON body as `json.dumps(json.loads(body), indent=1,
/// ensure_ascii=False)` lays it out, or `None` when it is not JSON. Laid
/// out from the text itself, so a number keeps every digit and an object
/// its keys, repeated ones too, as the server wrote them.
pub fn json_text(body: &str) -> Option<String> {
    serde_json::from_str::<serde::de::IgnoredAny>(body).ok()?;
    let src = body.as_bytes();
    let mut out = String::with_capacity(body.len() + body.len() / 4);
    let mut depth = 0usize;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', depth));
    };
    let skip_ws = |mut i: usize| {
        while i < src.len() && matches!(src[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        i
    };
    let mut i = 0;
    while i < src.len() {
        match src[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'"' => {
                let mut j = i + 1;
                while j < src.len() && src[j] != b'"' {
                    j += if src[j] == b'\\' { 2 } else { 1 };
                }
                let literal = &body[i..=j.min(src.len() - 1)];
                // Escapes as Python writes them; a lone surrogate as it came.
                match serde_json::from_str::<String>(literal) {
                    Ok(s) => out.push_str(&serde_json::to_string(&s).unwrap_or_default()),
                    Err(_) => out.push_str(literal),
                }
                i = j + 1;
            }
            open @ (b'{' | b'[') => {
                let close = if open == b'{' { b'}' } else { b']' };
                let next = skip_ws(i + 1);
                out.push(open as char);
                if src.get(next) == Some(&close) {
                    out.push(close as char);
                    i = next + 1;
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                    i += 1;
                }
            }
            close @ (b'}' | b']') => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(close as char);
                i += 1;
            }
            b',' => {
                out.push(',');
                newline(&mut out, depth);
                i += 1;
            }
            b':' => {
                out.push_str(": ");
                i += 1;
            }
            _ => {
                let mut j = i;
                while j < src.len()
                    && !matches!(
                        src[j],
                        b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b']' | b'}'
                    )
                {
                    j += 1;
                }
                out.push_str(&body[i..j]);
                i = j;
            }
        }
    }
    Some(out)
}

static MD_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\]\((https?://[^)\s\x1c-\x1f]+)\)").unwrap_or_else(|e| panic!("bad pattern: {e}"))
});

/// `requests`' exception class for a failure, as rp names it.
pub(crate) fn class_name(error: &HttpError) -> &'static str {
    match error {
        HttpError::Timeout => "Timeout",
        HttpError::Connect(_) | HttpError::Network(_) => "ConnectionError",
        HttpError::Build(_) => "InvalidURL",
    }
}

/// `url` read through Tavily's extract endpoint, as Markdown.
pub async fn tavily_extract(
    url: &str,
    api_key: &str,
    http: &dyn HttpClient,
) -> Result<Doc, FetchError> {
    let body = json!({
        "urls": [url],
        "extract_depth": "basic",
        "format": "markdown",
        "include_images": false,
    });
    let request = Request::new(Method::POST, TAVILY_EXTRACT_URL)
        .header("User-Agent", user_agent::RP_REQUESTS)
        .header("Accept", "*/*")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).unwrap_or_default())
        .timeouts(Duration::from_secs(5), Duration::from_secs(40));
    let unreachable = |e: HttpError| {
        FetchError::Failed(format!("tavily extract unreachable ({})", class_name(&e)))
    };
    let response = viy_http::send(http, request).await.map_err(unreachable)?;
    let status = response.status;
    let bytes = response.bytes().await.map_err(unreachable)?;
    if status != 200 {
        return Err(FetchError::Failed(format!("tavily extract HTTP {status}")));
    }
    let data: Value = serde_json::from_slice(&bytes)
        .map_err(|_| FetchError::Failed("tavily extract returned no JSON".into()))?;
    for res in data["results"].as_array().into_iter().flatten() {
        let text = res["raw_content"].as_str().unwrap_or_default();
        if py_strip(text).is_empty() {
            continue;
        }
        let text = html::tidy(text);
        let mut links: Vec<Option<String>> = Vec::new();
        for c in MD_LINK.captures_iter(&text) {
            let u = Some(c[1].to_string());
            if !links.contains(&u) && links.len() < 200 {
                links.push(u);
            }
        }
        let final_url = res["url"].as_str().filter(|u| !u.is_empty()).unwrap_or(url);
        let mut doc = Doc::new(url, final_url, text, Kind::Markdown, Backend::Tavily);
        doc.links = links;
        return Ok(doc);
    }
    let why = match &data["failed_results"][0]["error"] {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::Null | Value::Bool(false) | Value::String(_) => "no content".into(),
        Value::Array(a) if a.is_empty() => "no content".into(),
        Value::Object(o) if o.is_empty() => "no content".into(),
        Value::Number(n) if n.as_f64() == Some(0.0) => "no content".into(),
        other => other.to_string(),
    };
    Err(FetchError::Failed(format!(
        "tavily could not extract the page ({why})"
    )))
}

/// Pages read lately, by URL: small and short-lived, as a page is read in
/// parts within a few rounds and stale content should not outlive a turn
/// or two.
#[derive(Debug)]
pub struct Cache {
    size: usize,
    ttl: Duration,
    docs: IndexMap<String, Arc<Doc>>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new(24, Duration::from_secs(900))
    }
}

impl Cache {
    pub fn new(size: usize, ttl: Duration) -> Self {
        Self {
            size,
            ttl,
            docs: IndexMap::new(),
        }
    }

    pub fn get(&mut self, url: &str) -> Option<Arc<Doc>> {
        let i = self.docs.get_index_of(url)?;
        let doc = Arc::clone(&self.docs[i]);
        if doc.fetched.elapsed() > self.ttl {
            self.docs.shift_remove_index(i);
            return None;
        }
        let last = self.docs.len() - 1;
        self.docs.move_index(i, last);
        Some(doc)
    }

    /// Keeps `doc` under the URL asked for and the one it came from.
    pub fn put(&mut self, doc: Doc) -> Arc<Doc> {
        let doc = Arc::new(doc);
        let mut urls = vec![doc.url.clone()];
        if doc.final_url != doc.url {
            urls.push(doc.final_url.clone());
        }
        for url in urls {
            self.docs.shift_remove(&url);
            self.docs.insert(url, Arc::clone(&doc));
        }
        while self.docs.len() > self.size {
            self.docs.shift_remove_index(0);
        }
        doc
    }

    pub fn clear(&mut self) {
        self.docs.clear();
    }
}

static BLOCK_GAP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\n[\s\x1c-\x1f]*\n").unwrap_or_else(|e| panic!("bad pattern: {e}"))
});
static TERM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\w.\-/]+").unwrap_or_else(|e| panic!("bad pattern: {e}")));

fn first_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

/// `_blocks`: the paragraphs, each with the heading it sits under.
fn blocks(text: &str) -> Vec<(String, &str)> {
    let mut heading = String::new();
    let mut out = Vec::new();
    for block in BLOCK_GAP.split(text) {
        let b = py_strip(block);
        if b.is_empty() {
            continue;
        }
        if b.starts_with('#') {
            heading = first_chars(py_strip(b.trim_start_matches('#')), 120).to_string();
        }
        out.push((heading.clone(), b));
    }
    out
}

/// The passages that match `look_for` best, in page order, within
/// `max_chars`: the text, how many matched, and how many there are.
pub fn passages(text: &str, look_for: &str, max_chars: usize) -> (String, usize, usize) {
    let blocks = blocks(text);
    let query = look_for.to_lowercase();
    let mut terms: Vec<&str> = TERM
        .find_iter(&query)
        .map(|m| m.as_str())
        .filter(|t| t.chars().count() > 1)
        .collect();
    terms.sort_unstable();
    terms.dedup();
    let phrase = py_strip(&query);
    let mut scored: Vec<(usize, usize)> = Vec::new();
    for (i, (head, b)) in blocks.iter().enumerate() {
        let low = format!("{head} {b}").to_lowercase();
        let mut s = terms.iter().filter(|t| low.contains(**t)).count();
        if !phrase.is_empty() && low.contains(phrase) {
            s += 2;
        }
        if s > 0 {
            scored.push((s, i));
        }
    }
    if scored.is_empty() {
        return (String::new(), 0, blocks.len());
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut keep: Vec<usize> = Vec::new();
    let mut used = 0;
    for (_, i) in scored {
        let size = blocks[i].1.chars().count() + 40;
        if used + size > max_chars && !keep.is_empty() {
            continue;
        }
        keep.push(i);
        used += size;
        if used >= max_chars {
            break;
        }
    }
    keep.sort_unstable();
    let mut parts: Vec<String> = Vec::new();
    let mut last_head: Option<&str> = None;
    for &i in &keep {
        let (head, b) = &blocks[i];
        if !head.is_empty() && last_head != Some(head.as_str()) && !b.starts_with('#') {
            parts.push(format!("[under: {head}]"));
        }
        last_head = Some(head);
        parts.push(first_chars(b, max_chars).to_string());
    }
    (parts.join("\n\n"), keep.len(), blocks.len())
}

/// The part of `text` from character `start`, cut at a line break near
/// `max_chars`, and where it ends (in characters).
pub fn window(text: &str, start: i64, max_chars: usize) -> (String, usize) {
    let n = text.chars().count();
    let start = usize::try_from(start.max(0)).unwrap_or(usize::MAX).min(n);
    let mut end = n.min(start.saturating_add(max_chars));
    let at = byte_at(text, start);
    let mut to = at + byte_at(&text[at..], end - start);
    if end < n {
        // `int(max_chars * 0.7)`, as rpctl computes it.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let from = start.saturating_add((max_chars as f64 * 0.7) as usize);
        if from < end {
            let from_byte = at + byte_at(&text[at..], from - start);
            if let Some(nl) = text[from_byte..to].rfind('\n') {
                let cut = from + text[from_byte..from_byte + nl].chars().count();
                if cut > start {
                    end = cut;
                    to = from_byte + nl;
                }
            }
        }
    }
    (text[at..to].to_string(), end)
}

/// The byte offset of character `n` of `s` (its length past the end).
fn byte_at(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

/// The `[n]` links `part` shows, as `[n] url` lines, at most `limit`.
/// An error where rpctl's `int()` raises on a marker past its digit limit.
pub fn links_in(part: &str, links: &[Option<String>], limit: usize) -> Result<Vec<String>, String> {
    let mut numbers = html::markers(part)?;
    numbers.sort_unstable();
    numbers.dedup();
    let mut out = Vec::new();
    for n in numbers {
        if let Some(Some(url)) = n.checked_sub(1).and_then(|i| links.get(i)) {
            out.push(format!("[{n}] {url}"));
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "webfetch_tests.rs"]
mod tests;
