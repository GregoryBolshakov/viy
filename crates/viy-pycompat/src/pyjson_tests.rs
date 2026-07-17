//! Expected strings printed by Python 3.10.12's json.dumps on 2026-07-12.

use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn sample() -> Value {
    json!({
        "b": [1, -2.5, 1e16, 1e15, 1e-05, 0.0001, -0.0, 5e-324, 1.797_693_134_862_315_7e308, 0.1,
              123_456_789_012_345_678_i64],
        "a": "ключ 😀 \u{7f} \u{1f} \"q\" \\ \t\n",
        "e": [],
        "d": {},
        "n": null,
        "t": true,
    })
}

#[test]
fn defaults_match_python() {
    assert_eq!(
        dumps(&sample()),
        r#"{"b": [1, -2.5, 1e+16, 1000000000000000.0, 1e-05, 0.0001, -0.0, 5e-324, 1.7976931348623157e+308, 0.1, 123456789012345678], "a": "\u043a\u043b\u044e\u0447 \ud83d\ude00 \u007f \u001f \"q\" \\ \t\n", "e": [], "d": {}, "n": null, "t": true}"#
    );
}

#[test]
fn ensure_ascii_false_keeps_text_but_escapes_controls() {
    let opts = Dumps {
        ensure_ascii: false,
        ..Dumps::default()
    };
    assert_eq!(
        dumps_with(&sample(), opts),
        "{\"b\": [1, -2.5, 1e+16, 1000000000000000.0, 1e-05, 0.0001, -0.0, 5e-324, 1.7976931348623157e+308, 0.1, 123456789012345678], \"a\": \"ключ 😀 \u{7f} \\u001f \\\"q\\\" \\\\ \\t\\n\", \"e\": [], \"d\": {}, \"n\": null, \"t\": true}"
    );
}

#[test]
fn indent_and_sort_keys() {
    let two = Dumps {
        indent: Some(2),
        ..Dumps::default()
    };
    assert_eq!(
        dumps_with(&json!({"x": [1, {"y": []}], "a": {}}), two),
        "{\n  \"x\": [\n    1,\n    {\n      \"y\": []\n    }\n  ],\n  \"a\": {}\n}"
    );
    let zero = Dumps {
        indent: Some(0),
        ..Dumps::default()
    };
    assert_eq!(
        dumps_with(&json!({"x": [1, 2]}), zero),
        "{\n\"x\": [\n1,\n2\n]\n}"
    );
    let sorted = Dumps {
        sort_keys: true,
        ..Dumps::default()
    };
    assert_eq!(
        dumps_with(&json!({"b": 1, "a": 2, "ä": 3, "Z": 4}), sorted),
        r#"{"Z": 4, "a": 2, "b": 1, "\u00e4": 3}"#
    );
}

#[test]
// The tie cases spell out a double's exact value, more digits than repr needs.
#[allow(clippy::excessive_precision)]
fn float_repr_matches_python() {
    let cases = [
        (1e16, "1e+16"),
        (1.5e16, "1.5e+16"),
        (123_456_789_012_345_678.0, "1.2345678901234568e+17"),
        (1e-5, "1e-05"),
        (0.0001, "0.0001"),
        (-0.0, "-0.0"),
        (0.0, "0.0"),
        (1e15, "1000000000000000.0"),
        (1e22, "1e+22"),
        (2.5e-7, "2.5e-07"),
        (1e-4 * 3.0, "0.00030000000000000003"),
        // Exact ties: CPython rounds the last digit to even.
        (-117_195_891_500_213.125, "-117195891500213.12"),
        (1_877_840_344_204_777.25, "1877840344204777.2"),
        (f64::NAN, "nan"),
        (f64::NEG_INFINITY, "-inf"),
    ];
    for (x, want) in cases {
        assert_eq!(float_repr(x), want, "{x:e}");
    }
}
