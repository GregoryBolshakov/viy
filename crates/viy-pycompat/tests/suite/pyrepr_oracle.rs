//! pyrepr against CPython 3.10.12's repr() through the oracle. Zero
//! differences is the gate (F22).

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_pycompat::pyrepr;
use viy_testkit::fuzz;
use viy_testkit::fuzz::Gen;
use viy_testkit::fuzz::PY_STR_PIECES;
use viy_testkit::fuzz::REPR_PIECES;
use viy_testkit::oracle::Oracle;
use viy_testkit::oracle::Outcome;

const CASES: usize = 3000;

#[test]
fn repr_str_equals_python() {
    let pieces: Vec<&str> = PY_STR_PIECES.iter().chain(REPR_PIECES).copied().collect();
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    for _ in 0..fuzz::cases(CASES) {
        let s = g.string(&pieces, 8);
        let answer = oracle.call("repr", json!([s]), json!({})).unwrap();
        assert_eq!(
            answer.outcome,
            Outcome::Returned(json!(pyrepr::repr_str(&s))),
            "seed {seed:#x}, input {s:?}"
        );
    }
}

#[test]
fn repr_value_equals_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    for _ in 0..fuzz::cases(CASES) {
        let v = fuzz::json_value(&mut g, 3);
        let answer = oracle.call("repr", json!([v]), json!({})).unwrap();
        assert_eq!(
            answer.outcome,
            Outcome::Returned(json!(pyrepr::repr_value(&v))),
            "seed {seed:#x}, value {v}"
        );
        let answer = oracle.call("str", json!([v]), json!({})).unwrap();
        assert_eq!(
            answer.outcome,
            Outcome::Returned(json!(pyrepr::str_value(&v))),
            "str, seed {seed:#x}, value {v}"
        );
    }
}
