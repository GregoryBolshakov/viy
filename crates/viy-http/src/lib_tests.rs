use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;

use super::*;

/// A one-connection HTTP/1.1 server on 127.0.0.1: reads the request, hands
/// its head and body to the test, then lets `answer` write to the socket.
struct Server {
    url: String,
    seen: mpsc::Receiver<(String, Vec<u8>)>,
}

impl Server {
    fn start(answer: impl FnOnce(&mut TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, seen) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .map_or(0, |n| n.parse().unwrap());
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            tx.send((head, body)).unwrap();
            answer(&mut stream);
        });
        Self { url, seen }
    }
}

fn client() -> ReqwestHttp {
    ReqwestHttp::new().unwrap()
}

fn answer(text: &'static str) -> impl FnOnce(&mut TcpStream) + Send + 'static {
    move |stream| stream.write_all(text.as_bytes()).unwrap()
}

#[tokio::test]
async fn the_request_goes_out_as_given_and_nothing_names_viy() {
    let server = Server::start(answer(
        "HTTP/1.1 201 Created\r\nContent-Length: 5\r\n\r\nhello",
    ));
    let request = Request::new(Method::POST, format!("{}/v1/pods?tail=5", server.url))
        .header("User-Agent", user_agent::RP_REQUESTS)
        .header("Accept", "*/*")
        .header("Authorization", "Bearer k")
        .header("Content-Type", "application/json")
        .body(r#"{"a":1}"#);
    let response = send(&client(), request).await.unwrap();
    assert_eq!(response.status, 201);
    assert_eq!(response.bytes().await.unwrap(), b"hello");

    let (head, body) = server.seen.recv().unwrap();
    let mut lines: Vec<&str> = head.lines().collect();
    assert_eq!(lines.remove(0), "POST /v1/pods?tail=5 HTTP/1.1");
    lines.retain(|l| !l.is_empty() && !l.starts_with("Host: "));
    assert_eq!(
        lines,
        [
            "User-Agent: python-requests/2.34.2",
            "Accept: */*",
            "Authorization: Bearer k",
            "Content-Type: application/json",
            "Content-Length: 7",
        ]
    );
    assert_eq!(body, br#"{"a":1}"#);
    assert!(!head.to_lowercase().contains("viy"), "{head}");
}

#[tokio::test]
async fn reqwest_adds_no_user_agent_of_its_own() {
    let server = Server::start(answer("HTTP/1.1 204 No Content\r\n\r\n"));
    let response = send(&client(), Request::new(Method::GET, server.url.clone()))
        .await
        .unwrap();
    assert_eq!(response.status, 204);
    let (head, _) = server.seen.recv().unwrap();
    assert!(!head.contains("User-Agent"), "{head}");
}

#[tokio::test]
async fn a_body_arrives_in_pieces_and_ends() {
    let server = Server::start(|stream| {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n")
            .unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(50));
        stream.write_all(b"2\r\nde\r\n0\r\n\r\n").unwrap();
    });
    let mut response = send(&client(), Request::new(Method::GET, server.url.clone()))
        .await
        .unwrap();
    assert_eq!(response.chunk().await.unwrap(), Some(b"abc".to_vec()));
    assert_eq!(response.chunk().await.unwrap(), Some(b"de".to_vec()));
    assert_eq!(response.chunk().await.unwrap(), None);
}

#[tokio::test]
async fn a_refused_connection_is_a_connect_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    drop(listener);
    let error = send(&client(), Request::new(Method::GET, url))
        .await
        .unwrap_err();
    let HttpError::Connect(text) = &error else {
        panic!("{error:?}");
    };
    assert!(text.contains("Connection refused"), "{text}");
    assert!(!text.contains("127.0.0.1"), "the URL is dropped: {text}");
}

fn short(request: Request) -> Request {
    request.timeouts(Duration::from_millis(100), Duration::from_millis(100))
}

#[tokio::test]
async fn a_server_that_never_answers_times_out_after_connect_plus_read() {
    let server = Server::start(|_| thread::sleep(Duration::from_secs(5)));
    let start = Instant::now();
    let error = send(
        &client(),
        short(Request::new(Method::GET, server.url.clone())),
    )
    .await
    .unwrap_err();
    assert_eq!(error, HttpError::Timeout);
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(200), "{took:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");
}

#[tokio::test]
async fn a_body_that_stalls_times_out_per_piece() {
    let server = Server::start(|stream| {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n")
            .unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_secs(5));
    });
    let mut response = send(
        &client(),
        short(Request::new(Method::GET, server.url.clone())),
    )
    .await
    .unwrap();
    assert_eq!(response.chunk().await.unwrap(), Some(b"abc".to_vec()));
    let start = Instant::now();
    assert_eq!(response.chunk().await, Err(HttpError::Timeout));
    assert!(start.elapsed() < Duration::from_secs(2));
}

/// A client that never answers: `send`'s timeout holds for any client.
struct Silent;

impl HttpClient for Silent {
    fn request(&self, _: Request) -> BoxFuture<'_, Result<Response, HttpError>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(start_paused = true)]
async fn send_times_out_any_client_at_connect_plus_read() {
    let start = tokio::time::Instant::now();
    let request = Request::new(Method::GET, "https://example.invalid/")
        .timeouts(Duration::from_secs(5), Duration::from_secs(2));
    assert_eq!(
        send(&Silent, request).await.unwrap_err(),
        HttpError::Timeout
    );
    assert_eq!(start.elapsed(), Duration::from_secs(7));
}

#[test]
fn the_key_is_not_in_debug_output() {
    let request = Request::new(Method::GET, "https://rest.runpod.io/v1/pods")
        .header("authorization", "Bearer rpa_SECRET")
        .header("Accept", "*/*");
    let shown = format!("{request:?}");
    assert!(!shown.contains("rpa_SECRET"), "{shown}");
    assert!(
        shown.contains(r#"("authorization", "<redacted>")"#),
        "{shown}"
    );
    assert!(shown.contains(r#"("Accept", "*/*")"#), "{shown}");
    assert!(shown.contains("body_len: None"), "{shown}");
}

/// hyper sends no length for an empty body; a caller's own goes out once.
#[tokio::test]
async fn a_given_content_length_goes_out_once() {
    for (body, length) in [(None, "0"), (Some("abc"), "3")] {
        let server = Server::start(answer("HTTP/1.1 204 No Content\r\n\r\n"));
        let mut request =
            Request::new(Method::POST, server.url.clone()).header("Content-Length", length);
        if let Some(body) = body {
            request = request.body(body);
        }
        send(&client(), request).await.unwrap();
        let (head, _) = server.seen.recv().unwrap();
        let lengths: Vec<&str> = head
            .lines()
            .filter(|l| l.to_lowercase().starts_with("content-length"))
            .collect();
        assert_eq!(lengths, [format!("Content-Length: {length}")], "{head}");
    }
}

#[tokio::test]
async fn hyper_sends_no_length_for_an_empty_post() {
    let server = Server::start(answer("HTTP/1.1 204 No Content\r\n\r\n"));
    send(&client(), Request::new(Method::POST, server.url.clone()))
        .await
        .unwrap();
    let (head, _) = server.seen.recv().unwrap();
    assert!(!head.contains("Content-Length"), "{head}");
}
