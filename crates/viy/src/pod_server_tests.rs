//! The chat on its pod server (kb d041, d068): the engine, the chat and
//! [`PodServer`] together against a scripted RunPod and vLLM (RouteHttp,
//! nothing on the network), on paused time: a line without a ready pod is
//! given back and kept nowhere, a pod brought up elsewhere is noticed and
//! the turn goes to it, an up in another terminal shows the pod booting,
//! `/up` boots in the background with rpctl's failover question as a
//! question box while the chat stays usable, Ctrl-C stops a command
//! running beside the chat, quitting stops a boot as Ctrl-C does, `/down`
//! stops this chat's own up first, and the commands are parsed as the
//! subcommands are.

use std::sync::atomic::AtomicUsize;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_agent::Engine;
use viy_agent::chat::ChatTurns;
use viy_http::Method;
use viy_protocol::Answer;
use viy_protocol::Event;
use viy_protocol::Op;
use viy_protocol::TurnRefusedEvent;
use viy_testkit::exec::FakeExec;
use viy_testkit::exec::Reply as Ran;
use viy_testkit::http::Reply;
use viy_testkit::http::RouteHttp;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::sshd::free_port;

use super::*;

const PROXY: &str = "https://pod111-18999.proxy.runpod.net";

struct Rig {
    _dir: ScratchDir,
    paths: Paths,
    api: Arc<RouteHttp>,
    turns: Arc<ChatTurns>,
    engine: Engine,
}

fn config() -> Config {
    let mut cfg = Config::defaults();
    for (k, v) in [
        ("RUNPOD_API_KEY", "rpa_SYNTHETICKEY0000"),
        ("POD_NAME", "qwen-pod"),
        ("NETWORK_VOLUME_ID", "vol222"),
        ("VLLM_API_KEY", "sk-SYNTHETICENDPOINT"),
        ("TUNNEL", "0"),
        ("VLLM_PORT", "18999"),
        ("CHAT_AUTO_TITLE", "0"),
        ("CUDA_VERSIONS", ""),
        ("TOOLS_ENABLED", "0"),
    ] {
        cfg.set(k, v);
    }
    cfg.set("TUNNEL_LOCAL_PORT", &free_port().unwrap().to_string());
    cfg
}

/// The chat on its server, with `state` as pod.json and `answer` as
/// RunPod and vLLM; the engine started.
fn rig(
    state: Option<Value>,
    answer: impl Fn(&viy_http::Request) -> Reply + Send + Sync + 'static,
) -> Rig {
    let dir = ScratchDir::new("pod-server").unwrap();
    let paths = Paths::under(dir.path().join("home"));
    if let Some(state) = state {
        write_state(&paths, &state);
    }
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let cfg = config();
    let api = Arc::new(RouteHttp::new(answer));
    let http: Arc<dyn HttpClient> = api.clone();
    let chat = crate::dev::chat(&cfg, "", Arc::clone(&http), &paths, &project, true).unwrap();
    let server = PodServer::new(Parts {
        cfg,
        paths: paths.clone(),
        http,
        // A spawn starts nothing: a fork here would hold the session logs
        // of the tests beside it locked until it execs.
        exec: Arc::new(FakeExec::new(vec![Ran::Exited(0); 8])),
        time: Arc::new(viy_time::SystemTimeProvider),
        exe: PathBuf::from("/opt/viy/bin/viy"),
        url: None,
        desktop: false,
    });
    let turns = Arc::new(ChatTurns::with_server(chat, server));
    let engine = Engine::spawn(turns.clone());
    Rig {
        _dir: dir,
        paths,
        api,
        turns,
        engine,
    }
}

fn write_state(paths: &Paths, state: &Value) {
    std::fs::create_dir_all(paths.pod_state_dir()).unwrap();
    std::fs::write(paths.pod_state_file(), state.to_string()).unwrap();
}

