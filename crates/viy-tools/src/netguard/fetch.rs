//! The one way viy itself reads a URL from the internet (rpctl
//! netguard.py `get`): every hop parsed and its name looked up once, every
//! address public, the connection made to an address that was checked
//! (never a second lookup, so DNS rebinding cannot swap it), TLS verified
//! against the host name, redirects followed here and checked again, GET
//! only with fixed headers, the decoded body capped and a deadline on the
//! whole read.
//!
//! The HTTP/1 exchange is hyper's over a stream [`Connect`] opens, so the
//! address is ours, as Codex's TargetCheckedStreamConnector
//! (network-proxy/src/connect_policy.rs) connects to the address it
//! checked; reqwest would look the name up again.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::time::Instant;

use super::Resolver;
use super::Target;
use super::VetError;
use super::parse;
use super::url;
use super::vet;
use crate::approve::Pending;
use crate::approve::chars::is_py_space;

/// Redirects followed before giving up.
pub const MAX_REDIRECTS: usize = 5;

/// What a fetch may take, as rpctl's `get` defaults.
#[derive(Debug, Clone)]
pub struct Options {
    /// Decoded body bytes kept; more makes the response `truncated`.
    pub max_bytes: usize,
    /// The whole fetch, redirects and body included.
    pub deadline: Duration,
    /// The Accept header, rpctl's for pages when `None`.
    pub accept: Option<String>,
    pub max_redirects: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_bytes: 3_000_000,
            deadline: Duration::from_secs(30),
            accept: None,
            max_redirects: MAX_REDIRECTS,
        }
    }
}

/// Why a fetch gave no page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The URL, or a redirect, breaks a rule (rp's `Refused`); the model
    /// sees why.
    Refused(String),
    /// The URL was allowed but reading it failed (rp's `FetchError`).
    Failed(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(m) | Self::Failed(m) => f.write_str(m),
        }
    }
}

/// A page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Lower-case name to value (the last of a repeated header).
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    /// After redirects.
    pub url: String,
    /// The URLs that redirected, in order.
    pub redirects: Vec<String>,
    pub truncated: bool,
}

impl Response {
    pub fn content_type(&self) -> String {
        let ct = self.headers.get("content-type").map_or("", String::as_str);
        py_strip(ct.split(';').next().unwrap_or_default()).to_lowercase()
    }

    pub fn charset(&self) -> Option<String> {
        let ct = self.headers.get("content-type")?;
        ct.split(';').skip(1).find_map(|part| {
            let (k, v) = py_strip(part)
                .split_once('=')
                .unwrap_or((py_strip(part), ""));
            (k.eq_ignore_ascii_case("charset") && !v.is_empty())
                .then(|| v.trim_matches(['"', '\'', ' ']).to_lowercase())
        })
    }
}

/// A connected byte stream.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Why [`Connect`] gave no stream.
#[derive(Debug)]
pub enum ConnectError {
    /// The server's certificate does not verify for the host: no other
    /// address is tried.
    Certificate(String),
    Other(String),
}

/// Opens the connection to an address [`vet`] checked: TCP, and TLS for
/// https verified against `host` (rpctl's `_open_connection`, which its
/// tests replace the same way).
pub trait Connect: Send + Sync {
    fn connect<'a>(
        &'a self,
        ip: IpAddr,
        port: u16,
        scheme: &'a str,
        host: &'a str,
        timeout: Duration,
    ) -> Pending<'a, Result<Box<dyn Io>, ConnectError>>;
}

/// The system's: a TCP connection to the address, TLS 1.2 or later with
/// the system's root certificates (flags F23) and the host as SNI.
pub struct Direct;

/// The client's TLS settings, or why there are none (the provider
/// refusing its own defaults).
static TLS: LazyLock<Result<Arc<rustls::ClientConfig>, String>> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
        .map_err(|e| e.to_string())
});

