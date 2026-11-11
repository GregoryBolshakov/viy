//! rpctl's ConnectTests, PlainHttpTests and LimitTests (test_egress.py;
//! netaccess threats T4, T6, T9). No internet: the resolver and the
//! connector are faked, and upstream is a local server. The tests talk to
//! the proxy over plain sockets, as rpctl's; the proxy runs on a runtime of
//! its own.

use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::TcpListener as StdListener;
use std::net::TcpStream as StdStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::egress::NOT_ALLOWED;
use crate::egress::fakes::*;

/// A proxy serving a listener on this host's loopback (the sandbox's
/// namespace is the sandbox's business), with every lookup and connection
/// faked.
struct Harness {
    rt: tokio::runtime::Runtime,
    proxy: Proxy,
    addr: SocketAddr,
    up: Upstream,
    resolved: Arc<Mutex<Vec<String>>>,
    connected: Arc<Mutex<Vec<(IpAddr, u16)>>>,
}

struct Start<'a> {
    command: &'a str,
    user_hosts: &'a [&'a str],
    allow: &'a str,
    answers: Answers,
    limits: Limits,
}

impl Default for Start<'_> {
    fn default() -> Self {
        Self {
            command: "",
            user_hosts: &[],
            allow: "",
            answers: Box::new(|_| Ok(vec![ip(PUBLIC)])),
            limits: Limits::default(),
        }
    }
}

impl Harness {
    fn start(s: Start<'_>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let up = Upstream::start();
        let (resolved, connected) = (Arc::default(), Arc::default());
        let proxy = {
            let _in = rt.enter();
            Proxy::with(
                Policy::new(s.command, s.user_hosts.iter().copied(), s.allow),
                Arc::new(FakeResolver {
                    resolved: Arc::clone(&resolved),
                    answers: s.answers,
                }),
                Arc::new(ToUpstream {
                    port: up.port,
                    connected: Arc::clone(&connected),
                }),
                s.limits,
            )
        };
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        proxy.serve(listener);
        Self {
            rt,
            proxy,
            addr,
            up,
            resolved,
            connected,
        }
    }

    fn with(command: &str) -> Self {
        Self::start(Start {
            command,
            ..Start::default()
        })
    }

    fn open(&self) -> StdStream {
        let s = StdStream::connect_timeout(&self.addr, Duration::from_secs(5)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }

    /// Sends one request head; returns the status, the headers, what came
    /// after the head, and the socket.
    fn ask(&self, head: &[u8]) -> (Option<u16>, HashMap<String, String>, Vec<u8>, StdStream) {
        let mut s = self.open();
        let _ = s.write_all(head);
        let mut data = Vec::new();
        let mut buf = [0u8; 65536];
        while !data.windows(4).any(|w| w == b"\r\n\r\n") {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => data.extend_from_slice(&buf[..n]),
            }
        }
        let end = data
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(data.len());
        let top = String::from_utf8_lossy(&data[..end]).into_owned();
        let rest = data.get(end + 4..).unwrap_or_default().to_vec();
        let mut lines = top.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok());
        let headers = lines
            .filter_map(|l| l.split_once(": "))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (status, headers, rest, s)
    }

    fn connect(&self, target: &str) -> (Option<u16>, HashMap<String, String>, Vec<u8>, StdStream) {
        self.ask(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
    }

    fn status(&self, target: &str) -> Option<u16> {
        self.connect(target).0
    }

    fn assert_blocked(&self, target: &str, why: Option<&str>) {
        let (status, headers, _, _) = self.connect(target);
        assert_eq!(status, Some(403), "{target}");
        let said = headers
            .get("X-Proxy-Blocked")
            .unwrap_or_else(|| panic!("{target}: no X-Proxy-Blocked"));
        if let Some(why) = why {
            assert!(said.contains(why), "{target}: {said}");
        }
    }

    fn resolved(&self) -> Vec<String> {
        self.resolved.lock().unwrap().clone()
    }

    fn connected(&self) -> Vec<(IpAddr, u16)> {
        self.connected.lock().unwrap().clone()
    }

    fn wait_idle(&self) -> bool {
        self.rt
            .block_on(self.proxy.wait_idle(Duration::from_secs(2)))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.proxy.close();
    }
}

