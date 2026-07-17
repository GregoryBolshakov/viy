//! pyjson against CPython 3.10.12's json.dumps through the oracle, on
//! generated values and options. Zero differences is the gate (S1, F01).

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_pycompat::pyjson;
use viy_pycompat::pyjson::Dumps;
use viy_testkit::fuzz;
use viy_testkit::fuzz::Gen;
use viy_testkit::oracle::Oracle;
use viy_testkit::oracle::Outcome;

const CASES: usize = 2000;

#[test]
fn dumps_equals_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    for _ in 0..fuzz::cases(CASES) {
        let v = fuzz::json_value(&mut g, 3);
        let opts = Dumps {
            ensure_ascii: g.below(2) == 0,
            indent: *g.pick(&[None, Some(0), Some(1), Some(2), Some(4)]),
            sort_keys: g.below(3) == 0,
        };
        let kwargs = json!({
            "ensure_ascii": opts.ensure_ascii,
            "indent": opts.indent,
            "sort_keys": opts.sort_keys,
        });
        let answer = oracle.call("json.dumps", json!([v]), kwargs).unwrap();
        let Outcome::Returned(Value::String(want)) = answer.outcome else {
            panic!("seed {seed:#x}: json.dumps({v}) gave {:?}", answer.outcome);
        };
        assert_eq!(
            pyjson::dumps_with(&v, opts),
            want,
            "seed {seed:#x}, {opts:?}, value {v}"
        );
    }
}

#[test]
fn float_repr_equals_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    for _ in 0..fuzz::cases(CASES * 5) {
        let x = fuzz::float(&mut g);
        let answer = oracle.call("repr", json!([x]), json!({})).unwrap();
        let Outcome::Returned(Value::String(want)) = answer.outcome else {
            panic!("seed {seed:#x}: repr({x:e}) gave {:?}", answer.outcome);
        };
        assert_eq!(pyjson::float_repr(x), want, "seed {seed:#x}, {x:e}");
    }
}
