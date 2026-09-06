//! [`HttpClient`]s for tests (kb d037): they record every request and
//! play scripted replies, in order or by request, nothing on the network.
//! A reply can fail before any answer, or answer with a body that comes in
//! pieces and may then fail or hang; with tokio's paused clock, a hang shows how the
//! caller's timeouts and deadlines behave without waiting for them.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::PoisonError;

use viy_http::Body;
use viy_http::BoxFuture;
use viy_http::HttpClient;
use viy_http::HttpError;
use viy_http::Request;
use viy_http::Response;

/// What one request gets.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A status, then the body's pieces; the body ends after the last.
    Answer { status: u16, body: Vec<Piece> },
    /// No answer at all.
    Fail(HttpError),
    /// No answer, ever.
    Hang,
}

impl Reply {
    /// A status and a body in one piece.
    pub fn answer(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::Answer {
            status,
            body: vec![Piece::Data(body.into())],
        }
    }
}

/// One step of a body.
#[derive(Debug, Clone)]
pub enum Piece {
    Data(Vec<u8>),
    /// Data that takes this long to arrive.
    Late(std::time::Duration, Vec<u8>),
    /// The connection breaks here.
    Fail(HttpError),
    /// Nothing more comes, and the connection stays open.
    Hang,
}

#[derive(Debug, Default)]
pub struct FakeHttp {
    replies: Mutex<VecDeque<Reply>>,
    sent: Mutex<Vec<Request>>,
}

impl FakeHttp {
    pub fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            sent: Mutex::default(),
        }
    }

    /// Every request so far, in order.
    pub fn sent(&self) -> Vec<Request> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl HttpClient for FakeHttp {
    /// Panics when the script has no reply left: the code under test made
    /// a request its test did not expect.
    fn request(&self, request: Request) -> BoxFuture<'_, Result<Response, HttpError>> {
        let reply = self
            .replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let Some(reply) = reply else {
            panic!("FakeHttp: no reply scripted for {request:?}");
        };
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request);
        Box::pin(play(reply))
    }
}

/// An [`HttpClient`] that answers each request with what `answer` makes
/// of it, for code whose requests interleave (a background fetch beside a
/// poll); it records every request too.
pub struct RouteHttp {
    answer: Box<dyn Fn(&Request) -> Reply + Send + Sync>,
    sent: Mutex<Vec<Request>>,
}

impl RouteHttp {
    pub fn new(answer: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        Self {
            answer: Box::new(answer),
            sent: Mutex::default(),
        }
    }

    /// Every request so far, in order.
    pub fn sent(&self) -> Vec<Request> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl std::fmt::Debug for RouteHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteHttp").finish_non_exhaustive()
    }
}

impl HttpClient for RouteHttp {
    fn request(&self, request: Request) -> BoxFuture<'_, Result<Response, HttpError>> {
        let reply = (self.answer)(&request);
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request);
        Box::pin(play(reply))
    }
}

async fn play(reply: Reply) -> Result<Response, HttpError> {
    match reply {
        Reply::Answer { status, body } => {
            Ok(Response::new(status, Pieces(body.into_iter().collect())))
        }
        Reply::Fail(error) => Err(error),
        Reply::Hang => std::future::pending().await,
    }
}

struct Pieces(VecDeque<Piece>);

impl Body for Pieces {
    fn chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, HttpError>> {
        let next = self.0.pop_front();
        Box::pin(async move {
            match next {
                None => Ok(None),
                Some(Piece::Data(data)) => Ok(Some(data)),
                Some(Piece::Late(wait, data)) => {
                    tokio::time::sleep(wait).await;
                    Ok(Some(data))
                }
                Some(Piece::Fail(error)) => Err(error),
                Some(Piece::Hang) => std::future::pending().await,
            }
        })
    }
}
