//! rpctl's ParseTests, AddressTests and OutboundSecretTests
//! (test_netaccess.py), netguard's host rules fuzzed against rpctl's over
//! every special-purpose range of Python 3.10.12's tables (kb d018), and
//! its URL rules, redirect joins and secret check against rpctl's.

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

#[test]
fn shapes_that_are_refused() {
    let long = format!("http://{}.com/", "a".repeat(3000));
    for url in [
        "file:///etc/passwd",
        "gopher://x/",
        "ftp://x/",
        "javascript:alert(1)",
        "http://user:pw@example.com/",
        "http://@example.com/",
        "http://example.com:22/",
        "http://example.com:6379/",
        "http://exa mple.com/",
        "http://example.com/\r\nX: y",
        "http://example.com\\@evil.com/",
        &long,
        "",
        "/relative",
        "http:///nohost",
    ] {
        assert!(parse(url).is_err(), "{url}");
    }
}

#[test]
fn normalises() {
    let t = parse("HTTPS://Docs.Python.org/3/library/ssl.html?x=1#frag").unwrap();
    assert_eq!(
        (t.scheme.as_str(), t.host.as_str(), t.port),
        ("https", "docs.python.org", 443)
    );
    assert_eq!(t.url, "https://docs.python.org/3/library/ssl.html?x=1");
    assert_eq!(t.path, "/3/library/ssl.html?x=1");
    assert_eq!(parse("http://bücher.de/").unwrap().host, "xn--bcher-kva.de");
    assert_eq!(
        parse("http://example.com:8080").unwrap().url,
        "http://example.com:8080/"
    );
    assert_eq!(
        parse("http://[2606:4700::1]:8443/x").unwrap().url,
        "http://[2606:4700::1]:8443/x"
    );
    assert_eq!(parse("http://example.com:0/").unwrap().port, 80);
}

// rpctl's test_a_trailing_unicode_full_stop_is_refused_before_any_lookup
// (flags F54, fixed in rpctl 728277d).
#[test]
fn a_trailing_unicode_full_stop_is_refused_before_any_lookup() {
    for url in [
        "http://localhost\u{3002}/",
        "http://db.internal\u{ff0e}/",
        "http://nas.local\u{ff61}:8080/",
    ] {
        assert!(parse(url).is_err(), "{url}");
    }
    assert_eq!(
        parse("http://example.com\u{3002}/").unwrap().host,
        "example.com"
    );
}

#[test]
fn messages_are_rpctls() {
    let refused = |url: &str| parse(url).unwrap_err().0;
    assert_eq!(refused("  "), "no URL given");
    assert_eq!(
        refused("ftp://x/"),
        "only http and https URLs can be fetched, not ftp"
    );
    assert_eq!(
        refused("example.com/x"),
        "only http and https URLs can be fetched, not a bare path"
    );
    assert_eq!(
        refused("http://example.com:22/"),
        "port 22 is not allowed (only [80, 443, 8080, 8443])"
    );
    assert_eq!(
        refused("http://example.com:x/"),
        "the URL does not parse (Port could not be cast to integer value as 'x')"
    );
    assert_eq!(
        refused("http://[::1/"),
        "the URL does not parse (Invalid IPv6 URL)"
    );
    assert_eq!(refused("http:///x"), "the URL has no host");
}

#[test]
fn literal_and_shaped_secrets() {
    let lit = vec!["s3cret-value-123456".to_string()];
    assert!(outbound_secret("https://x.test/?k=s3cret-value-123456", &lit).is_some());
    assert!(outbound_secret("https://x.test/?k=s3cret%2Dvalue%2D123456", &lit).is_some());
    assert!(outbound_secret(&format!("q hf_{}", "a".repeat(34)), &[]).is_some());
    assert!(outbound_secret(&format!("https://x.test/tvly-{}", "A1b2".repeat(6)), &[]).is_some());
    assert_eq!(outbound_secret("https://docs.python.org/3/", &lit), None);
}

#[test]
fn redirects_join_as_rpctl() {
    let base = "http://pub.test/a/b/c?q=1";
    for (loc, want) in [
        ("/loop", "http://pub.test/loop"),
        ("d", "http://pub.test/a/b/d"),
        ("../d", "http://pub.test/a/d"),
        ("../../../../d", "http://pub.test/d"),
        ("?x", "http://pub.test/a/b/c?x"),
        ("", "http://pub.test/a/b/c?q=1"),
        ("//other.test/p", "http://other.test/p"),
        ("https://x.test", "https://x.test"),
        ("file:///etc/passwd", "file:///etc/passwd"),
        (".", "http://pub.test/a/b/"),
        ("#f", "http://pub.test/a/b/c?q=1#f"),
    ] {
        assert_eq!(url::join(base, loc).unwrap(), want, "{loc}");
    }
}

