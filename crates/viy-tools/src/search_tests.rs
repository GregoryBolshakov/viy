//! rpctl's SearchTests and the search client's VaultKeyTests
//! (test_netaccess.py), with Tavily through a scripted HTTP client and a
//! fake `tok`; with no DuckDuckGo fallback (kb d119), the cases that fell
//! back now check the error the model gets.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_http::HttpError;
use viy_testkit::http::FakeHttp;
use viy_testkit::http::Reply;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// rpctl's HITS: a fragment, a duplicate under `www.` with a trailing
/// slash, a `javascript:` URL, an escape sequence in a title and a zero
/// width space in content.
fn hits() -> Value {
    json!({"results": [
        {"title": "A", "url": "https://a.test/x#frag", "content": "alpha"},
        {"title": "A again", "url": "https://www.a.test/x/", "content": "dup"},
        {"title": "B", "url": "javascript:x", "content": "bad"},
        {"title": "C\u{1b}]0;t\u{7}", "url": "https://c.test/", "content": "c\u{200b}c"},
    ]})
}

fn ok(body: &Value) -> Reply {
    Reply::answer(200, serde_json::to_vec(body).unwrap_or_default())
}

fn cfg(over: &[(&str, &str)]) -> Config {
    let mut cfg = Config::defaults();
    cfg.set("TAVILY_API_KEY", "tvly-test");
    cfg.set("TAVILY_VAULT_TOKEN", "");
    for (k, v) in over {
        cfg.set(k, v);
    }
    cfg
}

/// A client whose `tok` cannot be found: these tests never reach the vault.
fn client(over: &[(&str, &str)]) -> SearchClient {
    SearchClient::new(
        &cfg(over),
        vec!["literal-secret-value".to_string()],
        Tok::with_env([]),
    )
}

async fn search(c: &mut SearchClient, http: &FakeHttp, q: &str) -> Result<Results, SearchError> {
    c.search(http, q, None, None, None).await
}

fn sent_body(http: &FakeHttp, i: usize) -> Value {
    let sent = http.sent();
    serde_json::from_slice(sent[i].body.as_deref().unwrap_or_default()).unwrap_or_default()
}

#[tokio::test]
async fn results_are_cleaned_and_deduped() {
    let mut c = client(&[]);
    let http = FakeHttp::new([ok(&hits())]);
    let r = search(&mut c, &http, "q").await.unwrap();
    let urls: Vec<&str> = r.hits.iter().map(|h| h.url.as_str()).collect();
    assert_eq!(urls, ["https://a.test/x", "https://c.test/"]);
    assert_eq!(r.hits[1].title, "C");
    assert_eq!(r.hits[1].content, "cc");
    assert_eq!(r.hits[0].published, "");
    assert!(!r.cached);
}

#[tokio::test]
async fn the_request_is_rps() {
    let mut c = client(&[]);
    let http = FakeHttp::new([ok(&hits())]);
    search(&mut c, &http, "  what   is\nnew ").await.unwrap();
    let sent = http.sent();
    assert_eq!(sent[0].method, Method::POST);
    assert_eq!(sent[0].url, TAVILY_URL);
    assert_eq!(
        sent[0].header_value("Authorization"),
        Some("Bearer tvly-test")
    );
    assert_eq!(
        sent[0].header_value("User-Agent"),
        Some(user_agent::RP_REQUESTS)
    );
    assert_eq!(
        (sent[0].connect_timeout, sent[0].read_timeout),
        (Duration::from_secs(5), Duration::from_secs(20))
    );
    assert_eq!(
        serde_json::to_string(&sent_body(&http, 0)).unwrap_or_default(),
        r#"{"query":"what is new","max_results":5,"search_depth":"basic","include_answer":false,"include_raw_content":false}"#
    );
}

