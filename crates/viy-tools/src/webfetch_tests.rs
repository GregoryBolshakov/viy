//! rpctl's ExtractTests for parts and FetchFallbackTests
//! (test_netaccess.py), with the direct read against a local server
//! (reached through a replaced connection, as netguard refuses loopback)
//! and Tavily through a scripted HTTP client; the page cache; JSON and
//! charsets.

use std::net::IpAddr;
use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use viy_testkit::http::FakeHttp;
use viy_testkit::http::Reply;

use super::*;
use crate::approve::Pending;
use crate::netguard::ConnectError;
use crate::netguard::Io;

#[test]
fn window_and_passages() {
    let text: Vec<String> = (0..40)
        .map(|i| format!("# S{i}\n\npara {i} {}", "filler ".repeat(30)))
        .collect();
    let text = text.join("\n\n");
    let (part, end) = window(&text, 0, 2000);
    assert!(part.chars().count() <= 2000);
    assert_eq!(part, text[..end]);
    let (out, matched, _) = passages(
        &format!("{text}\n\nthe rate limit is 5/s"),
        "rate limit",
        1000,
    );
    assert_eq!(matched, 1);
    assert!(out.contains("rate limit is 5/s"));
}

#[test]
fn links_in_lists_only_what_the_part_shows() {
    let links = [
        Some("https://a/".to_string()),
        Some("https://b/".to_string()),
    ];
    assert_eq!(links_in("see [2]", &links, 40).unwrap(), ["[2] https://b/"]);
    let links = [None, Some("https://b/".to_string())];
    assert_eq!(
        links_in("[1] [2] [3] [0] [2]", &links, 40).unwrap(),
        ["[2] https://b/"]
    );
}

#[test]
fn window_counts_characters_and_cuts_at_a_line() {
    let text = "ééé\nééé\nééé\n";
    assert_eq!(window(text, 0, 10), ("ééé\nééé".to_string(), 7));
    assert_eq!(window(text, 7, 10), ("\nééé\n".to_string(), 12));
    assert_eq!(window(text, 99, 10), (String::new(), 12));
    assert_eq!(window(text, -5, 3), ("ééé".to_string(), 3));
}

#[test]
fn passages_name_their_heading_and_keep_page_order() {
    let text = "# Setup\n\nInstall it.\n\nRate limits apply.\n\n# Limits\n\nThe rate limit is 5/s.";
    let (out, matched, total) = passages(text, "rate limit", 1000);
    assert_eq!((matched, total), (3, 5));
    assert_eq!(
        out,
        "[under: Setup]\n\nRate limits apply.\n\n# Limits\n\nThe rate limit is 5/s."
    );
    assert_eq!(passages(text, "nothing here", 1000), (String::new(), 0, 5));
}

/// The local server's pages, by path: status, type, body.
fn page(path: &str) -> (u16, &'static str, Vec<u8>) {
    let article = format!(
        "<html><title>T</title><p>{}</p></html>",
        "words ".repeat(100)
    );
    match path {
        "/ok" => (200, "text/html; charset=utf-8", article.into_bytes()),
        "/blocked" => (403, "text/html", article.into_bytes()),
        "/gone" => (404, "text/html", article.into_bytes()),
        "/thin" => (200, "text/html", b"<html><div id=app></div></html>".to_vec()),
        "/pdf" => (200, "application/pdf", b"%PDF-1.4".to_vec()),
        "/json" => (
            200,
            "application/json",
            br#"{"a": [1, 2.50, {}], "big": 123456789012345678901234567890, "s": "\u00e9\n", "e": []}"#.to_vec(),
        ),
        "/text" => (200, "text/plain", b"line\r\nzero\xe2\x80\x8bwidth".to_vec()),
        "/latin" => (200, "text/plain; charset=iso-8859-1", b"caf\xe9".to_vec()),
        "/meta" => (
            200,
            "text/html",
            b"<html><meta charset=\"windows-1251\"><p>\xcf\xf0\xe8\xe2\xe5\xf2</p></html>".to_vec(),
        ),
        "/empty-charset" => (
            200,
            "text/html; charset=\"\"",
            b"<html><meta charset=windows-1251><p>\xcf\xf0\xe8</p></html>".to_vec(),
        ),
        "/xml-html" => (200, "text/xml", b"<?xml?><html><p>x</p></html>".to_vec()),
        _ => (404, "text/plain", Vec::new()),
    }
}