/// ASCII names with full-width and ideographic full stops, inside or at
/// the end, against rpctl's `check_host` and the proxy's `canonical_host`:
/// UTS 46 and rpctl's IDNA 2003 map these stops alike, so the answers are
/// equal (flags F54).
#[cfg(feature = "oracle")]
#[test]
fn full_stops_match_rpctl() {
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    const PIECES: &[&str] = &[
        "a",
        "example",
        "com",
        "localhost",
        "local",
        "internal",
        "lan",
        "corp",
        "127",
        "0",
        "1",
        ".",
        "\u{3002}",
        "\u{ff0e}",
        "\u{ff61}",
        "-",
        "x",
    ];
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    for case in 0..cases(5000) {
        let host = g.string(PIECES, 6);
        let at = format!("case {case}, seed {:#x}: {host:?}", seed());
        let mine = match check_host(&host) {
            Ok(h) => Outcome::Returned(json!(h)),
            Err(Refused(m)) => Outcome::Raised {
                kind: "Refused".into(),
                message: m,
            },
        };
        let theirs = oracle
            .call("netguard.check_host", json!([host]), json!({}))
            .unwrap()
            .outcome;
        assert_eq!(theirs, mine, "check_host {at}");
        let mine = match crate::egress::canonical_host(&host) {
            Ok(h) => Outcome::Returned(json!(h)),
            Err(e) => Outcome::Raised {
                kind: "Blocked".into(),
                message: e.0,
            },
        };
        let theirs = oracle
            .call("egress.canonical_host", json!([host]), json!({}))
            .unwrap()
            .outcome;
        assert_eq!(theirs, mine, "canonical_host {at}");
    }
}

/// rpctl's answer, or the refusal it raised.
#[cfg(feature = "oracle")]
fn as_outcome(
    r: Result<impl Into<serde_json::Value>, String>,
    kind: &str,
) -> viy_testkit::oracle::Outcome {
    use viy_testkit::oracle::Outcome;
    match r {
        Ok(v) => Outcome::Returned(v.into()),
        Err(message) => Outcome::Raised {
            kind: kind.into(),
            message,
        },
    }
}

/// URL pieces for the parse and redirect fuzz: schemes, hosts of every
/// kind, ports in and out of range, userinfo, brackets, paths, the
/// characters rpctl refuses, and NFKC and full-stop spellings. Host
/// names are ASCII, as UTS 46 and rpctl's IDNA 2003 differ on others
/// (flags F53).
#[cfg(feature = "oracle")]
const URL_PIECES: &[&str] = &[
    "http",
    "https",
    "HTTP",
    "ftp",
    "file",
    "javascript",
    "h1+x",
    "://",
    "://",
    "//",
    ":",
    ":",
    "/",
    "/",
    "?",
    "#",
    "@",
    "user",
    "pw",
    "example.com",
    "Example.COM",
    "a.b",
    "localhost",
    "x.local",
    "x.corp",
    "127.0.0.1",
    "8.8.8.8",
    "0x7f.1",
    "2130706433",
    "[::1]",
    "[2606:4700::1]",
    "[::ffff:8.8.8.8]",
    "[1.2.3.4]",
    "[v1.x]",
    "[",
    "]",
    "80",
    "443",
    "8080",
    "8443",
    "22",
    "0",
    "00080",
    "65536",
    "99999999",
    "x",
    "-1",
    "%",
    "%2e",
    "%41",
    ".",
    "..",
    ";p",
    "a",
    "q=1",
    " ",
    "\t",
    "\\",
    "\x7f",
    "\u{3002}",
    "\u{2100}",
    "\u{ff0e}",
    "é",
];

