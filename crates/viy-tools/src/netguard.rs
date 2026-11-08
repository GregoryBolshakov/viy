//! The host rules every connection to the internet goes through (rpctl
//! netguard.py): which names and addresses are refused, and the one lookup
//! whose every address must be public.
//!
//! So far the part the egress proxy shares (`check_host`,
//! `address_problem`, `resolve`, `vet`); `fetch_url`'s URL rules and
//! reader come with phase 5 (WP 5.1).
//!
//! - A name is IDNA-encoded and lower-cased; local names (`localhost`,
//!   `*.local`, `*.internal`, ...) are refused before any lookup.
//!   An ASCII name is taken as rpctl's codec takes it (every label 1-63
//!   characters, nothing else checked); a non-ASCII one is encoded by UTS
//!   46, where rpctl's Python uses IDNA 2003 (flags F53).
//! - An address is refused unless globally routable: loopback, private,
//!   link-local (cloud metadata), CGNAT, multicast, reserved, and IPv4
//!   hidden in IPv6 (mapped, 6to4, Teredo, NAT64) unwrapped and checked.
//!   The classes are Python 3.10.12's `ipaddress` tables, the interpreter
//!   rpctl runs on (kb d018), not Rust's.
//! - A lookup answers within 5 s, and one address that is not public
//!   refuses the name: a mixed answer is how rebinding attacks start.

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::time::Duration;

use crate::approve::Pending;

/// Names that only ever mean this machine or the local network.
const LOCAL_SUFFIXES: [&str; 8] = [
    ".localhost",
    ".local",
    ".internal",
    ".lan",
    ".home.arpa",
    ".localdomain",
    ".intranet",
    ".corp",
];

/// How long a lookup may take (rp's `resolve` timeout).
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// The request breaks a rule; the message says which, and the model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why [`vet`] gave no addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VetError {
    /// An address the name resolves to is not public (rp's `Refused`).
    Refused(String),
    /// The lookup failed (rp's `FetchError`).
    Failed(String),
}

/// An IP literal as Python's `ipaddress.ip_address` reads one, without a
/// scope id (`fe80::1%eth0`), which no public address carries: viy refuses
/// such a literal as a name, where rpctl reads it as an address.
pub fn parse_ip(text: &str) -> Option<IpAddr> {
    text.parse().ok()
}

/// `host` (no brackets, no trailing dot) IDNA-encoded and lower-case, or
/// why it is refused: a local name or a non-public IP literal.
pub fn check_host(host: &str) -> Result<String, Refused> {
    if parse_ip(host).is_some() {
        let lower = host.to_ascii_lowercase();
        return match address_problem(&lower) {
            Some(why) => Err(Refused(why)),
            None => Ok(lower),
        };
    }
    let Some(encoded) = idna_encode(host) else {
        return Err(Refused("the host name is not a valid domain name".into()));
    };
    // IDNA maps a trailing 。 ． ｡ to "." after the caller stripped the
    // ASCII ones: strip again, or `localhost。` passes as `localhost.`
    // (flags F54, fixed in rpctl 728277d).
    let ascii = encoded.to_ascii_lowercase();
    let ascii = ascii.trim_end_matches('.');
    if ascii.is_empty()
        || !ascii
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return Err(Refused(
            "the host name contains characters a domain cannot have".into(),
        ));
    }
    if ascii == "localhost" || LOCAL_SUFFIXES.iter().any(|s| ascii.ends_with(s)) {
        return Err(Refused(format!(
            "{ascii} is a local name, not an internet host"
        )));
    }
    Ok(ascii.to_string())
}

/// A name as an ASCII domain, or `None`. ASCII is kept as Python's `idna`
/// codec keeps it: every label but the last 1-63 characters, the last
/// under 64. Anything else goes through UTS 46 (flags F53), and then the
/// same label lengths, a trailing empty label allowed, as Python's.
fn idna_encode(host: &str) -> Option<String> {
    let out = if host.is_ascii() {
        host.to_string()
    } else {
        idna::domain_to_ascii(host).ok()?
    };
    let labels: Vec<&str> = out.split('.').collect();
    let (last, rest) = labels.split_last()?;
    let ok = rest.iter().all(|l| (1..64).contains(&l.len())) && last.len() < 64;
    ok.then_some(out)
}

/// Why connecting to `ip` is refused, or `None` for a public address. A
/// scope id (`%eth0`) is ignored, as rp's.
pub fn address_problem(ip: &str) -> Option<String> {
    let bare = ip.split('%').next().unwrap_or_default();
    match parse_ip(bare) {
        Some(a) => ip_problem(a),
        None => Some(format!("{ip} is not an IP address")),
    }
}

