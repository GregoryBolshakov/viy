//! rpctl's GetTests (test_netaccess.py): a local server reached only
//! through a replaced [`Connect`], since netguard itself refuses loopback,
//! and a resolver that gives public addresses (and a private one for
//! `admin.*`).

use std::io::Write as _;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use super::*;
use crate::netguard::Resolver;

type Seen = Arc<Mutex<Vec<(String, String, Vec<(String, String)>)>>>;

/// The test pages, by path.
async fn reply(path: &str, stream: &mut TcpStream) {
    let head = |status: &str, headers: &[(&str, String)]| {
        let mut out = format!("HTTP/1.1 {status}\r\n");
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("Connection: close\r\n\r\n");
        out
    };
    let redirect = |to: &str| head("302 Found", &[("Location", to.to_string())]);
    let text = match path {
        "/page" => {
            let body = "<html><title>T</title><p>hello</p></html>";
            format!(
                "{}{body}",
                head(
                    "200 OK",
                    &[
                        ("Content-Type", "text/html; charset=utf-8".into()),
                        ("Content-Length", body.len().to_string())
                    ]
                )
            )
        }
        "/hop" => redirect("http://pub.test/page"),
        "/inward" => redirect("http://admin.corp-example.test/page"),
        "/loop" => redirect("/loop"),
        "/file" => redirect("file:///etc/passwd"),
        "/empty" => redirect(""),
        "/hang" => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            return;
        }
        "/bomb" => {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gz.write_all(&vec![b'A'; 20_000_000]).unwrap();
            let data = gz.finish().unwrap();
            let h = head(
                "200 OK",
                &[
                    ("Content-Type", "text/plain".into()),
                    ("Content-Encoding", "gzip".into()),
                    ("Content-Length", data.len().to_string()),
                ],
            );
            let _ = stream.write_all(h.as_bytes()).await;
            let _ = stream.write_all(&data).await;
            return;
        }
        "/drip" => {
            let h = head("200 OK", &[("Content-Type", "text/plain".into())]);
            let _ = stream.write_all(h.as_bytes()).await;
            for _ in 0..40 {
                if stream.write_all(b"x").await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            return;
        }
        _ => head("404 Not Found", &[("Content-Length", "0".into())]),
    };
    let _ = stream.write_all(text.as_bytes()).await;
}

/// A server on 127.0.0.1 and what it was asked.
async fn server() -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    buf.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                let mut lines = text.split("\r\n");
                let mut first = lines.next().unwrap_or_default().split(' ');
                let method = first.next().unwrap_or_default().to_string();
                let path = first.next().unwrap_or_default().to_string();
                let headers = lines
                    .filter_map(|l| l.split_once(": "))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                log.lock().unwrap().push((method, path.clone(), headers));
                reply(path.split('?').next().unwrap_or_default(), &mut stream).await;
            });
        }
    });
    (port, seen)
}

/// Every connection goes to the local server, in plain HTTP.
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

/// rpctl's `_public`.
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

async fn fetch(path: &str, opts: Options) -> (Result<Response, FetchError>, Seen) {
    let (port, seen) = server().await;
    let got = get(
        &format!("http://pub.test{path}"),
        &opts,
        &Public,
        &Local(port),
    )
    .await;
    (got, seen)
}

#[tokio::test]
async fn plain_get_with_fixed_headers() {
    let (got, seen) = fetch("/page", Options::default()).await;
    let r = got.unwrap();
    assert_eq!(
        (r.status, r.content_type(), r.charset()),
        (200, "text/html".to_string(), Some("utf-8".to_string()))
    );
    assert!(String::from_utf8_lossy(&r.body).contains("hello"));
    let seen = seen.lock().unwrap();
    let (method, path, headers) = seen.last().unwrap();
    assert_eq!((method.as_str(), path.as_str()), ("GET", "/page"));
    let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "Host",
            "User-Agent",
            "Accept",
            "Accept-Language",
            "Accept-Encoding",
            "Connection"
        ]
    );
    let get = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(get("Host"), Some("pub.test"));
    assert_eq!(get("User-Agent"), Some(viy_http::user_agent::FETCH));
    assert!(!get("User-Agent").unwrap().contains("viy"));
}