fn recv(s: &mut StdStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    let got = s.read(&mut buf).unwrap_or(0);
    buf.truncate(got);
    buf
}

// ------------------------------------------------------------------ CONNECT

#[test]
fn registry_tunnel_relays_to_the_vetted_address() {
    let h = Harness::with("");
    let (status, _, _, mut s) = h.connect("pypi.org:443");
    assert_eq!(status, Some(200));
    s.write_all(b"\x16\x03\x01hello").unwrap();
    assert_eq!(recv(&mut s, 100), b"\x16\x03\x01hello");
    assert_eq!(h.connected(), [(ip(PUBLIC), 443)]);
    assert_eq!(
        h.proxy.report()["reached"],
        json!([{"host": "pypi.org", "connections": 1}])
    );
}

#[test]
fn bytes_sent_with_the_connect_are_forwarded() {
    let h = Harness::with("");
    let (_, _, rest, mut s) = h.ask(b"CONNECT pypi.org:443 HTTP/1.1\r\n\r\nearly");
    let mut got = rest;
    while got.len() < 5 {
        let more = recv(&mut s, 100);
        if more.is_empty() {
            break;
        }
        got.extend(more);
    }
    assert_eq!(got, b"early");
}

#[test]
fn unknown_host_is_blocked_before_any_lookup() {
    let h = Harness::with("pip install requests");
    h.assert_blocked("collect.evil.test:443", Some(NOT_ALLOWED));
    assert_eq!(
        h.resolved(),
        Vec::<String>::new(),
        "no DNS: no DNS exfiltration"
    );
    assert_eq!(h.connected(), []);
    assert_eq!(
        h.proxy.report()["blocked"],
        json!([{"host": "collect.evil.test", "port": 443, "why": NOT_ALLOWED}])
    );
    assert!(h.proxy.lines().contains(&format!(
        "[net blocked] collect.evil.test:443 - {NOT_ALLOWED}"
    )));
}

#[test]
fn hosts_from_the_command_the_user_and_the_config() {
    let h = Harness::start(Start {
        command: "curl -fsSL https://example.com/install.txt",
        user_hosts: &["docs.example.org"],
        allow: "mirror.corp.test, *.pkg.test",
        ..Start::default()
    });
    for ok in [
        "example.com:443",
        "docs.example.org:443",
        "mirror.corp.test:443",
        "a.pkg.test:443",
        "b.a.pkg.test:80",
    ] {
        assert_eq!(h.status(ok), Some(200), "{ok}");
    }
    for no in [
        "pkg.test:443",
        "evil-pkg.test:443",
        "example.com.evil.test:443",
        "other.example.com:443",
    ] {
        h.assert_blocked(no, Some(NOT_ALLOWED));
    }
}

#[test]
fn port_rules() {
    let h = Harness::with("curl https://example.com:9443/x; git clone ssh://pypi.org:22/x");
    assert_eq!(h.status("example.com:9443"), Some(200));
    assert_eq!(h.status("example.com:80"), Some(200));
    h.assert_blocked("pypi.org:9443", Some("port 9443"));
    h.assert_blocked("pypi.org:22", Some("ssh"));
    h.assert_blocked("example.com:22", Some("ssh"));
    h.assert_blocked("pypi.org:8443", Some("port 8443"));
}

#[test]
fn local_names_are_refused() {
    let h = Harness::with("curl http://localhost/ http://nas.local/ http://router.lan/");
    for target in [
        "localhost:443",
        "LOCALHOST.:443",
        "nas.local:443",
        "x.internal:443",
        "router.lan:443",
        "a.localhost:443",
        "box.home.arpa:443",
    ] {
        h.assert_blocked(target, None);
    }
    assert_eq!(h.status("ⓛocalhost:443"), Some(400), "heads are ASCII");
    assert_eq!(h.resolved(), Vec::<String>::new());
}

