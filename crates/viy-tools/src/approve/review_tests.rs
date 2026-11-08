use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_http::HttpError;
use viy_testkit::http::FakeHttp;
use viy_testkit::http::Reply;
use viy_testkit::scratch::ScratchDir;

use super::*;

fn ok(content: &str) -> Reply {
    Reply::answer(
        200,
        json!({"choices": [{"message": {"content": content}}]}).to_string(),
    )
}

fn users(texts: &[&str]) -> Vec<String> {
    texts.iter().map(ToString::to_string).collect()
}

/// Builds a case's workspace: UTF-8 text, `hex:<bytes>` or `link:<target>`.
fn build(ws: &Path, files: &Value) {
    for (name, body) in files.as_object().into_iter().flatten() {
        let path = ws.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = body.as_str().unwrap();
        if let Some(hex) = body.strip_prefix("hex:") {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            std::fs::write(&path, bytes).unwrap();
        } else if let Some(target) = body.strip_prefix("link:") {
            symlink(target, &path).unwrap();
        } else {
            std::fs::write(&path, body).unwrap();
        }
    }
}

fn reply(spec: &Value) -> Reply {
    if let Some(class) = spec["raise"].as_str() {
        assert_eq!(class, "ConnectionError");
        return Reply::Fail(HttpError::Connect("scripted".into()));
    }
    let status = u16::try_from(spec["status"].as_u64().unwrap()).unwrap();
    let body = match &spec["json"] {
        Value::Null => spec["text"].as_str().unwrap().to_string(),
        doc => doc.to_string(),
    };
    Reply::answer(status, body)
}

/// rpctl's `approve.review` on the `approve/review` fixture: the same
/// requests (url, key, body, timeouts) and the same verdict.
#[tokio::test]
async fn review_sends_and_decides_as_rpctl() {
    let fixture = viy_testkit::fixtures::load("approve/review").unwrap();
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let scratch = ScratchDir::new("review").unwrap();
        let ws = scratch.path().join(name);
        std::fs::create_dir(&ws).unwrap();
        build(&ws, &case["files"]);
        let http = FakeHttp::new(case["replies"].as_array().unwrap().iter().map(reply));
        let requests: Vec<String> = case["users"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u.as_str().unwrap().to_string())
            .collect();
        let mut review = Review::new(
            "http://127.0.0.1:8000",
            "test-key",
            "qwen",
            &requests,
            case["cmd"].as_str().unwrap(),
            &ws,
        );
        review.network = case["network"].as_str();
        review.private_git = case["private_git"].as_bool().unwrap_or(false);
        review.server = case["server"].as_str();
        let got = review.run(&http).await;

        let sent = http.sent();
        let calls = case["calls"].as_array().unwrap();
        assert_eq!(sent.len(), calls.len(), "{name}");
        for (request, call) in sent.iter().zip(calls) {
            assert_eq!(request.url, call["url"].as_str().unwrap(), "{name}");
            assert_eq!(
                request.header_value("Authorization"),
                call["headers"]["Authorization"].as_str(),
                "{name}"
            );
            assert_eq!(
                request.header_value("Content-Type"),
                Some("application/json")
            );
            let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
            assert_eq!(body, call["json"], "{name}");
            // Key order too: the server sees the same document.
            assert_eq!(
                body.to_string(),
                call["json"].to_string(),
                "{name}: key order"
            );
            let timeout = |i: usize| Duration::from_secs(call["timeout"][i].as_u64().unwrap());
            assert_eq!(
                (request.connect_timeout, request.read_timeout),
                (timeout(0), timeout(1)),
                "{name}"
            );
        }
        let want = &case["result"];
        let got = match got {
            Ok((risk, reason)) => json!([risk.as_str(), reason]),
            Err(why) => json!([null, why]),
        };
        assert_eq!(&got, want, "{name}");
    }
}

