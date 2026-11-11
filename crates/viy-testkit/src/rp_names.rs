//! rp's names in what the model sees, read as viy's (flags F21, kb d131).
//! rpctl's fixtures, oracle answers and scenario recordings name rpctl
//! where viy names itself; [`to_viy`] puts viy's names in, from
//! `tests/rp-names.json`, which `tools/drive_rp.py` reads too.

use std::sync::OnceLock;

use serde_json::Value;

const MAP: &str = include_str!("../../../tests/rp-names.json");

/// The map's pairs; empty when the file is broken, which
/// `the_map_is_read` catches.
fn pairs() -> &'static [(String, String)] {
    static PAIRS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    PAIRS.get_or_init(|| {
        let doc: Value = serde_json::from_str(MAP).unwrap_or_default();
        doc["map"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string())))
            .collect()
    })
}

/// `text` with rp's names replaced by viy's.
pub fn to_viy(text: &str) -> String {
    pairs()
        .iter()
        .fold(text.to_string(), |s, (rp, viy)| s.replace(rp.as_str(), viy))
}

/// Every string in `value` through [`to_viy`].
pub fn value_to_viy(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(to_viy(s)),
        Value::Array(items) => Value::Array(items.iter().map(value_to_viy).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (to_viy(k), value_to_viy(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// An HTTP response rpctl wrote, with viy's names and its Content-Length
/// set to the body's new length; anything else, or bytes that are not
/// UTF-8, unchanged.
pub fn http_response_to_viy(bytes: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return bytes.to_vec();
    };
    if !head.starts_with("HTTP/") {
        return bytes.to_vec();
    }
    let body = to_viy(body);
    let head: Vec<String> = head
        .split("\r\n")
        .map(|line| match line.split_once(": ") {
            Some((name, _)) if name.eq_ignore_ascii_case("content-length") => {
                format!("{name}: {}", body.len())
            }
            _ => to_viy(line),
        })
        .collect();
    format!("{}\r\n\r\n{body}", head.join("\r\n")).into_bytes()
}

/// An oracle answer with viy's names: strings through [`to_viy`], the
/// oracle's `{"$bytes": "<base64>"}` through [`http_response_to_viy`].
pub fn answer_to_viy(value: &Value) -> Value {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("$bytes") => {
            let Some(bytes) = map["$bytes"].as_str().and_then(|b| STANDARD.decode(b).ok()) else {
                return value.clone();
            };
            serde_json::json!({"$bytes": STANDARD.encode(http_response_to_viy(&bytes))})
        }
        Value::String(s) => Value::String(to_viy(s)),
        Value::Array(items) => Value::Array(items.iter().map(answer_to_viy).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (to_viy(k), answer_to_viy(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
#[path = "rp_names_tests.rs"]
mod tests;
