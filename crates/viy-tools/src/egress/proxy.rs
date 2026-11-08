//! The proxy that serves one command's listener (rpctl egress.py `Proxy`).
//!
//! rpctl serves each connection on a thread of its own; here each is a
//! task on the runtime the proxy was made on (kb d012), and `close` ends
//! the listener and every connection through one signal.

use std::io;
use std::net::IpAddr;
use std::net::TcpListener as StdListener;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;
use std::time::Instant;

use indexmap::IndexMap;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::runtime::Handle;
use tokio::sync::watch;

use super::Policy;
use super::canonical_host;
use super::head::Answer;
use super::head::origin_form;
use super::head::parse_head;
use super::head::response;
use super::head::safe;
use super::head::split_http_url;
use super::split_authority;
use crate::approve::Pending;
use crate::netguard;
use crate::netguard::Resolver;
use crate::netguard::SystemResolver;
use crate::netguard::VetError;
use crate::sandbox::Egress;

/// Distinct blocked or failed targets kept for the report.
pub const MAX_REPORTED: usize = 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an answer to a connection over the caps may take to send.
const OVER_SEND_TIMEOUT: Duration = Duration::from_millis(500);
/// The pause before accepting again after an accept error.
const ACCEPT_RETRY: Duration = Duration::from_millis(10);

/// The proxy's caps (rp's `Proxy` keyword arguments).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Seconds without traffic either way before a connection is closed.
    pub idle_timeout: Duration,
    /// Simultaneous connections per command.
    pub max_active: usize,
    /// Connections per command.
    pub max_total: usize,
    /// Bytes of request line and headers.
    pub max_head: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            max_active: 32,
            max_total: 256,
            max_head: 16 * 1024,
        }
    }
}

/// Opens the upstream connection to a vetted address. A trait so tests
/// fake the internet.
pub trait Connector: Send + Sync {
    fn connect(
        &self,
        ip: IpAddr,
        port: u16,
        timeout: Duration,
    ) -> Pending<'_, io::Result<TcpStream>>;
}

/// The real connection, within `timeout`.
pub struct TcpConnector;

impl Connector for TcpConnector {
    fn connect(
        &self,
        ip: IpAddr,
        port: u16,
        timeout: Duration,
    ) -> Pending<'_, io::Result<TcpStream>> {
        Box::pin(async move {
            match tokio::time::timeout(timeout, TcpStream::connect((ip, port))).await {
                Ok(r) => r,
                Err(_) => Err(io::ErrorKind::TimedOut.into()),
            }
        })
    }
}

/// One host the command reached.
#[derive(Debug, Clone, Default)]
struct Reached {
    connections: u64,
    down: u64,
    up: u64,
    ports: std::collections::BTreeSet<u16>,
}

/// A blocked or failed target, as reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: Option<u16>,
    pub why: String,
}

#[derive(Default)]
struct State {
    closed: bool,
    active: usize,
    total: usize,
    listener: Option<StdListener>,
    /// In the order first reached, as rp's dict.
    reached: IndexMap<String, Reached>,
    blocked: Vec<(Target, u64)>,
    failed: Vec<(Target, u64)>,
    dropped: u64,
}

impl State {
    /// The entry of `host`, made at the end when new.
    fn reach(&mut self, host: &str) -> &mut Reached {
        self.reached.entry(host.to_string()).or_default()
    }
}

struct Inner {
    policy: Policy,
    resolver: Arc<dyn Resolver>,
    connector: Arc<dyn Connector>,
    limits: Limits,
    runtime: Handle,
    state: Mutex<State>,
    /// `true` once closed: every task stops.
    closing: watch::Sender<bool>,
}

/// Serves one command's listener. [`Egress::serve`] starts it,
/// [`Egress::close`] ends it and every connection; [`Proxy::report`] and
/// [`Proxy::lines`] are what the model and the user see.
#[derive(Clone)]
pub struct Proxy(Arc<Inner>);

/// What the proxy saw, for the tracer (rp's `events`).
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: &'static str,
    pub fields: Value,
}

/// The run_shell result's `network` entry (rp's `report`).
pub type Report = serde_json::Map<String, Value>;

