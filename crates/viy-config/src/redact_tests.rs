use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

/// Tokens are built at run time: written out, they would trip the secret
/// scan on commit.
fn token(prefix: &str, c: &str, n: usize) -> String {
    format!("{prefix}{}", c.repeat(n))
}

#[test]
fn known_formats() {
    let r = Redactor::default();
    for tok in [
        token("hf_", "a", 34),
        token("sk-", "B", 40),
        token("npm_", "c", 36),
        token("ghp_", "d", 36),
        token("rpa_", "E", 40),
        token("apify_api_", "f", 36),
        token("tvly-dev-", "g", 24),
        token("AKIA", "H", 16),
    ] {
        assert_eq!(
            r.text(&format!("key {tok} here")),
            "key [redacted] here",
            "{tok}"
        );
    }
}

#[test]
fn labelled_header_keeps_label() {
    let out = Redactor::default().text("Authorization: Bearer abcdefghijklmnopqrstuvwxyz");
    assert_eq!(out, "Authorization: Bearer [redacted]");
}

#[test]
fn literal_values_from_secret_names_only() {
    let cfg = [
        ("RUNPOD_API_KEY", "weird-token-1234567"),
        ("VLLM_PORT", "8000"),
        ("MODEL_REPO", "huihui-ai/some-long-model-name"),
    ];
    let env = [("MY_SECRET", "another-value-xyz"), ("SHORT_TOKEN", "abc")];
    let lits = literal_secrets(cfg.into_iter().chain(env));
    assert_eq!(lits, ["weird-token-1234567", "another-value-xyz"]);
}

#[test]
fn ordinary_text_and_images_untouched() {
    let r = Redactor::new(["literal-secret-value".to_string()]);
    let img = format!("data:image/png;base64,{}", token("sk-", "A", 50));
    assert_eq!(r.text(&img), img);
    assert_eq!(
        r.text("ask the model about sk-learn"),
        "ask the model about sk-learn"
    );
    assert_eq!(
        r.value(&json!({"a": ["x literal-secret-value", 3]})),
        json!({"a": ["x [redacted]", 3]})
    );
}

/// Not in rpctl's tests: a token right after a letter rpctl's Unicode `\b`
/// counts as a word character is masked too (kb d042), and one inside a
/// longer ASCII word still is not.
#[test]
fn a_token_next_to_a_non_ascii_letter_is_masked() {
    let r = Redactor::default();
    let hf = token("hf_", "a", 34);
    assert_eq!(r.text(&format!("é{hf}")), "é[redacted]");
    assert_eq!(r.text(&format!("x{hf}")), format!("x{hf}"));
    let aws = token("AKIA", "H", 16);
    assert_eq!(r.text(&format!("{aws}²")), "[redacted]²");
    assert_eq!(r.text(&format!("{aws}9")), format!("{aws}9"));
}

#[test]
fn longer_literal_masked_whole_and_short_or_unnamed_skipped() {
    let lits = literal_secrets([
        ("A_TOKEN", "  abcdefghijkl  "),
        ("B_TOKEN", "abcdefghijkl-and-more"),
        ("PASSWORD_HINT", "not a secret name at the end"),
        ("db_password", "lowercase-names-count"),
        ("GH_PAT", "ascii-only-value"),
        ("EMPTY_KEY", ""),
    ]);
    assert_eq!(
        lits,
        [
            "abcdefghijkl-and-more",
            "lowercase-names-count",
            "ascii-only-value",
            "abcdefghijkl",
        ]
    );
    let r = Redactor::new(lits);
    assert_eq!(r.text("x abcdefghijkl-and-more y"), "x [redacted] y");
}

/// rpctl's answers for generated text, literals and names
/// (`cargo test -p viy-config --features oracle`).
///
/// - The token formats, which netguard refuses to send out, match wherever
///   rpctl's do (the ASCII boundaries can only add matches, kb d042).
/// - Masking equals rpctl's for ASCII text without U+001C-U+001F; other text
///   differs where Python's `re` and the regex crate do (kb d042), and the
///   cases are counted.
/// - Secret names equal rpctl's but for the two kinds Python's `re` alone
///   accepts: a dotless or dotted i for `I` (`CREDENTıAL`) and a newline
///   after the name.
#[cfg(feature = "oracle")]
mod oracle {
    use serde_json::Value;
    use serde_json::json;
    use viy_testkit::fuzz;
    use viy_testkit::fuzz::Gen;
    use viy_testkit::oracle::Oracle;
    use viy_testkit::oracle::Outcome;

    use super::super::*;

    const PIECES: &[&str] = &[
        "sk-",
        "hf_",
        "npm_",
        "ghp_",
        "gho_",
        "ghx_",
        "github_pat_",
        "rpa_",
        "apify_api_",
        "tvly-",
        "xai-",
        "cio",
        "AKIA",
        "xoxb-",
        "xoxz-",
        "eyJ",
        ".",
        // Split so the secret scan does not take them for key blocks.
        concat!("-----BEGIN RSA PRIVATE ", "KEY-----"),
        concat!("-----END RSA PRIVATE ", "KEY-----"),
        concat!("-----BEGIN PRIVATE ", "KEY-----"),
        "Authorization: Bearer ",
        "authorization:bearer\t",
        "X-API-KEY:",
        "x-api-key: ",
        "Bearer ",
        "abcdefghij",
        "ABCDEFGHIJ",
        "0123456789",
        "Zz9",
        "a",
        "_",
        "-",
        "=",
        "==",
        "/",
        "+",
        "~",
        ":",
        " ",
        "\n",
        "\t",
        "é",
        "²",
        "\u{301}",
        "\u{a0}",
        "\u{1c}",
        "\u{1f}",
        "ı",
        "İ",
        "\u{212a}",
        "ſ",
        "日本",
        "data:",
        "literal-secret",
        "-value-",
        "LONGER-literal-secret-value",
    ];
    const LITERALS: &[&str] = &[
        "literal-secret-value",
        "LONGER-literal-secret-value",
        "abcdefghijabcdefghij",
        "secret-value-",
        "",
    ];
    const NAME_PIECES: &[&str] = &[
        "KEY",
        "key",
        "TOKEN",
        "Token",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "_PAT",
        "PAT",
        "CREDENTIAL",
        "CREDENTIALS",
        "credentialS",
        "RUNPOD_API",
        "_",
        "MY",
        "X",
        "S",
        "\n",
        " ",
        "\u{212a}EY",
        "ſECRET",
        "CREDENTıAL",
        "CREDENTİALS",
        "é",
    ];