#[tokio::test]
async fn redirects_are_followed_and_rechecked() {
    let (got, _) = fetch("/hop", Options::default()).await;
    let r = got.unwrap();
    assert_eq!(r.redirects, ["http://pub.test/hop"]);
    assert_eq!(r.url, "http://pub.test/page");
    let (got, _) = fetch("/inward", Options::default()).await;
    assert_eq!(
        got.unwrap_err(),
        FetchError::Refused(
            "admin.corp-example.test resolves to 10.0.0.7 is a private network address".into()
        )
    );
    let (got, _) = fetch("/file", Options::default()).await;
    assert_eq!(
        got.unwrap_err(),
        FetchError::Refused("only http and https URLs can be fetched, not file".into())
    );
    let (got, _) = fetch("/loop", Options::default()).await;
    assert_eq!(
        got.unwrap_err(),
        FetchError::Failed("more than 5 redirects".into())
    );
}

#[tokio::test]
async fn decompression_bomb_is_capped() {
    let opts = Options {
        max_bytes: 100_000,
        ..Options::default()
    };
    let r = fetch("/bomb", opts).await.0.unwrap();
    assert!(r.truncated);
    assert_eq!(r.body.len(), 100_000);
    assert!(r.body.iter().all(|&b| b == b'A'));
}

#[tokio::test]
async fn slow_drip_hits_the_deadline() {
    let started = Instant::now();
    let opts = Options {
        deadline: Duration::from_secs(1),
        ..Options::default()
    };
    let got = fetch("/drip", opts).await.0;
    assert_eq!(
        got.unwrap_err(),
        FetchError::Failed("the server sent the page too slowly (1 s deadline)".into())
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn a_refused_url_never_connects() {
    let (port, seen) = server().await;
    for url in [
        "http://localhost/",
        "http://127.0.0.1/",
        "http://pub.test:22/",
    ] {
        let got = get(url, &Options::default(), &Public, &Local(port)).await;
        assert!(matches!(got, Err(FetchError::Refused(_))), "{url}: {got:?}");
    }
    assert!(seen.lock().unwrap().is_empty());
}

fn inflate(mut z: Inflate, data: &[u8], cap: usize) -> Result<(String, bool), String> {
    let mut out = Capped::new(cap);
    // In pieces, as a body arrives.
    for piece in data.chunks(7) {
        z.feed(piece, &mut out)?;
        if out.over {
            break;
        }
    }
    Ok((String::from_utf8(out.out).unwrap(), out.over))
}

#[test]
fn deflate_gzip_and_zlib_are_checked_as_zlib_checks_them() {
    let gzip = |body: &[u8], name: bool| {
        let mut b = flate2::GzBuilder::new();
        if name {
            b = b.filename("page.html").comment("c").extra(vec![1, 2, 3]);
        }
        let mut z = b.write(Vec::new(), flate2::Compression::default());
        z.write_all(body).unwrap();
        z.finish().unwrap()
    };
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(b"zlib body").unwrap();
    let zlib = z.finish().unwrap();
    let mut d = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    d.write_all(b"raw body").unwrap();
    let raw = d.finish().unwrap();
    let ok = |s: &str| Ok((s.to_string(), false));
    assert_eq!(inflate(Inflate::zlib(), &zlib, 100), ok("zlib body"));
    assert_eq!(inflate(Inflate::raw(), &raw, 100), ok("raw body"));
    assert_eq!(
        inflate(Inflate::gzip(), &gzip(b"gz body", false), 100),
        ok("gz body")
    );
    assert_eq!(
        inflate(Inflate::gzip(), &gzip(b"named", true), 100),
        ok("named")
    );
    // Bytes after the end are ignored, as zlib's unused_data.
    let mut tail = gzip(b"gz body", false);
    tail.extend_from_slice(b"junk");
    assert_eq!(inflate(Inflate::gzip(), &tail, 100), ok("gz body"));
    // Cut short (no trailer): what was decoded stands.
    let cut = gzip(b"gz body", false);
    assert_eq!(
        inflate(Inflate::gzip(), &cut[..cut.len() - 8], 100),
        ok("gz body")
    );
    // A wrong CRC or length fails, as zlib's "incorrect data check".
    let mut bad = gzip(b"gz body", false);
    let n = bad.len();
    bad[n - 8] ^= 1;
    assert_eq!(
        inflate(Inflate::gzip(), &bad, 100),
        Err("incorrect data check".into())
    );
    let mut bad = gzip(b"gz body", false);
    bad[n - 1] ^= 1;
    assert_eq!(
        inflate(Inflate::gzip(), &bad, 100),
        Err("incorrect length check".into())
    );
    let mut bad = zlib.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    assert!(inflate(Inflate::zlib(), &bad, 100).is_err());
    assert_eq!(
        inflate(Inflate::gzip(), b"\x1f\x8c\x08\x00garbage", 100),
        Err("incorrect header check".into())
    );
    // The cap.
    assert_eq!(
        inflate(Inflate::zlib(), &zlib, 4),
        Ok(("zlib".into(), true))
    );
    assert!(matches!(
        Decoder::new(Some(" br ")),
        Err(FetchError::Failed(m)) if m == "unsupported content encoding br"
    ));
}

/// Every connection goes to the local TLS server, through [`Direct`].
struct ViaDirect(u16);

impl Connect for ViaDirect {
    fn connect<'a>(
        &'a self,
        _: IpAddr,
        _: u16,
        scheme: &'a str,
        host: &'a str,
        timeout: Duration,
    ) -> Pending<'a, Result<Box<dyn Io>, ConnectError>> {
        Direct.connect([127, 0, 0, 1].into(), self.0, scheme, host, timeout)
    }
}

