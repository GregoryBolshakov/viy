//! The character classes of rpctl's patterns and parsers, where Python's
//! and Rust's Unicode tables disagree (flags F41). Two kinds of use, with
//! two rules:
//!
//! - **Matching** (does this text hold `rm -rf`, `python3.12`): a class may
//!   be wider than Python's, so viy matches wherever rpctl does and
//!   sometimes more: `is_py_space`, `is_ascii_word` for boundaries,
//!   `is_broad_word` for runs of `\w`, `is_wide_digit` for `\d`.
//! - **Splitting** (is this word a file descriptor, an assignment): a wider
//!   class there moves words around and can ask less. These answer `None`
//!   when a non-ASCII character decides, and the caller then asks.

/// Python's `\s` and `str.isspace`: Unicode White_Space plus the four
/// information separators `\x1c`-`\x1f`, which Python counts and Rust does
/// not.
pub fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// A word character for boundary tests. ASCII only, so every character
/// Python's `\w` rejects is a boundary here too, and so are a few it
/// accepts (`\bgit` matches after `é`, where rpctl's does not).
pub(crate) fn is_ascii_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A word character for runs of `\w` in a match: anything Rust or Python
/// could count.
pub(super) fn is_broad_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A digit for `\d` in a match: every Unicode numeric character.
pub(super) fn is_wide_digit(c: char) -> bool {
    c.is_numeric()
}

/// Python's `text.isdigit()` for splitting: `Some` when the answer does not
/// depend on a non-ASCII character, `None` when it might.
pub(super) fn all_digits(text: &str) -> Option<bool> {
    if text.chars().all(|c| c.is_ascii_digit()) {
        return Some(!text.is_empty());
    }
    // A character no table counts as a digit settles it; a non-ASCII
    // numeric one is a digit to Python only sometimes (`²` yes, `½` no).
    if text.chars().any(|c| {
        if c.is_ascii() {
            !c.is_ascii_digit()
        } else {
            !c.is_numeric()
        }
    }) {
        return Some(false);
    }
    None
}

/// rpctl's `[A-Za-z_]\w*=.*` in full (no newline in the value), for
/// splitting: `None` when a non-ASCII character in the name decides.
pub(super) fn assignment(word: &str) -> Option<bool> {
    let Some((name, value)) = word.split_once('=') else {
        return Some(false);
    };
    let mut chars = name.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || chars.clone().any(|c| {
            if c.is_ascii() {
                !is_ascii_word(c)
            } else {
                !c.is_alphanumeric()
            }
        })
        || value.contains('\n')
    {
        return Some(false);
    }
    // A non-ASCII letter or digit is `\w` to Python only sometimes (U+0345,
    // a combining mark Rust calls alphabetic, is not).
    if chars.any(|c| !c.is_ascii()) {
        return None;
    }
    Some(true)
}