impl Connect for Direct {
    fn connect<'a>(
        &'a self,
        ip: IpAddr,
        port: u16,
        scheme: &'a str,
        host: &'a str,
        timeout: Duration,
    ) -> Pending<'a, Result<Box<dyn Io>, ConnectError>> {
        Box::pin(async move {
            let tcp = tokio::time::timeout(timeout, tokio::net::TcpStream::connect((ip, port)))
                .await
                .map_err(|_| ConnectError::Other("timed out".into()))?
                .map_err(|e| ConnectError::Other(e.to_string()))?;
            if scheme != "https" {
                return Ok(Box::new(tcp) as Box<dyn Io>);
            }
            let name = rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(|e| ConnectError::Other(e.to_string()))?;
            let config = TLS.as_ref().map_err(|e| ConnectError::Other(e.clone()))?;
            let tls = tokio_rustls::TlsConnector::from(Arc::clone(config));
            match tokio::time::timeout(timeout, tls.connect(name, tcp)).await {
                Err(_) => Err(ConnectError::Other("timed out".into())),
                Ok(Ok(stream)) => Ok(Box::new(stream) as Box<dyn Io>),
                Ok(Err(e)) => Err(
                    match e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
                        Some(rustls::Error::InvalidCertificate(why)) => {
                            ConnectError::Certificate(certificate_problem(why))
                        }
                        _ => ConnectError::Other(e.to_string()),
                    },
                ),
            }
        })
    }
}

/// Why a certificate did not verify, in words (OpenSSL's, where rpctl
/// shows its `verify_message`).
fn certificate_problem(why: &rustls::CertificateError) -> String {
    use rustls::CertificateError as E;
    match why {
        E::Expired | E::ExpiredContext { .. } => "certificate has expired".into(),
        E::NotValidYet | E::NotValidYetContext { .. } => "certificate is not yet valid".into(),
        E::NotValidForName | E::NotValidForNameContext { .. } => {
            "the certificate is not valid for this name".into()
        }
        E::UnknownIssuer => "unable to get local issuer certificate".into(),
        E::BadSignature => "certificate signature failure".into(),
        E::Revoked => "certificate revoked".into(),
        E::Other(other) if format!("{other:?}").contains("CaUsedAsEndEntity") => {
            "self-signed certificate".into()
        }
        other => other.to_string(),
    }
}

/// GETs `url` under every rule. `Refused` for policy, `Failed` for the
/// rest.
pub async fn get(
    url: &str,
    opts: &Options,
    resolver: &dyn Resolver,
    connect: &dyn Connect,
) -> Result<Response, FetchError> {
    let end = Instant::now() + opts.deadline;
    let slow = || {
        FetchError::Failed(format!(
            "the server sent the page too slowly ({:.0} s deadline)",
            opts.deadline.as_secs_f64()
        ))
    };
    let mut redirects: Vec<String> = Vec::new();
    let mut current = url.to_string();
    loop {
        let target = parse(&current).map_err(|r| FetchError::Refused(r.0))?;
        let addrs = vet(&target.host, target.port, resolver)
            .await
            .map_err(|e| match e {
                VetError::Refused(m) => FetchError::Refused(m),
                VetError::Failed(m) => FetchError::Failed(m),
            })?;
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FetchError::Failed("the fetch ran out of time".into()));
        }
        let wait = remaining.min(Duration::from_secs(15));
        let accept = opts.accept.as_deref().filter(|a| !a.is_empty());
        let asked = request(&target, &addrs, wait, accept, connect);
        let mut exchange = tokio::time::timeout_at(end, asked).await.map_err(|_| {
            FetchError::Failed(format!("could not connect to {} (timed out)", target.host))
        })??;
        let status = exchange.response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            // rpctl's `if not loc`: an empty header is none.
            let location = joined(exchange.response.headers(), "location");
            let Some(location) = location.filter(|l| !l.is_empty()) else {
                return Err(FetchError::Failed(format!(
                    "HTTP {status} without a Location header"
                )));
            };
            if redirects.len() >= opts.max_redirects {
                return Err(FetchError::Failed(format!(
                    "more than {} redirects",
                    opts.max_redirects
                )));
            }
            let location = py_strip(&location);
            // A Location that does not parse fails as parse() fails on it.
            current = url::join(&target.url, location).unwrap_or_else(|_| location.to_string());
            redirects.push(target.url);
            continue;
        }
        let headers = header_map(exchange.response.headers());
        let decoder = Decoder::new(headers.get("content-encoding").map(String::as_str))?;
        let read = read_body(exchange.response.body_mut(), opts.max_bytes, decoder);
        let (body, truncated) = match tokio::time::timeout_at(end, read).await {
            Err(_) => return Err(slow()),
            Ok(result) => result?,
        };
        return Ok(Response {
            status,
            headers,
            body,
            url: target.url,
            redirects,
            truncated,
        });
    }
}

