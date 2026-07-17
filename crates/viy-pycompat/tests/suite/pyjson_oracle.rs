//! pyjson against CPython 3.10.12's json.dumps through the oracle, on
//! generated values and options. Zero differences is the gate (S1, F01).

use pretty_assertions::assert_eq;
use serde_json::Map;
use serde_json::Number;
use serde_json::Value;
use serde_json::json;
use viy_pycompat::pyjson;
use viy_pycompat::pyjson::Dumps;
use viy_testkit::fuzz;
use viy_testkit::fuzz::Gen;
use viy_testkit::fuzz::PY_STR_PIECES;
use viy_testkit::oracle::Oracle;
use viy_testkit::oracle::Outcome;

const CASES: usize = 2000;

const FLOATS: &[f64] = &[
    0.0,
    -0.0,
    0.1,
    1e15,
    1e16,
    9_999_999_999_999_998.0,
    1e-4,
    1e-5,
    5e-324,
    f64::MAX,
    f64::MIN_POSITIVE,
    123_456.789,
    1e22,
    1e21,
    -2.5e-7,
];

fn float(g: &mut Gen) -> f64 {
    if g.below(2) == 0 {
        return *g.pick(FLOATS);
    }
    loop {
        let x = f64::from_bits(g.next_u64());
        if x.is_finite() {
            return x;
        }
    }
}

fn value(g: &mut Gen, depth: usize) -> Value {
    let kinds = if depth == 0 { 6 } else { 8 };
    match g.below(kinds) {
        0 => Value::Null,
        1 => Value::Bool(g.below(2) == 0),
        2 => json!(*g.pick(&[0i64, 1, -1, 42, i64::MIN, i64::MAX, 1 << 53])),
        3 => json!(*g.pick(&[u64::MAX, 1u64 << 63, 12_345_678_901_234_567_890])),
        4 => Number::from_f64(float(g)).map_or(Value::Null, Value::Number),
        5 => Value::String(g.string(PY_STR_PIECES, 6)),
        6 => Value::Array((0..g.below(4)).map(|_| value(g, depth - 1)).collect()),
        _ => {
            let mut map = Map::new();
            for _ in 0..g.below(4) {
                map.insert(g.string(PY_STR_PIECES, 3), value(g, depth - 1));
            }
            Value::Object(map)
        }
    }
}

#[test]
fn dumps_equals_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    for _ in 0..fuzz::cases(CASES) {
        let v = value(&mut g, 3);
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
        let x = float(&mut g);
        let answer = oracle.call("repr", json!([x]), json!({})).unwrap();
        let Outcome::Returned(Value::String(want)) = answer.outcome else {
            panic!("seed {seed:#x}: repr({x:e}) gave {:?}", answer.outcome);
        };
        assert_eq!(pyjson::float_repr(x), want, "seed {seed:#x}, {x:e}");
    }
}
