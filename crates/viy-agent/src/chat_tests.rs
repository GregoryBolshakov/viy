//! The turn through the engine against the scripted model server:
//! rpctl's AnswerGuaranteeTests and TitleTests, the history after a failed
//! or interrupted request, the activity file touched while streaming, and
//! the session the chat keeps (ChatAutosaveTests).

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use pretty_assertions::assert_eq;
use viy_http::ReqwestHttp;
use viy_protocol::Event;
use viy_protocol::Op;
use viy_testkit::mock_llm::MockLlm;
use viy_testkit::scratch::ScratchDir;
use viy_time::SystemTimeProvider;

use super::*;
use crate::engine::Engine;
use crate::sessions;

/// Places with no instruction files in them.
fn nowhere() -> Places {
    Places {
        workspace: std::path::PathBuf::from("/nonexistent/viy-workspace"),
        config_dir: std::path::PathBuf::from("/nonexistent/viy-config"),
    }
}

struct Rig {
    mock: MockLlm,
    turns: Arc<ChatTurns>,
    engine: Engine,
    touched: Arc<AtomicUsize>,
    store: Sessions,
    _scratch: ScratchDir,
}

/// A sessions directory of the test's own.
fn sessions_store() -> (ScratchDir, Sessions) {
    let scratch = ScratchDir::new("chat-sessions").unwrap();
    let store = Sessions::new(
        scratch.path().join("sessions"),
        Arc::new(SystemTimeProvider),
    );
    (scratch, store)
}

/// A chat on DEFAULTS with no tools and no title, plus `over`, against a
/// server that answers with `responses`, saved to a store of its own.
fn rig(responses: Value, over: &[(&str, &str)]) -> Rig {
    let mock = MockLlm::start(&json!({"responses": responses})).unwrap();
    let (scratch, store) = sessions_store();
    let touched = Arc::new(AtomicUsize::new(0));
    let chat = chat_touching(&mock, over, &store, Arc::clone(&touched));
    let turns = Arc::new(ChatTurns::new(chat));
    let engine = Engine::spawn(turns.clone());
    Rig {
        mock,
        turns,
        engine,
        touched,
        store,
        _scratch: scratch,
    }
}

/// The chat `rig` drives, for a test that calls it directly.
fn chat_at(mock: &MockLlm, over: &[(&str, &str)], store: &Sessions) -> Chat {
    chat_touching(mock, over, store, Arc::new(AtomicUsize::new(0)))
}

/// [`chat_at`], counting the activity file's touches in `touched`.
fn chat_touching(
    mock: &MockLlm,
    over: &[(&str, &str)],
    store: &Sessions,
    touched: Arc<AtomicUsize>,
) -> Chat {
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
    let touch: Touch = Arc::new(move || {
        touched.fetch_add(1, Ordering::SeqCst);
    });
    let mut chat = Chat::new(
        &cfg,
        nowhere(),
        endpoint,
        "m".into(),
        "2026-07-17 (Friday)".into(),
        touch,
    )
    .unwrap();
    chat.use_sessions(store.clone(), Some("/nonexistent/viy-project".into()));
    chat
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

    /// The chat's session log, replayed.
    fn logged_state(&self) -> sessions::State {
        let id = self.turns.with_chat(|c| c.session_id()).unwrap().unwrap();
        sessions::replay(&self.store.path_of(&id)).unwrap()
    }

    fn logged(&self) -> Vec<Value> {
        self.logged_state().messages
    }
}

fn tools_mode(body: &Value) -> &str {
    match body.get("tools") {
        None => "omit",
        Some(_) => body["tool_choice"].as_str().unwrap_or("auto"),
    }
}

