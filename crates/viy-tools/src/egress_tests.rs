//! rpctl's CommandHostTests and ConnectTests' IDNA test (test_egress.py),
//! and the policy fuzzed against rpctl's.

use pretty_assertions::assert_eq;

use super::*;

fn hosts(cmd: &str) -> Vec<(String, Vec<u16>)> {
    hosts_in_command(cmd)
        .into_iter()
        .map(|(h, p)| (h, p.into_iter().collect()))
        .collect()
}

fn want(rows: &[(&str, &[u16])]) -> Vec<(String, Vec<u16>)> {
    let mut out: Vec<(String, Vec<u16>)> = rows
        .iter()
        .map(|(h, p)| ((*h).to_string(), p.to_vec()))
        .collect();
    out.sort();
    out
}

#[test]
fn urls_anywhere_in_the_text() {
    let h = hosts(
        "pip install git+https://user:tok@github.com/a/b.git && \
         python3 -c 'urlopen(\"http://Example.COM:8080/x\")'; \
         curl https://bücher.de/ https://[2606:4700::1]:9443/",
    );
    assert_eq!(
        h,
        want(&[
            ("github.com", &[]),
            ("example.com", &[8080]),
            ("xn--bcher-kva.de", &[]),
            ("2606:4700::1", &[9443]),
        ])
    );
}

#[test]
fn bare_hosts_of_curl_wget_and_git_clone() {
    let h = hosts(
        "git -C src clone --depth 1 gitlab.example.org/a/b && \
         wget -O out.html dl.example.net/f.tar.gz | curl -sL api.example.io:8443/v1",
    );
    assert_eq!(
        h,
        want(&[
            ("gitlab.example.org", &[]),
            ("dl.example.net", &[]),
            ("api.example.io", &[8443]),
        ])
    );
}

#[test]
fn what_does_not_count() {
    let h = hosts(
        "curl -o notes.example.com -H 'Host: evil.test' https://ok.test/ ; \
         git clone git@github.com:a/b.git; echo collect.evil.test; \
         git log example.org; curl http://localhost:8000/ http://10.0.0.1/",
    );
    assert_eq!(h, want(&[("ok.test", &[])]));
}

#[test]
fn unparseable_quotes_still_scan_urls() {
    assert_eq!(hosts("curl 'https://a.test/ \"x"), want(&[("a.test", &[])]));
}

#[test]
fn describe() {
    let p = Policy::new(
        "curl https://example.com/ https://pypi.org/x",
        ["docs.test"],
        "*.corp.test",
    );
    assert_eq!(
        p.describe(),
        "registries + example.com (from the command) + docs.test (you allowed) + \
         *.corp.test (SANDBOX_NET_ALLOW)"
    );
    assert_eq!(
        Policy::new("pip install x", [], "").describe(),
        "registries"
    );
}

#[test]
fn parse_allow_keeps_hosts_and_subdomain_wildcards() {
    let (exact, suffixes) = parse_allow(" a.test,*.b.test  localhost, *.local,,");
    assert_eq!(exact.into_iter().collect::<Vec<_>>(), ["a.test"]);
    assert_eq!(suffixes.into_iter().collect::<Vec<_>>(), [".b.test"]);
}

/// rpctl's test_unicode_spellings_are_checked_after_idna, and where UTS 46
/// differs from rpctl's IDNA 2003 (flags F53).
#[test]
fn unicode_spellings_are_checked_after_idna() {
    for raw in ["ⓛocalhost", "１２７.０.０.１", "nas.ⓛocal"] {
        assert!(canonical_host(raw).is_err(), "{raw}");
    }
    assert_eq!(canonical_host("Bücher.DE.").unwrap(), "xn--bcher-kva.de");
    // UTS 46 where rpctl's codec gives strasse.de, xn--4xa.gr, ab.de and
    // xn--xy-g4b.de.
    assert_eq!(canonical_host("straße.de").unwrap(), "xn--strae-oqa.de");
    assert_eq!(canonical_host("ς.gr").unwrap(), "xn--3xa.gr");
    assert!(canonical_host("a\u{200d}b.de").is_err());
    assert!(canonical_host("x\u{378}y.de").is_err());
    // An ASCII name as rpctl's codec takes it: labels 1-63 characters, an
    // `xn--` label unchecked.
    assert_eq!(canonical_host("XN--ZZ.test").unwrap(), "xn--zz.test");
    assert!(canonical_host("a..b").is_err());
    assert!(canonical_host(&format!("{}.test", "a".repeat(64))).is_err());
    assert!(canonical_host(&format!("{}.test", "a".repeat(63))).is_ok());
}

