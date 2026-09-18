//! The request comparator against the cases compare_requests.py is
//! tested with (tools/parity/test_compare_requests.py).

use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn post(body: Value) -> Value {
    json!({"method": "POST", "path": "/v1/chat/completions",
           "headers": {"authorization": "Bearer k", "user-agent": "python-requests/2"},
           "body": body})
}

#[test]
fn key_order_counts_only_inside_tools() {
    let a = post(
        json!({"model": "m", "stream": true, "tools": [{"type": "function", "function": {"name": "x"}}]}),
    );
    let b = post(
        json!({"stream": true, "model": "m", "tools": [{"type": "function", "function": {"name": "x"}}]}),
    );
    assert_eq!(compare(std::slice::from_ref(&a), &[b]), None);
    let c = post(
        json!({"model": "m", "stream": true, "tools": [{"function": {"name": "x"}, "type": "function"}]}),
    );
    assert!(
        compare(&[a], &[c])
            .unwrap()
            .contains("body.tools[0]: key order differs")
    );
}

#[test]
fn json_in_strings_is_compared_parsed_and_prompts_as_strings() {
    let call = |args: &str| {
        post(json!({"messages": [
            {"role": "assistant", "tool_calls": [{"function": {"name": "f", "arguments": args}}]},
            {"role": "tool", "content": args},
        ]}))
    };
    assert_eq!(
        compare(
            &[call("{\"a\": 1, \"b\": 2}")],
            &[call("{\"b\":2,\"a\":1}")]
        ),
        None
    );
    let say = |text: &str| post(json!({"messages": [{"role": "system", "content": text}]}));
    let d = compare(&[say("Today is A.")], &[say("Today is B.")]).unwrap();
    assert!(d.starts_with("request 0 (POST /v1/chat/completions): body.messages[0].content: strings differ at char 9"), "{d}");
}

#[test]
fn numbers_compare_by_value_and_counts_and_headers_count() {
    assert_eq!(
        compare(&[post(json!({"t": 1.0}))], &[post(json!({"t": 1}))]),
        None
    );
    assert_eq!(
        compare(&[post(json!({}))], &[]),
        Some("A has 1 more request(s), the first: POST /v1/chat/completions".into())
    );
    let mut other = post(json!({}));
    other["headers"]["authorization"] = json!("Bearer z");
    assert!(
        compare(&[post(json!({}))], &[other])
            .unwrap()
            .ends_with("Authorization headers differ")
    );
    let mut named = post(json!({}));
    named["headers"]["user-agent"] = json!("viy/0.1");
    assert!(
        compare(&[post(json!({}))], &[named])
            .unwrap()
            .contains("names viy")
    );
}

#[test]
fn normalize_puts_placeholders_longest_first() {
    let v = json!({"/tmp/b/ws": ["in /tmp/b/ws and /tmp/b on 2026-07-17 (Friday)"]});
    assert_eq!(
        normalize(
            &v,
            &[
                ("/tmp/b", "{BASE}"),
                ("/tmp/b/ws", "{WORKSPACE}"),
                ("2026-07-17 (Friday)", "{TODAY}")
            ]
        ),
        json!({"{WORKSPACE}": ["in {WORKSPACE} and {BASE} on {TODAY}"]})
    );
}
