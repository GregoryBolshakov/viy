//! pystr against CPython 3.10.12 through the oracle: the character tables
//! exhaustively, the functions on generated strings. Zero differences is the
//! gate (S2). `VIY_FUZZ_CASES=100000` for a long run.

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_pycompat::pystr;
use viy_testkit::fuzz;
use viy_testkit::fuzz::Gen;
use viy_testkit::fuzz::PY_NUMBER_PIECES;
use viy_testkit::fuzz::PY_STR_PIECES;
use viy_testkit::oracle::Oracle;
use viy_testkit::oracle::Outcome;

const CASES: usize = 3000;

type CharClass = fn(char) -> bool;
const MAX_PIECES: usize = 10;

/// What Python returned, or the class of what it raised.
fn ask(oracle: &mut Oracle, func: &str, args: Value) -> Result<Value, String> {
    match oracle.call(func, args, json!({})).unwrap().outcome {
        Outcome::Returned(v) => Ok(v),
        Outcome::Raised { kind, .. } => Err(kind),
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn ranges_of(pred: fn(char) -> bool) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
        if !pred(c) {
            continue;
        }
        let cp = u32::from(c);
        match out.last_mut() {
            Some(last) if last.1 + 1 == cp => last.1 = cp,
            _ => out.push((cp, cp)),
        }
    }
    out
}

/// Python's ranges with the surrogates (not chars in Rust) cut out.
fn python_ranges(oracle: &mut Oracle, kind: &str) -> Vec<(u32, u32)> {
    let v = ask(oracle, "oracle.char_ranges", json!([kind])).unwrap();
    let mut out = Vec::new();
    for r in v.as_array().unwrap() {
        let lo = u32::try_from(r[0].as_u64().unwrap()).unwrap();
        let hi = u32::try_from(r[1].as_u64().unwrap()).unwrap();
        for (a, b) in [(lo, hi.min(0xD7FF)), (lo.max(0xE000), hi)] {
            if a <= b {
                out.push((a, b));
            }
        }
    }
    out
}

#[test]
fn character_tables_equal_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let checks: [(&str, CharClass); 4] = [
        ("isspace", pystr::is_space),
        ("isdigit", pystr::is_digit),
        ("isdecimal", |c| pystr::decimal_value(c).is_some()),
        ("splitlines", pystr::is_line_break),
    ];
    for (kind, pred) in checks {
        assert_eq!(ranges_of(pred), python_ranges(&mut oracle, kind), "{kind}");
    }
    for (lo, hi) in python_ranges(&mut oracle, "isdecimal") {
        for c in (lo..=hi).filter_map(char::from_u32) {
            let want = ask(&mut oracle, "unicodedata.decimal", json!([c.to_string()])).unwrap();
            assert_eq!(
                pystr::decimal_value(c).map(u64::from),
                want.as_u64(),
                "{c:?}"
            );
        }
    }
}

#[test]
fn strip_split_splitlines_equal_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    for _ in 0..fuzz::cases(CASES) {
        let s = g.string(PY_STR_PIECES, MAX_PIECES);
        let ctx = format!("seed {seed:#x}, input {s:?}");
        for (func, rust) in [
            ("str.strip", pystr::py_strip(&s)),
            ("str.lstrip", pystr::py_lstrip(&s)),
            ("str.rstrip", pystr::py_rstrip(&s)),
        ] {
            assert_eq!(
                ask(&mut oracle, func, json!([s])).unwrap(),
                json!(rust),
                "{func}, {ctx}"
            );
        }
        let maxsplit = *g.pick(&[-1i64, 0, 1, 2, 3]);
        let limit = usize::try_from(maxsplit).ok();
        let py = ask(&mut oracle, "str.split", json!([s, null, maxsplit])).unwrap();
        assert_eq!(
            pystr::py_split(&s, limit),
            strings(&py),
            "split {maxsplit}, {ctx}"
        );
        let py = ask(&mut oracle, "str.rsplit", json!([s, null, maxsplit])).unwrap();
        assert_eq!(
            pystr::py_rsplit(&s, limit),
            strings(&py),
            "rsplit {maxsplit}, {ctx}"
        );
        let py = ask(&mut oracle, "str.splitlines", json!([s])).unwrap();
        assert_eq!(pystr::py_splitlines(&s), strings(&py), "splitlines, {ctx}");
        for (func, rust) in [
            ("str.isspace", pystr::py_isspace(&s)),
            ("str.isdigit", pystr::py_isdigit(&s)),
        ] {
            assert_eq!(
                ask(&mut oracle, func, json!([s])).unwrap(),
                json!(rust),
                "{func}, {ctx}"
            );
        }
    }
}

#[test]
fn len_and_slice_equal_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    let bounds: Vec<Option<i64>> = [None, Some(i64::MIN), Some(i64::MAX)]
        .into_iter()
        .chain((-14..=14).map(Some))
        .collect();
    for _ in 0..fuzz::cases(CASES) {
        let s = g.string(PY_STR_PIECES, MAX_PIECES);
        let ctx = format!("seed {seed:#x}, input {s:?}");
        let len = ask(&mut oracle, "len", json!([s])).unwrap();
        assert_eq!(json!(pystr::py_len(&s)), len, "len, {ctx}");
        let (start, stop) = (*g.pick(&bounds), *g.pick(&bounds));
        let py = ask(
            &mut oracle,
            "operator.getitem",
            json!([s, {"$slice": [start, stop, null]}]),
        )
        .unwrap();
        assert_eq!(
            json!(pystr::py_slice(&s, start, stop)),
            py,
            "[{start:?}:{stop:?}], {ctx}"
        );
    }
}

#[test]
fn int_and_float_equal_python() {
    let mut oracle = Oracle::spawn().unwrap();
    let mut g = Gen::from_env();
    let seed = fuzz::seed();
    let cases = fuzz::cases(CASES);
    let (mut ints, mut floats) = (0, 0);
    for _ in 0..cases {
        let s = g.string(PY_NUMBER_PIECES, 6);
        let ctx = format!("seed {seed:#x}, input {s:?}");
        let want = match ask(&mut oracle, "int", json!([s])) {
            Ok(v) => {
                ints += 1;
                v.as_i64().ok_or(pystr::IntError::OutOfRange)
            }
            Err(kind) => {
                assert_eq!(kind, "ValueError", "int, {ctx}");
                Err(pystr::IntError::Invalid)
            }
        };
        assert_eq!(pystr::py_int(&s), want, "int, {ctx}");
        let rust = pystr::py_float(&s);
        match ask(&mut oracle, "float", json!([s])) {
            Ok(Value::Number(n)) => {
                floats += 1;
                let want = n.as_f64().unwrap();
                assert_eq!(rust.map(f64::to_bits), Some(want.to_bits()), "float, {ctx}");
            }
            Ok(other) => {
                let repr = other["$repr"].as_str().unwrap().to_string();
                let got = rust.map(|x| match x {
                    x if x.is_nan() => "nan".to_string(),
                    x if x > 0.0 => "inf".to_string(),
                    _ => "-inf".to_string(),
                });
                assert_eq!(got, Some(repr), "float, {ctx}");
            }
            Err(kind) => {
                assert_eq!(kind, "ValueError", "float, {ctx}");
                assert_eq!(rust, None, "float, {ctx}");
            }
        }
    }
    // Not vacuous: a share of the inputs must be numbers Python accepts.
    assert!(
        ints * 20 >= cases && floats * 10 >= cases,
        "{ints} ints, {floats} floats of {cases}"
    );
}