/// rpctl's ssl.SSLCertVerificationError branch: a self-signed certificate
/// for the right name is refused, and no other address is tried. The
/// certificate is made by openssl at run time (no key in the repository).
#[tokio::test]
async fn a_certificate_that_does_not_verify_is_refused() {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::PrivateKeyDer;
    use rustls::pki_types::pem::PemObject;

    let dir = viy_testkit::scratch::ScratchDir::new("tls").unwrap();
    let (cert, key) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
    let made = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
        ])
        .args(["-nodes", "-days", "2", "-subj", "/CN=pub.test"])
        .args(["-addext", "subjectAltName=DNS:pub.test", "-keyout"])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output();
    if !made.is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: no openssl to make a certificate");
        return;
    }
    let certs = vec![CertificateDer::from_pem_file(&cert).unwrap()];
    let key = PrivateKeyDer::from_pem_file(&key).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(Mutex::new(0));
    let count = Arc::clone(&accepted);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            *count.lock().unwrap() += 1;
            let _ = acceptor.accept(stream).await;
        }
    });
    struct Two;
    impl Resolver for Two {
        fn resolve<'a>(&'a self, _: &'a str, _: u16) -> Pending<'a, Result<Vec<IpAddr>, String>> {
            Box::pin(async {
                Ok(vec![
                    "93.184.216.34".parse().unwrap(),
                    "8.8.8.8".parse().unwrap(),
                ])
            })
        }
    }
    let got = get(
        "https://pub.test/",
        &Options::default(),
        &Two,
        &ViaDirect(port),
    )
    .await;
    assert_eq!(
        got.unwrap_err(),
        FetchError::Failed(
            "the TLS certificate of pub.test is not valid (self-signed certificate)".into()
        )
    );
    assert_eq!(*accepted.lock().unwrap(), 1, "no second address tried");
}

/// The connection goes to the addresses vet checked, the first three in
/// order, and nowhere else (rpctl's `_request` over `addrs[:3]`).
#[tokio::test]
async fn connects_only_to_the_checked_addresses() {
    struct Four;
    impl Resolver for Four {
        fn resolve<'a>(&'a self, _: &'a str, _: u16) -> Pending<'a, Result<Vec<IpAddr>, String>> {
            Box::pin(async {
                Ok(["93.184.216.34", "8.8.8.8", "2606:4700::1", "1.1.1.1"]
                    .iter()
                    .map(|a| a.parse().unwrap())
                    .collect())
            })
        }
    }
    struct Record(Mutex<Vec<(IpAddr, u16, String, String)>>);
    impl Connect for Record {
        fn connect<'a>(
            &'a self,
            ip: IpAddr,
            port: u16,
            scheme: &'a str,
            host: &'a str,
            _: Duration,
        ) -> Pending<'a, Result<Box<dyn Io>, ConnectError>> {
            self.0
                .lock()
                .unwrap()
                .push((ip, port, scheme.to_string(), host.to_string()));
            Box::pin(async { Err(ConnectError::Other("refused".into())) })
        }
    }
    let record = Record(Mutex::default());
    let got = get(
        "https://Pub.Test:8443/x",
        &Options::default(),
        &Four,
        &record,
    )
    .await;
    assert_eq!(
        got.unwrap_err(),
        FetchError::Failed("could not connect to pub.test (refused)".into())
    );
    let ip = |s: &str| s.parse::<IpAddr>().unwrap();
    let https = |a: &str| (ip(a), 8443, "https".to_string(), "pub.test".to_string());
    assert_eq!(
        *record.0.lock().unwrap(),
        [
            https("93.184.216.34"),
            https("8.8.8.8"),
            https("2606:4700::1")
        ]
    );
}