impl Proxy {
    /// A proxy for `policy` with the system's resolver and real
    /// connections, on the current runtime.
    pub fn new(policy: Policy) -> Self {
        Self::with(
            policy,
            Arc::new(SystemResolver),
            Arc::new(TcpConnector),
            Limits::default(),
        )
    }

    /// A proxy with its lookups, connections and caps given. Must be made
    /// inside a tokio runtime, which serves it.
    pub fn with(
        policy: Policy,
        resolver: Arc<dyn Resolver>,
        connector: Arc<dyn Connector>,
        limits: Limits,
    ) -> Self {
        Self(Arc::new(Inner {
            policy,
            resolver,
            connector,
            limits,
            runtime: Handle::current(),
            state: Mutex::default(),
            closing: watch::Sender::new(false),
        }))
    }

    pub fn policy(&self) -> &Policy {
        &self.0.policy
    }

    /// Connections the command opened, refused ones included.
    pub fn total(&self) -> usize {
        self.0.lock().total
    }

    /// Tests: waits until no connection is being handled.
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.0.lock().active == 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// Whether the listener is closed.
    pub fn listener_closed(&self) -> bool {
        self.0.lock().listener.is_none()
    }

    /// The `network` entry of a run_shell result: hosts reached with their
    /// connection counts, targets blocked, and failed ones and the count
    /// not listed when there are any.
    pub fn report(&self) -> Report {
        let st = self.0.lock();
        let listed = |rows: &[(Target, u64)]| {
            rows.iter()
                .map(|(t, _)| json!({"host": t.host, "port": t.port, "why": t.why}))
                .collect::<Vec<_>>()
        };
        let mut out = Report::new();
        out.insert(
            "reached".into(),
            st.reached
                .iter()
                .map(|(h, r)| json!({"host": h, "connections": r.connections}))
                .collect(),
        );
        out.insert("blocked".into(), listed(&st.blocked).into());
        if !st.failed.is_empty() {
            out.insert("failed".into(), listed(&st.failed).into());
        }
        if st.dropped > 0 {
            out.insert("not_listed".into(), st.dropped.into());
        }
        out
    }

    /// The printed summary: `[net] pypi.org (3), files.pythonhosted.org
    /// (12, 41 MB)` and one `[net blocked]` line per blocked target.
    pub fn lines(&self) -> Vec<String> {
        let st = self.0.lock();
        let mut out = Vec::new();
        if !st.reached.is_empty() {
            let hosts: Vec<String> = st
                .reached
                .iter()
                .map(|(h, r)| {
                    if r.down >= 1_000_000 {
                        format!("{h} ({}, {:.0} MB)", r.connections, r.down as f64 / 1e6)
                    } else {
                        format!("{h} ({})", r.connections)
                    }
                })
                .collect();
            out.push(format!("[net] {}", hosts.join(", ")));
        }
        for (label, rows) in [("blocked", &st.blocked), ("failed", &st.failed)] {
            for (t, _) in rows {
                let port = t.port.map(|p| format!(":{p}")).unwrap_or_default();
                out.push(format!("[net {label}] {}{port} - {}", t.host, t.why));
            }
        }
        if st.dropped > 0 {
            out.push(format!("[net blocked] {} more not listed", st.dropped));
        }
        out
    }

    /// For the tracer: `egress_allowed` per host, `egress_blocked` and
    /// `egress_failed` per target.
    pub fn events(&self) -> Vec<Event> {
        let st = self.0.lock();
        let mut out: Vec<Event> = st
            .reached
            .iter()
            .map(|(h, r)| Event {
                kind: "egress_allowed",
                fields: json!({"host": h, "ports": r.ports, "connections": r.connections,
                               "bytes_down": r.down, "bytes_up": r.up}),
            })
            .collect();
        for (kind, rows) in [
            ("egress_blocked", &st.blocked),
            ("egress_failed", &st.failed),
        ] {
            out.extend(rows.iter().map(|(t, n)| Event {
                kind,
                fields: json!({"host": t.host, "port": t.port, "reason": t.why, "count": n}),
            }));
        }
        out
    }

