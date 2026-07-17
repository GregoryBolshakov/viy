//! Expected values printed by CPython 3.10.12's repr() on 2026-07-12.

use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

#[test]
fn repr_str_matches_python() {
    let cases = [
        ("", "''"),
        ("it's", "\"it's\""),
        ("say \"hi\"", "'say \"hi\"'"),
        ("both ' and \"", "'both \\' and \"'"),
        ("back\\slash", "'back\\\\slash'"),
        ("tab\tnl\ncr\r", "'tab\\tnl\\ncr\\r'"),
        ("\0\x1f\x7f", "'\\x00\\x1f\\x7f'"),
        // U+00A0 (Zs) and U+00AD (Cf) are not printable, é is.
        ("\u{a0}\u{ad}\u{e9}", "'\\xa0\\xad\u{e9}'"),
        // Cf, unassigned, private use.
        ("\u{200b}\u{378}\u{e000}", "'\\u200b\\u0378\\ue000'"),
        ("😀\u{e0001}\u{10ffff}", "'😀\\U000e0001\\U0010ffff'"),
        ("ключ 日本", "'ключ 日本'"),
    ];
    for (s, want) in cases {
        assert_eq!(repr_str(s), want, "repr({s:?})");
    }
}

#[test]
fn repr_value_matches_python() {
    let v = json!({"a": [1, 2.5, null, true, false, "x'y"], "b": {}, "c": -0.0, "d": 1e16});
    assert_eq!(
        repr_value(&v),
        "{'a': [1, 2.5, None, True, False, \"x'y\"], 'b': {}, 'c': -0.0, 'd': 1e+16}"
    );
    assert_eq!(str_value(&json!("plain")), "plain");
    assert_eq!(str_value(&json!(["plain"])), "['plain']");
}

#[test]
fn repr_of_undecodable_file_names() {
    // Python: repr(os.fsdecode(b"a\xff\xc3")) == "'a\\udcff\\udcc3'"
    assert_eq!(repr_bytes_path(b"a\xff\xc3"), "'a\\udcff\\udcc3'");
    assert_eq!(repr_bytes_path("ключ".as_bytes()), "'ключ'");
}