/// `parse` against rpctl's over generated URLs.
#[cfg(feature = "oracle")]
#[test]
fn parse_matches_rpctl() {
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::tuple;

    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    const SCHEMES: &[&str] = &["http", "https", "HTTPS", "ftp", "", " http", "h1+x"];
    const SEPS: &[&str] = &["://", "://", "://", ":/", ":", "//", ":\\\\"];
    const HOSTS: &[&str] = &[
        "example.com",
        "Example.COM",
        "a.b",
        "pub.test",
        "x.local",
        "localhost",
        "127.0.0.1",
        "8.8.8.8",
        "0x7f.1",
        "[::1]",
        "[2606:4700::1]",
        "[1.2.3.4]",
        "[v1.x]",
        "user@",
        "user:pw@",
        "@",
        "[",
        "]",
        "",
        "é",
        "\u{3002}",
        "\u{2100}",
        "%41",
        ".",
        "-",
    ];
    const PORTS: &[&str] = &[
        "", "", ":80", ":443", ":8080", ":8443", ":22", ":0", ":00080", ":65536", ":x", ":", ":-1",
        ":\u{663}",
    ];
    const PATHS: &[&str] = &[
        "", "/", "/a", "/a/../b", "?q=1", "#f", ";p", "%2e", " ", "\t", "/\x7f", "\\", "/é",
    ];
    let mut ok = 0;
    for case in 0..cases(20000) {
        let url = if case % 4 == 3 {
            g.string(URL_PIECES, 10)
        } else if case % 4 == 0 {
            // Mostly accepted: the rules' edges inside a well-formed URL.
            format!(
                "{}://{}{}{}",
                g.pick(&["http", "https", "HTTP"]),
                g.pick(&[
                    "example.com",
                    "Example.COM",
                    "a.b",
                    "pub.test",
                    "8.8.8.8",
                    "[2606:4700::1]",
                    "[::ffff:8.8.8.8]",
                    "x.local",
                    "a.b.\u{3002}",
                    "a.b.",
                    "é.com"
                ]),
                g.pick(&["", ":80", ":443", ":8080", ":8443", ":0", ":00443", ":22"]),
                g.string(
                    &[
                        "/", "a", "..", ".", "?", "q=1", "#", "f", ";p", "%2e", "&", "=", "é"
                    ],
                    5
                )
            )
        } else {
            format!(
                "{}{}{}{}{}",
                g.pick(SCHEMES),
                g.pick(SEPS),
                if g.below(3) == 0 {
                    g.string(HOSTS, 3)
                } else {
                    g.pick(HOSTS).to_string()
                },
                g.pick(PORTS),
                g.string(PATHS, 3)
            )
        };
        let at = format!("case {case}, seed {:#x}: {url:?}", seed());
        let mine = parse(&url).map_err(|r| r.0).map(|t| {
            tuple(vec![
                json!(t.scheme),
                json!(t.host),
                json!(t.port),
                json!(t.path),
                json!(t.url),
            ])
        });
        let mine = as_outcome(mine, "Refused");
        let theirs = oracle
            .call("netguard.parse", json!([url]), json!({}))
            .unwrap()
            .outcome;
        // A scope id: rpctl reads an address, viy a name it refuses.
        if url.contains('%') && url.contains('[') && theirs != mine {
            assert!(
                matches!(mine, viy_testkit::oracle::Outcome::Raised { .. }),
                "parse {at}: {mine:?}"
            );
            continue;
        }
        assert_eq!(theirs, mine, "parse {at}");
        if matches!(mine, viy_testkit::oracle::Outcome::Returned(_)) {
            ok += 1;
        }
    }
    eprintln!("parse_matches_rpctl: {ok} accepted");
    assert!(ok * 20 > cases(20000), "too few URLs accepted: {ok}");
}

/// The next URL of a redirect against rpctl's, from parsed URLs and
/// Location values.
#[cfg(feature = "oracle")]
#[test]
fn redirects_match_rpctl() {
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;

    const BASES: &[&str] = &[
        "http://pub.test/",
        "http://pub.test/a/b/c?q=1",
        "https://pub.test:8443/a/;p/b;x?y",
        "http://[2606:4700::1]/a/",
        "http://pub.test/a/b/",
    ];
    const LOCATION: &[&str] = &[
        "/",
        "/",
        "a",
        "b",
        "..",
        ".",
        "//",
        "?",
        "#",
        ";",
        "x=1",
        "http:",
        "https:",
        "ftp:",
        "//other.test",
        "pub.test",
        ":8080",
        " ",
        "\t",
        "[",
        "]",
        "::1",
        "%2f",
        "\\",
        "@",
    ];
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    for case in 0..cases(20000) {
        let base = *g.pick(BASES);
        let loc = g.string(LOCATION, 7);
        let at = format!("case {case}, seed {:#x}: {base:?} + {loc:?}", seed());
        let stripped = loc.trim_matches(crate::approve::chars::is_py_space);
        let mine = as_outcome(url::join(base, stripped), "ValueError");
        let theirs = oracle
            .call("netguard.redirect", json!([base, loc]), json!({}))
            .unwrap()
            .outcome;
        assert_eq!(theirs, mine, "redirect {at}");
    }
}

/// `outbound_secret` against rpctl's: the same answer, or viy refusing
/// where an ASCII word boundary meets a non-ASCII character (kb d042).
#[cfg(feature = "oracle")]
#[test]
fn outbound_secret_matches_rpctl() {
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    const PIECES: &[&str] = &[
        "https://x.test/?k=",
        "q ",
        " ",
        "/",
        "=",
        "%",
        "%2D",
        "%252D",
        "%25",
        "%2",
        "%zz",
        "%C3",
        "%A9",
        "é",
        "hf_",
        "sk-",
        "tvly-",
        "ghp_",
        "AKIA",
        "aaaaaaaaaa",
        "A1b2A1b2A1",
        "0123456789",
        "-",
        "_",
        "s3cret",
        "value",
        "123456",
        "s3cret-value-123456",
    ];
    let literals = vec!["s3cret-value-123456".to_string()];
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    for case in 0..cases(20000) {
        let text = g.string(PIECES, 9);
        let at = format!("case {case}, seed {:#x}: {text:?}", seed());
        let mine = outbound_secret(&text, &literals).map(|m| m.replace("viy's", "rp's"));
        let theirs = oracle
            .call(
                "netguard.outbound_secret",
                json!([text, literals]),
                json!({}),
            )
            .unwrap()
            .outcome;
        if theirs == Outcome::Returned(json!(null)) && mine.is_some() && !text.is_ascii() {
            continue;
        }
        assert_eq!(
            theirs,
            Outcome::Returned(json!(mine)),
            "outbound_secret {at}"
        );
    }
}