/// A server on 127.0.0.1 answering [`page`].
async fn server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    buf.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let path = head.split(' ').nth(1).unwrap_or_default().to_string();
                let (status, ctype, body) = page(&path);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    });
    port
}

/// Every connection goes to the local server.
struct Local(u16);

impl Connect for Local {
    fn connect<'a>(
        &'a self,
        _: IpAddr,
        _: u16,
        _: &'a str,
        _: &'a str,
        _: Duration,
    ) -> Pending<'a, Result<Box<dyn Io>, ConnectError>> {
        Box::pin(async move {
            TcpStream::connect(("127.0.0.1", self.0))
                .await
                .map(|s| Box::new(s) as Box<dyn Io>)
                .map_err(|e| ConnectError::Other(e.to_string()))
        })
    }
}

/// Public addresses, a private one for `admin.*`.
struct Public;

impl Resolver for Public {
    fn resolve<'a>(&'a self, host: &'a str, _: u16) -> Pending<'a, Result<Vec<IpAddr>, String>> {
        let ip = if host.starts_with("admin.") {
            "10.0.0.7"
        } else {
            "93.184.216.34"
        };
        Box::pin(async move { Ok(vec![ip.parse().unwrap()]) })
    }
}

fn tavily_page(text: &str) -> Reply {
    Reply::answer(
        200,
        serde_json::to_vec(&json!({
            "results": [{"url": "https://s.test/final", "raw_content": text}],
            "failed_results": [],
        }))
        .unwrap(),
    )
}

async fn read(path: &str, key: Option<&str>, http: &FakeHttp) -> Result<Doc, FetchError> {
    let port = server().await;
    let net = Net {
        resolver: &Public,
        connect: &Local(port),
        http,
        tavily_key: key,
    };
    fetch(&format!("http://pub.test{path}"), &net, &Options::default()).await
}

#[tokio::test]
async fn a_page_is_read_directly() {
    let http = FakeHttp::new([]);
    let doc = read("/ok", Some("key"), &http).await.unwrap();
    assert_eq!(
        (
            doc.kind,
            doc.backend,
            doc.status,
            doc.title.as_str(),
            doc.note
        ),
        (Kind::Html, Backend::Direct, 200, "T", None)
    );
    assert_eq!(doc.url, "http://pub.test/ok");
    assert!(doc.text.starts_with("words words"));
    assert!(http.sent().is_empty());
}

#[tokio::test]
async fn blocked_or_thin_pages_go_to_tavily() {
    for path in ["/blocked", "/thin", "/pdf"] {
        let http = FakeHttp::new([tavily_page("from [tavily](https://t.test/a) ok")]);
        let doc = read(path, Some("key"), &http).await.unwrap();
        assert_eq!(
            (doc.backend, doc.kind),
            (Backend::Tavily, Kind::Markdown),
            "{path}"
        );
        assert_eq!(doc.final_url, "https://s.test/final");
        assert_eq!(doc.links, [Some("https://t.test/a".to_string())]);
        let why = match path {
            "/blocked" => "HTTP 403",
            "/thin" => "the page has almost no text without JavaScript",
            _ => "the URL serves application/pdf, not text",
        };
        assert_eq!(
            doc.note.as_deref(),
            Some(format!("read via Tavily because the direct read failed: {why}").as_str())
        );
        let sent = http.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].url, TAVILY_EXTRACT_URL);
        assert_eq!(sent[0].header_value("Authorization"), Some("Bearer key"));
        let body: Value = serde_json::from_slice(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"urls": [format!("http://pub.test{path}")], "extract_depth": "basic",
                   "format": "markdown", "include_images": false})
        );
    }
}

#[tokio::test]
async fn a_status_not_worth_retrying_stands() {
    let http = FakeHttp::new([]);
    let doc = read("/gone", Some("key"), &http).await.unwrap();
    assert_eq!((doc.status, doc.note), (404, None));
    assert!(http.sent().is_empty());
}