/// A response with the connection that carries it; dropping it closes
/// the connection.
struct Exchange {
    response: hyper::Response<hyper::body::Incoming>,
    conn: tokio::task::JoinHandle<()>,
}

impl Drop for Exchange {
    fn drop(&mut self) {
        self.conn.abort();
    }
}

/// The request target as it goes on the wire. rpctl's http.client sends
/// the path as written; the http crate refuses `<`, `>` and `` ` `` in a
/// path and `"`, `<`, `>` in a query, so those are percent-encoded, which
/// servers read alike. A non-ASCII path fails, as http.client's ASCII
/// encoding of the request line raises.
fn wire_path(path: &str) -> Result<String, String> {
    if !path.is_ascii() {
        return Err("its path is not ASCII".into());
    }
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let encode = |text: &str, refused: &[char]| -> String {
        text.chars()
            .map(|c| {
                if refused.contains(&c) {
                    format!("%{:02X}", c as u32)
                } else {
                    c.to_string()
                }
            })
            .collect()
    };
    let mut out = encode(path, &['<', '>', '`']);
    if let Some(q) = query {
        out.push('?');
        out.push_str(&encode(q, &['"', '<', '>']));
    }
    Ok(out)
}

/// rpctl's `_request`: the first three addresses in turn, each within
/// `wait` for the response head.
async fn request(
    target: &Target,
    addrs: &[IpAddr],
    wait: Duration,
    accept: Option<&str>,
    connect: &dyn Connect,
) -> Result<Exchange, FetchError> {
    let path = wire_path(&target.path)
        .map_err(|why| FetchError::Failed(format!("could not request {} ({why})", target.url)))?;
    let mut last = String::new();
    for &ip in addrs.iter().take(3) {
        let stream = match connect
            .connect(ip, target.port, &target.scheme, &target.host, wait)
            .await
        {
            Ok(stream) => stream,
            Err(ConnectError::Certificate(why)) => {
                return Err(FetchError::Failed(format!(
                    "the TLS certificate of {} is not valid ({why})",
                    target.host
                )));
            }
            Err(ConnectError::Other(why)) => {
                last = why;
                continue;
            }
        };
        let req = hyper::Request::get(path.as_str())
            .header("Host", &target.netloc)
            .header("User-Agent", viy_http::user_agent::FETCH)
            .header(
                "Accept",
                accept.unwrap_or("text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5"),
            )
            .header("Accept-Language", "en;q=0.9,*;q=0.5")
            .header("Accept-Encoding", "gzip, deflate")
            .header("Connection", "close")
            .body(Empty::<Bytes>::new())
            .map_err(|e| FetchError::Failed(format!("could not request {} ({e})", target.url)))?;
        let exchange = async {
            let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
                .title_case_headers(true)
                .handshake(TokioIo::new(stream))
                .await
                .map_err(|e| e.to_string())?;
            let conn = tokio::spawn(async move {
                let _ = conn.await;
            });
            match sender.send_request(req).await {
                Ok(response) => Ok(Exchange { response, conn }),
                Err(e) => {
                    conn.abort();
                    Err(e.to_string())
                }
            }
        };
        match tokio::time::timeout(wait, exchange).await {
            Ok(Ok(exchange)) => return Ok(exchange),
            Ok(Err(why)) => last = why,
            Err(_) => last = "timed out".into(),
        }
    }
    Err(FetchError::Failed(format!(
        "could not connect to {} ({last})",
        target.host
    )))
}

