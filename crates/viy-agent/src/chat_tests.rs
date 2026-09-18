//! The turn through the engine against the scripted model server:
//! rpctl's AnswerGuaranteeTests and TitleTests, the history after a failed
//! or interrupted request, and the activity file touched while streaming.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use pretty_assertions::assert_eq;
use viy_http::ReqwestHttp;
use viy_protocol::Event;
use viy_protocol::Op;
use viy_testkit::mock_llm::MockLlm;

use super::*;
use crate::engine::Engine;

struct Rig {
    mock: MockLlm,
    turns: Arc<ChatTurns>,
    engine: Engine,
    touched: Arc<AtomicUsize>,
}

/// A chat on DEFAULTS with no tools and no title, plus `over`, against a
/// server that answers with `responses`.
fn rig(responses: Value, over: &[(&str, &str)]) -> Rig {
    let mock = MockLlm::start(&json!({"responses": responses})).unwrap();
    let mut cfg = Config::defaults();
    for (key, value) in [
        ("VLLM_API_KEY", "k"),
        ("TOOLS_ENABLED", "0"),
        ("SEARCH_ENABLED", "0"),
        ("CHAT_AUTO_TITLE", "0"),
    ]
    .iter()
    .chain(over)
    {
        cfg.set(key, value);
    }
    let backend = cfg.get("BACKEND").unwrap_or("runpod").to_string();
    let endpoint = Endpoint {
        base: mock.uri(),
        key: "k".into(),
        is_vllm: backend != "openai",
        backend,
        http: Arc::new(ReqwestHttp::new().unwrap()),
    };
    let touched = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&touched);
    let touch: Touch = Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
    });
    let chat = Chat::new(
        &cfg,
        endpoint,
        "m".into(),
        "2026-07-17 (Friday)".into(),
        touch,
    )
    .unwrap();
    let turns = Arc::new(ChatTurns::new(chat));
    let engine = Engine::spawn(turns.clone());
    Rig {
        mock,
        turns,
        engine,
        touched,
    }
}

impl Rig {
    /// Runs a turn to its end, and its title if it started one.
    async fn send(&mut self, text: &str) -> Vec<EventMsg> {
        let id = self
            .engine
            .submit(Op::UserTurn { text: text.into() })
            .unwrap();
        let (mut ended, mut naming) = (false, false);
        let mut seen = Vec::new();
        while !ended || naming {
            let Event { id: of, msg } = self.engine.next_event().await.unwrap();
            assert_eq!(of, id);
            match &msg {
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) => ended = true,
                EventMsg::Background(b) => naming = b.state == BackgroundState::Started,
                _ => {}
            }
            seen.push(msg);
        }
        seen
    }

    fn messages(&self) -> Vec<Value> {
        self.turns.with_chat(|c| c.messages().to_vec()).unwrap()
    }

    fn sent(&self) -> Vec<Value> {
        self.mock.chat_bodies()
    }
}

fn tools_mode(body: &Value) -> &str {
    match body.get("tools") {
        None => "omit",
        Some(_) => body["tool_choice"].as_str().unwrap_or("auto"),
    }
}

fn codes(events: &[EventMsg]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            EventMsg::Notice(n) => Some(n.code.clone()),
            _ => None,
        })
        .collect()
}