#[tokio::test]
async fn refusals_are_never_retried() {
    let port = server().await;
    let http = FakeHttp::new([tavily_page("x")]);
    let net = Net {
        resolver: &Public,
        connect: &Local(port),
        http: &http,
        tavily_key: Some("key"),
    };
    for url in ["http://admin.corp-example.test/", "file:///etc/passwd"] {
        let got = fetch(url, &net, &Options::default()).await;
        assert!(matches!(got, Err(FetchError::Refused(_))), "{url}: {got:?}");
    }
    assert!(http.sent().is_empty());
}

#[tokio::test]
async fn without_a_key_the_direct_result_stands() {
    let http = FakeHttp::new([]);
    let doc = read("/blocked", None, &http).await.unwrap();
    assert_eq!(doc.status, 403);
    assert_eq!(
        doc.note.as_deref(),
        Some("direct read: HTTP 403 (no Tavily key for a retry)")
    );
    let doc = read("/blocked", Some(""), &http).await.unwrap();
    assert!(doc.note.unwrap().contains("no Tavily key"));
    let err = read("/pdf", None, &http).await.unwrap_err();
    assert_eq!(
        err,
        FetchError::Failed("the URL serves application/pdf, not text".into())
    );
}

#[tokio::test]
async fn a_tavily_failure_joins_the_direct_one() {
    let http = FakeHttp::new([Reply::answer(432, "{}")]);
    let doc = read("/blocked", Some("key"), &http).await.unwrap();
    assert_eq!(
        doc.note.as_deref(),
        Some("direct read: HTTP 403; tavily extract HTTP 432")
    );
    let http = FakeHttp::new([Reply::Fail(HttpError::Timeout)]);
    let err = read("/pdf", Some("key"), &http).await.unwrap_err();
    assert_eq!(
        err,
        FetchError::Failed(
            "the URL serves application/pdf, not text; tavily extract unreachable (Timeout)".into()
        )
    );
}

