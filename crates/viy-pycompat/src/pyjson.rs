//! `json.dumps` as CPython 3.10 writes it, for text the model or a person
//! reads (docs/flags.md F01, kb d009). Files and request bodies use serde's
//! own format (F02); this is only for strings whose bytes matter.
//!
//! Defaults are Python's: separators `", "` and `": "` (`","` between items
//! once `indent` is set), `ensure_ascii` with lowercase `\uXXXX` escapes and
//! surrogate pairs above U+FFFF, floats as `repr()` writes them.

use std::fmt::Write;

use serde_json::Map;
use serde_json::Number;
use serde_json::Value;

/// The keyword arguments of `json.dumps` that rpctl passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dumps {
    pub ensure_ascii: bool,
    /// `indent=n`: newlines and n spaces per level; `None` is one line.
    pub indent: Option<usize>,
    pub sort_keys: bool,
}

impl Default for Dumps {
    fn default() -> Self {
        Self {
            ensure_ascii: true,
            indent: None,
            sort_keys: false,
        }
    }
}

/// `json.dumps(value)` with Python's defaults.
pub fn dumps(value: &Value) -> String {
    dumps_with(value, Dumps::default())
}

/// `json.dumps(value, ensure_ascii=..., indent=..., sort_keys=...)`.
pub fn dumps_with(value: &Value, opts: Dumps) -> String {
    let mut out = String::new();
    write_value(&mut out, value, opts, 0);
    out
}

fn write_value(out: &mut String, value: &Value, opts: Dumps, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_string(out, s, opts.ensure_ascii),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                separator(out, i, opts, level + 1);
                write_value(out, item, opts, level + 1);
            }
            close(out, opts, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (key, item)) in entries(map, opts.sort_keys).into_iter().enumerate() {
                separator(out, i, opts, level + 1);
                write_string(out, key, opts.ensure_ascii);
                out.push_str(": ");
                write_value(out, item, opts, level + 1);
            }
            close(out, opts, level);
            out.push('}');
        }
    }
}

fn entries(map: &Map<String, Value>, sort: bool) -> Vec<(&String, &Value)> {
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    if sort {
        // UTF-8 byte order is code point order, which is Python's str order.
        entries.sort_by(|a, b| a.0.cmp(b.0));
    }
    entries
}

/// What goes before the i-th item of a container whose items sit at `level`.
fn separator(out: &mut String, i: usize, opts: Dumps, level: usize) {
    match opts.indent {
        None if i > 0 => out.push_str(", "),
        None => {}
        Some(width) => {
            if i > 0 {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(width * level));
        }
    }
}

fn close(out: &mut String, opts: Dumps, level: usize) {
    if let Some(width) = opts.indent {
        out.push('\n');
        out.push_str(&" ".repeat(width * level));
    }
}

fn write_number(out: &mut String, n: &Number) {
    if let Some(i) = n.as_i64() {
        let _ = write!(out, "{i}");
    } else if let Some(u) = n.as_u64() {
        let _ = write!(out, "{u}");
    } else if let Some(x) = n.as_f64() {
        out.push_str(&json_float(x));
    }
}

/// `float.__repr__` as `json.dumps` uses it, with its names for the
/// non-finite values.
fn json_float(x: f64) -> String {
    if x.is_nan() {
        "NaN".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        float_repr(x)
    }
}

/// `repr(x)` for a float: the shortest digits that round-trip, positional
/// when the decimal exponent is in -4..=15, else `d.ddde+XX`.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    let (digits, exp) = shortest_digits(x.abs());
    if (-4..=15).contains(&exp) {
        // CPython's decpt: where the point goes in the digit string.
        let decpt = exp + 1;
        let body = if decpt <= 0 {
            let zeros = usize::try_from(-decpt).unwrap_or(0);
            format!("0.{}{digits}", "0".repeat(zeros))
        } else {
            let point = usize::try_from(decpt).unwrap_or(0);
            if point >= digits.len() {
                format!("{digits}{}.0", "0".repeat(point - digits.len()))
            } else {
                format!("{}.{}", &digits[..point], &digits[point..])
            }
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let frac = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{sign}{first}{frac}e{esign}{:02}", exp.unsigned_abs())
    }
}

/// The digits and decimal exponent of CPython's dtoa mode 0: the fewest
/// significant digits that read back as `x`, and of those the nearest to
/// `x`, a tie going to the even digit. Rust's `{:e}` finds the same count
/// but rounds a tie up (117195891500213.125 gives ...13, CPython ...12), so
/// the digits are taken from its exact mode, which rounds half to even.
fn shortest_digits(x: f64) -> (String, i32) {
    let split = |sci: &str| {
        let (mantissa, exp) = sci.split_once('e').unwrap_or((sci, "0"));
        let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
        (digits, exp.parse::<i32>().unwrap_or(0))
    };
    let shortest = split(&format!("{x:e}"));
    let precision = shortest.0.len().saturating_sub(1);
    let exact = format!("{x:.precision$e}");
    if exact.parse::<f64>().ok() == Some(x) {
        split(&exact)
    } else {
        shortest
    }
}

fn write_string(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if u32::from(c) < 0x20 => push_escape(out, u32::from(c)),
            // ensure_ascii escapes everything outside ' '..='~', DEL included.
            c if ensure_ascii && !(' '..='~').contains(&c) => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    push_escape(out, u32::from(*unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_escape(out: &mut String, unit: u32) {
    let _ = write!(out, "\\u{unit:04x}");
}

#[cfg(test)]
#[path = "pyjson_tests.rs"]
mod tests;