/// The notices' codes, without the context length said before the first
/// request.
fn codes(events: &[EventMsg]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            EventMsg::Notice(n) if n.code != "context_length" => Some(n.code.clone()),
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
    assert_eq!(rig.logged(), rig.messages());
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
    // Written to the session as soon as it came.
    assert_eq!(rig.logged_state().title.as_deref(), Some("Saying hello"));
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
    assert_eq!(
        rig.turns.with_chat(|c| c.title()).unwrap().as_deref(),
        Some("hello world")
    );
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
        nowhere(),
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

#[tokio::test]
async fn the_first_request_to_a_server_asks_its_context_length_once() {
    let mut rig = rig(
        json!([{"stream": [{"content": "one"}]}, {"stream": [{"content": "two"}]}, {"stream": [{"content": "three"}]}]),
        &[],
    );
    let first = rig.send("a").await;
    rig.send("b").await;
    let said = first.iter().find_map(|e| match e {
        EventMsg::Notice(n) if n.code == "context_length" => Some(n.message.clone()),
        _ => None,
    });
    assert_eq!(
        said.as_deref(),
        Some(
            "context 65536 tokens, 49152 usable for history after the 16384-token reply budget (auto-compact at 85%)"
        )
    );
    let paths = |rig: &Rig| -> Vec<String> {
        rig.mock
            .requests()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path))
            .collect()
    };
    assert_eq!(
        paths(&rig),
        [
            "GET /v1/models",
            "POST /v1/chat/completions",
            "POST /v1/chat/completions"
        ]
    );
    // A replaced pod is asked again.
    rig.turns.with_chat(Chat::server_changed).unwrap();
    rig.send("c").await;
    assert_eq!(paths(&rig)[3], "GET /v1/models");
}

#[tokio::test]
async fn a_turn_calibrates_reports_the_context_and_keeps_its_numbers() {
    let prose = "The compiler checks every borrow twice before the fox jumps. ".repeat(30);
    let mut rig = rig(
        json!([
            {"stream": [{"reasoning": "abc"}, {"content": "fine"},
                        {"usage": {"prompt_tokens": 600, "completion_tokens": 12, "total_tokens": 612}}]},
            {"stream": [{"reasoning": "hmm"},
                        {"usage": {"prompt_tokens": 700, "completion_tokens": 5, "total_tokens": 705}}]},
            {"stream": [{"content": "retried"},
                        {"usage": {"prompt_tokens": 9000, "completion_tokens": 3, "total_tokens": 9003}}]},
        ]),
        &[],
    );
    let events = rig.send(&prose).await;
    let sent = rig.sent()[0]["messages"].as_array().unwrap().clone();
    let mut want = Estimator::default();
    want.calibrate(&sent, 0, Some(600));
    let cpt = rig.turns.with_chat(|c| c.chars_per_token()).unwrap();
    assert_eq!(cpt, want.chars_per_token);
    assert_ne!(cpt, crate::context::DEFAULT_CHARS_PER_TOKEN);
    let (used, budget) = rig
        .turns
        .with_chat(|c| (c.context_state(&[]).0, c.prompt_budget()))
        .unwrap();
    assert!(events.contains(&EventMsg::TokenCount(TokenCountEvent {
        used,
        budget,
        auto_compact_at: Some((budget as f64 * 0.85) as u64),
    })));
    let stats = rig
        .turns
        .with_chat(|c| c.stats().cloned())
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            stats.prompt_tokens,
            stats.completion_tokens,
            stats.reasoning_chars,
            stats.finish.as_deref()
        ),
        (Some(600), Some(12), 3, Some("stop"))
    );
    assert_eq!(stats.context_tokens, used);
    assert!(stats.ttft_s.is_some());
    assert_eq!(
        serde_json::to_value(&stats)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        [
            "elapsed_s",
            "ttft_s",
            "prompt_tokens",
            "completion_tokens",
            "tok_per_s",
            "reasoning_chars",
            "finish",
            "context_tokens",
            "context_budget",
            "context_pct",
            "chars_per_token"
        ]
    );
    // A retried turn sent no schemas the way they are counted: no calibration.
    rig.send("again").await;
    assert_eq!(rig.turns.with_chat(|c| c.chars_per_token()).unwrap(), cpt);
    let stats = rig
        .turns
        .with_chat(|c| c.stats().cloned())
        .unwrap()
        .unwrap();
    assert_eq!(
        (stats.prompt_tokens, stats.reasoning_chars),
        (Some(9000), 3)
    );
}