fn tracked() -> Value {
    json!({"pod_id": "pod111", "volume_id": "vol222", "tunnel_only": false, "cost_per_hr": 1.89})
}

fn json_reply(value: Value) -> Reply {
    Reply::answer(200, value.to_string())
}

/// A streamed reply saying `text`.
fn stream(text: &str) -> Reply {
    let chunk = |delta: Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        )
    };
    Reply::answer(
        200,
        format!(
            "{}{}data: [DONE]\n\n",
            chunk(json!({"content": text}), Value::Null),
            chunk(json!({}), json!("stop"))
        ),
    )
}

/// vLLM behind RunPod's proxy: healthy, 32768 tokens, answering "4".
fn vllm(r: &viy_http::Request) -> Option<Reply> {
    let url = r.url.as_str();
    if url == format!("{PROXY}/health") {
        Some(Reply::answer(200, ""))
    } else if url == format!("{PROXY}/v1/models") {
        Some(json_reply(
            json!({"data": [{"id": "x", "max_model_len": 32768}]}),
        ))
    } else if url == format!("{PROXY}/v1/chat/completions") {
        Some(stream("4"))
    } else {
        None
    }
}

impl Rig {
    /// Events until `ends` says one ends the wait, inclusive.
    async fn until(&mut self, ends: impl Fn(&EventMsg) -> bool) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Some(event) = self.engine.next_event().await {
            let last = ends(&event.msg);
            seen.push(event);
            if last {
                return seen;
            }
        }
        panic!("the engine stopped: {seen:#?}");
    }

    async fn line(&mut self, text: &str) -> Vec<Event> {
        let id = self
            .engine
            .submit(Op::UserTurn { text: text.into() })
            .unwrap();
        let ended = move |m: &EventMsg| {
            matches!(
                m,
                EventMsg::TurnRefused(_) | EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
            )
        };
        let events = self.until(ended).await;
        assert_eq!(events.last().unwrap().id, id);
        events
    }

    async fn command(&mut self, text: &str) -> Vec<Event> {
        let id = self
            .engine
            .submit(Op::Command { text: text.into() })
            .unwrap();
        let events = self
            .until(|m| matches!(m, EventMsg::CommandComplete(_)))
            .await;
        assert_eq!(events.last().unwrap().id, id);
        events
    }

    async fn pod_state(&mut self, want: PodState) -> PodStatusEvent {
        let events = self
            .until(|m| matches!(m, EventMsg::PodStatus(s) if s.state == want))
            .await;
        match &events.last().unwrap().msg {
            EventMsg::PodStatus(s) => s.clone(),
            _ => unreachable!(),
        }
    }

    fn sent(&self) -> Vec<String> {
        self.api
            .sent()
            .iter()
            .filter(|r| !r.url.contains("/logs"))
            .map(|r| format!("{} {}", r.method, r.url))
            .collect()
    }

    fn messages(&self) -> usize {
        self.turns.with_chat(|c| c.messages().len()).unwrap()
    }
}

fn notices(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.msg {
            EventMsg::Notice(n) => Some(n.message.clone()),
            _ => None,
        })
        .collect()
}

fn refused(events: &[Event]) -> Option<TurnRefusedEvent> {
    events.iter().find_map(|e| match &e.msg {
        EventMsg::TurnRefused(r) => Some(r.clone()),
        _ => None,
    })
}

/// Whether `task` is a run of pod command `name`.
fn is_pod(task: &BackgroundTask, name: &str) -> bool {
    matches!(task, BackgroundTask::Pod { command, .. } if command == name)
}

fn no_runpod(r: &viy_http::Request) -> Reply {
    panic!("unexpected {} {}", r.method, r.url)
}