    /// Tests: what a command's traffic would have counted.
    #[cfg(any(test, feature = "test-support"))]
    pub fn record_reached(&self, host: &str, connections: u64, down: u64, up: u64, port: u16) {
        let mut st = self.0.lock();
        let r = st.reach(host);
        r.connections += connections;
        r.down += down;
        r.up += up;
        r.ports.insert(port);
    }

    /// Tests: a blocked target, as the proxy records one.
    #[cfg(any(test, feature = "test-support"))]
    pub fn record_blocked(&self, host: &str, port: Option<u16>, why: &str) {
        self.0.record(false, host, port, why);
    }
}

/// A lock, whether or not a holder panicked: the proxy's counts stay
/// usable for the report.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Stops listening without waiting for the accept loop: a connection
    /// attempt after this is refused, not left in the backlog.
    fn drop_listener(&self) {
        if let Some(l) = self.lock().listener.take() {
            shut(&l);
        }
    }

    fn record(&self, failed: bool, host: &str, port: Option<u16>, why: &str) {
        let mut st = self.lock();
        let st = &mut *st;
        let table = if failed {
            &mut st.failed
        } else {
            &mut st.blocked
        };
        let key = Target {
            host: host.to_string(),
            port,
            why: why.to_string(),
        };
        if let Some((_, n)) = table.iter_mut().find(|(t, _)| *t == key) {
            *n += 1;
        } else if table.len() < MAX_REPORTED {
            table.push((key, 1));
        } else {
            st.dropped += 1;
        }
    }

    async fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        let mut closing = self.closing.subscribe();
        loop {
            let got = tokio::select! {
                got = listener.accept() => got,
                _ = closing.wait_for(|c| *c) => return,
            };
            let conn = match got {
                Ok((conn, _)) => conn,
                // Out of descriptors, or a connection reset before it was
                // taken: try again, unless the proxy is closing.
                Err(_) => {
                    if self.lock().closed {
                        return;
                    }
                    tokio::time::sleep(ACCEPT_RETRY).await;
                    continue;
                }
            };
            let over = {
                let mut st = self.lock();
                if st.closed {
                    return;
                }
                st.total += 1;
                let over = if st.total > self.limits.max_total {
                    Some(format!(
                        "more than {} connections from one command",
                        self.limits.max_total
                    ))
                } else if st.active >= self.limits.max_active {
                    Some(format!(
                        "more than {} connections at once",
                        self.limits.max_active
                    ))
                } else {
                    None
                };
                if over.is_none() {
                    st.active += 1;
                }
                over
            };
            if let Some(over) = over {
                self.record(false, "(any)", None, &over);
                let mut conn = conn;
                let answer = response(503, &over);
                let _ = tokio::time::timeout(OVER_SEND_TIMEOUT, conn.write_all(&answer)).await;
                continue;
            }
            let me = Arc::clone(&self);
            self.runtime.spawn(async move {
                let mut closing = me.closing.subscribe();
                tokio::select! {
                    () = Arc::clone(&me).handle(conn) => {}
                    _ = closing.wait_for(|c| *c) => {}
                }
                me.lock().active -= 1;
            });
        }
    }

    /// The head, and the bytes after it; `Ok(None)` when the command hung
    /// up first or went quiet for the idle timeout.
    async fn read_head(&self, conn: &mut TcpStream) -> Result<Option<(Vec<u8>, Vec<u8>)>, Answer> {
        let max = self.limits.max_head;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                if end + 4 > max {
                    break;
                }
                let rest = buf.split_off(end + 4);
                buf.truncate(end);
                return Ok(Some((buf, rest)));
            }
            if buf.len() > max {
                break;
            }
            match tokio::time::timeout(self.limits.idle_timeout, conn.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return Ok(None),
                Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        Err(Answer::blocked(
            400,
            format!("the request head is over {} KiB", max / 1024),
        ))
    }

    async fn handle(self: Arc<Self>, mut conn: TcpStream) {
        let mut host = "(bad request)".to_string();
        let mut port = None;
        match self.admit(&mut conn, &mut host, &mut port).await {
            Ok(Some(up)) => self.relay(conn, up, &host).await,
            Ok(None) => {}
            Err(answer) => {
                self.record(answer.failed, &host, port, &answer.reason);
                let _ = tokio::time::timeout(
                    self.limits.idle_timeout,
                    conn.write_all(&response(answer.status, &answer.reason)),
                )
                .await;
            }
        }
    }

    /// One connection from its head to the upstream stream to relay, with
    /// the first bytes sent; `Ok(None)` when there is nothing to relay.
    /// `host` and `port` say what to record against an answer.
    async fn admit(
        &self,
        conn: &mut TcpStream,
        host: &mut String,
        port: &mut Option<u16>,
    ) -> Result<Option<TcpStream>, Answer> {
        let Some((head, rest)) = self.read_head(conn).await? else {
            return Ok(None);
        };
        let head = parse_head(&head)?;
        let (raw_host, p, kind) = if head.method == "CONNECT" {
            let (raw, p) = split_authority(&head.target).map_err(|e| Answer::blocked(400, e.0))?;
            let Some(p) = p else {
                return Err(Answer::blocked(400, "CONNECT needs host:port"));
            };
            *port = Some(p);
            *host = safe(raw, 80);
            let ok = format!("{} 200 Connection established\r\n\r\n", head.version);
            (raw.to_string(), p, Kind::Connect(ok.into_bytes()))
        } else {
            let lower = head.target.to_ascii_lowercase();
            if !lower.starts_with("http://") {
                return Err(Answer::blocked(
                    400,
                    if lower.starts_with("https:") {
                        "HTTPS goes through CONNECT"
                    } else {
                        "only CONNECT and absolute http:// requests are accepted"
                    },
                ));
            }
            let Some(url) = split_http_url(&head.target) else {
                return Err(Answer::blocked(400, "the request URL does not parse"));
            };
            *port = Some(url.port);
            if url.netloc.contains('@') {
                return Err(Answer::blocked(
                    403,
                    "user names and passwords in the target are refused",
                ));
            }
            *host = safe(&url.hostname, 80);
            (url.hostname.clone(), url.port, Kind::Http(url))
        };
        let canonical = canonical_host(&raw_host).map_err(|e| Answer::blocked(403, e.0))?;
        *host = canonical;
        if let Some(why) = self.policy.why_not(host, p) {
            return Err(Answer::blocked(403, why));
        }
        // The upstream head first: a bad one is answered before any lookup.
        let kind = match kind {
            Kind::Http(url) => {
                let mut first = origin_form(&head, &url, host, p)?;
                first.extend_from_slice(&rest);
                Sent::Upstream(first)
            }
            Kind::Connect(ok) => Sent::Client(ok),
        };
        let mut up = self.open(host, p).await?;
        {
            let mut st = self.lock();
            if st.closed {
                return Ok(None);
            }
            let r = st.reach(host);
            r.connections += 1;
            r.ports.insert(p);
        }
        let idle = self.limits.idle_timeout;
        let sent = match kind {
            Sent::Upstream(first) => write(&mut up, &first, idle).await,
            Sent::Client(ok) => {
                let sent = write(conn, &ok, idle).await;
                if sent.is_ok() && !rest.is_empty() {
                    write(&mut up, &rest, idle).await
                } else {
                    sent
                }
            }
        };
        Ok(sent.is_ok().then_some(up))
    }

    /// A stream connected to a vetted address of `host`.
    async fn open(&self, host: &str, port: u16) -> Result<TcpStream, Answer> {
        let addrs = match netguard::parse_ip(host) {
            Some(ip) => vec![ip], // canonical_host checked it
            None => netguard::vet(host, port, self.resolver.as_ref())
                .await
                .map_err(|e| match e {
                    VetError::Refused(why) => Answer::blocked(403, why),
                    VetError::Failed(why) => Answer::failed(502, why),
                })?,
        };
        let mut last = None;
        for &ip in addrs.iter().take(3) {
            // vet checked; a resolver must not undo it.
            if netguard::ip_problem(ip).is_some() {
                return Err(Answer::blocked(
                    403,
                    format!("{host} resolves to a non-public address"),
                ));
            }
            match self.connector.connect(ip, port, CONNECT_TIMEOUT).await {
                Ok(s) => return Ok(s),
                Err(e) => last = Some(e),
            }
        }
        let why = last.map_or_else(|| "NotFound".to_string(), |e| netguard::kind(&e));
        Err(Answer::failed(
            502,
            format!("could not connect to {host}:{port} ({why})"),
        ))
    }

    /// Copies both ways until both sides close; each direction half-closes
    /// the other on EOF, and an error or `idle_timeout` without traffic
    /// either way ends both.
    async fn relay(&self, conn: TcpStream, up: TcpStream, host: &str) {
        let idle = self.limits.idle_timeout;
        let last = Mutex::new(Instant::now());
        let (conn_r, conn_w) = conn.into_split();
        let (up_r, up_w) = up.into_split();
        let down = self.pump(up_r, conn_w, true, host, &last, idle);
        let upload = self.pump(conn_r, up_w, false, host, &last, idle);
        // The first error or idle stop drops both directions.
        let _ = tokio::try_join!(down, upload);
    }

    async fn pump(
        &self,
        mut src: OwnedReadHalf,
        mut dst: OwnedWriteHalf,
        down: bool,
        host: &str,
        last: &Mutex<Instant>,
        idle: Duration,
    ) -> Result<(), ()> {
        let mut buf = vec![0u8; 65536];
        loop {
            let n = match tokio::time::timeout(idle, src.read(&mut buf)).await {
                Err(_) => {
                    if lock(last).elapsed() >= idle {
                        return Err(());
                    }
                    continue;
                }
                Ok(Err(_)) => return Err(()),
                Ok(Ok(0)) => {
                    let _ = dst.shutdown().await;
                    return Ok(());
                }
                Ok(Ok(n)) => n,
            };
            *lock(last) = Instant::now();
            write(&mut dst, &buf[..n], idle).await?;
            if let Some(r) = self.lock().reached.get_mut(host) {
                if down {
                    r.down += n as u64;
                } else {
                    r.up += n as u64;
                }
            }
        }
    }
}

