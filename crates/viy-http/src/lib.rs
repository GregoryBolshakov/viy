//! HTTP requests behind a seam (kb d037): the [`HttpClient`] trait after
//! Codex's (exec-server/src/client_api.rs), object-safe with boxed futures,
//! its system implementation [`ReqwestHttp`], and in viy-testkit a scripted
//! fake. Below viy-pod, viy-tools and viy-agent, which all make requests.
//!
//! Timeouts follow `requests`' `(connect, read)` pair rather than reqwest's
//! total: [`send`] waits at most connect plus read for the status line, then
//! at most read for each piece of the body, so a stream that stays open can
//! run as long as data keeps coming. [`send`] applies them, not the
//! implementations, so the fake times out exactly as the real client does.
//!
//! Requests carry the headers their caller gives and nothing that names
//! viy (CLAUDE.md): reqwest adds no User-Agent of its own, and each
//! destination sends the one rp sends there ([`user_agent`], flags.md F23,
//! kb d021).

mod reqwest_client;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub use reqwest::Method;
pub use reqwest_client::ReqwestHttp;

/// A boxed future, as Codex's traits return.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The User-Agent each destination gets: the one rp sends there (kb d021).
pub mod user_agent {
    /// rp's `requests` 2.34.2 (its pipx venv): RunPod's REST, GraphQL and
    /// log APIs. Checked against the pod/api fixture.
    pub const RP_REQUESTS: &str = "python-requests/2.34.2";

    /// fetch_url's pages: rp's Firefox 130 without its trailing
    /// `rpctl-fetch` (kb d021, f018).
    pub const FETCH: &str =
        "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
}

/// `requests`' timeout when rp gives one number: 60 s to connect and 60 s
/// for each read.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// One request: what goes on the wire, and how long to wait.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub url: String,
    /// In the order they are sent.
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
}

impl Request {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            connect_timeout: DEFAULT_TIMEOUT,
            read_timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn timeouts(mut self, connect: Duration, read: Duration) -> Self {
        self.connect_timeout = connect;
        self.read_timeout = read;
        self
    }

    /// The value of the first header called `name`, in any case.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Keys stay out of logs and failed assertions: the Authorization value is
/// one, and a body may hold others (the create-pod body carries the pod's
/// tokens), so only its length shows.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(n, v)| {
                let shown = if n.eq_ignore_ascii_case("authorization") {
                    "<redacted>"
                } else {
                    v.as_str()
                };
                (n.as_str(), shown)
            })
            .collect();
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body_len", &self.body.as_ref().map(Vec::len))
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .finish()
    }
}

/// Why a request got no answer. Codex's `TransportError` kinds and texts
/// (http-client/src/error.rs); an HTTP error status is an answer, not this.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("timeout")]
    Timeout,
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("request build error: {0}")]
    Build(String),
}

/// The body of a response as it arrives.
pub trait Body: Send {
    /// The next piece, `None` at the end.
    fn chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, HttpError>>;
}

/// A status and a body still to be read.
pub struct Response {
    pub status: u16,
    body: Box<dyn Body>,
    read_timeout: Option<Duration>,
}

impl Response {
    pub fn new(status: u16, body: impl Body + 'static) -> Self {
        Self {
            status,
            body: Box::new(body),
            read_timeout: None,
        }
    }

    /// The next piece of the body, `None` at the end; after [`send`], a
    /// wait longer than the read timeout is [`HttpError::Timeout`].
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        match self.read_timeout {
            Some(limit) => tokio::time::timeout(limit, self.body.chunk())
                .await
                .unwrap_or(Err(HttpError::Timeout)),
            None => self.body.chunk().await,
        }
    }

    /// The whole body.
    pub async fn bytes(mut self) -> Result<Vec<u8>, HttpError> {
        let mut all = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            all.extend(chunk);
        }
        Ok(all)
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Sends HTTP requests: the network, or a test's script.
pub trait HttpClient: Send + Sync {
    /// Sends `request` and resolves once the status line is in. No timeouts
    /// here: callers go through [`send`].
    fn request(&self, request: Request) -> BoxFuture<'_, Result<Response, HttpError>>;
}

/// Sends `request` through `client` with its timeouts (module docs).
pub async fn send(client: &dyn HttpClient, request: Request) -> Result<Response, HttpError> {
    let read_timeout = request.read_timeout;
    let head = request.connect_timeout.saturating_add(read_timeout);
    let mut response = tokio::time::timeout(head, client.request(request))
        .await
        .unwrap_or(Err(HttpError::Timeout))?;
    response.read_timeout = Some(read_timeout);
    Ok(response)
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
