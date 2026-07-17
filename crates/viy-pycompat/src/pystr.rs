//! Python 3.10 `str` semantics where Rust's differ (docs/flags.md F05).
//!
//! Lengths and indexes count code points, not bytes. Whitespace is
//! `str.isspace()`'s set, which has `\x1c`-`\x1f` (rpctl's Shift+Tab marker
//! is `\x1e`) and lacks nothing Rust has; line breaks are `str.splitlines()`'s,
//! which has `\v`, `\f`, `\x1c`-`\x1e`, `\x85`, U+2028 and U+2029. `int()`
//! and `float()` follow CPython's C code: non-ASCII decimal digits and spaces
//! are first mapped to ASCII, then only C `isspace` is stripped.
//!
//! The tables come from the installed Python 3.10.12
//! (`tools/gen_pystr_tables.py`); the oracle tests compare every function
//! with CPython (`cargo test -p viy-pycompat --features oracle`).

use std::cmp::Ordering;

use crate::pystr_tables::DECIMAL;
use crate::pystr_tables::DIGIT;
use crate::pystr_tables::LINE_BREAK;
use crate::pystr_tables::SPACE;

/// CPython's `_PY_LONG_DEFAULT_MAX_STR_DIGITS`: `int()` refuses more digits.
const INT_MAX_STR_DIGITS: usize = 4300;

fn in_ranges(table: &[(u32, u32)], c: char) -> bool {
    let cp = u32::from(c);
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                Ordering::Less
            } else if lo > cp {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// `c.isspace()`.
pub fn is_space(c: char) -> bool {
    in_ranges(SPACE, c)
}

/// `c.isdigit()`: decimal digits plus superscripts, circled digits and the
/// like.
pub fn is_digit(c: char) -> bool {
    in_ranges(DIGIT, c)
}

/// Whether `str.splitlines()` breaks at `c`.
pub fn is_line_break(c: char) -> bool {
    in_ranges(LINE_BREAK, c)
}

/// `unicodedata.decimal(c)`, the value `int()` gives a decimal digit.
pub fn decimal_value(c: char) -> Option<u32> {
    let cp = u32::from(c);
    let i = DECIMAL.partition_point(|&(_, hi, _)| hi < cp);
    DECIMAL
        .get(i)
        .filter(|&&(lo, _, _)| lo <= cp)
        .map(|&(lo, _, v)| v + (cp - lo))
}

/// `s.isspace()`: non-empty and all whitespace.
pub fn py_isspace(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_space)
}

/// `s.isdigit()`: non-empty and all digits.
pub fn py_isdigit(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_digit)
}

/// `len(s)`: code points.
pub fn py_len(s: &str) -> usize {
    s.chars().count()
}

/// `s[start:stop]`, with Python's negative indexes and clamping.
pub fn py_slice(s: &str, start: Option<i64>, stop: Option<i64>) -> &str {
    let n = i64::try_from(py_len(s)).unwrap_or(i64::MAX);
    let index = |i: Option<i64>, default: i64| match i {
        None => default,
        Some(i) if i < 0 => i.saturating_add(n).max(0),
        Some(i) => i.min(n),
    };
    let (a, b) = (index(start, 0), index(stop, n));
    if a >= b {
        return "";
    }
    let byte = |i: i64| {
        usize::try_from(i)
            .ok()
            .and_then(|i| s.char_indices().nth(i))
            .map_or(s.len(), |(b, _)| b)
    };
    &s[byte(a)..byte(b)]
}

/// `s.strip()`.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `s.lstrip()`.
pub fn py_lstrip(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

/// `s.rstrip()`.
pub fn py_rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// `s.split(None, maxsplit)`; `None` is Python's default of -1, no limit.
pub fn py_split(s: &str, maxsplit: Option<usize>) -> Vec<&str> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let at = |i: usize| chars.get(i).map_or(s.len(), |&(b, _)| b);
    let space = |i: usize| chars.get(i).is_some_and(|&(_, c)| is_space(c));
    let n = chars.len();
    let mut out = Vec::new();
    let mut left = maxsplit.unwrap_or(usize::MAX);
    let mut i = 0;
    while left > 0 {
        left -= 1;
        while i < n && space(i) {
            i += 1;
        }
        if i == n {
            break;
        }
        let j = i;
        i += 1;
        while i < n && !space(i) {
            i += 1;
        }
        out.push(&s[at(j)..at(i)]);
    }
    if i < n {
        while i < n && space(i) {
            i += 1;
        }
        if i != n {
            out.push(&s[at(i)..]);
        }
    }
    out
}

