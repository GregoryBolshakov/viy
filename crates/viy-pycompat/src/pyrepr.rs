//! `repr()` as CPython 3.10 writes it, for text that carries Python
//! formatting into a message (docs/flags.md F22): `{risk!r}`, `%r`,
//! `str(arg)` of a non-string tool argument (`{'a': 1}`).

use std::fmt::Write;

use serde_json::Value;

use crate::pyjson::float_repr;
use crate::pystr::is_printable;

/// `repr(s)` for a str: single quotes unless the text has a single quote and
/// no double one; backslash, the quote, `\t`, `\n`, `\r` escaped; other
/// non-printable characters as `\xNN`, `\uNNNN` or `\UNNNNNNNN`.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        let cp = u32::from(c);
        match c {
            c if c == quote || c == '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if cp < 0x20 || cp == 0x7f => {
                let _ = write!(out, "\\x{cp:02x}");
            }
            c if cp < 0x7f || is_printable(c) => out.push(c),
            _ if cp <= 0xff => {
                let _ = write!(out, "\\x{cp:02x}");
            }
            _ if cp <= 0xffff => {
                let _ = write!(out, "\\u{cp:04x}");
            }
            _ => {
                let _ = write!(out, "\\U{cp:08x}");
            }
        }
    }
    out.push(quote);
    out
}

/// `repr(v)` of the Python object `json.loads` would make of `v`: None,
/// True/False, ints, floats, str, lists and dicts.
pub fn repr_value(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v);
    out
}

/// `str(v)`: as `repr` except that a str is itself.
pub fn str_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => repr_value(other),
    }
}

fn write_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "{u}");
            } else if let Some(x) = n.as_f64() {
                out.push_str(&float_repr(x));
            }
        }
        Value::String(s) => out.push_str(&repr_str(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&repr_str(key));
                out.push_str(": ");
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
#[path = "pyrepr_tests.rs"]
mod tests;