/// [`address_problem`] of a parsed address.
pub fn ip_problem(a: IpAddr) -> Option<String> {
    let shown = show(a);
    if let IpAddr::V6(v6) = a {
        let n = u128::from(v6);
        let mapped = n >> 32 == 0xffff;
        let embedded = if mapped {
            Some(n as u32)
        } else if n >> 112 == 0x2002 {
            Some((n >> 80) as u32) // 6to4
        } else if n >> 96 == 0x2001_0000 {
            Some(!(n as u32)) // Teredo's client
        } else if n >> 32 == NAT64 >> 32 || n >> 80 == NAT64_LOCAL >> 80 {
            Some(n as u32)
        } else {
            None
        };
        if let Some(v4) = embedded {
            if let Some(why) = ip_problem(IpAddr::V4(Ipv4Addr::from(v4))) {
                return Some(format!("{shown} wraps {why}"));
            }
            if mapped {
                return None;
            }
        }
    }
    let c = Class::of(a);
    if c.loopback {
        Some(format!("{shown} is this machine (loopback)"))
    } else if c.link_local {
        Some(format!(
            "{shown} is link-local (cloud metadata lives there)"
        ))
    } else if c.private || c.site_local {
        Some(format!("{shown} is a private network address"))
    } else if c.multicast || c.reserved || c.unspecified {
        Some(format!("{shown} is not a unicast internet address"))
    } else if !c.global {
        Some(format!("{shown} is not a public internet address"))
    } else {
        None
    }
}

/// An address as Python 3.10 prints it: RFC 5952, except that an
/// IPv4-mapped address stays in hextets (`::ffff:7f00:1`), where Rust
/// prints the dotted quad.
pub fn show(a: IpAddr) -> String {
    match a {
        IpAddr::V6(v6) if u128::from(v6) >> 32 == 0xffff => {
            let s = v6.segments();
            format!("::ffff:{:x}:{:x}", s[6], s[7])
        }
        a => a.to_string(),
    }
}

const NAT64: u128 = 0x0064_ff9b_0000_0000_0000_0000_0000_0000; // 64:ff9b::/96
const NAT64_LOCAL: u128 = 0x0064_ff9b_0001_0000_0000_0000_0000_0000; // 64:ff9b:1::/48

/// Python 3.10.12's `_IPv4Constants._private_networks`.
const PRIVATE4: [(u32, u8); 14] = [
    (0x0000_0000, 8),
    (0x0a00_0000, 8),
    (0x7f00_0000, 8),
    (0xa9fe_0000, 16),
    (0xac10_0000, 12),
    (0xc000_0000, 24),
    (0xc000_00aa, 31),
    (0xc000_0200, 24),
    (0xc0a8_0000, 16),
    (0xc612_0000, 15),
    (0xc633_6400, 24),
    (0xcb00_7100, 24),
    (0xf000_0000, 4),
    (0xffff_ffff, 32),
];
/// `_private_networks_exceptions`: 192.0.0.9 and 192.0.0.10.
const PRIVATE4_EXCEPTIONS: [(u32, u8); 2] = [(0xc000_0009, 32), (0xc000_000a, 32)];

/// Python 3.10.12's `_IPv6Constants._private_networks`.
const PRIVATE6: [(u128, u8); 10] = [
    (1, 128),
    (0, 128),
    (0xffff_0000_0000, 96),
    (NAT64_LOCAL, 48),
    (0x0100_0000_0000_0000_0000_0000_0000_0000, 64),
    (0x2001_0000_0000_0000_0000_0000_0000_0000, 23),
    (0x2001_0db8_0000_0000_0000_0000_0000_0000, 32),
    (0x2002_0000_0000_0000_0000_0000_0000_0000, 16),
    (0xfc00_0000_0000_0000_0000_0000_0000_0000, 7),
    (0xfe80_0000_0000_0000_0000_0000_0000_0000, 10),
];
const PRIVATE6_EXCEPTIONS: [(u128, u8); 6] = [
    (0x2001_0001_0000_0000_0000_0000_0000_0001, 128),
    (0x2001_0001_0000_0000_0000_0000_0000_0002, 128),
    (0x2001_0003_0000_0000_0000_0000_0000_0000, 32),
    (0x2001_0004_0112_0000_0000_0000_0000_0000, 48),
    (0x2001_0020_0000_0000_0000_0000_0000_0000, 28),
    (0x2001_0030_0000_0000_0000_0000_0000_0000, 28),
];
/// `_IPv6Constants._reserved_networks`.
const RESERVED6: [(u128, u8); 15] = [
    (0x0000 << 112, 8),
    (0x0100 << 112, 8),
    (0x0200 << 112, 7),
    (0x0400 << 112, 6),
    (0x0800 << 112, 5),
    (0x1000 << 112, 4),
    (0x4000 << 112, 3),
    (0x6000 << 112, 3),
    (0x8000 << 112, 3),
    (0xa000 << 112, 3),
    (0xc000 << 112, 3),
    (0xe000 << 112, 4),
    (0xf000 << 112, 5),
    (0xf800 << 112, 6),
    (0xfe00 << 112, 9),
];