#[tokio::test(start_paused = true)]
async fn a_chat_without_a_pod_gives_the_line_back_and_keeps_nothing() {
    let mut rig = rig(None, no_runpod);
    let status = rig.pod_state(PodState::Absent).await;
    assert_eq!((status.backend.as_str(), status.gpu), ("runpod", None));
    let events = rig.line("what is 2+2").await;
    assert_eq!(
        refused(&events),
        Some(TurnRefusedEvent {
            text: "what is 2+2".into(),
            message: "no pod is running: /up starts one.".into(),
        })
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.msg, EventMsg::TurnStarted(_)))
    );
    assert_eq!(rig.messages(), 0);
    assert_eq!(rig.turns.with_chat(|c| c.saved()), Some(false));
    // What works without the model still does.
    let help = rig.command("/stats").await;
    assert_eq!(notices(&help), ["{}"]);
    assert_eq!(rig.sent(), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn a_pod_brought_up_elsewhere_is_noticed_and_the_turn_goes_to_it() {
    let healthy = Arc::new(AtomicUsize::new(0));
    let up = Arc::clone(&healthy);
    let mut rig = rig(None, move |r| {
        if up.load(Ordering::SeqCst) == 0 && r.url.ends_with("/health") {
            return Reply::answer(503, "");
        }
        vllm(r).unwrap_or_else(|| no_runpod(r))
    });
    rig.pod_state(PodState::Absent).await;
    // rp up in another terminal: pod.json first, vLLM not answering yet.
    write_state(&rig.paths, &tracked());
    let events = rig.line("what is 2+2").await;
    assert_eq!(
        refused(&events).unwrap().message,
        "the pod is not answering: /status says why, /up brings it back."
    );
    healthy.store(1, Ordering::SeqCst);
    let became = rig
        .until(|m| matches!(m, EventMsg::PodStatus(s) if s.state == PodState::Ready))
        .await;
    // rp chat's preflight warning, once the pod is there.
    assert!(
        notices(&became)
            .iter()
            .any(|n| n.starts_with("This pod is reached through RunPod's proxy")),
        "{became:#?}"
    );
    let Some(EventMsg::PodStatus(status)) = became.last().map(|e| e.msg.clone()) else {
        unreachable!()
    };
    assert_eq!(status.price_usd_per_hour, Some(1.89));
    assert_eq!(status.tunnel_up, None);
    assert!(status.idle_seconds_left.is_some());
    let events = rig.line("what is 2+2").await;
    assert!(
        events.iter().any(|e| matches!(&e.msg, EventMsg::Notice(n) if n.message.starts_with("context 32768 tokens"))),
        "{events:#?}"
    );
    assert_eq!(rig.messages(), 2);
    let sent = rig.sent();
    assert!(sent.contains(&format!("GET {PROXY}/v1/models")), "{sent:?}");
    assert_eq!(
        sent.last().map(String::as_str),
        Some(format!("POST {PROXY}/v1/chat/completions").as_str())
    );
    let body: Value =
        serde_json::from_slice(rig.api.sent().last().unwrap().body.as_deref().unwrap()).unwrap();
    assert_eq!(
        body["model"],
        Config::defaults().get("SERVED_MODEL_NAME").unwrap()
    );
}

/// Another rp or viy holding the pod lock: booting for an up, stopping for
/// a stop or down; this process's own hold says nothing (its commands do).
/// The whole path, a real rp holding it, is the pty test's.
#[test]
fn the_lock_holder_says_what_another_terminal_does() {
    let holder = |verb: &str, pid| Holder {
        tool: Some(viy_pod::process::Tool::Rp),
        verb: verb.into(),
        pid,
    };
    assert_eq!(
        holder_state(holder("up", 42), 7),
        Some(PodState::Booting {
            stage: "rp up in another terminal".into()
        })
    );
    assert_eq!(
        holder_state(holder("down", 42), 7),
        Some(PodState::Stopping)
    );
    assert_eq!(holder_state(holder("up", 7), 7), None);
}

/// RunPod with no GPU on the primary and one on the backup volume, and
/// the pod there answering at once (rpctl's failover, viy-pod's test).
fn failover_runpod(r: &viy_http::Request) -> Reply {
    let backup = json!({"id": "bk", "name": "qwen38-weights-backup", "dataCenterId": "US-MO-1"});
    let volumes = json!([
        {"id": "vol222", "name": "qwen38-weights", "dataCenterId": "EUR-IS-1"},
        backup,
    ]);
    let url = r.url.as_str();
    let body: Value = r
        .body
        .as_deref()
        .and_then(|b| serde_json::from_slice(b).ok())
        .unwrap_or_default();
    if let Some(reply) = vllm(r) {
        reply
    } else if url.contains("/logs") {
        Reply::answer(200, "")
    } else if url.ends_with("/v1/pods") && r.method == Method::GET {
        json_reply(json!([]))
    } else if url.ends_with("/v1/pods") && body["dataCenterIds"] == json!(["US-MO-1"]) {
        json_reply(json!({"id": "pod111", "costPerHr": 1.89, "networkVolumeId": "bk"}))
    } else if url.ends_with("/v1/pods") {
        Reply::answer(
            500,
            json!({"error": "There are no instances currently available"}).to_string(),
        )
    } else if url.ends_with("/networkvolumes/vol222") {
        json_reply(volumes[0].clone())
    } else if url.ends_with("/v1/networkvolumes") {
        json_reply(volumes)
    } else if url.ends_with("/pods/pod111") {
        json_reply(
            json!({"id": "pod111", "name": "qwen-pod", "desiredStatus": "RUNNING", "costPerHr": 1.89}),
        )
    } else {
        no_runpod(r)
    }
}

#[tokio::test(start_paused = true)]
async fn up_boots_in_the_background_and_asks_the_failover_question_in_a_box() {
    let mut rig = rig(None, failover_runpod);
    rig.pod_state(PodState::Absent).await;
    // The command is done at once; the boot goes on beside the chat.
    let started = rig.command("/up").await;
    assert!(started.iter().any(|e| matches!(
        &e.msg,
        EventMsg::Background(b) if is_pod(&b.task, "up")
            && b.state == BackgroundState::Started
    )));
    let asked = rig
        .until(|m| matches!(m, EventMsg::QuestionRequest(_)))
        .await;
    let Some(EventMsg::QuestionRequest(question)) = asked.last().map(|e| e.msg.clone()) else {
        unreachable!()
    };
    assert!(
        question
            .question
            .prompt
            .contains("Try the backup volume in US-MO-1?"),
        "{question:?}"
    );
    // While it waits, the chat is usable, and the pod is booting.
    let stats = rig.command("/stats").await;
    assert_eq!(notices(&stats), ["{}"]);
    let events = rig.line("hi").await;
    assert!(
        refused(&events)
            .unwrap()
            .message
            .starts_with("the pod is still booting ("),
        "{events:#?}"
    );
    rig.engine
        .submit(Op::QuestionAnswer {
            id: question.question_id,
            answer: Answer::Yes,
        })
        .unwrap();
    let booted = rig
        .until(|m| matches!(m, EventMsg::Background(b) if b.state == BackgroundState::Done))
        .await;
    let said = notices(&booted);
    assert!(
        said.iter().any(|n| n.starts_with("vLLM is serving.")),
        "{said:#?}"
    );
    rig.pod_state(PodState::Ready).await;
    let answered = rig.line("what is 2+2").await;
    assert!(refused(&answered).is_none(), "{answered:#?}");
    assert_eq!(rig.messages(), 2);
}

/// RunPod whose log follows forever.
fn logs_runpod(r: &viy_http::Request) -> Reply {
    if r.url.contains("/logs") {
        Reply::Hang
    } else if r.url.ends_with("/pods/pod111") {
        json_reply(json!({"id": "pod111", "desiredStatus": "RUNNING"}))
    } else {
        vllm(r).unwrap_or_else(|| no_runpod(r))
    }
}

#[tokio::test(start_paused = true)]
async fn ctrl_c_with_no_turn_stops_a_command_running_beside_the_chat() {
    let mut rig = rig(Some(tracked()), logs_runpod);
    rig.command("/logs -f").await;
    rig.engine.submit(Op::Interrupt).unwrap();
    let ended = rig
        .until(|m| {
            matches!(m, EventMsg::Background(b)
                if is_pod(&b.task, "logs")
                && matches!(b.state, BackgroundState::Done | BackgroundState::Failed))
        })
        .await;
    assert!(!ended.is_empty());
}

#[tokio::test(start_paused = true)]
async fn quitting_stops_a_boot_as_ctrl_c_does() {
    let mut rig = rig(None, |r| {
        if r.url.ends_with("/health") {
            // Never ready.
            return Reply::answer(503, "");
        }
        failover_runpod(r)
    });
    rig.command("/up").await;
    let asked = rig
        .until(|m| matches!(m, EventMsg::QuestionRequest(_)))
        .await;
    let Some(EventMsg::QuestionRequest(q)) = asked.last().map(|e| e.msg.clone()) else {
        unreachable!()
    };
    rig.engine
        .submit(Op::QuestionAnswer {
            id: q.question_id,
            answer: Answer::Yes,
        })
        .unwrap();
    rig.until(|m| matches!(m, EventMsg::Notice(n) if n.message.contains("pod id:")))
        .await;
    rig.engine.submit(Op::Shutdown).unwrap();
    let closed = rig.until(|m| matches!(m, EventMsg::ShutdownComplete)).await;
    assert!(
        closed
            .iter()
            .any(|e| matches!(&e.msg, EventMsg::Notice(n) if n.message == "/up: interrupted")),
        "{closed:#?}"
    );
    // The interrupted boot was recorded, as after Ctrl-C at `viy up`.
    let row = Records::new(&rig.paths).history(1).pop().unwrap();
    assert_eq!(row["outcome"], "interrupted");
}

#[tokio::test(start_paused = true)]
async fn the_commands_are_parsed_as_their_subcommands() {
    let mut rig = rig(None, no_runpod);
    let bad = rig.command("/up --timeout soon").await;
    let said = notices(&bad);
    assert!(
        said[0].starts_with("error: invalid value 'soon' for '--timeout <TIMEOUT>'"),
        "{said:?}"
    );
    let help = rig.command("/up --help").await;
    assert!(notices(&help)[0].contains("Usage: viy up"), "{help:#?}");
    let install = rig.command("/reap --install").await;
    assert_eq!(
        notices(&install),
        ["/reap --install: run viy reap --install in a terminal"]
    );
    let foreground = rig.command("/shutdown 90m --foreground").await;
    assert!(notices(&foreground)[0].starts_with("/shutdown --foreground: a countdown needs"));
    let boots = rig.command("/boots").await;
    assert_eq!(notices(&boots).len(), 1);
    // Not a pod command: the chat's own.
    let unknown = rig.command("/upp").await;
    assert_eq!(notices(&unknown), ["unknown command: /upp  (try /help)"]);
    assert_eq!(rig.sent(), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn down_stops_this_chats_own_up_first() {
    // 0: no pod yet; 1: up created it; 2: down deleted it.
    let pod = Arc::new(AtomicUsize::new(0));
    let deleted = Arc::clone(&pod);
    let seen = Arc::clone(&pod);
    let mut rig = rig(None, move |r| {
        if r.url.ends_with("/health") {
            return Reply::answer(503, "");
        }
        if r.method == Method::DELETE {
            seen.store(2, Ordering::SeqCst);
            return json_reply(json!({}));
        }
        if r.url.ends_with("/v1/pods") && r.method == Method::GET {
            return json_reply(if seen.load(Ordering::SeqCst) == 1 {
                json!([{"id": "pod111", "name": "qwen-pod", "desiredStatus": "RUNNING"}])
            } else {
                json!([])
            });
        }
        let reply = failover_runpod(r);
        if r.url.ends_with("/v1/pods") && matches!(&reply, Reply::Answer { status: 200, .. }) {
            seen.store(1, Ordering::SeqCst);
        }
        reply
    });
    rig.command("/up").await;
    let asked = rig
        .until(|m| matches!(m, EventMsg::QuestionRequest(_)))
        .await;
    let Some(EventMsg::QuestionRequest(q)) = asked.last().map(|e| e.msg.clone()) else {
        unreachable!()
    };
    rig.engine
        .submit(Op::QuestionAnswer {
            id: q.question_id,
            answer: Answer::Yes,
        })
        .unwrap();
    rig.until(|m| matches!(m, EventMsg::Notice(n) if n.message.contains("pod id:")))
        .await;
    rig.command("/down").await;
    let downed = rig
        .until(|m| {
            matches!(m, EventMsg::Background(b)
                if is_pod(&b.task, "down")
                && matches!(b.state, BackgroundState::Done | BackgroundState::Failed))
        })
        .await;
    let said = notices(&downed);
    let up_ended = said.iter().position(|n| n == "/up: interrupted");
    let watching = said
        .iter()
        .position(|n| n.starts_with("The killed viy up may already have asked RunPod"));
    assert!(
        matches!((up_ended, watching), (Some(a), Some(b)) if a < b),
        "{said:#?}"
    );
    assert_eq!(deleted.load(Ordering::SeqCst), 2);
    rig.pod_state(PodState::Absent).await;
}

/// A stop or a down is let finish when the chat closes: quitting must not
/// leave a pod billing that the user asked to stop.
#[tokio::test(start_paused = true)]
async fn quitting_lets_a_down_finish() {
    let pod = Arc::new(AtomicUsize::new(1));
    let seen = Arc::clone(&pod);
    let mut rig = rig(Some(tracked()), move |r| {
        if r.method == Method::DELETE {
            seen.store(0, Ordering::SeqCst);
            return json_reply(json!({}));
        }
        if r.url.ends_with("/v1/pods") && r.method == Method::GET {
            return json_reply(if seen.load(Ordering::SeqCst) == 1 {
                json!([{"id": "pod111", "name": "qwen-pod", "desiredStatus": "RUNNING"}])
            } else {
                json!([])
            });
        }
        vllm(r).unwrap_or_else(|| no_runpod(r))
    });
    rig.command("/down").await;
    rig.engine.submit(Op::Shutdown).unwrap();
    let closed = rig.until(|m| matches!(m, EventMsg::ShutdownComplete)).await;
    let said = notices(&closed);
    assert!(
        said.iter().any(|n| n.starts_with("Terminated pod111.")),
        "{said:#?}"
    );
    assert!(!said.iter().any(|n| n == "/down: interrupted"), "{said:#?}");
    assert_eq!(pod.load(Ordering::SeqCst), 0);
}

/// Off a terminal the chat waits for a pod command before it closes, as a
/// terminal waits for the subcommand.
#[tokio::test(start_paused = true)]
async fn the_plain_chat_waits_for_a_pod_command() {
    let mut rig = rig(Some(tracked()), |r| {
        if r.url.ends_with("/pods/pod111") {
            return json_reply(
                json!({"id": "pod111", "name": "qwen-pod", "desiredStatus": "RUNNING", "costPerHr": 1.89}),
            );
        }
        vllm(r).unwrap_or_else(|| no_runpod(r))
    });
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tx.send("/status".to_string()).unwrap();
    drop(tx);
    let mut out = Vec::new();
    crate::dev::plain_chat(&mut rig.engine, Vec::new(), rx, &mut out)
        .await
        .unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("vllm:      ready"), "{out}");
}
