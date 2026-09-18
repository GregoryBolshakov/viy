//! rpctl's ReasoningTests, BrokenToolArgsTests and TitleTests on the
//! request bodies, and the message helpers.

use pretty_assertions::assert_eq;

use super::*;

fn settings(over: &[(&str, &str)]) -> Settings {
    let mut cfg = Config::defaults();
    for (key, value) in over {
        cfg.set(key, value);
    }
    let is_vllm = cfg.get("BACKEND") != Some("openai");
    Settings::from_config(&cfg, "m".into(), is_vllm).unwrap()
}

fn specs() -> Vec<Value> {
    vec![viy_tools::web::search_tool(), viy_tools::web::fetch_tool()]
}

#[test]
fn vllm_payload_carries_sampling_and_effort() {
    let body = settings(&[]).payload(
        &[json!({"role": "user", "content": "hi"})],
        &[],
        Tools::Auto,
        true,
    );
    assert_eq!(
        (&body["temperature"], &body["top_p"], &body["top_k"]),
        (&json!(1.0), &json!(0.95), &json!(20))
    );
    assert_eq!(
        body["chat_template_kwargs"],
        json!({"reasoning_effort": "medium", "preserve_thinking": false})
    );
    let low = settings(&[("REASONING_EFFORT", "low")]).payload(&[], &[], Tools::Auto, true);
    assert_eq!(low["chat_template_kwargs"]["reasoning_effort"], "low");
    assert_eq!(body["max_tokens"], 16384);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
}

#[test]
fn hosted_api_gets_no_vllm_fields() {
    let body = settings(&[("BACKEND", "openai")]).payload(&[], &[], Tools::Auto, true);
    assert!(body.get("top_k").is_none());
    assert!(body.get("chat_template_kwargs").is_none());
}

#[test]
fn thinking_off_changes_only_the_template_kwargs() {
    let s = settings(&[]);
    let on = s.payload(&[], &specs(), Tools::None, true);
    let off = s.payload(&[], &specs(), Tools::None, false);
    assert_eq!(
        off["chat_template_kwargs"],
        json!({"enable_thinking": false, "preserve_thinking": false})
    );
    let without = |mut b: Value| {
        b.as_object_mut().unwrap().remove("chat_template_kwargs");
        b
    };
    assert_eq!(without(on), without(off));
}

#[test]
fn a_bad_number_in_the_config_is_an_error() {
    let mut cfg = Config::defaults();
    cfg.set("MAX_TOKENS", "lots");
    assert_eq!(
        Settings::from_config(&cfg, "m".into(), true),
        Err("MAX_TOKENS=\"lots\" is not a number".into())
    );
}

const CUT: &str = r##"{"path": "a.py", "content": "#!/usr/bin/env python3\nimport"##;

#[test]
fn payload_repairs_a_saved_chat_without_touching_it() {
    let bad = json!({"role": "assistant", "content": null, "tool_calls": [
        {"id": "c1", "type": "function", "function": {"name": "write_file", "arguments": CUT}},
        {"id": "c2", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\": \"b\"}"}},
    ]});
    let history = vec![
        json!({"role": "user", "content": "go"}),
        bad.clone(),
        json!({"role": "tool", "tool_call_id": "c1", "content": "{}"}),
    ];
    let body = settings(&[]).payload(&history, &[], Tools::Auto, true);
    let sent = body["messages"].as_array().unwrap();
    for m in sent {
        for tc in m["tool_calls"].as_array().into_iter().flatten() {
            assert!(args_ok(&tc["function"]["arguments"]));
        }
    }
    assert_eq!(sent[1]["tool_calls"][0]["function"]["arguments"], "{}");
    assert_eq!(
        sent[1]["tool_calls"][1]["function"]["arguments"],
        "{\"path\": \"b\"}"
    );
    assert_eq!(history[1], bad);
    let good = vec![json!({"role": "user", "content": "hi"})];
    assert_eq!(
        settings(&[]).payload(&good, &[], Tools::Auto, true)["messages"],
        json!(good)
    );
}