#[tokio::test]
async fn the_context_state_equals_rpctl() {
    let fixture = viy_testkit::fixtures::load("chat/context").unwrap();
    let states: Vec<&Value> = fixture
        .cases
        .iter()
        .filter(|c| c["kind"] == "states")
        .collect();
    assert_eq!(states.len(), 3);
    for case in states {
        let mut cfg = Config::defaults();
        for (key, value) in [
            ("VLLM_API_KEY", "k"),
            ("INSTRUCTIONS_ENABLED", "0"),
            ("TOOLS_ENABLED", "0"),
        ] {
            cfg.set(key, value);
        }
        for (key, value) in case["config"].as_object().unwrap() {
            cfg.set(key, value.as_str().unwrap());
        }
        let endpoint = Endpoint {
            base: "http://127.0.0.1:9".into(),
            key: "k".into(),
            backend: "runpod".into(),
            is_vllm: true,
            http: Arc::new(ReqwestHttp::new().unwrap()),
        };
        let mut chat = Chat::new(
            &cfg,
            nowhere(),
            endpoint,
            "m".into(),
            "2026-07-03 (Friday)".into(),
            Arc::new(|| {}),
        )
        .unwrap();
        chat.messages = case["history"].as_array().unwrap().clone();
        let extra = case["extra"].as_array().unwrap().clone();
        let name = case["name"].as_str().unwrap();
        assert_eq!(
            chat.system_prompt()
                .map(Value::String)
                .unwrap_or(Value::Null),
            case["system"],
            "{name}"
        );
        let (tokens, fraction) = chat.context_state(&extra);
        assert_eq!(
            (tokens, fraction),
            (
                case["tokens"].as_u64().unwrap(),
                case["fraction"].as_f64().unwrap()
            ),
            "{name}"
        );
    }
}

// ------------------------------------------------------------ sessions

#[tokio::test]
async fn finished_turn_is_logged() {
    let mut rig = rig(json!([{"stream": [{"content": "hi"}]}]), &[]);
    rig.send("hello").await;
    assert_eq!(rig.logged(), rig.messages());
    let roles: Vec<Value> = rig.messages().iter().map(|m| m["role"].clone()).collect();
    assert_eq!(roles, [json!("user"), json!("assistant")]);
}

#[tokio::test]
async fn user_message_is_on_disk_before_the_reply() {
    let mut rig = rig(json!([{"delay": 30, "stream": [{"content": "ok"}]}]), &[]);
    rig.engine
        .submit(Op::UserTurn {
            text: "first".into(),
        })
        .unwrap();
    while !matches!(
        rig.engine.next_event().await.unwrap().msg,
        EventMsg::RoundStarted(_)
    ) {}
    let path = rig.store.path_of(&rig_session(&rig));
    assert_eq!(
        sessions::replay(&path).unwrap().messages,
        [json!({"role": "user", "content": "first"})]
    );
    rig.engine.submit(Op::Interrupt).unwrap();
}

/// The session's id while a turn has the chat: its one log file.
fn rig_session(rig: &Rig) -> String {
    let files: Vec<_> = std::fs::read_dir(rig.store.dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    assert_eq!(files.len(), 1);
    files[0].file_stem().unwrap().to_string_lossy().into_owned()
}

#[tokio::test]
async fn an_interrupted_turn_is_logged_as_it_was_closed() {
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
    while !matches!(
        rig.engine.next_event().await.unwrap().msg,
        EventMsg::AgentMessageContentDelta(_)
    ) {}
    rig.engine.submit(Op::Interrupt).unwrap();
    while !matches!(rig.engine.next_event().await.unwrap(), Event { id: ref of, msg: EventMsg::TurnAborted(_) } if *of == id)
    {
    }
    assert_eq!(rig.messages()[1]["content"], "half an ans\n\n[interrupted]");
    assert_eq!(rig.logged(), rig.messages());
}

#[tokio::test]
async fn closed_terminal_saves_the_open_turn() {
    let mut rig = rig(json!([{"delay": 30, "stream": []}]), &[]);
    rig.engine
        .submit(Op::UserTurn {
            text: "long careful prompt".into(),
        })
        .unwrap();
    while !matches!(
        rig.engine.next_event().await.unwrap().msg,
        EventMsg::RoundStarted(_)
    ) {}
    let session = rig_session(&rig);
    rig.engine.submit(Op::Shutdown).unwrap();
    while !matches!(
        rig.engine.next_event().await.unwrap().msg,
        EventMsg::ShutdownComplete
    ) {}
    let path = rig.store.path_of(&session);
    let st = sessions::replay(&path).unwrap();
    assert_eq!(st.messages[0]["content"], "long careful prompt");
    assert_eq!(
        st.messages[1]["content"],
        "[interrupted: the chat was closed]"
    );
    // The log was released with its end.
    let events = sessions::read_events(&path).unwrap();
    assert_eq!(events.last().unwrap()["t"], "end");
    assert!(rig.store.open(&session).is_ok());
}

#[tokio::test]
async fn history_marks_where_the_model_thought_and_the_server_never_sees_it() {
    let mut rig = rig(
        json!([
            {"stream": [{"reasoning": "ok"}, {"content": "done"}]},
            {"stream": [{"content": "plain"}]},
        ]),
        &[],
    );
    rig.send("fix it").await;
    rig.send("again").await;
    let thought: Vec<Option<bool>> = rig
        .logged()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| m["thought"].as_bool())
        .collect();
    assert_eq!(thought, [Some(true), None]);
    let second = &rig.sent()[1]["messages"];
    assert!(!second.to_string().contains("thought"));
    assert!(rig.messages().iter().any(|m| m.get("thought").is_some()));
}