fn last_content(body: &Value) -> String {
    let msgs = body["messages"].as_array().unwrap();
    msgs.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_plain_turn_sends_the_history_and_keeps_no_reasoning() {
    let mut rig = rig(
        json!([
            {"stream": [{"reasoning": "think"}, {"content": "one"}]},
            {"stream": [{"content": "two"}]},
        ]),
        &[],
    );
    let first = rig.send("a").await;
    assert!(first.contains(&EventMsg::RoundStarted(RoundStartedEvent {
        round: 1,
        kind: RoundKind::Turn
    })));
    assert!(
        matches!(first.last(), Some(EventMsg::TurnComplete(t)) if t.outcome == TurnOutcome::Answered)
    );
    rig.send("b").await;
    assert_eq!(
        rig.messages(),
        [
            json!({"role": "user", "content": "a"}),
            json!({"role": "assistant", "content": "one", "thought": true}),
            json!({"role": "user", "content": "b"}),
            json!({"role": "assistant", "content": "two"}),
        ]
    );
    let sent = rig.sent();
    assert_eq!(sent[1]["messages"][0]["role"], "system");
    assert_eq!(
        sent[1]["messages"][2],
        json!({"role": "assistant", "content": "one"})
    );
    assert!(rig.touched.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn an_empty_reply_is_asked_again_without_tools() {
    let mut rig = rig(
        json!([{"stream": [{"reasoning": "hmm"}]}, {"stream": [{"content": "here it is"}]}]),
        &[("SEARCH_ENABLED", "1")],
    );
    let events = rig.send("look around").await;
    let sent = rig.sent();
    assert_eq!(
        sent.iter().map(tools_mode).collect::<Vec<_>>(),
        ["auto", "omit"]
    );
    assert!(last_content(&sent[1]).ends_with(&format!("\n\n{NO_ANSWER_NOTICE}")));
    assert_eq!(codes(&events), ["no_answer_retry"]);
    assert_eq!(
        rig.messages().last(),
        Some(&json!({"role": "assistant", "content": "here it is", "thought": true}))
    );
    // The notice was for that request only.
    assert_eq!(
        rig.messages()[0],
        json!({"role": "user", "content": "look around"})
    );
}

#[tokio::test]
async fn no_turn_is_committed_with_an_empty_answer() {
    let mut rig = rig(
        json!([{"stream": []}, {"stream": [{"content": "  "}]}]),
        &[],
    );
    let events = rig.send("look around").await;
    let text = rig.messages()[1]["content"].as_str().unwrap().to_string();
    assert!(text.starts_with("[no answer: the model stopped"), "{text}");
    assert_eq!(codes(&events), ["no_answer_retry", "no_answer"]);
}

#[tokio::test]
async fn a_cut_off_reply_is_not_retried() {
    let mut rig = rig(
        json!([{"stream": [{"reasoning": "long"}, {"finish": "length"},
                           {"usage": {"prompt_tokens": 9, "completion_tokens": 300, "total_tokens": 309}}]}]),
        &[("MAX_TOKENS", "300")],
    );
    let events = rig.send("prove it").await;
    assert_eq!(rig.sent().len(), 1);
    assert_eq!(
        rig.messages()[1]["content"],
        "[no answer: the reply hit the token limit first.]"
    );
    let cut = events.iter().find_map(|e| match e {
        EventMsg::Notice(n) if n.code == "length_cut" => Some(n.message.clone()),
        _ => None,
    });
    assert_eq!(
        cut.as_deref(),
        Some(
            "[stopped at the 300-token limit after 300 tokens before it reached an answer; the \
             reply is cut off. Raise the budget with /maxtok <n> (it also counts the reasoning trace).]"
        )
    );
}

#[tokio::test]
async fn a_tool_call_that_cannot_run_is_dropped_and_the_answer_asked_for() {
    let mut rig = rig(
        json!([
            {"stream": [{"content": "Let me search."},
                        {"tool_call": {"id": "c1", "name": "web_search", "arguments": "{\"query\": \"x\"}"}},
                        {"finish": "tool_calls"}]},
            {"stream": [{"content": "From memory: x."}]},
        ]),
        &[("SEARCH_ENABLED", "1")],
    );
    rig.send("search x").await;
    assert_eq!(
        rig.sent().iter().map(tools_mode).collect::<Vec<_>>(),
        ["auto", "omit"]
    );
    assert_eq!(
        rig.messages()[1],
        json!({"role": "assistant", "content": "From memory: x."})
    );
}

#[tokio::test]
async fn with_no_tool_rounds_the_first_request_is_the_capped_one() {
    let mut rig = rig(
        json!([{"stream": [{"content": "ok"}]}]),
        &[("SEARCH_ENABLED", "1"), ("SEARCH_MAX_ROUNDS", "0")],
    );
    let events = rig.send("hi").await;
    let sent = rig.sent();
    assert_eq!(tools_mode(&sent[0]), "none");
    assert!(last_content(&sent[0]).ends_with(TOOL_BUDGET_NOTICE));
    assert!(events.contains(&EventMsg::RoundStarted(RoundStartedEvent {
        round: 1,
        kind: RoundKind::CappedFinal
    })));
}

#[tokio::test]
async fn a_failed_request_with_no_progress_changes_nothing() {
    let mut rig = rig(
        json!([{"stream": [{"content": "b"}]}, {"status": 503, "body": "down"}]),
        &[],
    );
    rig.send("a").await;
    let events = rig.send("again").await;
    assert_eq!(rig.messages().len(), 2);
    assert!(matches!(
        events.last(),
        Some(EventMsg::TurnComplete(t)) if t.outcome == TurnOutcome::Failed { message: "the request failed".into() }
    ));
}

#[tokio::test]
async fn an_interrupted_turn_keeps_what_streamed() {
    let mut rig = rig(
        json!([{"stream": [{"content": "half an ans"}, {"delay": 30}]}]),
        &[],
    );
    let id = rig
        .engine
        .submit(Op::UserTurn {
            text: "build it".into(),
        })
        .unwrap();
    loop {
        let event = rig.engine.next_event().await.unwrap();
        if matches!(event.msg, EventMsg::AgentMessageContentDelta(_)) {
            break;
        }
    }
    rig.engine.submit(Op::Interrupt).unwrap();
    loop {
        let event = rig.engine.next_event().await.unwrap();
        if event.id == id && matches!(event.msg, EventMsg::TurnAborted(_)) {
            break;
        }
    }
    assert_eq!(
        rig.messages(),
        [
            json!({"role": "user", "content": "build it"}),
            json!({"role": "assistant", "content": "half an ans\n\n[interrupted]"}),
        ]
    );
}

#[tokio::test]
async fn an_interrupt_before_any_text_keeps_the_question() {
    let mut rig = rig(json!([{"delay": 30, "stream": []}]), &[]);
    let id = rig
        .engine
        .submit(Op::UserTurn {
            text: "long careful prompt".into(),
        })
        .unwrap();
    loop {
        if matches!(
            rig.engine.next_event().await.unwrap().msg,
            EventMsg::RoundStarted(_)
        ) {
            break;
        }
    }
    rig.engine.submit(Op::Interrupt).unwrap();
    while !matches!(rig.engine.next_event().await.unwrap(), Event { id: ref of, msg: EventMsg::TurnAborted(_) } if *of == id)
    {
    }
    assert_eq!(
        rig.messages()[1],
        json!({"role": "assistant", "content": "[interrupted]"})
    );
}

fn titled(responses: Value, over: &[(&str, &str)]) -> Rig {
    let mut over = over.to_vec();
    over.push(("CHAT_AUTO_TITLE", "1"));
    rig(responses, &over)
}

#[tokio::test]
async fn chat_names_itself_once_in_background() {
    let mut rig = titled(
        json!([
            {"stream": [{"content": "hi there"}]},
            {"message": "Saying hello"},
            {"stream": [{"content": "again"}]},
        ]),
        &[],
    );
    let first = rig.send("hello").await;
    rig.send("again").await;
    assert_eq!(rig.sent().len(), 3);
    let body = &rig.sent()[1];
    assert_eq!(body["max_tokens"], 20);
    assert_eq!(
        body["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
    assert!(first.contains(&EventMsg::TitleChanged(TitleChangedEvent {
        title: "Saying hello".into()
    })));
    assert_eq!(
        rig.turns.with_chat(|c| c.title()).unwrap().as_deref(),
        Some("Saying hello")
    );
}

#[tokio::test]
async fn failed_title_request_keeps_default_and_is_tried_again() {
    let mut rig = titled(
        json!([
            {"stream": [{"content": "hi"}]},
            {"status": 500, "body": "no"},
            {"stream": [{"content": "again"}]},
            {"message": "Second try"},
        ]),
        &[],
    );
    let first = rig.send("hello world").await;
    assert!(first.contains(&background(BackgroundState::Failed)));
    assert_eq!(rig.turns.with_chat(|c| c.title()).unwrap(), None);
    rig.send("again").await;
    assert_eq!(
        rig.turns.with_chat(|c| c.title()).unwrap().as_deref(),
        Some("Second try")
    );
}

#[tokio::test]
async fn hosted_backend_gets_no_vllm_fields() {
    let mut rig = titled(
        json!([{"stream": [{"content": "hi"}]}, {"message": "T"}]),
        &[("BACKEND", "openai")],
    );
    rig.send("hello").await;
    assert!(rig.sent()[1].get("chat_template_kwargs").is_none());
    assert!(rig.sent()[0].get("top_k").is_none());
}

#[tokio::test]
async fn no_title_without_autosave_or_an_answer() {
    let mut rig = titled(
        json!([{"stream": [{"content": "hi"}]}]),
        &[("CHAT_AUTOSAVE", "0")],
    );
    rig.send("hello").await;
    assert_eq!(rig.sent().len(), 1);
    let mut rig = titled(json!([{"stream": [{"finish": "length"}]}]), &[]);
    rig.send("hello").await;
    assert_eq!(rig.sent().len(), 1, "a placeholder answer names nothing");
}

#[tokio::test]
async fn the_context_length_comes_from_the_server() {
    let mock = MockLlm::start(&json!({"max_model_len": 262144, "responses": []})).unwrap();
    let endpoint = Endpoint {
        base: mock.uri(),
        key: "k".into(),
        backend: "runpod".into(),
        is_vllm: true,
        http: Arc::new(ReqwestHttp::new().unwrap()),
    };
    let mut chat = Chat::new(
        &Config::defaults(),
        endpoint,
        "m".into(),
        String::new(),
        Arc::new(|| {}),
    )
    .unwrap();
    assert_eq!(chat.max_model_len(), 65536);
    assert_eq!(chat.discover_max_model_len().await, 262144);
    let requests = mock.requests();
    assert_eq!(
        (requests[0].method.as_str(), requests[0].path.as_str()),
        ("GET", "/v1/models")
    );
}
