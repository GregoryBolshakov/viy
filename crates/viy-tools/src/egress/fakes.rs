//! Test stand-ins for the internet: a local upstream, a resolver with
//! fixed answers and a connector that dials the upstream, shared by the
//! proxy's tests and the live sandbox tests.

use std::io;
use std::io::Read;
use std::io::Write;
use std::net::IpAddr;
use std::net::TcpListener as StdListener;
use std::net::TcpStream as StdStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::net::TcpStream;

use super::Connector;
use crate::approve::Pending;
use crate::netguard::Resolver;

pub(crate) const PUBLIC: &str = "93.184.216.34";

pub(crate) fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// A local server standing in for every internet host: plain HTTP gets a
/// fixed reply (its request head is kept), anything else is echoed.
pub(crate) struct Upstream {
    pub(crate) port: u16,
    heads: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    pub(crate) fn start() -> Self {
        let srv = StdListener::bind("127.0.0.1:0").unwrap();
        let port = srv.local_addr().unwrap().port();
        let heads: Arc<Mutex<Vec<String>>> = Arc::default();
        let kept = Arc::clone(&heads);
        std::thread::spawn(move || {
            for c in srv.incoming() {
                let Ok(c) = c else { return };
                let kept = Arc::clone(&kept);
                std::thread::spawn(move || serve(c, &kept));
            }
        });
        Self { port, heads }
    }

    pub(crate) fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }
}

fn serve(mut c: StdStream, heads: &Mutex<Vec<String>>) {
    let mut buf = vec![0u8; 65536];
    let Ok(n) = c.read(&mut buf) else { return };
    let mut data = buf[..n].to_vec();
    if [&b"GET "[..], b"POST", b"HEAD"].contains(&data.get(..4).unwrap_or_default()) {
        while !data.windows(4).any(|w| w == b"\r\n\r\n") {
            match c.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => data.extend_from_slice(&buf[..n]),
            }
        }
        let end = data.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        heads
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&data[..end]).into_owned());
        let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        return;
    }
    while !data.is_empty() {
        if c.write_all(&data).is_err() {
            return;
        }
        data = match c.read(&mut buf) {
            Ok(n) => buf[..n].to_vec(),
            Err(_) => return,
        };
    }
}

pub(crate) type Answers = Box<dyn Fn(&str) -> Result<Vec<IpAddr>, String> + Send + Sync>;

pub(crate) struct FakeResolver {
    pub(crate) resolved: Arc<Mutex<Vec<String>>>,
    pub(crate) answers: Answers,
}

impl Resolver for FakeResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        _port: u16,
    ) -> Pending<'a, Result<Vec<IpAddr>, String>> {
        self.resolved.lock().unwrap().push(host.to_string());
        let out = (self.answers)(host);
        Box::pin(async move { out })
    }
}

/// Connects every vetted address to the local upstream instead.
pub(crate) struct ToUpstream {
    pub(crate) port: u16,
    pub(crate) connected: Arc<Mutex<Vec<(IpAddr, u16)>>>,
}

impl Connector for ToUpstream {
    fn connect(
        &self,
        ip: IpAddr,
        port: u16,
        _timeout: Duration,
    ) -> Pending<'_, io::Result<TcpStream>> {
        self.connected.lock().unwrap().push((ip, port));
        let up = self.port;
        Box::pin(async move { TcpStream::connect(("127.0.0.1", up)).await })
    }
}
