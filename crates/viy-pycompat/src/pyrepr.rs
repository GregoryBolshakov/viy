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
    repr_code_points(&s.chars().map(u32::from).collect::<Vec<_>>())
}

/// `repr()` of the str Python makes of a file name: its bytes decoded as
/// UTF-8 with surrogateescape, so each undecodable byte b is U+DC00+b and
/// shows as `\udcNN`.
pub fn repr_bytes_path(name: &[u8]) -> String {
    let mut cps = Vec::with_capacity(name.len());
    for chunk in name.utf8_chunks() {
        cps.extend(chunk.valid().chars().map(u32::from));
        cps.extend(chunk.invalid().iter().map(|&b| 0xDC00 + u32::from(b)));
    }
    repr_code_points(&cps)
}

/// CPython's `unicode_repr` over code points, lone surrogates allowed.
fn repr_code_points(cps: &[u32]) -> String {
    let squote = u32::from('\'');
    let dquote = u32::from('"');
    let quote = if cps.contains(&squote) && !cps.contains(&dquote) {
        dquote
    } else {
        squote
    };
    let printable = |cp: u32| char::from_u32(cp).is_some_and(is_printable);
    let mut out = String::with_capacity(cps.len() + 2);
    let push = |out: &mut String, cp: u32| out.extend(char::from_u32(cp));
    push(&mut out, quote);
    for &cp in cps {
        match cp {
            cp if cp == quote || cp == u32::from('\\') => {
                out.push('\\');
                push(&mut out, cp);
            }
            0x09 => out.push_str("\\t"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            cp if cp < 0x20 || cp == 0x7f => {
                let _ = write!(out, "\\x{cp:02x}");
            }
            cp if cp < 0x7f || printable(cp) => push(&mut out, cp),
            cp if cp <= 0xff => {
                let _ = write!(out, "\\x{cp:02x}");
            }
            cp if cp <= 0xffff => {
                let _ = write!(out, "\\u{cp:04x}");
            }
            cp => {
                let _ = write!(out, "\\U{cp:08x}");
            }
        }
    }
    push(&mut out, quote);
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
