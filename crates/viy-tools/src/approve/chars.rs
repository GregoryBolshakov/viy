//! The character classes rpctl's patterns use, each chosen so that viy finds
//! a match wherever rpctl does: where Python's class is wider than Rust's,
//! the wider one, and a word boundary wherever Python could see one.

/// Python's `\s` and `str.isspace`: Unicode White_Space plus the four
/// information separators `\x1c`-`\x1f`, which Python counts and Rust does
/// not.
pub(super) fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// A word character for boundary tests. ASCII only, so every character
/// Python's `\w` rejects is a boundary here too, and so are a few it
/// accepts (`\bgit` matches after `é`, where rpctl's does not).
pub(super) fn is_ascii_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A word character for runs of `\w`: anything Rust or Python could count.
pub(super) fn is_broad_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