#[test]
fn payload_tool_modes() {
    let s = settings(&[]);
    assert_eq!(
        s.payload(&[], &specs(), Tools::Auto, true)["tool_choice"],
        "auto"
    );
    let none = s.payload(&[], &specs(), Tools::None, true);
    assert_eq!(none["tool_choice"], "none");
    assert_eq!(
        none["tools"],
        s.payload(&[], &specs(), Tools::Auto, true)["tools"]
    );
    let omit = s.payload(&[], &specs(), Tools::Omit, true);
    assert!(omit.get("tools").is_none() && omit.get("tool_choice").is_none());
    // No tools offered: neither key, whatever the mode.
    assert!(
        s.payload(&[], &[], Tools::Auto, true)
            .get("tool_choice")
            .is_none()
    );
}

#[test]
fn the_server_never_sees_thought_marks() {
    let history = vec![
        json!({"role": "user", "content": "q"}),
        json!({"role": "assistant", "content": "a", "thought": true}),
    ];
    let body = settings(&[]).payload(&history, &[], Tools::Auto, true);
    assert_eq!(
        body["messages"][1],
        json!({"role": "assistant", "content": "a"})
    );
}

#[test]
fn a_closed_turn_keeps_that_it_thought_not_the_reasoning() {
    let turn = vec![
        json!({"role": "user", "content": "q"}),
        json!({"role": "assistant", "content": null, "reasoning": "plan", "tool_calls": []}),
    ];
    assert_eq!(
        without_reasoning(&turn),
        [
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": null, "tool_calls": [], "thought": true}),
        ]
    );
}

#[test]
fn a_notice_rides_on_the_last_message_for_one_request() {
    let msgs = vec![json!({"role": "user", "content": "hi"})];
    assert_eq!(
        with_notice(&msgs, "[n]"),
        [json!({"role": "user", "content": "hi\n\n[n]"})]
    );
    let parts = vec![json!({"role": "user", "content": [{"type": "text", "text": "look"}]})];
    assert_eq!(
        with_notice(&parts, "[n]")[0]["content"],
        json!([{"type": "text", "text": "look"}, {"type": "text", "text": "\n\n[n]"}])
    );
    let none = vec![json!({"role": "tool", "content": null})];
    assert_eq!(with_notice(&none, "[n]")[0]["content"], "\n\n[n]");
}

#[test]
fn clean_title() {
    use super::clean_title;
    assert_eq!(
        clean_title("Title: \"Fixing the SSH tunnel.\"\n").as_deref(),
        Some("Fixing the SSH tunnel")
    );
    assert_eq!(
        clean_title("<think>hmm</think>\n**Pod costs**").as_deref(),
        Some("Pod costs")
    );
    assert_eq!(clean_title("<think>never closed"), None);
    let long = clean_title(&"x ".repeat(80)).unwrap();
    assert!(long.ends_with('…'));
    assert_eq!(long.chars().count(), TITLE_CHARS);
    assert_eq!(
        clean_title("TITLE : Rust ports").as_deref(),
        Some("Rust ports")
    );
    assert_eq!(
        clean_title("Titles of books").as_deref(),
        Some("Titles of books")
    );
}

#[test]
fn the_title_names_the_first_exchange_with_an_answer() {
    let messages = vec![
        json!({"role": "user", "content": [{"type": "text", "text": "see"}, {"type": "image_url"}, {"type": "text", "text": "this"}]}),
        json!({"role": "assistant", "content": "[no answer: the reply hit the token limit first.]"}),
        json!({"role": "user", "content": "again"}),
        json!({"role": "assistant", "content": "It is a cat."}),
    ];
    let (user, reply) = title_pair(&messages).unwrap();
    assert_eq!(reply, "It is a cat.");
    let body = settings(&[]).title_body(user, reply);
    assert_eq!(
        body["messages"][0]["content"],
        "Write a title of 3 to 6 words for the conversation below. Reply with the title only: \
         no quotes, no trailing punctuation, no preamble.\n\nUser: see this\n\nAssistant: It is a cat."
    );
    assert_eq!(body["max_tokens"], 20);
    assert_eq!(
        body["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
    assert_eq!(title_pair(&messages[..2]), None);
}