/// One real page over TLS with the system's roots (run by hand: it
/// reaches the internet).
#[tokio::test]
#[ignore = "reaches the internet"]
async fn a_real_page() {
    let r = get(
        "https://example.com/",
        &Options::default(),
        &crate::netguard::SystemResolver,
        &Direct,
    )
    .await
    .unwrap();
    assert_eq!((r.status, r.content_type()), (200, "text/html".to_string()));
    assert!(String::from_utf8_lossy(&r.body).contains("Example Domain"));
}

/// Characters rpctl's http.client sends as written and the http crate
/// refuses go percent-encoded; a non-ASCII path fails before any
/// connection, as http.client fails to encode the request line.
#[tokio::test]
async fn quoted_queries_and_odd_paths() {
    let (got, seen) = fetch(
        "/page?q=\"exact phrase\"<b>".replace(' ', "+").as_str(),
        Options::default(),
    )
    .await;
    assert_eq!(got.unwrap().status, 200);
    assert_eq!(
        seen.lock().unwrap()[0].1,
        "/page?q=%22exact+phrase%22%3Cb%3E"
    );
    let (got, seen) = fetch("/a`b<c>{d}|e^f\"g", Options::default()).await;
    assert_eq!(got.unwrap().status, 404);
    assert_eq!(seen.lock().unwrap()[0].1, "/a%60b%3Cc%3E{d}|e^f\"g");
    let (got, seen) = fetch("/caf\u{e9}", Options::default()).await;
    assert_eq!(
        got.unwrap_err(),
        FetchError::Failed(
            "could not request http://pub.test/caf\u{e9} (its path is not ASCII)".into()
        )
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// rpctl's `if not loc` and `accept or default`.
#[tokio::test]
async fn an_empty_location_or_accept_is_none() {
    let (got, seen) = fetch("/empty", Options::default()).await;
    assert_eq!(
        got.unwrap_err(),
        FetchError::Failed("HTTP 302 without a Location header".into())
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    let opts = Options {
        accept: Some(String::new()),
        ..Options::default()
    };
    let (got, seen) = fetch("/page", opts).await;
    assert_eq!(got.unwrap().status, 200);
    let seen = seen.lock().unwrap();
    let accept = seen[0]
        .2
        .iter()
        .find(|(k, _)| k == "Accept")
        .map(|(_, v)| v.clone());
    assert_eq!(
        accept.as_deref(),
        Some("text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5")
    );
}

/// The deadline covers connecting and waiting for the head too, over
/// every address tried.
#[tokio::test]
async fn the_deadline_covers_the_request() {
    struct Three;
    impl Resolver for Three {
        fn resolve<'a>(&'a self, _: &'a str, _: u16) -> Pending<'a, Result<Vec<IpAddr>, String>> {
            Box::pin(async {
                Ok(["93.184.216.34", "8.8.8.8", "1.1.1.1"]
                    .iter()
                    .map(|a| a.parse().unwrap())
                    .collect())
            })
        }
    }
    let (port, _) = server().await;
    let opts = Options {
        deadline: Duration::from_secs(1),
        ..Options::default()
    };
    let started = Instant::now();
    let got = get("http://pub.test/hang", &opts, &Three, &Local(port)).await;
    assert_eq!(
        got.unwrap_err(),
        FetchError::Failed("could not connect to pub.test (timed out)".into())
    );
    assert!(
        started.elapsed() < Duration::from_millis(1900),
        "{:?}",
        started.elapsed()
    );
}
