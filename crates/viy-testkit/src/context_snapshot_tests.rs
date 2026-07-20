use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::scenario;

#[test]
fn rp_recordings_match_their_snapshots() {
    for name in scenario::recorded().unwrap() {
        let log = scenario::rp_log(&name).unwrap();
        let text = format_context_snapshot(
            &name,
            &log.chat_bodies(),
            &ContextSnapshotOptions::default().include_settings(),
        );
        assert_snapshot(
            &scenario::dir().join(format!("{name}.rp.context.txt")),
            &text,
        )
        .unwrap();
    }
}

#[test]
fn windows_split_where_messages_diverge_or_settings_change() {
    let sys = json!({"role": "system", "content": "S"});
    let u1 = json!({"role": "user", "content": "one"});
    let a1 = json!({"role": "assistant", "content": "first\nsecond"});
    let u2 = json!({"role": "user", "content": "two"});
    let bodies = [
        json!({"stream": true, "messages": [sys, u1]}),
        json!({"stream": true, "messages": [sys, u1, a1, u2]}),
        json!({"stream": false, "max_tokens": 20, "messages": [u2]}),
    ];
    let text = format_context_snapshot("demo", &bodies, &ContextSnapshotOptions::default());
    assert_eq!(
        text,
        "Scenario: demo

## Window 1
-- request 1 (stream, tools omit) --
00:system:
    S
01:user:
    one
-- request 2 (stream, tools omit) --
02:assistant:
    first
    second
03:user:
    two

## Window 2 (after request 2: messages diverged at item 00, settings changed (max_tokens, stream))
-- request 3 (no stream, tools omit) --
00:user:
    two
"
    );
}

#[test]
fn tool_calls_results_and_long_text_render_readably() {
    let long = "é".repeat(200);
    let body = json!({"messages": [
        {"role": "assistant", "content": null, "reasoning": "think",
         "tool_calls": [{"id": "c1", "type": "function",
                         "function": {"name": "read_file", "arguments": "{\"path\": \"a\"}"}}]},
        {"role": "tool", "tool_call_id": "c1", "content": long},
        {"role": "user", "content": (0..30).map(|n| n.to_string()).collect::<Vec<_>>().join("\n")},
    ]});
    let text = format_context_snapshot(
        "t",
        &[body],
        &ContextSnapshotOptions::default().replace("read_file", "READ"),
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[4], "00:assistant:");
    assert_eq!(lines[5], "    [reasoning] think");
    assert_eq!(lines[6], r#"    -> READ(c1) {"path": "a"}"#);
    assert_eq!(lines[7], "01:tool[c1]:");
    assert_eq!(lines[8], format!("    {}... (200 chars)", "é".repeat(160)));
    assert_eq!(lines[9], "02:user:");
    assert_eq!(lines[10 + 23], "    23");
    assert_eq!(lines[10 + 24], "    ... (6 more lines)");
}
