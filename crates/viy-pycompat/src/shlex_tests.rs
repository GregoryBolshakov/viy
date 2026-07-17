//! Expected values printed by CPython 3.10.12's shlex on 2026-07-12.

use pretty_assertions::assert_eq;

use super::*;

fn ok(words: &[&str]) -> Result<Vec<String>, ShlexError> {
    Ok(words.iter().map(ToString::to_string).collect())
}

#[test]
fn split_matches_python() {
    let cases: &[(&str, Result<Vec<String>, ShlexError>)] = &[
        ("a b  c", ok(&["a", "b", "c"])),
        ("'' x", ok(&["", "x"])),
        ("a\"b c\"d", ok(&["ab cd"])),
        ("a\\ b", ok(&["a b"])),
        ("\"a\\\"b\\c\"", ok(&["a\"b\\c"])),
        ("'a\\b'", ok(&["a\\b"])),
        ("echo \"unclosed", Err(ShlexError::NoClosingQuotation)),
        ("trail\\", Err(ShlexError::NoEscapedCharacter)),
        // U+3000 is whitespace to str.split(), not to shlex.
        ("ы 😀\u{3000}z", ok(&["ы", "😀\u{3000}z"])),
        ("a\tb\nc\rd", ok(&["a", "b", "c", "d"])),
        ("x#y # z", ok(&["x#y", "#", "z"])),
    ];
    for (s, want) in cases {
        assert_eq!(&split(s), want, "split({s:?})");
    }
}

#[test]
fn approve_lexer_matches_python() {
    let cases: &[(&str, &[&str])] = &[
        (
            "git status && ls | wc -l",
            &["git", "status", "&&", "ls", "|", "wc", "-l"],
        ),
        ("a&&b", &["a", "&&", "b"]),
        ("a;;b", &["a", ";;", "b"]),
        ("x &| y", &["x", "&|", "y"]),
        ("a'&'b", &["a&b"]),
        ("a|&b", &["a", "|&", "b"]),
        ("cd /tmp&&rm -rf x", &["cd", "/tmp", "&&", "rm", "-rf", "x"]),
    ];
    for (s, want) in cases {
        assert_eq!(tokens(s, "&|;", ""), ok(want), "{s:?}");
    }
}

#[test]
fn egress_lexer_matches_python() {
    let cases: &[(&str, &[&str])] = &[
        (
            "curl x.com # comment\nwget y",
            &["curl", "x.com", "wget", "y"],
        ),
        ("a(b)c", &["a", "(", "b", ")", "c"]),
        ("x>y 2>&1", &["x", ">", "y", "2", ">&", "1"]),
        // With comments on, # ends a word too.
        ("echo hi#there", &["echo", "hi"]),
        ("a&#x\nb", &["a", "&", "b"]),
        ("(cd x)", &["(", "cd", "x", ")"]),
    ];
    for (s, want) in cases {
        assert_eq!(tokens(s, DEFAULT_PUNCTUATION, "#"), ok(want), "{s:?}");
    }
}

#[test]
fn quote_and_join_match_python() {
    let cases = [
        ("", "''"),
        ("abc", "abc"),
        ("a b", "'a b'"),
        ("it's", "'it'\"'\"'s'"),
        ("x@y%z+=:,./-_", "x@y%z+=:,./-_"),
        ("ы", "'ы'"),
        ("$HOME", "'$HOME'"),
        ("a\nb", "'a\nb'"),
    ];
    for (s, want) in cases {
        assert_eq!(quote(s), want, "quote({s:?})");
    }
    assert_eq!(join(&["a b", "", "c'd", "ok"]), "'a b' '' 'c'\"'\"'d' ok");
    assert_eq!(
        ShlexError::NoClosingQuotation.to_string(),
        "No closing quotation"
    );
}