/// A header's bytes as http.client reads them (Latin-1).
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// `{k.lower(): v for k, v in resp.getheaders()}`: the last value wins.
fn header_map(headers: &hyper::HeaderMap) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, value) in headers {
        out.insert(name.as_str().to_string(), latin1(value.as_bytes()));
    }
    out
}

/// `resp.getheader(name)`: every value, joined with ", ".
fn joined(headers: &hyper::HeaderMap, name: &str) -> Option<String> {
    let values: Vec<String> = headers
        .get_all(name)
        .iter()
        .map(|v| latin1(v.as_bytes()))
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// Python's `str.strip()`.
fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// The body's Content-Encoding.
#[derive(Debug)]
enum Decoder {
    Identity,
    Gzip,
    /// zlib-wrapped or raw, decided on the first bytes.
    Deflate,
}

impl Decoder {
    fn new(encoding: Option<&str>) -> Result<Self, FetchError> {
        let enc = py_strip(encoding.unwrap_or("identity")).to_lowercase();
        match enc.as_str() {
            "" | "identity" => Ok(Self::Identity),
            "gzip" | "x-gzip" => Ok(Self::Gzip),
            "deflate" => Ok(Self::Deflate),
            _ => Err(FetchError::Failed(format!(
                "unsupported content encoding {enc}"
            ))),
        }
    }
}

/// The first `cap` bytes of the decoded body, and whether there was more.
struct Capped {
    out: Vec<u8>,
    cap: usize,
    over: bool,
}

impl Capped {
    fn new(cap: usize) -> Self {
        Self {
            out: Vec::new(),
            cap,
            over: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let room = self.cap - self.out.len();
        if bytes.len() > room {
            self.out.extend_from_slice(&bytes[..room]);
            self.over = true;
        } else {
            self.out.extend_from_slice(bytes);
        }
    }
}

/// Where a gzip stream is.
enum Gz {
    /// The header's bytes so far.
    Header(Vec<u8>),
    Body,
    /// The trailer's bytes so far (CRC-32 and length, little-endian).
    Trailer(Vec<u8>),
    Done,
}

/// zlib's `decompressobj`, as rpctl's `_read_body` uses it: zlib-wrapped,
/// raw or gzip deflate, each checked as zlib checks it (a bad header or
/// check value fails; a stream cut short keeps what it gave; bytes after
/// the end are ignored).
struct Inflate {
    z: flate2::Decompress,
    /// `None` for zlib and raw.
    gzip: Option<(Gz, flate2::Crc)>,
    ended: bool,
}

impl Inflate {
    fn zlib() -> Self {
        Self {
            z: flate2::Decompress::new(true),
            gzip: None,
            ended: false,
        }
    }

    fn raw() -> Self {
        Self {
            z: flate2::Decompress::new(false),
            gzip: None,
            ended: false,
        }
    }

    fn gzip() -> Self {
        Self {
            gzip: Some((Gz::Header(Vec::new()), flate2::Crc::new())),
            ..Self::raw()
        }
    }

    /// Decodes `input` into `out`; `Err` is a corrupt stream.
    fn feed(&mut self, mut input: &[u8], out: &mut Capped) -> Result<(), String> {
        if let Some((Gz::Header(head), _)) = &mut self.gzip {
            head.extend_from_slice(input);
            let Some(len) = gzip_header_len(head)? else {
                return Ok(());
            };
            let rest = head.split_off(len);
            if let Some(g) = self.gzip.as_mut() {
                g.0 = Gz::Body;
            }
            return self.feed(&rest, out);
        }
        if !self.ended {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let (in0, out0) = (self.z.total_in(), self.z.total_out());
                let status = self
                    .z
                    .decompress(input, &mut buf, flate2::FlushDecompress::None)
                    .map_err(|e| e.to_string())?;
                let used = (self.z.total_in() - in0) as usize;
                let made = (self.z.total_out() - out0) as usize;
                input = &input[used..];
                if let Some((_, crc)) = &mut self.gzip {
                    crc.update(&buf[..made]);
                }
                out.push(&buf[..made]);
                if out.over {
                    return Ok(());
                }
                if status == flate2::Status::StreamEnd {
                    self.ended = true;
                    if let Some(g) = &mut self.gzip {
                        g.0 = Gz::Trailer(Vec::new());
                    }
                    break;
                }
                if used == 0 && made == 0 {
                    return Ok(());
                }
            }
        }
        if let Some((state, crc)) = &mut self.gzip
            && let Gz::Trailer(trailer) = state
        {
            let take = (8 - trailer.len()).min(input.len());
            trailer.extend_from_slice(&input[..take]);
            if trailer.len() == 8 {
                let word = |i: usize| {
                    u32::from_le_bytes([trailer[i], trailer[i + 1], trailer[i + 2], trailer[i + 3]])
                };
                if word(0) != crc.sum() {
                    return Err("incorrect data check".into());
                }
                if word(4) != crc.amount() {
                    return Err("incorrect length check".into());
                }
                *state = Gz::Done;
            }
        }
        Ok(())
    }
}

/// The length of the gzip header at the start of `head` (RFC 1952), `None`
/// while it is incomplete.
fn gzip_header_len(head: &[u8]) -> Result<Option<usize>, String> {
    if head.len() < 10 {
        if head.first().is_some_and(|&b| b != 0x1f) || head.get(1).is_some_and(|&b| b != 0x8b) {
            return Err("incorrect header check".into());
        }
        return Ok(None);
    }
    if head[..2] != [0x1f, 0x8b] {
        return Err("incorrect header check".into());
    }
    if head[2] != 8 {
        return Err("unknown compression method".into());
    }
    let flags = head[3];
    let mut at = 10;
    if flags & 0x04 != 0 {
        let Some(len) = head.get(at..at + 2) else {
            return Ok(None);
        };
        at += 2 + usize::from(u16::from_le_bytes([len[0], len[1]]));
    }
    for bit in [0x08, 0x10] {
        if flags & bit != 0 {
            let Some(end) = head.get(at..).and_then(|r| r.iter().position(|&b| b == 0)) else {
                return Ok(None);
            };
            at += end + 1;
        }
    }
    if flags & 0x02 != 0 {
        at += 2;
    }
    Ok((head.len() >= at).then_some(at))
}

/// rpctl's `_read_body`: at most `max_bytes` decoded, and whether there
/// was more. Compressed bytes are read no further than twice the cap and
/// 64 KiB.
async fn read_body(
    body: &mut hyper::body::Incoming,
    max_bytes: usize,
    decoder: Decoder,
) -> Result<(Vec<u8>, bool), FetchError> {
    let raw_cap = max_bytes.saturating_mul(2).saturating_add(65536);
    let mut out = Capped::new(max_bytes);
    let mut inflate = match decoder {
        Decoder::Identity => None,
        Decoder::Gzip => Some(Inflate::gzip()),
        Decoder::Deflate => None,
    };
    let deflate = matches!(decoder, Decoder::Deflate);
    let mut raw_seen = 0usize;
    while let Some(frame) = body.frame().await {
        let frame =
            frame.map_err(|e| FetchError::Failed(format!("reading the page failed ({e})")))?;
        let Ok(chunk) = frame.into_data() else {
            continue;
        };
        if chunk.is_empty() {
            continue;
        }
        raw_seen += chunk.len();
        if deflate && inflate.is_none() {
            inflate = Some(if chunk[0] == 0x78 {
                Inflate::zlib()
            } else {
                Inflate::raw()
            });
        }
        match &mut inflate {
            None => out.push(&chunk),
            Some(z) => z
                .feed(&chunk, &mut out)
                .map_err(|e| FetchError::Failed(format!("the compressed body is corrupt ({e})")))?,
        }
        if out.over || raw_seen > raw_cap {
            return Ok((out.out, true));
        }
    }
    Ok((out.out, out.over))
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
