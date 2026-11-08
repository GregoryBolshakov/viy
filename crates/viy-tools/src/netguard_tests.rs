//! rpctl's AddressTests (test_netaccess.py) and netguard's host rules
//! fuzzed against rpctl's over every special-purpose range of Python
//! 3.10.12's tables (kb d018).

use std::net::IpAddr;

use pretty_assertions::assert_eq;

use super::*;

#[test]
fn non_public_addresses() {
    for ip in [
        "127.0.0.1",
        "127.255.0.9",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "::",
        "fe80::1",
        "fc00::1",
        "fd12::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "2002:7f00:1::",
        "64:ff9b::7f00:1",
        "64:ff9b::a00:1",
        "2001:0:4136:e378:8000:63bf:80ff:fffe",
    ] {
        assert!(address_problem(ip).is_some(), "{ip}");
    }
}

#[test]
fn public_addresses() {
    for ip in [
        "93.184.216.34",
        "8.8.8.8",
        "2606:4700::6810:84e5",
        "::ffff:8.8.8.8",
    ] {
        assert_eq!(address_problem(ip), None, "{ip}");
    }
}

struct Fixed(Vec<IpAddr>);

impl Resolver for Fixed {
    fn resolve<'a>(&'a self, _: &'a str, _: u16) -> Pending<'a, Result<Vec<IpAddr>, String>> {
        let out = self.0.clone();
        Box::pin(async move { Ok(out) })
    }
}

#[tokio::test]
async fn a_mixed_answer_is_refused() {
    let ip = |s: &str| s.parse::<IpAddr>().unwrap();
    let mixed = Fixed(vec![ip("93.184.216.34"), ip("127.0.0.1")]);
    assert_eq!(
        vet("rebind.test", 80, &mixed).await,
        Err(VetError::Refused(
            "rebind.test resolves to 127.0.0.1 is this machine (loopback)".into()
        ))
    );
    let public = Fixed(vec![ip("93.184.216.34")]);
    assert_eq!(
        vet("rebind.test", 80, &public).await,
        Ok(vec![ip("93.184.216.34")])
    );
}

#[test]
fn messages_name_the_address_as_rpctl_prints_it() {
    assert_eq!(
        address_problem("::ffff:127.0.0.1").unwrap(),
        "::ffff:7f00:1 wraps 127.0.0.1 is this machine (loopback)"
    );
    assert_eq!(
        address_problem("fe80::1%eth0").unwrap(),
        "fe80::1 is link-local (cloud metadata lives there)"
    );
    assert_eq!(address_problem("x").unwrap(), "x is not an IP address");
}

#[test]
fn a_scoped_literal_is_a_name_and_refused() {
    // rpctl reads `2606:4700::1%x` as a public address; viy refuses it.
    assert!(check_host("2606:4700::1%x").is_err());
    assert_eq!(check_host("2606:4700::ABCD").unwrap(), "2606:4700::abcd");
}

// ------------------------------------------------------------------- oracle

/// Addresses at the edges and inside every range of Python 3.10.12's
/// tables, as IPv4 and in each IPv6 wrapping rp unwraps.
#[cfg(feature = "oracle")]
fn edge_addresses(g: &mut viy_testkit::fuzz::Gen) -> Vec<String> {
    let mut v4: Vec<u32> = vec![0, 1, u32::MAX, 0x6440_0000, 0x6480_0000, 0x643f_ffff];
    let ranges4 = PRIVATE4.iter().chain(&PRIVATE4_EXCEPTIONS).copied().chain([
        (0x7f00_0000, 8),
        (0xa9fe_0000, 16),
        (0xe000_0000, 4),
        (0xf000_0000, 4),
        (0x6440_0000, 10),
    ]);
    for (net, len) in ranges4 {
        let size = 1u64 << (32 - u32::from(len));
        let end = (u64::from(net) + size - 1) as u32;
        v4.extend([
            net,
            end,
            net.wrapping_sub(1),
            end.wrapping_add(1),
            net + (g.next_u64() % size) as u32,
        ]);
    }
    let mut out = Vec::new();
    for &a in &v4 {
        let quad = Ipv4Addr::from(a);
        out.push(quad.to_string());
        out.push(format!("::ffff:{quad}"));
        out.push(Ipv6Addr::from((0x2002u128 << 112) | (u128::from(a) << 80)).to_string());
        out.push(Ipv6Addr::from((0x2001_0000u128 << 96) | u128::from(!a)).to_string());
        out.push(Ipv6Addr::from(NAT64 | u128::from(a)).to_string());
        out.push(Ipv6Addr::from(NAT64_LOCAL | u128::from(a)).to_string());
    }
    let ranges6 = PRIVATE6
        .iter()
        .chain(&PRIVATE6_EXCEPTIONS)
        .chain(&RESERVED6)
        .copied()
        .chain([
            (0xfe80 << 112, 10),
            (0xfec0 << 112, 10),
            (0xff00 << 112, 8),
            (0x2606_4700 << 96, 32),
        ]);
    for (net, len) in ranges6 {
        let host_bits = 128 - u32::from(len);
        let mask = u128::MAX.checked_shr(u32::from(len)).unwrap_or(0);
        let end = net | mask;
        let inside = net | (u128::from(g.next_u64()) << 64 | u128::from(g.next_u64())) & mask;
        let _ = host_bits;
        for a in [net, end, net.wrapping_sub(1), end.wrapping_add(1), inside] {
            out.push(Ipv6Addr::from(a).to_string());
        }
    }
    out
}

/// `address_problem` and `check_host` against rpctl's, over the edges of
/// every range and over generated spellings (ASCII: non-ASCII names are
/// UTS 46 here, flags F53).
#[cfg(feature = "oracle")]
#[test]
fn host_rules_match_rpctl() {
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    const PIECES: &[&str] = &[
        "0",
        "1",
        "9",
        "10",
        "127",
        "255",
        "256",
        "00",
        "01",
        "0x",
        "f",
        "ff",
        "ffff",
        "fe80",
        "2002",
        "2001",
        "64",
        "ff9b",
        "db8",
        ".",
        ".",
        ":",
        ":",
        "::",
        "%",
        "eth0",
        "a",
        "z",
        "-",
        "x",
        "local",
        "localhost",
        "LOCALHOST",
        "lan",
        "corp",
        "xn--",
        "_",
        " ",
        "1.2.3.4",
    ];
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let mut inputs = edge_addresses(&mut g);
    for _ in 0..cases(20000) {
        inputs.push(g.string(PIECES, 8));
    }
    for (case, text) in inputs.iter().enumerate() {
        let at = format!("case {case}, seed {:#x}: {text:?}", seed());
        assert_eq!(
            oracle
                .call("netguard.address_problem", json!([text]), json!({}))
                .unwrap()
                .outcome,
            Outcome::Returned(json!(address_problem(text))),
            "address_problem {at}"
        );
        let mine = match check_host(text) {
            Ok(h) => Outcome::Returned(json!(h)),
            Err(Refused(m)) => Outcome::Raised {
                kind: "Refused".into(),
                message: m,
            },
        };
        let theirs = oracle
            .call("netguard.check_host", json!([text]), json!({}))
            .unwrap()
            .outcome;
        // A scope id: rpctl reads an address, viy a name it refuses.
        if text.contains('%') && parse_ip(text.split('%').next().unwrap_or_default()).is_some() {
            assert!(
                matches!(mine, Outcome::Raised { .. }),
                "check_host {at}: {mine:?}"
            );
            continue;
        }
        assert_eq!(theirs, mine, "check_host {at}");
    }
}