#[tokio::test]
async fn tavily_answers_without_content() {
    let cases = [
        (
            Reply::answer(200, "not json"),
            "tavily extract returned no JSON",
        ),
        (
            Reply::answer(
                200,
                r#"{"results": [{"raw_content": "  "}], "failed_results": [{"error": "blocked by robots"}]}"#,
            ),
            "tavily could not extract the page (blocked by robots)",
        ),
        (
            Reply::answer(200, r#"{"results": []}"#),
            "tavily could not extract the page (no content)",
        ),
    ];
    for (reply, want) in cases {
        let http = FakeHttp::new([reply]);
        let err = tavily_extract("https://s.test/", "key", &http)
            .await
            .unwrap_err();
        assert_eq!(err, FetchError::Failed(want.into()));
    }
}

#[tokio::test]
async fn text_json_and_charsets() {
    let http = FakeHttp::new([]);
    let doc = read("/json", None, &http).await.unwrap();
    assert_eq!(doc.kind, Kind::Json);
    assert_eq!(
        doc.text,
        "{\n \"a\": [\n  1,\n  2.50,\n  {}\n ],\n \"big\": 123456789012345678901234567890,\n \"s\": \"é\\n\",\n \"e\": []\n}"
    );
    let doc = read("/text", None, &http).await.unwrap();
    assert_eq!(
        (doc.kind, doc.text.as_str()),
        (Kind::Text, "line\nzerowidth")
    );
    let doc = read("/latin", None, &http).await.unwrap();
    assert_eq!(doc.text, "café");
    let doc = read("/meta", None, &http).await.unwrap();
    assert_eq!(doc.text, "Привет");
    // An empty charset is none: the <meta> one counts.
    let doc = read("/empty-charset", None, &http).await.unwrap();
    assert_eq!(doc.text, "При");
    let doc = read("/xml-html", None, &http).await.unwrap();
    assert_eq!((doc.kind, doc.text.as_str()), (Kind::Html, "x"));
}

#[test]
fn json_layout_as_python_writes_it() {
    assert_eq!(json_text("[]").as_deref(), Some("[]"));
    assert_eq!(json_text(" \"a\\/b\\u0041\" ").as_deref(), Some("\"a/bA\""));
    assert_eq!(
        json_text(r#"{"k":{"x" : [ ] ,"y":null},"z":"\t"}"#).as_deref(),
        Some("{\n \"k\": {\n  \"x\": [],\n  \"y\": null\n },\n \"z\": \"\\t\"\n}")
    );
    // A lone surrogate stays escaped.
    assert_eq!(
        json_text(r#"["\ud800"]"#).as_deref(),
        Some("[\n \"\\ud800\"\n]")
    );
    // Numbers as written, where Python writes Infinity.
    assert_eq!(json_text("[1e400]").as_deref(), Some("[\n 1e400\n]"));
    // Not JSON to serde_json, which Python reads: the body is shown as sent.
    assert_eq!(json_text("[NaN]"), None);
    assert_eq!(json_text("{bad}"), None);
    assert_eq!(json_text("NaN"), None);
}

fn doc(url: &str, final_url: &str) -> Doc {
    Doc::new(url, final_url, "t", Kind::Text, Backend::Direct)
}

#[test]
fn the_cache_keeps_both_urls_recent_first_for_a_while() {
    let mut cache = Cache::new(3, Duration::from_secs(900));
    cache.put(doc("https://a/", "https://a/final"));
    cache.put(doc("https://b/", "https://b/"));
    assert_eq!(cache.get("https://a/final").unwrap().url, "https://a/");
    // a/final was used last, so a third page drops https://a/ first.
    cache.put(doc("https://c/", "https://c/"));
    cache.put(doc("https://d/", "https://d/"));
    assert!(cache.get("https://a/").is_none());
    assert!(cache.get("https://b/").is_none());
    assert!(cache.get("https://a/final").is_some());
    assert!(cache.get("https://d/").is_some());
    let mut old = doc("https://e/", "https://e/");
    old.fetched = Instant::now()
        .checked_sub(Duration::from_secs(901))
        .unwrap();
    cache.put(old);
    assert!(cache.get("https://e/").is_none());
    cache.clear();
    assert!(cache.get("https://d/").is_none());
}

/// `window`, `passages`, `links_in` and `_tidy` against rpctl's over
/// generated text: what the model is sent of a page.
#[cfg(feature = "oracle")]
#[test]
fn parts_match_rpctl() {
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;
    use viy_testkit::oracle::tuple;

    const PIECES: &[&str] = &[
        "\n", "\n", "\n\n", "\n \n", " ", "  ", "\t", "# ", "## Rate", "rate", "limit", "Rate",
        "LIMIT", "5/s", "é", "ß", "İ", "[1]", "[2]", "[10]", "[0]", "[x]", "- ", "|", "```", "-",
        "a.b", "x_y", "\u{200b}", "\r\n", "\u{a0}", "\u{1c}", "word", "words ", "#",
    ];
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let call = |oracle: &mut Oracle, f: &str, args: Value| -> Value {
        match oracle.call(f, args, json!({})).unwrap().outcome {
            Outcome::Returned(v) => v,
            other => panic!("{f}: {other:?}"),
        }
    };
    for case in 0..cases(3000) {
        let text = g.string(PIECES, 60);
        let at = format!("seed {} case {case}: {text:?}", seed());
        let max = *g.pick(&[1usize, 5, 10, 40, 100, 1000]);
        let start = *g.pick(&[-3i64, 0, 0, 7, 30, 200]);
        let (part, end) = window(&text, start, max);
        assert_eq!(
            call(&mut oracle, "webfetch.window", json!([text, start, max])),
            tuple(vec![json!(part), json!(end)]),
            "window {start} {max}: {at}"
        );
        let look_for = g.string(
            &["rate", " ", "limit", "5/s", "Rate limit", "é", "x", "a.b"],
            3,
        );
        let (out, matched, total) = passages(&text, &look_for, max);
        assert_eq!(
            call(
                &mut oracle,
                "webfetch.passages",
                json!([text, look_for, max])
            ),
            tuple(vec![json!(out), json!(matched), json!(total)]),
            "passages {look_for:?} {max}: {at}"
        );
        let links = [
            Some("https://a.test/".to_string()),
            None,
            Some("https://c.test/".to_string()),
        ];
        let limit = *g.pick(&[1usize, 2, 40]);
        assert_eq!(
            call(
                &mut oracle,
                "webfetch.links_in",
                json!([text, links, limit])
            ),
            json!(links_in(&text, &links, limit).unwrap()),
            "links_in: {at}"
        );
        assert_eq!(
            call(&mut oracle, "webfetch.tidy", json!([text])),
            json!(html::tidy(&text)),
            "tidy: {at}"
        );
    }
}
