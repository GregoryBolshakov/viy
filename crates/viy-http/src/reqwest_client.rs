//! The system [`HttpClient`]: one reqwest client, so connections are pooled
//! (Codex's http-client README), rustls with the system's root certificates
//! (flags.md F23), HTTP/1.1 with title-case header names, as rp's
//! `requests` sends them (kb d046).

use crate::Body;
use crate::BoxFuture;
use crate::HttpClient;
use crate::HttpError;
use crate::Request;
use crate::Response;

/// How long an idle connection is kept for reuse. vLLM closes one idle for
/// 5 s (uvicorn's keep-alive); a request sent on it just before that,
/// through the SSH tunnel's round trip, fails with "connection closed
/// before message completed" (measured 2026-07-26: every request after
/// 4.9-5.0 s idle failed, none after 2 s). rp opens a connection per
/// request, so it never meets this.
pub(crate) const POOL_IDLE: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    /// Fails only if the TLS backend cannot start.
    pub fn new() -> Result<Self, HttpError> {
        reqwest::Client::builder()
            .http1_title_case_headers()
            .pool_idle_timeout(POOL_IDLE)
            .build()
            .map(|client| Self { client })
            .map_err(from_reqwest)
    }
}

impl HttpClient for ReqwestHttp {
    fn request(&self, request: Request) -> BoxFuture<'_, Result<Response, HttpError>> {
        Box::pin(async move {
            let mut builder = self.client.request(request.method, &request.url);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder.send().await.map_err(from_reqwest)?;
            Ok(Response::new(
                response.status().as_u16(),
                ReqwestBody(response),
            ))
        })
    }
}

struct ReqwestBody(reqwest::Response);

impl Body for ReqwestBody {
    fn chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, HttpError>> {
        Box::pin(async move {
            self.0
                .chunk()
                .await
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                .map_err(from_reqwest)
        })
    }
}

/// Codex's mapping (http-client/src/transport.rs `map_error`), with the URL
/// dropped and the causes spelled out: reqwest's own text is only "error
/// sending request".
fn from_reqwest(error: reqwest::Error) -> HttpError {
    let error = error.without_url();
    if error.is_timeout() {
        HttpError::Timeout
    } else if error.is_connect() {
        HttpError::Connect(causes(&error))
    } else if error.is_builder() {
        HttpError::Build(causes(&error))
    } else {
        HttpError::Network(causes(&error))
    }
}

fn causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let said = cause.to_string();
        if !text.ends_with(&said) {
            text.push_str(": ");
            text.push_str(&said);
        }
        source = cause.source();
    }
    text
}
