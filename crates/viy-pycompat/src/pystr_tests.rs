//! Expected values printed by Python 3.10.12 on 2026-07-12; the oracle tests
//! check the same functions against CPython on generated inputs.

use pretty_assertions::assert_eq;

use super::*;

/// Input, split(), split(None, 1), rsplit(None, 1), strip().
type SplitCase<'a> = (
    &'a str,
    &'a [&'a str],
    &'a [&'a str],
    &'a [&'a str],
    &'a str,
);

#[test]
fn split_and_strip_use_python_whitespace() {
    let cases: &[SplitCase] = &[
        (
            "  a b  c  ",
            &["a", "b", "c"],
            &["a", "b  c  "],
            &["  a b", "c"],
            "a b  c",
        ),
        ("", &[], &[], &[], ""),
        ("   ", &[], &[], &[], ""),
        ("a\x1eb", &["a", "b"], &["a", "b"], &["a", "b"], "a\x1eb"),
        (
            "одна  две\u{3000}三",
            &["одна", "две", "三"],
            &["одна", "две\u{3000}三"],
            &["одна  две", "三"],
            "одна  две\u{3000}三",
        ),
        // U+200B is not whitespace to Python.
        (
            "x\u{a0}y\u{200b}z",
            &["x", "y\u{200b}z"],
            &["x", "y\u{200b}z"],
            &["x", "y\u{200b}z"],
            "x\u{a0}y\u{200b}z",
        ),
        (
            "a\x1c\x1d\x1fb",
            &["a", "b"],
            &["a", "b"],
            &["a", "b"],
            "a\x1c\x1d\x1fb",
        ),
    ];
    for &(s, split, split1, rsplit1, strip) in cases {
        assert_eq!(py_split(s, None), split, "{s:?}.split()");
        assert_eq!(py_split(s, Some(1)), split1, "{s:?}.split(None, 1)");
        assert_eq!(py_rsplit(s, Some(1)), rsplit1, "{s:?}.rsplit(None, 1)");
        assert_eq!(py_strip(s), strip, "{s:?}.strip()");
    }
    assert_eq!(py_strip("\x1e x \u{3000}"), "x");
    assert_eq!(py_lstrip("\x1f x "), "x ");
    assert_eq!(py_rstrip(" x \u{2029}"), " x");
}

#[test]
fn splitlines_breaks_where_python_does() {
    let cases: &[(&str, &[&str])] = &[
        ("a\nb\r\nc\rd", &["a", "b", "c", "d"]),
        ("a\n", &["a"]),
        ("\n", &[""]),
        ("", &[]),
        (
            "x\x0by\x0cz\x1cw\u{85}v\u{2028}u\u{2029}t",
            &["x", "y", "z", "w", "v", "u", "t"],
        ),
        ("a\r", &["a"]),
        ("\r\n\r\n", &["", ""]),
        ("a\x1fb", &["a\x1fb"]),
    ];
    for &(s, lines) in cases {
        assert_eq!(py_splitlines(s), lines, "{s:?}.splitlines()");
    }
}

#[test]
fn int_follows_cpython() {
    use IntError::Invalid;
    use IntError::OutOfRange;
    let cases: &[(&str, Result<i64, IntError>)] = &[
        ("12", Ok(12)),
        (" 12 ", Ok(12)),
        ("+7", Ok(7)),
        ("-0", Ok(0)),
        ("1_000", Ok(1000)),
        ("_1", Err(Invalid)),
        ("1_", Err(Invalid)),
        ("1__0", Err(Invalid)),
        ("١٢٣", Ok(123)),
        ("\u{2003} 42\u{2003}", Ok(42)),
        // str.isspace() is true for \x1c and \x1f, but int() strips only C
        // isspace after mapping non-ASCII spaces.
        ("\x1c12", Err(Invalid)),
        ("12\x1f", Err(Invalid)),
        ("²", Err(Invalid)),
        ("1 2", Err(Invalid)),
        ("9223372036854775807", Ok(i64::MAX)),
        ("9223372036854775808", Err(OutOfRange)),
        ("-9223372036854775808", Ok(i64::MIN)),
        ("-9223372036854775809", Err(OutOfRange)),
        ("", Err(Invalid)),
        ("0x10", Err(Invalid)),
        ("007", Ok(7)),
        ("1\x00", Err(Invalid)),
    ];
    for &(s, want) in cases {
        assert_eq!(py_int(s), want, "int({s:?})");
    }
    assert_eq!(py_int(&"0".repeat(4300)), Ok(0));
    assert_eq!(py_int(&"0".repeat(4301)), Err(Invalid));
}

#[test]
fn float_follows_cpython() {
    let cases: &[(&str, Option<f64>)] = &[
        ("1e3", Some(1000.0)),
        (" 1.5 ", Some(1.5)),
        ("inf", Some(f64::INFINITY)),
        ("-Infinity", Some(f64::NEG_INFINITY)),
        ("1_000.5", Some(1000.5)),
        ("1_e5", None),
        ("1e1_0", Some(1e10)),
        (".5", Some(0.5)),
        ("1.", Some(1.0)),
        (".", None),
        ("e5", None),
        ("١.٥", Some(1.5)),
        ("1.5€", None),
        ("0x1p3", None),
        ("1e400", Some(f64::INFINITY)),
        ("\x1c1", None),
        ("+.5e-3", Some(0.0005)),
        ("infinity_", None),
        ("infi", None),
    ];
    for &(s, want) in cases {
        assert_eq!(py_float(s), want, "float({s:?})");
    }
    assert!(py_float("nAn").is_some_and(f64::is_nan));
    assert!(py_float("-0").is_some_and(|x| x == 0.0 && x.is_sign_negative()));
}

#[test]
fn len_and_slice_count_code_points() {
    let s = "приветмир😀日本";
    assert_eq!(py_len(s), 12);
    assert_eq!(py_slice(s, Some(2), Some(5)), "иве");
    assert_eq!(py_slice(s, Some(-3), None), "😀日本");
    assert_eq!(py_slice(s, None, Some(-20)), "");
    assert_eq!(py_slice(s, Some(5), Some(2)), "");
    assert_eq!(py_slice(s, Some(-2), Some(-1)), "日");
    assert_eq!(py_slice(s, Some(3), Some(100)), "ветмир😀日本");
    assert_eq!(py_slice(s, Some(i64::MIN), Some(i64::MAX)), s);
}

#[test]
fn character_classes() {
    assert!(is_digit('²') && is_digit('①') && is_digit('١'));
    assert!(!is_digit('Ⅷ'));
    assert!(is_space('\x1e') && !is_space('\u{200b}'));
    assert!(!py_isdigit("") && py_isdigit("12²") && !py_isdigit("1a"));
    assert!(py_isspace("\x1c\u{3000}") && !py_isspace(""));
    assert_eq!(decimal_value('٥'), Some(5));
    assert_eq!(decimal_value('²'), None);
    assert_eq!(decimal_value('9'), Some(9));
}