/// `s.rsplit(None, maxsplit)`; `None` is no limit.
pub fn py_rsplit(s: &str, maxsplit: Option<usize>) -> Vec<&str> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let at = |i: usize| chars.get(i).map_or(s.len(), |&(b, _)| b);
    let space = |i: usize| chars.get(i).is_some_and(|&(_, c)| is_space(c));
    let mut out = Vec::new();
    let mut left = maxsplit.unwrap_or(usize::MAX);
    // `end` is one past the char CPython's `i` points at; 0 means i < 0.
    let mut end = chars.len();
    while left > 0 {
        left -= 1;
        while end > 0 && space(end - 1) {
            end -= 1;
        }
        if end == 0 {
            break;
        }
        let j = end;
        end -= 1;
        while end > 0 && !space(end - 1) {
            end -= 1;
        }
        out.push(&s[at(end)..at(j)]);
    }
    if end > 0 {
        while end > 0 && space(end - 1) {
            end -= 1;
        }
        if end > 0 {
            out.push(&s[..at(end)]);
        }
    }
    out.reverse();
    out
}

/// `s.splitlines()` (without `keepends`; rpctl never passes it).
pub fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut j = 0;
    while j < s.len() {
        let rest = &s[j..];
        let Some((k, c)) = rest.char_indices().find(|&(_, c)| is_line_break(c)) else {
            out.push(rest);
            break;
        };
        out.push(&rest[..k]);
        let mut next = k + c.len_utf8();
        if c == '\r' && rest.as_bytes().get(next) == Some(&b'\n') {
            next += 1;
        }
        j += next;
    }
    out
}

/// Why `int(s)` gave no `i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntError {
    /// Python raises `ValueError` too.
    Invalid,
    /// Python returns a number that does not fit an `i64` (docs/flags.md F38).
    OutOfRange,
}

/// CPython's `_PyUnicode_TransformDecimalAndSpaceToASCII`: code points below
/// 127 kept, other whitespace to a space, decimal digits to ASCII, anything
/// else to `?` and the end of the result.
fn to_ascii_digits(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        if let Ok(b) = u8::try_from(u32::from(c))
            && b < 127
        {
            out.push(b);
        } else if is_space(c) {
            out.push(b' ');
        } else if let Some(d) = decimal_value(c).and_then(|d| u8::try_from(d).ok()) {
            out.push(b'0' + d);
        } else {
            out.push(b'?');
            break;
        }
    }
    out
}

/// C's `isspace` in the "C" locale, which `Py_ISSPACE` is.
fn c_isspace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// `int(s)` for a `str`, base 10.
pub fn py_int(s: &str) -> Result<i64, IntError> {
    let b = to_ascii_digits(s);
    // A NUL ends the C string; anything after it makes the literal invalid.
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while at(i) != 0 && c_isspace(at(i)) {
        i += 1;
    }
    let negative = match at(i) {
        b'+' => {
            i += 1;
            false
        }
        b'-' => {
            i += 1;
            true
        }
        _ => false,
    };
    if at(i) == b'_' {
        return Err(IntError::Invalid);
    }
    let mut digits = 0usize;
    let mut value: Option<i128> = Some(0);
    let mut prev = 0u8;
    while at(i).is_ascii_digit() || at(i) == b'_' {
        let c = at(i);
        if c == b'_' {
            if prev == b'_' {
                return Err(IntError::Invalid);
            }
        } else {
            digits += 1;
            value = value
                .and_then(|v| v.checked_mul(10))
                .and_then(|v| v.checked_add(i128::from(c - b'0')));
        }
        prev = c;
        i += 1;
    }
    if prev == b'_' || digits == 0 || digits > INT_MAX_STR_DIGITS {
        return Err(IntError::Invalid);
    }
    while at(i) != 0 && c_isspace(at(i)) {
        i += 1;
    }
    if i != b.len() {
        return Err(IntError::Invalid);
    }
    let value = value.map(|v| if negative { -v } else { v });
    value
        .and_then(|v| i64::try_from(v).ok())
        .ok_or(IntError::OutOfRange)
}

/// `float(s)` for a `str`; `None` where Python raises `ValueError`.
pub fn py_float(s: &str) -> Option<f64> {
    let b = to_ascii_digits(s);
    // _Py_string_to_number_with_underscores: an underscore only between
    // digits, then dropped.
    let mut text = Vec::with_capacity(b.len());
    if b.contains(&b'_') {
        let mut prev = 0u8;
        for &c in &b {
            if c == 0 {
                return None;
            }
            if c == b'_' {
                if !prev.is_ascii_digit() {
                    return None;
                }
            } else {
                if prev == b'_' && !c.is_ascii_digit() {
                    return None;
                }
                text.push(c);
            }
            prev = c;
        }
        if prev == b'_' {
            return None;
        }
    } else {
        text = b;
    }
    let mut lo = 0;
    let mut hi = text.len();
    while lo < hi && c_isspace(text[lo]) {
        lo += 1;
    }
    while lo + 1 < hi && c_isspace(text[hi - 1]) {
        hi -= 1;
    }
    // Rust's grammar is CPython's: an optional sign, then inf, infinity or
    // nan in any case, or digits with an optional point and exponent.
    std::str::from_utf8(&text[lo..hi]).ok()?.parse().ok()
}

#[cfg(test)]
#[path = "pystr_tests.rs"]
mod tests;