fn in4(a: u32, (net, len): (u32, u8)) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
    a & mask == net & mask
}

fn in6(a: u128, (net, len): (u128, u8)) -> bool {
    let mask = u128::MAX.checked_shl(128 - u32::from(len)).unwrap_or(0);
    a & mask == net & mask
}

/// Python 3.10.12's `ipaddress` properties of one address.
struct Class {
    loopback: bool,
    link_local: bool,
    private: bool,
    site_local: bool,
    multicast: bool,
    reserved: bool,
    unspecified: bool,
    global: bool,
}

impl Class {
    fn of(a: IpAddr) -> Self {
        match a {
            IpAddr::V4(v4) => Self::v4(v4),
            IpAddr::V6(v6) => Self::v6(v6),
        }
    }

    fn v4_private(n: u32) -> bool {
        PRIVATE4.iter().any(|&r| in4(n, r)) && !PRIVATE4_EXCEPTIONS.iter().any(|&r| in4(n, r))
    }

    fn v4(a: Ipv4Addr) -> Self {
        let n = u32::from(a);
        let private = Self::v4_private(n);
        Self {
            loopback: in4(n, (0x7f00_0000, 8)),
            link_local: in4(n, (0xa9fe_0000, 16)),
            private,
            site_local: false,
            multicast: in4(n, (0xe000_0000, 4)),
            reserved: in4(n, (0xf000_0000, 4)),
            unspecified: n == 0,
            global: !in4(n, (0x6440_0000, 10)) && !private,
        }
    }

    fn v6(a: Ipv6Addr) -> Self {
        let n = u128::from(a);
        let private = if n >> 32 == 0xffff {
            Self::v4_private(n as u32)
        } else {
            PRIVATE6.iter().any(|&r| in6(n, r)) && !PRIVATE6_EXCEPTIONS.iter().any(|&r| in6(n, r))
        };
        Self {
            loopback: n == 1,
            link_local: in6(n, (0xfe80 << 112, 10)),
            private,
            site_local: in6(n, (0xfec0 << 112, 10)),
            multicast: in6(n, (0xff00 << 112, 8)),
            reserved: RESERVED6.iter().any(|&r| in6(n, r)),
            unspecified: n == 0,
            global: !private,
        }
    }
}

/// Looks a name up: every address it has, in the resolver's order, or why
/// not, in rp's words. A trait so tests fake the internet.
pub trait Resolver: Send + Sync {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> Pending<'a, Result<Vec<IpAddr>, String>>;
}

/// The system's resolver (`getaddrinfo`), within [`RESOLVE_TIMEOUT`].
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> Pending<'a, Result<Vec<IpAddr>, String>> {
        Box::pin(async move {
            let found = tokio::time::timeout(
                RESOLVE_TIMEOUT,
                tokio::net::lookup_host((host.to_string(), port)),
            )
            .await;
            let addrs = match found {
                Err(_) => return Err(format!("looking up {host} timed out")),
                Ok(Err(e)) => return Err(format!("could not resolve {host} ({})", kind(&e))),
                Ok(Ok(addrs)) => addrs,
            };
            let mut out: Vec<IpAddr> = Vec::new();
            for a in addrs {
                if !out.contains(&a.ip()) {
                    out.push(a.ip());
                }
            }
            if out.is_empty() {
                return Err(format!("{host} has no addresses"));
            }
            Ok(out)
        })
    }
}

/// An I/O error's kind as a word, where rpctl gives Python's exception
/// class (kb d092).
pub(crate) fn kind(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "TimedOut".into(),
        k => format!("{k:?}"),
    }
}

/// The addresses to connect to for `host`, every one public.
pub async fn vet(host: &str, port: u16, resolver: &dyn Resolver) -> Result<Vec<IpAddr>, VetError> {
    let addrs = resolver
        .resolve(host, port)
        .await
        .map_err(VetError::Failed)?;
    for &ip in &addrs {
        if let Some(why) = ip_problem(ip) {
            return Err(VetError::Refused(format!("{host} resolves to {why}")));
        }
    }
    Ok(addrs)
}

#[cfg(test)]
#[path = "netguard_tests.rs"]
mod tests;