#[test]
fn ip_literals_in_every_ssrf_form() {
    let forms = [
        "127.0.0.1",
        "127.255.0.9",
        "127.1",
        "2130706433",
        "0x7f.0.0.1",
        "0x7f000001",
        "0177.0.0.1",
        "10.0.0.1",
        "192.168.1.1",
        "172.16.0.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "0",
        "[::1]",
        "[::]",
        "[::ffff:127.0.0.1]",
        "[::ffff:10.0.0.1]",
        "[2002:7f00:1::]",
        "[64:ff9b::7f00:1]",
        "[fe80::1]",
        "[fd12::1]",
        "[2001:0:4136:e378:8000:63bf:80ff:fffe]",
        "1.2.3.4.5",
    ];
    // Writing them in the command does not make them reachable.
    let cmd = format!(
        "curl {}",
        forms
            .iter()
            .map(|f| format!("http://{f}/"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let h = Harness::with(&cmd);
    for f in forms {
        h.assert_blocked(&format!("{f}:443"), None);
        let status = h
            .ask(format!("GET http://{f}/ HTTP/1.1\r\n\r\n").as_bytes())
            .0;
        assert!(matches!(status, Some(400 | 403)), "{f}: {status:?}");
    }
    assert_eq!(h.resolved(), Vec::<String>::new());
    assert_eq!(h.connected(), []);
}

#[test]
fn public_ip_literal_from_the_command() {
    let h = Harness::with(&format!("curl http://{PUBLIC}/"));
    assert_eq!(h.status(&format!("{PUBLIC}:443")), Some(200));
    assert_eq!(h.resolved(), Vec::<String>::new());
    h.assert_blocked("8.8.8.8:443", Some(NOT_ALLOWED));
}

#[test]
fn public_name_resolving_inward_is_refused() {
    let answers: HashMap<&str, Vec<IpAddr>> = HashMap::from([
        ("rebind.example.com", vec![ip("127.0.0.1")]),
        ("mixed.example.com", vec![ip(PUBLIC), ip("10.0.0.7")]),
        ("meta.example.com", vec![ip("169.254.169.254")]),
        ("v6.example.com", vec![ip("::ffff:127.0.0.1")]),
    ]);
    let cmd = format!(
        "curl {}",
        answers
            .keys()
            .map(|h| format!("https://{h}/"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let names: Vec<&str> = answers.keys().copied().collect();
    let h = Harness::start(Start {
        command: &cmd,
        answers: Box::new(move |h| Ok(answers[h].clone())),
        ..Start::default()
    });
    for name in names {
        h.assert_blocked(&format!("{name}:443"), Some("resolves to"));
    }
    assert_eq!(h.connected(), []);
}

#[test]
fn lookup_failure_answers_502() {
    let h = Harness::start(Start {
        answers: Box::new(|h| Err(format!("could not resolve {h} (Uncategorized)"))),
        ..Start::default()
    });
    assert_eq!(h.status("pypi.org:443"), Some(502));
    assert_eq!(h.proxy.report()["failed"][0]["host"], "pypi.org");
}

// --------------------------------------------------------------- plain HTTP

#[test]
fn rewritten_to_origin_form_with_connection_close() {
    let h = Harness::with("curl http://example.com/a");
    let (status, ..) = h.ask(
        b"GET http://example.com/a?b=1 HTTP/1.1\r\nHost: evil.test\r\n\
          Proxy-Authorization: Basic eDp5\r\nProxy-Connection: keep-alive\r\n\
          Connection: keep-alive\r\nKeep-Alive: 300\r\nUser-Agent: t\r\n\r\n",
    );
    assert_eq!(status, Some(200));
    let heads = h.up.heads();
    let head: Vec<&str> = heads.last().unwrap().split("\r\n").collect();
    assert_eq!(head[0], "GET /a?b=1 HTTP/1.1");
    let names: Vec<String> = head[1..]
        .iter()
        .map(|l| l.split(':').next().unwrap().to_lowercase())
        .collect();
    for gone in ["proxy-authorization", "proxy-connection", "keep-alive"] {
        assert!(!names.iter().any(|n| n == gone), "{gone}");
    }
    assert!(
        head.contains(&"Host: example.com"),
        "never the client's Host"
    );
    assert!(head.contains(&"Connection: close"));
    assert_eq!(names.iter().filter(|n| *n == "connection").count(), 1);
    assert!(head.contains(&"User-Agent: t"));
}

#[test]
fn port_in_host_header_and_blocked_host() {
    let h = Harness::with("curl http://example.com:8080/");
    assert_eq!(
        h.ask(b"GET http://example.com:8080/ HTTP/1.1\r\n\r\n").0,
        Some(200)
    );
    let heads = h.up.heads();
    assert!(
        heads
            .last()
            .unwrap()
            .split("\r\n")
            .any(|l| l == "Host: example.com:8080")
    );
    let (status, headers, _, _) = h.ask(b"GET http://evil.test/x HTTP/1.1\r\n\r\n");
    assert_eq!(status, Some(403));
    assert_eq!(headers["X-Proxy-Blocked"], NOT_ALLOWED);
}

#[test]
fn userinfo_and_smuggling_shapes() {
    let h = Harness::with("curl http://example.com/");
    assert_eq!(
        h.ask(b"GET http://u:p@example.com/ HTTP/1.1\r\n\r\n").0,
        Some(403)
    );
    assert_eq!(
        h.ask(
            b"POST http://example.com/ HTTP/1.1\r\nContent-Length: 1\r\n\
              Transfer-Encoding: chunked\r\n\r\n"
        )
        .0,
        Some(400)
    );
}

#[test]
fn requests_that_are_not_proxy_requests() {
    let h = Harness::with("");
    let heads: [&[u8]; 11] = [
        b"GET / HTTP/1.1\r\nHost: pypi.org\r\n\r\n",
        b"GET https://pypi.org/ HTTP/1.1\r\n\r\n",
        b"CONNECT pypi.org HTTP/1.1\r\n\r\n",
        b"CONNECT pypi.org:99999 HTTP/1.1\r\n\r\n",
        b"CONNECT pypi.org:4x3 HTTP/1.1\r\n\r\n",
        b"HELLO\r\n\r\n",
        b"GET http://pypi.org/ HTTP/2.0\r\n\r\n",
        b"GET http://pypi.org/ HTTP/1.1\r\nbad header\r\n\r\n",
        b"GET http://pypi.org/ HTTP/1.1\r\n folded: x\r\n\r\n",
        b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03\r\n\r\n",
        b"GET http://pypi.org/\xff HTTP/1.1\r\n\r\n",
    ];
    for head in heads {
        let (status, headers, _, _) = h.ask(head);
        assert_eq!(status, Some(400), "{}", String::from_utf8_lossy(head));
        assert!(headers.contains_key("X-Proxy-Blocked"));
    }
    assert_eq!(h.connected(), []);
    assert!(!h.proxy.report()["blocked"].as_array().unwrap().is_empty());
}

#[test]
fn oversized_head() {
    let h = Harness::with("");
    let big = format!(
        "GET http://pypi.org/ HTTP/1.1\r\nX: {}\r\n\r\n",
        "a".repeat(17 * 1024)
    );
    let (status, headers, _, _) = h.ask(big.as_bytes());
    assert_eq!(status, Some(400));
    assert!(headers["X-Proxy-Blocked"].contains("16 KiB"));
    // No end of head at all.
    let mut s = h.open();
    s.write_all(&[b'A'; 20 * 1024]).unwrap();
    let got = recv(&mut s, 200);
    assert!(String::from_utf8_lossy(&got).contains(" 400 "), "{got:?}");
}

#[test]
fn garbage_and_hangups_do_not_break_the_proxy() {
    let h = Harness::with("");
    let mut s = h.open();
    s.write_all(&b"\x00\xff".repeat(100)).unwrap();
    drop(s);
    drop(h.open());
    assert_eq!(h.status("pypi.org:443"), Some(200));
}

// ------------------------------------------------------------------- limits

fn limited(limits: Limits) -> Harness {
    Harness::start(Start {
        limits,
        ..Start::default()
    })
}

#[test]
fn simultaneous_cap() {
    let h = limited(Limits {
        max_active: 2,
        ..Limits::default()
    });
    let held: Vec<StdStream> = (0..2).map(|_| h.connect("pypi.org:443").3).collect();
    let (status, headers, _, _) = h.connect("pypi.org:443");
    assert_eq!(status, Some(503));
    assert!(headers["X-Proxy-Blocked"].contains("at once"));
    drop(held);
    assert!(h.wait_idle());
    assert_eq!(h.status("pypi.org:443"), Some(200));
}

#[test]
fn total_cap() {
    let h = limited(Limits {
        max_total: 3,
        ..Limits::default()
    });
    for _ in 0..3 {
        drop(h.connect("pypi.org:443").3);
    }
    assert_eq!(h.status("pypi.org:443"), Some(503));
}

#[test]
fn idle_timeout_closes_a_silent_tunnel() {
    let h = limited(Limits {
        idle_timeout: Duration::from_millis(300),
        ..Limits::default()
    });
    let (_, _, _, mut s) = h.connect("pypi.org:443");
    let t = Instant::now();
    assert_eq!(recv(&mut s, 10), b"");
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn traffic_keeps_a_tunnel_open() {
    let h = limited(Limits {
        idle_timeout: Duration::from_millis(400),
        ..Limits::default()
    });
    let (_, _, _, mut s) = h.connect("pypi.org:443");
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(200));
        s.write_all(b"x").unwrap();
        assert_eq!(recv(&mut s, 10), b"x");
    }
}

#[test]
fn close_ends_tunnels_and_the_listener() {
    let h = Harness::with("");
    let (_, _, _, mut s) = h.connect("pypi.org:443");
    h.proxy.close();
    // An empty read or a reset: either way the tunnel is gone.
    assert_eq!(recv(&mut s, 10), b"");
    let refused = match StdStream::connect_timeout(&h.addr, Duration::from_secs(2)) {
        Err(_) => true,
        Ok(mut c) => {
            c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let _ = c.write_all(b"CONNECT pypi.org:443 HTTP/1.1\r\n\r\n");
            recv(&mut c, 10).is_empty()
        }
    };
    assert!(refused);
    assert!(h.proxy.listener_closed());
    assert_eq!(h.proxy.report()["reached"][0]["connections"], 1);
}

#[test]
fn blocked_entries_are_capped() {
    let h = Harness::with("");
    for i in 0..MAX_REPORTED + 5 {
        h.connect(&format!("h{i}.evil.test:443"));
    }
    let rep = h.proxy.report();
    assert_eq!(rep["blocked"].as_array().unwrap().len(), MAX_REPORTED);
    assert_eq!(rep["not_listed"], 5);
}

#[test]
fn a_late_serve_after_close_closes_the_listener() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let proxy = {
        let _in = rt.enter();
        Proxy::new(Policy::default())
    };
    proxy.close();
    let listener = StdListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    proxy.serve(listener);
    assert!(StdStream::connect_timeout(&addr, Duration::from_secs(2)).is_err());
}

#[test]
fn megabytes_are_rounded_as_rp_prints_them() {
    let h = Harness::with("");
    h.proxy.record_reached("pypi.org", 3, 20_000, 900, 443);
    h.proxy
        .record_reached("files.pythonhosted.org", 12, 41_000_000, 5_000, 443);
    h.proxy.record_reached("a.test", 1, 2_500_000, 0, 443);
    assert_eq!(
        h.proxy.lines(),
        ["[net] pypi.org (3), files.pythonhosted.org (12, 41 MB), a.test (1, 2 MB)"]
    );
}

// ------------------------------------------------------------------- oracle

/// The whole of a connection, viy against rpctl's `Proxy._handle` (the
/// oracle's `egress.handle`): what the command reads, what upstream reads,
/// the lookups, the connections, the report and the printed lines, for
/// generated request heads and commands
/// (`cargo test -p viy-tools --features oracle`).
#[cfg(feature = "oracle")]
#[test]
fn requests_match_rpctl() {
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    const METHODS: &[&str] = &["CONNECT", "GET", "POST", "HEAD", "get", "G ET", "", "C\x01"];
    const HOSTS: &[&str] = &[
        "pypi.org",
        "Pypi.Org.",
        "example.com",
        "evil.test",
        "rebind.example.com",
        "mixed.example.com",
        "nx.example.com",
        "127.0.0.1",
        "127.1",
        "0x7f.1",
        "2130706433",
        "93.184.216.34",
        "8.8.8.8",
        "[::1]",
        "[2606:4700::1]",
        "[::ffff:8.8.8.8]",
        "[::ffff:127.0.0.1]",
        "[1.2.3.4]",
        "[v1.x]",
        "localhost",
        "a.local",
        "xn--bcher-kva.de",
        "a..b",
        "-a.test",
        "a_b.test",
        "1.2.3.4.5",
        "u:p@example.com",
        "@",
        "%",
        "[",
        "]",
        "",
    ];
    const PORTS: &[&str] = &[
        "", ":443", ":80", ":8080", ":9443", ":22", ":0", ":", ":99999", ":4x3", ":+80", ":080",
    ];
    const PATHS: &[&str] = &["", "/", "/a?b=1", "/x#f", "?q", "/a b", "/\x7f", "#"];
    const VERSIONS: &[&str] = &["HTTP/1.1", "HTTP/1.0", "HTTP/2.0", "http/1.1", ""];
    const HEADERS: &[&str] = &[
        "Host: evil.test\r\n",
        "Proxy-Authorization: Basic eDp5\r\n",
        "Connection: keep-alive\r\n",
        "Keep-Alive: 300\r\n",
        "User-Agent: t\r\n",
        "Content-Length: 1\r\n",
        "Transfer-Encoding: chunked\r\n",
        "TE: trailers\r\n",
        "X:  spaced \t\r\n",
        "bad header\r\n",
        " folded: x\r\n",
        "X: a\x01b\r\n",
        "X: a\x7fb\r\n",
        "X: é\r\n",
    ];
    const COMMANDS: &[&str] = &[
        "",
        "pip install requests",
        "curl https://example.com:9443/ http://[2606:4700::1]:8080/ && wget evil.test:8080/x",
        "curl http://rebind.example.com/ https://mixed.example.com/ nx.example.com",
        "git clone ssh://pypi.org:22/x; curl http://93.184.216.34/ http://8.8.8.8:0/",
        "curl -o example.com https://xn--bcher-kva.de/ http://[v1.x]/",
    ];
    let answers = json!({
        "rebind.example.com": ["127.0.0.1"],
        "mixed.example.com": [PUBLIC, "10.0.0.7"],
        "nx.example.com": "could not resolve nx.example.com (gaierror)",
    });

    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    for case in 0..cases(1500) {
        let method = *g.pick(METHODS);
        let host = *g.pick(HOSTS);
        let port = *g.pick(PORTS);
        let target = if method == "CONNECT" && g.below(4) > 0 {
            format!("{host}{port}")
        } else {
            let scheme = *g.pick(&["http://", "HTTP://", "https://", "", "http:/"]);
            format!("{scheme}{host}{port}{}", g.pick(PATHS))
        };
        let version = *g.pick(VERSIONS);
        let headers = g.string(HEADERS, 4);
        let body = *g.pick(&["", "early", "\x16\x03\x01"]);
        let mut head = format!("{method} {target} {version}\r\n{headers}\r\n").into_bytes();
        if g.below(10) == 0 {
            head.push(0xff);
        }
        head.extend_from_slice(body.as_bytes());
        let command = *g.pick(COMMANDS);
        let at = format!(
            "case {case}, seed {:#x}: {:?} with {command:?}",
            seed(),
            String::from_utf8_lossy(&head)
        );

        let want = oracle
            .call(
                "egress.handle",
                json!([{"$bytes": b64(&head)}, command, answers]),
                json!({}),
            )
            .unwrap()
            .outcome;
        let Outcome::Returned(want) = want else {
            panic!("{at}: {want:?}")
        };
        // rpctl's refusals name it; viy's name viy (flags F21, kb d131)
        let want = viy_testkit::rp_names::answer_to_viy(&want);
        let got = viy_handle(&head, command, &answers);
        assert_eq!(got, want, "{at}");
    }
}

#[cfg(feature = "oracle")]
fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// viy's side of the oracle's `egress.handle`, in its shape.
#[cfg(feature = "oracle")]
fn viy_handle(head: &[u8], command: &str, answers: &serde_json::Value) -> serde_json::Value {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    // Upstream: reads until the proxy half-closes, then closes.
    let sink = StdListener::bind("127.0.0.1:0").unwrap();
    let sink_port = sink.local_addr().unwrap().port();
    let upstream: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
    let kept = Arc::clone(&upstream);
    std::thread::spawn(move || {
        if let Ok((mut c, _)) = sink.accept() {
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut all = Vec::new();
            let _ = c.read_to_end(&mut all);
            *kept.lock().unwrap() = Some(all);
        }
    });
    let table = answers.clone();
    let (resolved, connected) = (Arc::default(), Arc::default());
    let proxy = {
        let _in = rt.enter();
        Proxy::with(
            Policy::new(command, [], ""),
            Arc::new(FakeResolver {
                resolved: Arc::clone(&resolved),
                answers: Box::new(move |h| match table.get(h) {
                    Some(serde_json::Value::String(e)) => Err(e.clone()),
                    Some(list) => Ok(list
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|a| ip(a.as_str().unwrap()))
                        .collect()),
                    None => Ok(vec![ip(PUBLIC)]),
                }),
            }),
            Arc::new(ToUpstream {
                port: sink_port,
                connected: Arc::clone(&connected),
            }),
            Limits {
                idle_timeout: Duration::from_secs(2),
                ..Limits::default()
            },
        )
    };
    let listener = StdListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    proxy.serve(listener);
    let mut c = StdStream::connect(addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(head).unwrap();
    c.shutdown(std::net::Shutdown::Write).unwrap();
    let mut out = Vec::new();
    let _ = c.read_to_end(&mut out);
    let went_up = !connected.lock().unwrap().is_empty();
    let deadline = Instant::now() + Duration::from_secs(5);
    while went_up && upstream.lock().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(rt.block_on(proxy.wait_idle(Duration::from_secs(5))));
    let up = upstream.lock().unwrap().clone();
    let resolved = resolved.lock().unwrap().clone();
    let connected: Vec<serde_json::Value> = connected
        .lock()
        .unwrap()
        .iter()
        .map(|(a, p)| json!([crate::netguard::show(*a), p]))
        .collect();
    let out = json!({
        "client": {"$bytes": b64(&out)},
        "upstream": up.map(|u| json!({"$bytes": b64(&u)})),
        "resolved": resolved,
        "connected": connected,
        "report": proxy.report(),
        "lines": proxy.lines(),
    });
    proxy.close();
    out
}
