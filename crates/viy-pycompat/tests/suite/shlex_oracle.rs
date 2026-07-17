//! shlex against CPython 3.10.12 through the oracle: split, the lexers of
//! approve.py and egress.py, quote and join, on generated commands. Zero
//! differences is the gate (S2, F06: 100,000 inputs).

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_pycompat::shlex;
use viy_testkit::fuzz;
use viy_testkit::fuzz::Gen;
use viy_testkit::fuzz::SHELL_PIECES;
use viy_testkit::oracle::Oracle;
use viy_testkit::oracle::Outcome;

const CASES: usize = 3000;
const MAX_PIECES: usize = 12;

/// Python's answer as Rust's: the words, or the ValueError's message.
fn python(oracle: &mut Oracle, func: &str, args: Value) -> Result<Vec<String>, String> {
    match oracle.call(func, args, json!({})).unwrap().outcome {
        Outcome::Returned(v) => Ok(v
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w.as_str().unwrap().to_string())
            .collect()),
        Outcome::Raised { kind, message } => {
            assert_eq!(kind, "ValueError", "{func}: {message}");
            Err(message)
        }
    }
}

fn rust(r: Result<Vec<String>, shlex::ShlexError>) -> Result<Vec<String>, String> {
    r.map_err(|e| e.to_string())
}

#[test]
fn lexers_equal_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    let cases = fuzz::cases(CASES);
    let mut parsed = 0;
    for _ in 0..cases {
        let s = g.string(SHELL_PIECES, MAX_PIECES);
        let ctx = format!("seed {seed:#x}, input {s:?}");
        let want = python(&mut oracle, "shlex.split", json!([s]));
        parsed += usize::from(want.is_ok());
        assert_eq!(rust(shlex::split(&s)), want, "split, {ctx}");
        // approve.plain_pipeline's lexer
        let want = python(&mut oracle, "oracle.shlex_tokens", json!([s, "&|;", ""]));
        assert_eq!(rust(shlex::tokens(&s, "&|;", "")), want, "approve, {ctx}");
        // egress.hosts_in_command's lexer: punctuation_chars=True, comments on
        let want = python(&mut oracle, "oracle.shlex_tokens", json!([s, true, "#"]));
        assert_eq!(
            rust(shlex::tokens(&s, shlex::DEFAULT_PUNCTUATION, "#")),
            want,
            "egress, {ctx}"
        );
    }
    // Not vacuous: unbalanced quotes must not make most inputs errors.
    assert!(parsed * 3 >= cases, "{parsed} of {cases} parsed");
    eprintln!("shlex: {parsed} of {cases} inputs parsed");
}

#[test]
fn quote_and_join_equal_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    for _ in 0..fuzz::cases(CASES) {
        let words: Vec<String> = (0..g.below(4)).map(|_| g.string(SHELL_PIECES, 4)).collect();
        let ctx = format!("seed {seed:#x}, words {words:?}");
        for w in &words {
            let answer = oracle.call("shlex.quote", json!([w]), json!({})).unwrap();
            assert_eq!(
                answer.outcome,
                Outcome::Returned(json!(shlex::quote(w))),
                "quote, {ctx}"
            );
        }
        let answer = oracle
            .call("shlex.join", json!([words]), json!({}))
            .unwrap();
        assert_eq!(
            answer.outcome,
            Outcome::Returned(json!(shlex::join(&words))),
            "join, {ctx}"
        );
    }
}