/// The prompts are rpctl's, byte for byte (the fixture's bodies carry them
/// too; this names the constants).
#[test]
fn prompts_are_rpctls() {
    let fixture = viy_testkit::fixtures::load("approve/review").unwrap();
    let system = |name: &str| {
        fixture.case(name).unwrap()["calls"][0]["json"]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(system("requirements_attached"), REVIEW_SYSTEM);
    assert_eq!(
        system("private_git_push"),
        format!("{REVIEW_SYSTEM}{REVIEW_PRIVATE_GIT}")
    );
    assert_eq!(system("server_no_files"), REVIEW_REMOTE_SYSTEM);
}

/// The doctrines' section (new in viy, kb d109) goes last in the system
/// message, after rpctl's prompt and the private-git note, local or
/// remote; the user's message is untouched.
#[test]
fn a_policy_follows_rpctls_prompt() {
    let ws = std::env::temp_dir();
    let users = ["fetch the data".to_string()];
    let base = Review::new("http://x", "k", "m", &users, "curl https://a.test", &ws);
    let plain = base.body().unwrap();
    let policy = "\n\n# The user's security policy\n\nNo tunnels.";
    let with = Review {
        policy: Some(policy),
        ..base.clone()
    };
    let body = with.body().unwrap();
    assert_eq!(
        body["messages"][0]["content"],
        format!("{REVIEW_SYSTEM}{policy}")
    );
    assert_eq!(body["messages"][1], plain["messages"][1]);
    let git = Review {
        private_git: true,
        ..with.clone()
    };
    assert_eq!(
        git.body().unwrap()["messages"][0]["content"],
        format!("{REVIEW_SYSTEM}{REVIEW_PRIVATE_GIT}{policy}")
    );
    let remote = Review {
        server: Some("host a"),
        ..with
    };
    assert_eq!(
        remote.body().unwrap()["messages"][0]["content"],
        format!("{REVIEW_REMOTE_SYSTEM}{policy}")
    );
}

fn rated(risk: &str, reason: &str) -> Reply {
    ok(&json!({"risk": risk, "reason": reason}).to_string())
}

/// The review with doctrines (kb d109): rpctl's request first, the
/// doctrines' second, the higher rating holds, whatever the doctrine said.
#[tokio::test]
async fn doctrines_can_only_make_the_review_stricter() {
    let ws = std::env::temp_dir();
    let users = ["get it".to_string()];
    let review = Review::new("http://x", "k", "m", &users, "curl https://a.test", &ws);
    let policy = || Ok(Some("\n\n# The user's security policy".to_string()));
    let systems = |http: &FakeHttp| -> Vec<String> {
        http.sent()
            .iter()
            .map(|r| {
                let b: Value = serde_json::from_slice(r.body.as_deref().unwrap()).unwrap();
                b["messages"][0]["content"].as_str().unwrap().to_string()
            })
            .collect()
    };

    // Stricter: the doctrine's rating and reason.
    let http = FakeHttp::new([rated("low", "routine"), rated("high", "breaks rule 2")]);
    let got = review_with_doctrines(&review, policy(), &http).await;
    assert_eq!(got, Ok((Risk::High, "breaks rule 2".into())));
    assert_eq!(
        systems(&http),
        [
            REVIEW_SYSTEM.to_string(),
            format!("{REVIEW_SYSTEM}\n\n# The user's security policy")
        ]
    );

    // A doctrine that talked the reviewer down changes nothing.
    let http = FakeHttp::new([rated("high", "unknown host"), rated("low", "allowed")]);
    let got = review_with_doctrines(&review, policy(), &http).await;
    assert_eq!(got, Ok((Risk::High, "unknown host".into())));

    // Critical needs no second request; none without doctrines either.
    let http = FakeHttp::new([rated("critical", "a paste site")]);
    let got = review_with_doctrines(&review, policy(), &http).await;
    assert_eq!(got, Ok((Risk::Critical, "a paste site".into())));
    assert_eq!(http.sent().len(), 1);
    let http = FakeHttp::new([rated("low", "routine")]);
    let got = review_with_doctrines(&review, Ok(None), &http).await;
    assert_eq!(got, Ok((Risk::Low, "routine".into())));
    assert_eq!(http.sent().len(), 1);

    // A failure of either is the failure: the user is asked.
    let http = FakeHttp::new([rated("low", "routine"), ok("no verdict")]);
    let got = review_with_doctrines(&review, policy(), &http).await;
    assert_eq!(
        got,
        Err("review with your doctrines: reviewer reply had no verdict".into())
    );
    let http = FakeHttp::new([rated("low", "routine")]);
    let got = review_with_doctrines(&review, Err("no fence".into()), &http).await;
    assert_eq!(got, Err("the doctrines could not be read: no fence".into()));
}

// ------------------------------------------------- rpctl tests, ported

/// test_egress ChatEgressTests.test_reviewer_sees_the_network
#[tokio::test]
async fn reviewer_sees_the_network() {
    let none: Vec<String> = Vec::new();
    let http = FakeHttp::new([ok(r#"{"risk":"low","reason":"ok"}"#), ok("{}")]);
    let mut review = Review::new("http://x", "k", "m", &none, "pip install x", Path::new("."));
    review.network = Some("registries + a.test (from the command)");
    review.run(&http).await.unwrap();
    review.network = None;
    let _ = review.run(&http).await;
    let user = |i: usize| -> String {
        let body: Value = serde_json::from_slice(http.sent()[i].body.as_deref().unwrap()).unwrap();
        body["messages"][1]["content"].as_str().unwrap().to_string()
    };
    assert!(user(0).contains("<network>\nregistries + a.test (from the command)\n</network>"));
    assert!(user(1).contains("whole network"));
    assert!(REVIEW_SYSTEM.contains("<network>"));
}

fn system_of(http: &FakeHttp) -> String {
    let body: Value = serde_json::from_slice(http.sent()[0].body.as_deref().unwrap()).unwrap();
    body["messages"][0]["content"].as_str().unwrap().to_string()
}

/// test_sandboxgit ApproverTests.test_reviewer_hears_which_git_the_command_sees
#[tokio::test]
async fn reviewer_hears_which_git_the_command_sees() {
    let scratch = ScratchDir::new("review-git").unwrap();
    let hi = users(&["hi"]);
    let mut systems = Vec::new();
    for private_git in [false, true] {
        let http = FakeHttp::new([ok(r#"{"risk": "low", "reason": "ok"}"#)]);
        let mut review = Review::new("http://x", "k", "m", &hi, "git push", scratch.path());
        review.private_git = private_git;
        assert_eq!(review.run(&http).await, Ok((Risk::Low, "ok".to_string())));
        systems.push(system_of(&http));
    }
    assert!(!systems[0].contains("private"));
    assert!(systems[1].starts_with(REVIEW_SYSTEM));
    assert!(systems[1].contains("agent's own private repository"));
    assert!(systems[1].contains("third party"));
}

/// test_remote ReviewTests.test_server_review_describes_the_server_not_this_machine
#[tokio::test]
async fn server_review_describes_the_server_not_this_machine() {
    let scratch = ScratchDir::new("review-remote").unwrap();
    std::fs::write(scratch.path().join("run.sh"), "echo LOCAL-FILE").unwrap();
    let http = FakeHttp::new([ok(r#"{"risk":"low","reason":"ok"}"#)]);
    let asks = users(&["restart it"]);
    let mut review = Review::new("http://x", "k", "m", &asks, "sh run.sh", scratch.path());
    review.server = Some("hetzner (user@203.0.113.7), sandboxed");
    assert_eq!(review.run(&http).await, Ok((Risk::Low, "ok".to_string())));
    let body: Value = serde_json::from_slice(http.sent()[0].body.as_deref().unwrap()).unwrap();
    let system = body["messages"][0]["content"].as_str().unwrap();
    let user = body["messages"][1]["content"].as_str().unwrap();
    assert_eq!(system, REVIEW_REMOTE_SYSTEM);
    let rules = REVIEW_SYSTEM.split_once("\n\n").unwrap().1;
    assert!(system.ends_with(rules));
    assert!(user.contains("<server>\nhetzner (user@203.0.113.7), sandboxed\n</server>"));
    assert!(!user.contains("<network>"));
    assert!(!user.contains("LOCAL-FILE"));
}

/// test_features ReviewFilesTests.test_long_inline_program_is_not_a_file
#[test]
fn long_inline_program_is_not_a_file() {
    let scratch = ScratchDir::new("review-files").unwrap();
    for cmd in [
        format!("python3 -c '{}'", "x".repeat(300)),
        format!("curl -s x | python3 -c 'import json\n{}'", "y".repeat(300)),
        format!("cat {}", "z".repeat(300)),
    ] {
        assert_eq!(files_for_review(&cmd, scratch.path()), Ok(Vec::new()));
    }
}

/// test_features ReviewFilesTests.test_named_script_is_still_read
#[test]
fn named_script_is_still_read() {
    let scratch = ScratchDir::new("review-files").unwrap();
    std::fs::write(scratch.path().join("fetch.py"), "print(1)").unwrap();
    let cmd = format!("python3 fetch.py '{}'", "x".repeat(300));
    assert_eq!(
        files_for_review(&cmd, scratch.path()),
        Ok(vec![("fetch.py".to_string(), "print(1)".to_string())])
    );
}

/// test_features ReviewFilesTests.test_review_never_raises: rpctl patches
/// files_for_review to raise; viy's lookup cannot panic, so the failures
/// it can meet are what this checks: a name rpctl's lookup raises on, and
/// a server that does not answer.
#[tokio::test]
async fn review_never_raises() {
    let none: Vec<String> = Vec::new();
    let http = FakeHttp::new([]);
    let review = Review::new("http://x", "k", "m", &none, "cat 'a\0b'", Path::new("."));
    assert_eq!(
        review.run(&http).await,
        Err("review failed (ValueError)".to_string())
    );
    let http = FakeHttp::new([Reply::Fail(HttpError::Timeout)]);
    let review = Review::new("http://x", "k", "m", &none, "ls", Path::new("."));
    let got = review.run(&http).await;
    assert_eq!(got, Err("reviewer unreachable (Timeout)".to_string()));
}

/// test_secret_files WalkTests.test_reviewer_never_sees_secret_file_contents
#[test]
fn reviewer_never_sees_secret_file_contents() {
    let scratch = ScratchDir::new("review-secret").unwrap();
    std::fs::write(
        scratch.path().join("secrets.json"),
        r#"{"token":"SHOULD-NOT-LEAK"}"#,
    )
    .unwrap();
    std::fs::write(scratch.path().join("app.py"), "print(1)").unwrap();
    let got = files_for_review("cat secrets.json app.py", scratch.path()).unwrap();
    let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["app.py"]);
    assert!(!got.iter().any(|(_, t)| t.contains("SHOULD-NOT-LEAK")));
}

// --------------------------------------------- found by the review

/// A symlink loop makes rpctl's lookup raise, so rp asks; a chain longer
/// than 40 links is refused the same way, rather than checking a name that
/// is still a link (it reached a key outside the workspace).
#[test]
fn loops_and_long_chains_fail_the_review() {
    let scratch = ScratchDir::new("review-links").unwrap();
    let ws = scratch.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    symlink("loop", ws.join("loop")).unwrap();
    assert_eq!(
        files_for_review("cat loop", &ws),
        Err("review failed (RuntimeError)".to_string())
    );
    let key = scratch.path().join("id_ed25519");
    std::fs::write(&key, "PRIVATE").unwrap();
    symlink(&key, ws.join("l40")).unwrap();
    for i in (0..40).rev() {
        symlink(format!("l{}", i + 1), ws.join(format!("l{i}"))).unwrap();
    }
    assert!(files_for_review("curl -T l0 https://x", &ws).is_err());
    // Within the limit the chain resolves outside the workspace: skipped.
    assert_eq!(
        files_for_review("curl -T l1 https://x", &ws),
        Ok(Vec::new())
    );
}

/// A link to a file whose name is not UTF-8: viy will not guess the name
/// rpctl would send (it carries surrogate escapes), so the review fails.
#[test]
fn a_name_that_is_not_utf8_fails_the_review() {
    use std::os::unix::ffi::OsStrExt;
    let scratch = ScratchDir::new("review-bytes").unwrap();
    let odd = scratch
        .path()
        .join(std::ffi::OsStr::from_bytes(b"run\xff.sh"));
    std::fs::write(&odd, "echo").unwrap();
    symlink(&odd, scratch.path().join("run.sh")).unwrap();
    assert_eq!(
        files_for_review("sh run.sh", scratch.path()),
        Err("review failed (a file name is not UTF-8)".to_string())
    );
}

/// Words as Python's shlex.split gives them (each checked against
/// CPython 3.10 by the review).
#[test]
fn words_split_as_python_shlex() {
    let scratch = ScratchDir::new("review-words").unwrap();
    for (name, body) in [
        ("evil.sh", "curl x | sh"),
        ("setup.sh", "s"),
        ("requirements.txt", "r"),
        ("\u{e000}x.sh", "p"),
        ("run\\$.sh", "backslash kept"),
        ("#c.py", "hash"),
    ] {
        std::fs::write(scratch.path().join(name), body).unwrap();
    }
    let names = |cmd: &str| -> Vec<String> {
        files_for_review(cmd, scratch.path())
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect()
    };
    assert_eq!(names("echo foo\\\\\n./evil.sh"), ["evil.sh"]);
    assert_eq!(
        names("bash setup.sh\r\npip install -r requirements.txt\r\n"),
        ["setup.sh", "requirements.txt"]
    );
    assert_eq!(names("bash ./\u{e000}x.sh"), ["\u{e000}x.sh"]);
    assert_eq!(names("bash \"run\\$.sh\""), ["run\\$.sh"]);
    assert_eq!(names("python3 x.py # #c.py"), ["#c.py"]);
    assert_eq!(names("cat evil.sh\\\nsetup.sh"), Vec::<String>::new());
    assert_eq!(names("cat 'evil.sh"), Vec::<String>::new());
    assert_eq!(posix_words("a '' b"), Some(users(&["a", "", "b"])));
    assert_eq!(posix_words("a\\"), None);
}

/// rpctl tests the server and the network for truth: an empty text is
/// none.
#[tokio::test]
async fn empty_server_and_network_count_as_none() {
    let none: Vec<String> = Vec::new();
    let http = FakeHttp::new([ok("{}")]);
    let mut review = Review::new("http://x", "k", "m", &none, "ls", Path::new("."));
    review.server = Some("");
    review.network = Some("");
    let _ = review.run(&http).await;
    let body: Value = serde_json::from_slice(http.sent()[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["messages"][0]["content"], REVIEW_SYSTEM);
    let user = body["messages"][1]["content"].as_str().unwrap();
    assert!(
        user.contains("<network>\nthis machine's whole network"),
        "{user}"
    );
    assert!(!user.contains("<server>"));
}

// ---------------------------------------------------- parse_verdict

#[test]
fn verdicts() {
    let cases: [(&str, Verdict); 9] = [
        (
            r#"{"risk": "low", "reason": "fine"}"#,
            Ok((Risk::Low, "fine".into())),
        ),
        (
            "<think>{\"risk\": \"low\"}</think> {\"risk\": \" HIGH\\n\", \"reason\": \" a \\t b \"}",
            Ok((Risk::High, "a b".into())),
        ),
        (
            r#"{"risk": "medium", "reason": ""}"#,
            Ok((Risk::Medium, "(no reason given)".into())),
        ),
        ("no braces", Err("reviewer reply had no verdict".into())),
        ("} then {", Err("reviewer reply had no verdict".into())),
        ("{risk}", Err("reviewer verdict was not valid JSON".into())),
        (
            r#"{"risk": "extreme"}"#,
            Err("reviewer gave an unknown risk level \"extreme\"".into()),
        ),
        (
            r#"{"risk": 1}"#,
            Err("reviewer gave an unknown risk level \"1\"".into()),
        ),
        (
            "<think>unclosed {\"risk\": \"critical\", \"reason\": \"r\"}",
            Ok((Risk::Critical, "r".into())),
        ),
    ];
    for (text, want) in cases {
        assert_eq!(parse_verdict(text), want, "{text:?}");
    }
    let long = format!(r#"{{"risk": "low", "reason": "{}"}}"#, "é".repeat(400));
    assert_eq!(parse_verdict(&long).unwrap().1.chars().count(), 300);
}

// ------------------------------------------------------------ the oracle

/// rpctl's parse_verdict, secret_name_reason and files_for_review against
/// viy's on generated input (`cargo test -p viy-tools --features oracle`).
#[cfg(feature = "oracle")]
mod oracle {
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::fuzz::cases;
    use viy_testkit::fuzz::seed;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;
    use viy_testkit::scratch::ScratchDir;

    use super::super::*;
    use crate::secrets::secret_name_reason;

    /// The reason rpctl would read: a string, or none.
    fn reason_is_text(text: &str) -> bool {
        let text = without_think(text);
        let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) else {
            return true;
        };
        if b < a {
            return true;
        }
        match serde_json::from_str::<Value>(&text[a..=b]) {
            Ok(v) => matches!(v.get("reason"), None | Some(Value::String(_))),
            Err(_) => true,
        }
    }

    #[test]
    fn parse_verdict_matches_rpctl() {
        let mut oracle = Oracle::spawn().unwrap();
        let mut g = Gen::from_env();
        let pieces = [
            "{",
            "}",
            "{",
            "}",
            "\"risk\"",
            "\"risk\"",
            ":",
            ": ",
            "\"low\"",
            "\" HIGH \"",
            "\"critical\"",
            "\"Medium\\n\"",
            "\"reason\"",
            "\"reason\"",
            "\"a  b\\tc\"",
            "\"\"",
            ",",
            ", ",
            "<think>",
            "</think>",
            " ",
            "\n",
            "1",
            "null",
            "true",
            "[",
            "]",
            "\"\\u00e9\"",
            "\"x\"",
            "NaN",
            "\"lo\\u0077\"",
            "\"\\u2003low\"",
            "\"\\u001clow\"",
            "text ",
        ];
        let mut stricter = 0;
        for case in 0..cases(20000) {
            let text = g.string(&pieces, 12);
            let theirs = match oracle
                .call("approve.parse_verdict", json!([text]), json!({}))
                .unwrap()
                .outcome
            {
                Outcome::Returned(v) => v["$tuple"].clone(),
                other => panic!("{text:?}: {other:?}"),
            };
            let ours = parse_verdict(&text);
            let at = format!("case {case}, seed {:#x}: {text:?}", seed());
            match (&ours, theirs[0].as_str()) {
                (Ok((risk, reason)), Some(their_risk)) => {
                    assert_eq!(risk.as_str(), their_risk, "{at}");
                    if reason_is_text(&text) {
                        assert_eq!(Some(reason.as_str()), theirs[1].as_str(), "{at}");
                    }
                }
                (Ok(_), None) => panic!("{at}: viy runs on {ours:?}, rpctl says {theirs}"),
                (Err(_), Some(_)) => {
                    // Only JSON Python reads and serde does not (NaN).
                    assert!(text.contains("NaN"), "{at}: viy {ours:?}, rpctl {theirs}");
                    stricter += 1;
                }
                (Err(why), None) => {
                    let theirs = theirs[1].as_str().unwrap();
                    let unknown = "reviewer gave an unknown risk level";
                    // NaN: serde refuses what Python's json reads (F50).
                    let worded = why.starts_with(unknown) && theirs.starts_with(unknown);
                    let nan = text.contains("NaN") && why.ends_with("not valid JSON");
                    if !(worded || nan) {
                        assert_eq!(why, theirs, "{at}");
                    }
                }
            }
        }
        eprintln!("parse_verdict: {stricter} replies only Python's JSON reads");
    }

    #[test]
    fn secret_names_match_rpctl() {
        let mut oracle = Oracle::spawn().unwrap();
        let mut g = Gen::from_env();
        let pieces = [
            "secret",
            "secrets",
            ".env",
            ".",
            ".",
            "json",
            "yml",
            "yaml",
            "pub",
            "-",
            "_",
            "id_rsa",
            "PEM",
            ".key",
            "x",
            "\n",
            "SECRETS",
            "example",
            "credentials",
            ".netrc",
            "é",
            "İ",
        ];
        for case in 0..cases(20000) {
            let name = g.string(&pieces, 5);
            let theirs = oracle
                .call("sandbox.secret_name_reason", json!([name]), json!({}))
                .unwrap()
                .outcome;
            let Outcome::Returned(theirs) = theirs else {
                panic!("{name:?}: {theirs:?}");
            };
            assert_eq!(
                secret_name_reason(&name),
                theirs.as_str(),
                "case {case}, seed {:#x}: {name:?}",
                seed()
            );
        }
    }

    #[test]
    fn files_for_review_matches_rpctl() {
        let mut oracle = Oracle::spawn().unwrap();
        let mut g = Gen::from_env();
        let scratch = ScratchDir::new("review-oracle").unwrap();
        let ws = scratch.path();
        std::fs::create_dir(ws.join("sub")).unwrap();
        for (name, body) in [
            ("a.py", "print(1)\n"),
            ("secrets.json", "{}"),
            ("sub/b.txt", "b\r\nb"),
            ("long.txt", &"l".repeat(4500)),
            ("with space", "s"),
            ("#c.py", "hash"),
            ("run\\$.sh", "b"),
            ("\u{e000}", "p"),
        ] {
            std::fs::write(ws.join(name), body).unwrap();
        }
        std::os::unix::fs::symlink("a.py", ws.join("ln")).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", ws.join("out")).unwrap();
        let pieces = [
            "a.py",
            "secrets.json",
            "sub/b.txt",
            "long.txt",
            "ln",
            "out",
            "'with space'",
            "\r",
            "\\\\",
            "\"run\\$.sh\"",
            "\u{e000}",
            "\\\n",
            "\\$",
            "with",
            "space",
            "sub/../a.py",
            "./a.py",
            "/etc/hostname",
            "--x=",
            "-c",
            "-",
            " ",
            " ",
            " ",
            "'",
            "\"",
            "\n",
            "#c.py",
            "\\",
            "$X",
            "missing",
            "..",
            "=",
        ];
        for case in 0..cases(4000) {
            let cmd = g.string(&pieces, 8);
            let theirs = oracle
                .call(
                    "approve.files_for_review",
                    json!([cmd, ws.to_str().unwrap()]),
                    json!({}),
                )
                .unwrap()
                .outcome;
            let Outcome::Returned(theirs) = theirs else {
                panic!("{cmd:?}: {theirs:?}");
            };
            let theirs: Vec<(String, String)> = theirs
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    let t = &t["$tuple"];
                    (t[0].as_str().unwrap().into(), t[1].as_str().unwrap().into())
                })
                .collect();
            let ours = files_for_review(&cmd, ws).unwrap();
            assert_eq!(ours, theirs, "case {case}, seed {:#x}: {cmd:?}", seed());
        }
    }
}

// ------------------------------------------------------- the scenarios

/// The reviewer requests rp sent in the two reviewer scenarios
/// (`tests/scenarios/network-*`), rebuilt by viy from the same inputs: the
/// user's line, the command, the network the egress policy described (an
/// input here; the policy is WP 4.8) and the workspace. The whole turn is
/// replayed once the tool loop exists (WP 4.10).
#[tokio::test]
async fn reviewer_requests_equal_rps_in_the_scenarios() {
    for (name, want) in [
        (
            "network-command-refused-by-review",
            Ok((
                Risk::Critical,
                "uploads notes.txt to a paste service".to_string(),
            )),
        ),
        (
            "network-review-fails-user-declines",
            Err("reviewer returned HTTP 503".to_string()),
        ),
    ] {
        let scenario = viy_testkit::scenario::load(name).unwrap();
        let log = viy_testkit::scenario::rp_log(name).unwrap();
        let reviews: Vec<Value> = log
            .chat_bodies()
            .into_iter()
            .filter(|b| b["max_tokens"] == 400)
            .collect();
        let scratch = ScratchDir::new("review-scenario").unwrap();
        build(scratch.path(), &scenario["workspace"]);
        let user: Vec<String> = vec![scenario["steps"][0]["user"].as_str().unwrap().into()];
        let call = scenario["responses"][1]["stream"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|e| e["tool_call"]["arguments"].as_str())
            .unwrap();
        let args: Value = serde_json::from_str(call).unwrap();
        let command = args["command"].as_str().unwrap();
        let first = reviews[0]["messages"][1]["content"].as_str().unwrap();
        let network = first
            .split_once("<network>\n")
            .and_then(|(_, rest)| rest.split_once("\n</network>"))
            .map(|(n, _)| n)
            .unwrap();
        // The replies the scenario gives the reviewer's requests.
        let replies = scenario["responses"]
            .as_array()
            .unwrap()
            .iter()
            .skip(2)
            .take(reviews.len())
            .map(|r| match r["message"].as_str() {
                Some(text) => ok(text),
                None => Reply::answer(
                    u16::try_from(r["status"].as_u64().unwrap()).unwrap(),
                    r["body"].as_str().unwrap(),
                ),
            });
        let http = FakeHttp::new(replies);
        let mut review = Review::new(
            "http://127.0.0.1:1",
            "fake-key",
            scenario["model"].as_str().unwrap_or("qwen3.8-abliterated"),
            &user,
            command,
            scratch.path(),
        );
        review.network = Some(network);
        assert_eq!(review.run(&http).await, want, "{name}");
        let sent: Vec<Value> = http
            .sent()
            .iter()
            .map(|r| serde_json::from_slice(r.body.as_deref().unwrap()).unwrap())
            .collect();
        assert_eq!(sent, reviews, "{name}");
    }
}