/// Where a non-ASCII character decides whether a scheme starts, viy counts
/// only what both readings of it find: matches do not overlap, so reading
/// `ⓛ` as rpctl's Python may not would count `other.test`, which rp never
/// counts.
#[test]
fn an_unsure_word_boundary_counts_only_what_both_readings_find() {
    assert_eq!(hosts("ⓛa://b.c-d://evil.test:x://other.test"), want(&[]));
    assert_eq!(
        hosts("ⓛa://b.test x https://c.test"),
        want(&[("c.test", &[])])
    );
    assert_eq!(hosts("é https://c.test"), want(&[("c.test", &[])]));
    assert_eq!(
        hosts("\u{a0}https://c.test\u{1f}x"),
        want(&[("c.test", &[])])
    );
}

#[test]
fn the_lexer_reads_comments_and_operators_as_rpctl_shlex() {
    let words = |cmd| shlex_words(cmd).unwrap();
    assert_eq!(words("a;b&&c|d"), ["a", ";", "b", "&&", "c", "|", "d"]);
    assert_eq!(
        words("curl x.test/#y z.test\ngit"),
        ["curl", "x.test/", "git"]
    );
    assert_eq!(words("a)|b"), ["a", ")|", "b"]);
    assert_eq!(
        words(r#"'' "a\"b" "a\xb" a\ b"#),
        ["", "a\"b", "a\\xb", "a b"]
    );
    assert_eq!(shlex_words("'a"), None);
    assert_eq!(shlex_words("a\\"), None);
}

// ------------------------------------------------------------------- oracle

#[cfg(feature = "oracle")]
const HOST_PIECES: &[&str] = &[
    "a",
    "test",
    "example",
    "com",
    "pypi",
    "org",
    ".",
    ".",
    "-",
    "_",
    "0",
    "1",
    "127",
    "255",
    "0x7f",
    "0x",
    "07",
    "x",
    "localhost",
    "LOCALHOST",
    "local",
    "lan",
    "corp",
    "home.arpa",
    "internal",
    "[",
    "]",
    ":",
    "::",
    "::1",
    "ffff",
    "2002",
    "64:ff9b",
    "fe80",
    "%",
    "%25eth0",
    "@",
    "xn--",
    "xn--bcher-kva",
    "v1",
    " ",
    "\t",
    "*",
    "/",
    "?",
    "#",
    "9",
    "65535",
    "2130706433",
];

/// The policy against rpctl's: canonical hosts and their refusals, the
/// hosts a command names, SANDBOX_NET_ALLOW, `why_not` and `describe`
/// (`cargo test -p viy-tools --features oracle`). ASCII only where IDNA
/// decides: non-ASCII names are UTS 46 here (flags F53).
#[cfg(feature = "oracle")]
#[test]
fn policy_matches_rpctl() {
    use serde_json::Value;
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    const CMD_PIECES: &[&str] = &[
        "curl ",
        "wget ",
        "git ",
        "clone ",
        "-C ",
        "src ",
        "--depth ",
        "1 ",
        "-o ",
        "-H ",
        "-x ",
        "https://",
        "http://",
        "ssh://",
        "git+https://",
        "ftp://",
        "user:tok@",
        "a.test",
        "Example.COM",
        "pypi.org",
        "[2606:4700::1]",
        "[::1]",
        "127.0.0.1",
        "93.184.216.34",
        "localhost",
        ":8080",
        ":22",
        ":0",
        ":99999",
        "/x",
        "/",
        "#",
        "?",
        "'",
        "\"",
        "\\",
        " ",
        "\n",
        ";",
        "&&",
        "|",
        "(",
        ")",
        ">",
        "&",
        "echo ",
        "/usr/bin/curl ",
        "ⓛocal",
        "\u{1f}",
        "a.local",
        "x.internal",
        "dl.example.net",
        "api.example.io:8443/v1",
        "*",
    ];
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let mut call = |f: &str, args: Value| oracle.call(f, args, json!({})).unwrap().outcome;
    for case in 0..cases(5000) {
        let raw = g.string(HOST_PIECES, 6);
        let at = |what: &str| format!("{what} case {case}, seed {:#x}", seed());
        let mine = match canonical_host(&raw) {
            Ok(h) => Outcome::Returned(json!(h)),
            Err(e) => Outcome::Raised {
                kind: "Blocked".into(),
                message: e.0,
            },
        };
        let theirs = call("egress.canonical_host", json!([raw]));
        if raw.contains('%') {
            // A scope id: rpctl may read an address, viy refuses a name
            // (netguard::parse_ip).
            assert!(
                matches!(mine, Outcome::Raised { .. }) || theirs == mine,
                "{} {raw:?}: {mine:?} {theirs:?}",
                at("canonical_host")
            );
        } else {
            assert_eq!(theirs, mine, "{} {raw:?}", at("canonical_host"));
        }

        let cmd = g.string(CMD_PIECES, 14);
        let mine: serde_json::Map<String, Value> = hosts_in_command(&cmd)
            .into_iter()
            .map(|(h, p)| (h, json!(p)))
            .collect();
        let theirs = call("egress.hosts_in_command", json!([cmd]));
        let Outcome::Returned(Value::Object(theirs)) = theirs else {
            panic!("{}: {theirs:?}", at("hosts_in_command"))
        };
        // Compared as sets of keys: a map's order is not the policy's.
        let sorted = |m: &serde_json::Map<String, Value>| {
            let mut v: Vec<(String, Value)> = m.clone().into_iter().collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };
        if !cmd.is_ascii() {
            // Where a non-ASCII character decides a word boundary viy may
            // see fewer hosts, never another (`url_hosts`).
            for (h, p) in &mine {
                assert_eq!(theirs.get(h), Some(p), "{} {cmd:?}", at("hosts_in_command"));
            }
        } else {
            assert_eq!(
                sorted(&theirs),
                sorted(&mine),
                "{} {cmd:?}",
                at("hosts_in_command")
            );
        }

        let allow = g.string(&[HOST_PIECES, &[",", " ", "*.", ", "]].concat(), 8);
        let (exact, suffixes) = parse_allow(&allow);
        assert_eq!(
            call("egress.parse_allow", json!([allow])),
            Outcome::Returned(json!([exact, suffixes])),
            "{} {allow:?}",
            at("parse_allow")
        );

        let user: Vec<String> = (0..g.below(3)).map(|_| g.string(HOST_PIECES, 4)).collect();
        let probes: Vec<(String, u16)> = (0..4)
            .map(|_| {
                let host = match g.below(3) {
                    0 => (*g.pick(&REGISTRIES)).to_string(),
                    1 => hosts_in_command(&cmd)
                        .keys()
                        .next()
                        .cloned()
                        .unwrap_or_else(|| "a.test".into()),
                    _ => g.string(HOST_PIECES, 3),
                };
                (host, *g.pick(&[80, 443, 22, 8080, 8443, 9443, 1]))
            })
            .collect();
        if !cmd.is_ascii() {
            continue; // fewer command hosts, checked above
        }
        let policy = Policy::new(&cmd, user.iter().map(String::as_str), &allow);
        let mine = json!({
            "describe": policy.describe(),
            "why_not": probes.iter().map(|(h, p)| policy.why_not(h, *p)).collect::<Vec<_>>(),
        });
        assert_eq!(
            call(
                "egress.policy",
                json!([
                    cmd,
                    user,
                    allow,
                    probes
                        .iter()
                        .map(|(h, p)| json!([h, p]))
                        .collect::<Vec<_>>()
                ])
            ),
            Outcome::Returned(mine),
            "{} {cmd:?} {user:?} {allow:?} {probes:?}",
            at("policy")
        );
    }
}

/// The proxy's answers as rpctl writes them, for every status and odd
/// reasons.
#[cfg(feature = "oracle")]
#[test]
fn responses_match_rpctl() {
    use serde_json::json;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    let mut oracle = Oracle::spawn().unwrap();
    for status in [400, 403, 502, 503, 200, 500] {
        for reason in ["why", "", "é\x00\n", &"x".repeat(300)] {
            let got = oracle
                .call("egress.response", json!([status, reason]), json!({}))
                .unwrap()
                .outcome;
            let Outcome::Returned(got) = got else {
                panic!("{got:?}")
            };
            let want = String::from_utf8(response(status, reason)).unwrap();
            assert_eq!(got["$bytes"].is_string(), true);
            assert_eq!(
                decode64(got["$bytes"].as_str().unwrap()),
                want.into_bytes(),
                "{status} {reason:?}"
            );
        }
    }
}

#[cfg(feature = "oracle")]
fn decode64(s: &str) -> Vec<u8> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let vals: Vec<u32> = s
        .bytes()
        .filter(|&b| b != b'=')
        .map(|b| T.iter().position(|&t| t == b).unwrap() as u32)
        .collect();
    let mut out = Vec::new();
    for chunk in vals.chunks(4) {
        let mut n = 0u32;
        for (i, v) in chunk.iter().enumerate() {
            n |= v << (18 - 6 * i);
        }
        out.extend(n.to_be_bytes()[1..chunk.len()].iter());
    }
    out
}

// rpctl's ConnectTests.test_a_trailing_unicode_full_stop_is_still_a_local_name
// (flags F54, fixed in rpctl 728277d).
#[test]
fn a_trailing_unicode_full_stop_is_still_a_local_name() {
    for raw in [
        "localhost\u{3002}",
        "db.internal\u{3002}",
        "nas.local\u{ff0e}",
        "a.lan\u{ff61}",
        "localhost.\u{3002}",
    ] {
        assert!(canonical_host(raw).is_err(), "{raw}");
    }
    assert_eq!(
        canonical_host("example.com\u{3002}").unwrap(),
        "example.com"
    );
    assert_eq!(
        hosts("curl http://localhost\u{3002}/ https://db.internal\u{3002}/x"),
        want(&[])
    );
}