/// What a connection admitted is: a CONNECT tunnel, or plain HTTP.
enum Kind {
    Connect(Vec<u8>),
    Http(super::head::HttpUrl),
}

/// The first bytes of an admitted connection: the tunnel's answer to the
/// command, or the request head to upstream.
enum Sent {
    Client(Vec<u8>),
    Upstream(Vec<u8>),
}

/// Writes all of `data` within `idle`.
async fn write<W: AsyncWrite + Unpin>(s: &mut W, data: &[u8], idle: Duration) -> Result<(), ()> {
    match tokio::time::timeout(idle, s.write_all(data)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

impl Egress for Proxy {
    fn serve(&self, listener: StdListener) {
        let inner = Arc::clone(&self.0);
        // A listener that cannot be served is shut at once: the command's
        // connections are refused, not left waiting in the backlog.
        let mine = match listener.try_clone() {
            Ok(l) if l.set_nonblocking(true).is_ok() => l,
            _ => return shut(&listener),
        };
        {
            let mut st = inner.lock();
            if st.closed {
                return shut(&listener); // closed mid-handover
            }
            st.listener = Some(listener);
        }
        let runtime = inner.runtime.clone();
        runtime.spawn(async move {
            match TcpListener::from_std(mine) {
                Ok(l) => inner.accept_loop(l).await,
                Err(_) => inner.drop_listener(),
            }
        });
    }

    fn close(&self) {
        self.0.lock().closed = true;
        self.0.drop_listener();
        self.0.closing.send_replace(true);
    }
}

/// Shuts a listening socket down at once, as rp's `_shut`: a connection
/// attempt after this is refused, whatever task still holds a copy.
fn shut(l: &StdListener) {
    use std::os::fd::AsRawFd;
    // SAFETY: a valid descriptor this proxy holds.
    unsafe {
        libc::shutdown(l.as_raw_fd(), libc::SHUT_RDWR);
    }
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