#[tokio::test]
async fn chat_redacts_only_when_enabled() {
    let tok = format!("hf_{}", "q".repeat(34));
    for (flag, masked) in [("0", false), ("1", true)] {
        let mut rig = rig(
            json!([{"stream": [{"content": "fine"}]}]),
            &[("CHAT_REDACT", flag)],
        );
        rig.send(&format!("token {tok}")).await;
        let id = rig.turns.with_chat(|c| c.session_id()).unwrap().unwrap();
        let raw = std::fs::read_to_string(rig.store.path_of(&id)).unwrap();
        assert_eq!(!raw.contains(&tok), masked, "CHAT_REDACT={flag}");
        // What the model reads is never masked.
        assert!(rig.sent()[0].to_string().contains(&tok));
    }
}

/// A chat for the session commands, called directly; its events go to
/// `rx`.
struct Direct {
    mock: MockLlm,
    chat: Chat,
    ctx: TurnContext,
    rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    store: Sessions,
    _scratch: ScratchDir,
}

fn direct(responses: Value, over: &[(&str, &str)]) -> Direct {
    let mock = MockLlm::start(&json!({"responses": responses})).unwrap();
    let (scratch, store) = sessions_store();
    let chat = chat_at(&mock, over, &store);
    let (ctx, rx) = TurnContext::detached();
    Direct {
        mock,
        chat,
        ctx,
        rx,
        store,
        _scratch: scratch,
    }
}

impl Direct {
    async fn send(&mut self, text: &str) {
        self.chat.send(&self.ctx, json!(text)).await;
    }

    /// The notices said since the last call.
    fn said(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            if let EventMsg::Notice(n) = event.msg
                && n.code != "context_length"
            {
                out.push(n.message);
            }
        }
        out
    }

    fn id(&self) -> String {
        self.chat.session_id().unwrap()
    }

    fn replayed(&self, id: &str) -> sessions::State {
        sessions::replay(&self.store.path_of(id)).unwrap()
    }
}

#[tokio::test]
async fn new_keeps_the_old_chat() {
    let mut d = direct(json!([{"stream": [{"content": "hi"}]}]), &[]);
    d.send("first chat").await;
    let first = d.id();
    d.said();
    d.chat.new_session(&d.ctx, None).await;
    assert_eq!(
        d.said(),
        [
            "saved 'first chat' -- viy chat -r to come back to it",
            "new chat"
        ]
    );
    assert_eq!(d.chat.messages(), Vec::<Value>::new());
    assert_ne!(d.id(), first);
    assert_eq!(d.replayed(&first).messages.len(), 2);
    assert!(d.chat.attach_session(&d.ctx, &first).await);
    assert_eq!(d.chat.messages().len(), 2);
}

#[tokio::test]
async fn fork_goes_on_in_a_copy_and_keeps_the_original() {
    let mut d = direct(
        json!([{"stream": [{"content": "hi"}]}, {"stream": [{"content": "there"}]}]),
        &[],
    );
    d.send("first chat").await;
    let first = d.id();
    d.said();
    d.chat.fork_session(&d.ctx, "").await;
    let said = d.said().join("\n");
    assert_ne!(d.id(), first);
    assert!(said.contains(&format!("/resume {first}")), "{said}");
    assert_eq!(d.chat.title().as_deref(), Some("first chat (fork)"));
    assert_eq!(d.chat.messages().len(), 2);
    d.send("only in the fork").await;
    d.chat.end_session(&d.ctx, "exit").await;
    assert_eq!(d.replayed(&first).messages.len(), 2);
    let twin = d.replayed(&d.id());
    assert_eq!(twin.messages.len(), 4);
    assert_eq!(twin.title.as_deref(), Some("first chat (fork)"));
    // The original is released: it can be resumed.
    assert!(d.chat.attach_session(&d.ctx, &first).await);
    assert_eq!(d.chat.messages().len(), 2);
}