    fn call(oracle: &mut Oracle, func: &str, args: Value) -> Value {
        match oracle.call(func, args, json!({})).unwrap().outcome {
            Outcome::Returned(value) => value,
            Outcome::Raised { kind, message } => panic!("rpctl raised {kind}: {message}"),
        }
    }

    fn python_only_name(name: &str) -> bool {
        name.ends_with('\n') || name.contains(['ı', 'İ'])
    }

    #[test]
    fn token_formats_match_wherever_rpctls_do() {
        let seed = fuzz::seed();
        let mut g = Gen::new(seed);
        let mut oracle = Oracle::spawn().unwrap();
        let (mut both, mut viy_only) = (0, 0);
        for _ in 0..fuzz::cases(3000) {
            let text = g.string(PIECES, 14);
            let theirs = call(&mut oracle, "redact.any_pattern", json!([text]));
            let ours = PATTERNS.iter().any(|p| p.is_match(&text));
            match (theirs.as_bool().unwrap(), ours) {
                (true, false) => {
                    panic!("seed {seed:#x}: rpctl finds a token in {text:?}, viy none")
                }
                (false, true) => viy_only += 1,
                (true, true) => both += 1,
                (false, false) => {}
            }
        }
        eprintln!("redact.any_pattern: {both} found by both, {viy_only} by viy only");
    }

    #[test]
    fn masking_equals_rpctls_on_ascii_text() {
        let seed = fuzz::seed();
        let mut g = Gen::new(seed);
        let mut oracle = Oracle::spawn().unwrap();
        let mut other = (0, Vec::new());
        for _ in 0..fuzz::cases(3000) {
            let text = g.string(PIECES, 14);
            let literals: Vec<String> = (0..g.below(3))
                .map(|_| g.pick(LITERALS).to_string())
                .collect();
            let theirs = call(&mut oracle, "redact.text", json!([text, literals]));
            let ours = Redactor::new(literals.clone()).text(&text);
            if text
                .chars()
                .all(|c| c.is_ascii() && !('\u{1c}'..='\u{1f}').contains(&c))
            {
                assert_eq!(
                    json!(ours),
                    theirs,
                    "seed {seed:#x}: {text:?} with {literals:?}"
                );
            } else {
                other.0 += 1;
                if json!(ours) != theirs {
                    other.1.push((text, ours, theirs));
                }
            }
        }
        eprintln!(
            "redact.text: of {} other texts, {} masked differently; the first few:",
            other.0,
            other.1.len()
        );
        for (text, ours, theirs) in other.1.iter().take(5) {
            eprintln!("  {text:?}\n    viy   {ours:?}\n    rpctl {theirs}");
        }
    }

    #[test]
    fn secret_names_as_rpctl() {
        let seed = fuzz::seed();
        let mut g = Gen::new(seed);
        let mut oracle = Oracle::spawn().unwrap();
        let mut python_only = 0;
        for _ in 0..fuzz::cases(3000) {
            let name = g.string(NAME_PIECES, 4);
            let theirs = call(&mut oracle, "redact.is_secret_name", json!([name]));
            let ours = is_secret_name(&name);
            if theirs == json!(ours) {
                continue;
            }
            assert!(
                theirs == json!(true) && python_only_name(&name),
                "seed {seed:#x}: {name:?}: viy {ours}, rpctl {theirs}"
            );
            python_only += 1;
        }
        eprintln!("redact.is_secret_name: {python_only} names only Python's re accepts");
    }

    #[test]
    fn literal_secrets_as_rpctl() {
        let seed = fuzz::seed();
        let mut g = Gen::new(seed);
        let mut oracle = Oracle::spawn().unwrap();
        let values = [
            "short",
            "exactly-12ch",
            "  padded-value-here  ",
            "padded-value-here",
            "日本語の秘密の値です、十二",
            "a-much-longer-secret-value",
            "",
        ];
        for _ in 0..fuzz::cases(1000) {
            let cfg: serde_json::Map<String, Value> = (0..g.below(5))
                .map(|_| (g.string(NAME_PIECES, 3), json!(g.pick(&values))))
                .filter(|(name, _)| !python_only_name(name))
                .collect();
            let theirs = call(&mut oracle, "redact.literal_secrets", json!([cfg, {}]));
            let mut theirs: Vec<String> = serde_json::from_value(theirs).unwrap();
            let pairs = cfg.iter().map(|(k, v)| (k.as_str(), v.as_str().unwrap()));
            let mut ours = literal_secrets(pairs);
            // rpctl orders equal lengths as a set iterates; compare those as
            // sets.
            let lens = |v: &[String]| v.iter().map(|s| s.chars().count()).collect::<Vec<_>>();
            assert_eq!(lens(&ours), lens(&theirs), "seed {seed:#x}: {cfg:?}");
            ours.sort();
            theirs.sort();
            assert_eq!(ours, theirs, "seed {seed:#x}: {cfg:?}");
        }
    }
}