#[tokio::test]
async fn dead_key_disables_tavily_once_and_loudly() {
    let mut c = client(&[]);
    let http = FakeHttp::new([Reply::answer(432, "{}")]);
    let e1 = search(&mut c, &http, "q1").await.unwrap_err();
    let e2 = search(&mut c, &http, "q2").await.unwrap_err();
    assert_eq!(http.sent().len(), 1);
    assert_eq!(
        e1.0,
        "no backend returned results (tavily: the Tavily plan's credits are used up (HTTP 432))"
    );
    assert_eq!(
        e2.0,
        "no backend returned results (tavily: Tavily is off for this chat \
         (the Tavily plan's credits are used up (HTTP 432)))"
    );
    assert_eq!(
        c.take_loud(),
        [
            "web search: the Tavily plan's credits are used up (HTTP 432); no web search \
          for the rest of this chat. Check TAVILY_API_KEY."
        ]
    );
    assert_eq!(c.take_loud(), Vec::<String>::new());
    assert!(c.status().contains("credits"), "{}", c.status());
}

#[tokio::test(start_paused = true)]
async fn outage_opens_the_breaker() {
    let mut c = client(&[]);
    let http = FakeHttp::new([
        Reply::answer(503, "{}"),
        Reply::answer(503, "{}"),
        ok(&hits()),
    ]);
    for q in ["a", "b", "c"] {
        assert!(search(&mut c, &http, q).await.is_err());
    }
    assert_eq!(http.sent().len(), 2);
    assert!(!c.tavily.usable());
    assert_eq!(c.status(), "Tavily paused for 120 s");
    assert_eq!(
        search(&mut c, &http, "d").await.unwrap_err().0,
        "no backend returned results (tavily: Tavily is paused after repeated failures)"
    );
    tokio::time::advance(Duration::from_secs(120)).await;
    assert_eq!(c.status(), "Tavily");
    assert_eq!(search(&mut c, &http, "e").await.unwrap().hits.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_rate_limit_pauses_at_once_and_a_dead_line_twice() {
    let mut c = client(&[]);
    let http = FakeHttp::new([Reply::answer(429, "{}")]);
    assert_eq!(
        search(&mut c, &http, "a").await.unwrap_err().0,
        "no backend returned results (tavily: Tavily HTTP 429)"
    );
    assert_eq!(c.status(), "Tavily paused for 60 s");

    let mut c = client(&[]);
    let http = FakeHttp::new([
        Reply::Fail(HttpError::Timeout),
        Reply::Fail(HttpError::Connect("refused".into())),
    ]);
    assert_eq!(
        search(&mut c, &http, "a").await.unwrap_err().0,
        "no backend returned results (tavily: Tavily unreachable (Timeout))"
    );
    assert!(c.tavily.usable());
    assert_eq!(
        search(&mut c, &http, "b").await.unwrap_err().0,
        "no backend returned results (tavily: Tavily unreachable (ConnectionError))"
    );
    assert!(!c.tavily.usable());
}

#[tokio::test]
async fn other_answers_say_what_came_back() {
    let mut c = client(&[]);
    let long = format!("{{\"detail\": \"{}\"}}", "x".repeat(200));
    let http = FakeHttp::new([
        Reply::answer(400, long.clone()),
        Reply::answer(200, "<html>"),
        ok(&json!({"results": []})),
        ok(&json!(null)),
    ]);
    let e = search(&mut c, &http, "a").await.unwrap_err().0;
    let shown: String = long.chars().take(120).collect();
    assert_eq!(
        e,
        format!("no backend returned results (tavily: Tavily HTTP 400: {shown})")
    );
    assert_eq!(
        search(&mut c, &http, "b").await.unwrap_err().0,
        "no backend returned results (tavily: Tavily returned no JSON)"
    );
    for q in ["c", "d"] {
        assert_eq!(
            search(&mut c, &http, q).await.unwrap_err().0,
            "no backend returned results (tavily: no results)"
        );
    }
    // None of these is an outage.
    assert!(c.tavily.usable());
}

#[tokio::test]
async fn cache_and_secret_queries() {
    let mut c = client(&[]);
    let http = FakeHttp::new([ok(&hits())]);
    search(&mut c, &http, "same q").await.unwrap();
    let r = search(&mut c, &http, "same  Q").await.unwrap();
    assert_eq!(http.sent().len(), 1);
    assert!(r.cached);
    let secret = format!("sk-{}", "a".repeat(30));
    for q in ["look up literal-secret-value", secret.as_str()] {
        let e = search(&mut c, &http, q).await.unwrap_err();
        assert!(
            e.0.starts_with("refused: the query would leave this machine"),
            "{e}"
        );
    }
    assert_eq!(http.sent().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_cache_keeps_ten_minutes_and_64_queries() {
    let mut c = client(&[]);
    let http = FakeHttp::new((0..67).map(|_| ok(&hits())));
    search(&mut c, &http, "q").await.unwrap();
    tokio::time::advance(CACHE_TTL).await;
    assert!(!search(&mut c, &http, "q").await.unwrap().cached);
    for i in 0..64 {
        search(&mut c, &http, &format!("other {i}")).await.unwrap();
    }
    // "q" was the oldest of 65 entries.
    assert!(!search(&mut c, &http, "q").await.unwrap().cached);
    assert!(search(&mut c, &http, "other 63").await.unwrap().cached);
    assert_eq!(http.sent().len(), 67);
}

#[tokio::test]
async fn bad_queries_are_refused_before_any_request() {
    let mut c = client(&[]);
    let http = FakeHttp::new([]);
    assert_eq!(
        search(&mut c, &http, " \n ").await.unwrap_err().0,
        "no query given"
    );
    assert_eq!(
        search(&mut c, &http, &"é".repeat(401)).await.unwrap_err().0,
        "the query is longer than 400 characters"
    );
}

#[tokio::test]
async fn arguments_are_clamped() {
    let mut c = client(&[]);
    let http = FakeHttp::new([ok(&hits())]);
    c.search(
        &http,
        "q",
        Some(&json!("50")),
        Some("decade"),
        Some("https://Docs.Python.org/3/x"),
    )
    .await
    .unwrap();
    let body = sent_body(&http, 0);
    assert_eq!(body["max_results"], 10);
    assert!(body.get("time_range").is_none());
    assert_eq!(body["include_domains"], json!(["docs.python.org"]));
}

#[tokio::test]
async fn recency_and_site_reach_tavily() {
    let mut c = client(&[("TAVILY_SEARCH_DEPTH", "advanced")]);
    let http = FakeHttp::new([ok(&hits())]);
    c.search(
        &http,
        "q",
        Some(&json!(3)),
        Some("week"),
        Some("rust-lang.org"),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_string(&sent_body(&http, 0)).unwrap_or_default(),
        r#"{"query":"q","max_results":3,"search_depth":"advanced","include_answer":false,"include_raw_content":false,"time_range":"week","include_domains":["rust-lang.org"]}"#
    );
}

#[test]
fn result_counts() {
    let cases: [(Option<Value>, Option<i64>, usize); 14] = [
        (None, Some(5), 5),
        (None, Some(50), 10),
        (None, None, 5),
        (Some(json!(null)), Some(7), 7),
        (Some(json!(0)), Some(7), 7),
        (Some(json!("")), Some(7), 7),
        (Some(json!(false)), Some(7), 7),
        (Some(json!("0")), Some(7), 1),
        (Some(json!(" 3 ")), None, 3),
        (Some(json!("3.5")), Some(7), 5),
        (Some(json!(3.9)), None, 3),
        (Some(json!(-2)), None, 1),
        (Some(json!(true)), None, 1),
        (Some(json!([1])), Some(7), 5),
    ];
    for (k, configured, want) in cases {
        assert_eq!(
            result_count(k.as_ref(), configured),
            want,
            "{k:?} {configured:?}"
        );
    }
}

#[test]
fn sites() {
    for (site, want) in [
        ("docs.python.org", Some("docs.python.org")),
        (" HTTP://Docs.Python.org/3/x ", Some("docs.python.org")),
        ("https://Docs.Python.org/3/x", Some("docs.python.org")),
        ("http://a.b", Some("a.b")),
        ("ab", None),
        ("", None),
        ("exa mple.com", None),
        ("bücher.de", None),
        ("site:evil.test", None),
    ] {
        assert_eq!(clean_site(site).as_deref(), want, "{site:?}");
    }
}

#[test]
fn normalise_keeps_k_and_cuts_content() {
    let raw: Vec<Value> = (0..5)
        .map(|i| {
            json!({"title": i, "url": format!(" https://x.test/{i} "), "content": "w  ".repeat(10),
                   "published": "2026-07-19"})
        })
        .collect();
    let out = normalise(&raw, 7, 3);
    assert_eq!(out.len(), 3);
    assert_eq!(
        out[2],
        Hit {
            title: "2".into(),
            url: "https://x.test/2".into(),
            content: "w w w w".into(),
            published: "2026-07-19".into(),
        }
    );
}

/// rpctl's fake `tok` (VaultKeyTests): it fails when the PIN reaches it,
/// lists an open and a PIN-gated token, and gives only the open one's
/// value.
const FAKE_TOK: &str = r#"#!/bin/sh
[ -n "$TOK_PIN" ] && { echo "pin leaked" >&2; exit 9; }
case "$1" in
list) printf '%s\n' "tavily-dev                       $TAVILY_DEV                     open (no PIN to read)" \
                      "npm-token                        $NPM_TOKEN" ;;
get) [ "$2" = tavily-dev ] && echo tvly-from-vault-01234 || exit 5 ;;
esac
"#;

fn fake_tok() -> (ScratchDir, Tok) {
    let dir = ScratchDir::new("search-tok").unwrap();
    let exe = dir.path().join("tok");
    fs::write(&exe, FAKE_TOK).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    // A sibling test can fork while the script is open for writing, and the
    // child keeps the descriptor until it execs: retry while exec says
    // ETXTBSY, as vault_tests.rs does.
    for _ in 0..100 {
        let run = std::process::Command::new(&exe)
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !run.is_err_and(|e| e.kind() == std::io::ErrorKind::ExecutableFileBusy) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut path = dir.path().as_os_str().to_owned();
    path.push(":/usr/bin:/bin");
    let tok = Tok::with_env([
        (OsString::from("PATH"), path),
        (OsString::from("TOK_PIN"), OsString::from("should-not-pass")),
    ]);
    (dir, tok)
}

#[tokio::test]
async fn search_client_uses_it_and_guards_it() {
    let (_dir, tok) = fake_tok();
    let mut c = SearchClient::new(
        &cfg(&[("TAVILY_API_KEY", ""), ("TAVILY_VAULT_TOKEN", "tavily-dev")]),
        vec![],
        tok,
    );
    assert_eq!(c.key(), "tvly-from-vault-01234");
    let http = FakeHttp::new([]);
    let e = search(&mut c, &http, "look up tvly-from-vault-01234")
        .await
        .unwrap_err();
    assert!(e.0.starts_with("refused:"), "{e}");
    assert!(c.secrets().contains(&"tvly-from-vault-01234".to_string()));
}

#[test]
fn config_key_wins_and_empty_name_skips_the_vault() {
    let (_dir, tok) = fake_tok();
    let mut c = SearchClient::new(
        &cfg(&[
            ("TAVILY_API_KEY", "tvly-config"),
            ("TAVILY_VAULT_TOKEN", "tavily-dev"),
        ]),
        vec![],
        tok.clone(),
    );
    assert_eq!(c.key(), "tvly-config");
    let mut c = SearchClient::new(&cfg(&[("TAVILY_API_KEY", "")]), vec![], tok);
    assert_eq!(c.key(), "");
    assert_eq!(c.status(), "no Tavily key");
    assert_eq!(c.take_loud(), Vec::<String>::new());
}

#[tokio::test]
async fn gated_key_is_reported_once() {
    let (_dir, tok) = fake_tok();
    let mut c = SearchClient::new(
        &cfg(&[("TAVILY_API_KEY", ""), ("TAVILY_VAULT_TOKEN", "npm-token")]),
        vec![],
        tok,
    );
    assert_eq!(c.key(), "");
    let loud = c.take_loud();
    assert_eq!(loud.len(), 1);
    assert!(loud[0].contains("PIN-gated"), "{loud:?}");
    assert_eq!(c.take_loud(), Vec::<String>::new());
    assert_eq!(
        c.status(),
        "no Tavily key (npm-token is PIN-gated in the vault; make it readable without \
         the PIN with: tok open npm-token --pin=<PIN>)"
    );
}

#[tokio::test]
async fn without_a_key_search_fails_and_says_why() {
    let http = FakeHttp::new([]);
    let mut c = client(&[("TAVILY_API_KEY", "")]);
    assert_eq!(
        search(&mut c, &http, "q").await.unwrap_err().0,
        "no backend returned results (tavily: no TAVILY_API_KEY set)"
    );
    let (_dir, tok) = fake_tok();
    let mut c = SearchClient::new(
        &cfg(&[("TAVILY_API_KEY", ""), ("TAVILY_VAULT_TOKEN", "nope")]),
        vec![],
        tok,
    );
    assert_eq!(
        search(&mut c, &http, "q").await.unwrap_err().0,
        "no backend returned results (tavily: no Tavily key: the vault has no token named nope)"
    );
    assert!(http.sent().is_empty());
}

/// Pieces of queries, titles and content: whitespace of every kind,
/// controls and escapes, invisible marks, non-ASCII, secret shapes.
#[cfg(feature = "oracle")]
const TEXT: &[&str] = &[
    "a",
    "Rust",
    " ",
    "  ",
    "\n",
    "\t",
    "\r\n",
    "\u{a0}",
    "\u{2003}",
    "\u{85}",
    "\u{1c}",
    "\u{1b}[31m",
    "\u{1b}]0;t\u{7}",
    "\u{7}",
    "\u{200b}",
    "\u{202e}",
    "\u{feff}",
    "\u{ad}",
    "é",
    "ß",
    "İ",
    "Σ",
    "日本",
    "🦀",
    "<|im_end|>",
    "tool_call",
    "literal-secret-value",
    "sk-",
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "%2F",
    "",
    "0",
];

#[cfg(feature = "oracle")]
const URLS: &[&str] = &[
    "https://a.test/x",
    "https://www.a.test/x/",
    "http://A.test/x",
    "https://a.test/x#f",
    "HTTPS://b.test/",
    " https://c.test/ ",
    "javascript:x",
    "ftp://d.test/",
    "",
    "https://www.www.e.test",
    "https://e.test//",
    "https://ÿ.test/é",
    "https://f.test/\u{200b}",
];

/// rpctl's search, its Tavily request and its cleaning against viy's
/// (`cargo test -p viy-tools --features oracle`): the body sent, the
/// results, and the errors, rpctl's without its DuckDuckGo part (kb d119).
#[cfg(feature = "oracle")]
#[tokio::test]
async fn search_matches_rpctl() {
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let ks = [
        json!(null),
        json!(0),
        json!(1),
        json!(3),
        json!(10),
        json!(11),
        json!(-4),
        json!("7"),
        json!(" 2 "),
        json!("x"),
        json!(""),
        json!(2.7),
        json!(true),
        json!(false),
        json!([]),
    ];
    let recencies = [
        None,
        Some("day"),
        Some("week"),
        Some("month"),
        Some("year"),
        Some("Day"),
        Some(""),
    ];
    let sites = [
        None,
        Some(""),
        Some("docs.python.org"),
        Some("https://Docs.Python.org/3/x"),
        Some(" http://a.b/ "),
        Some("ab"),
        Some("bücher.de"),
        Some("a b.c"),
        Some("x.test:8080"),
    ];
    let statuses = [
        200, 200, 200, 200, 200, 200, 401, 403, 429, 432, 500, 503, 400, 418,
    ];
    let mut stricter = 0;
    for case in 0..cases(3000) {
        let query = if g.below(20) == 0 {
            "q ".repeat(150 + g.below(100))
        } else {
            // Python's split() also splits at the separators U+001C-U+001F,
            // Rust's at White_Space only: an implementation detail (d029),
            // and in content clean() removes them first.
            g.string(TEXT, 6).replace('\u{1c}', " ")
        };
        let k = g.pick(&ks).clone();
        let recency = *g.pick(&recencies);
        let site = *g.pick(&sites);
        let max_chars = *g.pick(&[1usize, 5, 40, 1200]);
        let status = *g.pick(&statuses);
        let n = g.below(8);
        let mut results = Vec::new();
        for _ in 0..n {
            let mut h = serde_json::Map::new();
            let mut opt = |g: &mut Gen, name: &str, v: Value| {
                if g.below(6) != 0 {
                    h.insert(name.to_string(), v);
                }
            };
            let url = json!(format!(
                "{}{}",
                g.pick(URLS),
                g.pick(&["", "", "p", "/q?x=1"])
            ));
            opt(&mut g, "url", url);
            let title = if g.below(8) == 0 {
                json!(null)
            } else {
                json!(g.string(TEXT, 5))
            };
            opt(&mut g, "title", title);
            let content = if g.below(8) == 0 {
                json!(null)
            } else {
                json!(g.string(TEXT, 12))
            };
            opt(&mut g, "content", content);
            let date = *g.pick(&["2026-07-19", "", "Mon, 3 Nov 2026 10:00:00 GMT \u{1b}[1m"]);
            opt(&mut g, "published_date", json!(date));
            results.push(Value::Object(h));
        }
        let body = match g.below(12) {
            0 => "<html>".to_string(),
            1 => "null".to_string(),
            _ => json!({"results": results}).to_string(),
        };

        let mut cfg = cfg(&[("SEARCH_MAX_CHARS_PER_RESULT", &max_chars.to_string())]);
        cfg.set("TAVILY_API_KEY", "tvly-oracle-key");
        let mut c = SearchClient::new(&cfg, vec!["literal-secret-value".into()], Tok::with_env([]));
        let http = FakeHttp::new([Reply::answer(status, body.clone())]);
        let got = c.search(&http, &query, Some(&k), recency, site).await;
        let sent = http.sent();
        let mut ours = match got {
            Ok(r) => {
                let hits: Vec<Value> = r
                    .hits
                    .iter()
                    .map(|h| {
                        json!({"title": h.title, "url": h.url, "content": h.content,
                               "published": h.published})
                    })
                    .collect();
                json!({"results": hits, "backend": BACKEND, "quality": QUALITY})
            }
            Err(e) => json!({"error": e.0}),
        };
        ours["sent"] = match sent.first() {
            Some(req) => serde_json::from_slice(req.body.as_deref().unwrap_or_default()).unwrap(),
            None => Value::Null,
        };

        let at = || {
            format!(
                "case {case}, seed {:#x}: {query:?} k={k} {recency:?} {site:?} {status} {body}",
                seed()
            )
        };
        let args = json!([query, k, recency, site, max_chars, status, body]);
        let Outcome::Returned(mut theirs) = oracle
            .call("search.tavily", args, json!({}))
            .unwrap()
            .outcome
        else {
            panic!("rpctl raised: {}", at());
        };
        if let Some(Value::String(e)) = theirs.get_mut("error") {
            // viy's name for the secret settings (kb d114).
            *e = e
                .replace("; duckduckgo: ddg)", ")")
                .replace("of rp's secret settings", "of viy's secret settings");
        }
        // Stricter only: a token shape after a non-ASCII letter is one at
        // an ASCII word boundary, not at Python's (kb d042).
        let shaped = "refused: the query would leave this machine and it contains something \
                      shaped like an API token or private key";
        if ours["error"] == shaped
            && !theirs["error"]
                .as_str()
                .is_some_and(|e| e.starts_with("refused:"))
        {
            stricter += 1;
            continue;
        }
        // Key order counts: the request body is sent as written.
        assert_eq!(ours.to_string(), theirs.to_string(), "{}", at());
    }
    eprintln!("search_matches_rpctl: {stricter} queries refused by viy alone (kb d042)");
}