#[tokio::test]
async fn fork_with_a_title_and_nothing_to_fork() {
    let mut d = direct(json!([{"stream": [{"content": "hi"}]}]), &[]);
    d.chat.fork_session(&d.ctx, "").await;
    assert_eq!(d.said(), ["nothing to fork yet"]);
    assert!(!d.store.path_of(&d.id()).exists());
    d.send("first chat").await;
    d.chat.fork_session(&d.ctx, "Plan B").await;
    d.chat.end_session(&d.ctx, "exit").await;
    let row = sessions::summary(&d.store.path_of(&d.id())).unwrap();
    assert_eq!(row.title, "Plan B");
    assert!(!row.title_auto);
}

#[tokio::test]
async fn rename_command() {
    let mut d = direct(json!([]), &[]);
    d.chat.rename_session(&d.ctx, "Design notes");
    d.chat.end_session(&d.ctx, "exit").await;
    assert_eq!(d.store.list(None)[0].title, "Design notes");
    assert_eq!(d.said(), ["renamed to 'Design notes'"]);
    d.chat.rename_session(&d.ctx, "");
    assert_eq!(d.said(), [format!("Design notes  [{}]", d.id())]);
}

#[tokio::test]
async fn without_autosave_there_is_no_session() {
    let mut d = direct(
        json!([{"stream": [{"content": "hi"}]}]),
        &[("CHAT_AUTOSAVE", "0")],
    );
    d.send("hello").await;
    assert_eq!(d.chat.session_id(), None);
    assert_eq!(d.store.list(None), []);
    d.chat.rename_session(&d.ctx, "x");
    d.chat.fork_session(&d.ctx, "").await;
    assert_eq!(
        d.said(),
        [
            "autosave is off (CHAT_AUTOSAVE=0); nothing to name",
            "autosave is off (CHAT_AUTOSAVE=0); there is nothing to fork"
        ]
    );
}

#[tokio::test]
async fn date_is_fixed_for_the_chat_and_a_new_chat_gets_today() {
    let mut d = direct(json!([]), &[]);
    let first = d.chat.system_prompt().unwrap();
    assert!(first.contains("Today is 2026-07-17 (Friday)."));
    d.chat.new_session(&d.ctx, None).await;
    let today = crate::prompt::today();
    assert_eq!(d.chat.today(), today);
    assert!(
        d.chat
            .system_prompt()
            .unwrap()
            .contains(&format!("Today is {today}."))
    );
}

#[tokio::test]
async fn resuming_a_chat_open_elsewhere_offers_a_copy() {
    let mut d = direct(json!([{"stream": [{"content": "hi"}]}]), &[]);
    let mut other = d.store.create(Some("/nonexistent/viy-project"), Some("m"));
    let msgs = [
        json!({"role": "user", "content": "held"}),
        json!({"role": "assistant", "content": "elsewhere"}),
    ];
    other.sync(Some("sys"), &msgs, false).unwrap();
    let held = other.id().to_string();

    // Declined: nothing changes.
    let ctx = d.ctx.clone();
    let answering = tokio::spawn(answer_next(d.rx, ctx.clone(), Answer::No));
    assert!(!d.chat.attach_session(&ctx, &held).await);
    let (prompt, rx) = answering.await.unwrap();
    d.rx = rx;
    assert_eq!(
        prompt,
        "that chat is open in another viy chat. Continue a copy of it instead? [Y/n] "
    );
    assert_eq!(d.chat.messages(), Vec::<Value>::new());

    // Accepted: a copy, the original left to its writer.
    let answering = tokio::spawn(answer_next(d.rx, ctx.clone(), Answer::Yes));
    assert!(d.chat.attach_session(&ctx, &held).await);
    d.rx = answering.await.unwrap().1;
    assert_ne!(d.id(), held);
    assert_eq!(d.chat.messages(), msgs);
    assert_eq!(d.chat.title().as_deref(), Some("held (fork)"));
    let copied = d.replayed(&d.id());
    assert_eq!(copied.meta["project"], "/nonexistent/viy-project");
    assert_eq!(copied.meta["model"], "m");
    d.send("go on").await;
    other.close("exit").unwrap();
    assert_eq!(d.replayed(&held).messages.len(), 2);
    assert_eq!(d.replayed(&d.id()).messages.len(), 4);
    let _ = &d.mock;
}

/// Answers the next question that comes out of `rx` with `answer`; its
/// prompt, and the receiver back.
async fn answer_next(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    ctx: TurnContext,
    answer: Answer,
) -> (String, tokio::sync::mpsc::UnboundedReceiver<Event>) {
    loop {
        if let EventMsg::QuestionRequest(q) = rx.recv().await.unwrap().msg {
            ctx.answer(&q.question_id, answer);
            return (q.question.prompt, rx);
        }
    }
}
