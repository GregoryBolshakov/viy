//! The slash commands (WP 3.12): rpctl's RewindTests and
//! ResumeShowsWholeChatTests (test_sessions.py) where they run without
//! tools, and each command's words.

use pretty_assertions::assert_eq;
use viy_protocol::Answer;
use viy_protocol::HistoryItem;
use viy_protocol::RewriteReason;

use super::*;

fn ok(n: usize) -> Value {
    json!(vec![json!({"stream": [{"content": "ok"}]}); n])
}

fn turn(n: usize) -> Vec<Value> {
    vec![
        json!({"role": "user", "content": format!("question {n}")}),
        json!({"role": "assistant", "content": format!("answer {n}")}),
    ]
}

fn summary_pair(text: &str) -> Vec<Value> {
    vec![
        json!({"role": "user", "content": format!(
            "[Summary of the conversation so far. Continue from here.]\n\n{text}")}),
        json!({"role": "assistant", "content": "Understood. I'll continue from the summary above."}),
    ]
}

impl Direct {
    async fn cmd(&mut self, line: &str) {
        self.chat.command(&self.ctx, line).await;
    }

    /// Every event since the last call.
    fn events(&mut self) -> Vec<EventMsg> {
        std::iter::from_fn(|| self.rx.try_recv().ok())
            .map(|e| e.msg)
            .collect()
    }

    /// `line` run with the next question answered `answer`; its question
    /// and the events.
    async fn cmd_answering(&mut self, line: &str, answer: Answer) -> (Question, Vec<EventMsg>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let rx_old = std::mem::replace(&mut self.rx, rx);
        let ctx = self.ctx.clone();
        let answering = tokio::spawn(async move {
            let mut rx_old = rx_old;
            let mut seen = Vec::new();
            loop {
                let event = rx_old.recv().await.unwrap();
                if let EventMsg::QuestionRequest(q) = &event.msg {
                    ctx.answer(&q.question_id, answer);
                    return (q.question.clone(), seen, rx_old);
                }
                seen.push(event.msg);
            }
        });
        self.chat.command(&self.ctx, line).await;
        let (question, mut seen, rx_old) = answering.await.unwrap();
        drop(tx);
        self.rx = rx_old;
        seen.extend(self.events());
        (question, seen)
    }
}

fn notices(events: &[EventMsg]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            EventMsg::Notice(n) if n.code != "context_length" => Some(n.message.clone()),
            _ => None,
        })
        .collect()
}

fn rewritten(events: &[EventMsg]) -> Option<(RewriteReason, Vec<HistoryItem>)> {
    events.iter().find_map(|e| match e {
        EventMsg::HistoryRewritten(h) => Some((h.reason, h.items.clone())),
        _ => None,
    })
}

fn prefill(events: &[EventMsg]) -> Option<String> {
    events.iter().find_map(|e| match e {
        EventMsg::InputPrefill(p) => Some(p.text.clone()),
        _ => None,
    })
}

fn user(text: &str) -> HistoryItem {
    HistoryItem::User { text: text.into() }
}

fn agent(text: &str) -> HistoryItem {
    HistoryItem::Agent { text: text.into() }
}

// ------------------------------------------------------------- /rewind

#[tokio::test]
async fn rewind_drops_the_turns_and_puts_the_message_back() {
    let mut d = direct(ok(3), &[]);
    for text in ["first", "second", "third"] {
        d.send(text).await;
    }
    d.events();
    d.cmd("/rewind 2").await;
    let events = d.events();
    let contents: Vec<Value> = d
        .chat
        .messages()
        .iter()
        .map(|m| m["content"].clone())
        .collect();
    assert_eq!(contents, [json!("first"), json!("ok")]);
    assert_eq!(prefill(&events).as_deref(), Some("second"));
    let said = notices(&events);
    assert!(
        said.iter().any(|s| s.contains("4 messages left the chat")),
        "{said:?}"
    );
    assert_eq!(
        rewritten(&events),
        Some((RewriteReason::Rewind, vec![user("first"), agent("ok")]))
    );
    assert_eq!(d.replayed(&d.id()).messages, d.chat.messages());
    // The log keeps what was dropped.
    assert_eq!(d.store.search("third", None)[0].id, d.id());
}

#[tokio::test]
async fn rewind_without_a_number_picks_and_enter_takes_the_last() {
    let mut d = direct(ok(2), &[]);
    d.send("first").await;
    d.send("second").await;
    d.events();
    let (question, events) = d.cmd_answering("/rewind", Answer::Pick(1)).await;
    assert_eq!(question.options, ["first", "second"]);
    assert_eq!(question.default, Answer::Pick(1));
    assert_eq!(question.unattended, Answer::No);
    assert_eq!(
        format!("{} {}", question.prompt, question.hint()),
        "rewind to before which message? [1-2, Enter = 2, q = cancel]"
    );
    assert_eq!(d.chat.messages().len(), 2);
    assert_eq!(prefill(&events).as_deref(), Some("second"));
}

#[tokio::test]
async fn rewind_cancel_and_bad_numbers_change_nothing() {
    let mut d = direct(ok(1), &[]);
    d.send("first").await;
    d.events();
    let (_, events) = d.cmd_answering("/rewind", Answer::No).await;
    assert_eq!(prefill(&events), None);
    d.cmd("/rewind 5").await;
    let events = d.events();
    assert_eq!(
        notices(&events),
        ["usage: /rewind [1-1]  (no argument lists them)"]
    );
    d.cmd("/rewind x").await;
    assert_eq!(d.chat.messages().len(), 2);
    assert_eq!(prefill(&d.events()), None);
}

#[tokio::test]
async fn a_summary_is_not_a_rewind_point() {
    let mut d = direct(ok(0), &[]);
    d.chat.messages = summary_pair("x");
    d.cmd("/rewind").await;
    assert_eq!(
        notices(&d.events()),
        ["nothing to rewind (the earlier turns were compacted)"]
    );
    assert_eq!(d.chat.messages().len(), 2);
}

#[tokio::test]
async fn rewound_images_are_attached_again() {
    let mut d = direct(ok(0), &[]);
    d.chat.messages = vec![
        json!({"role": "user", "content": [
            {"type": "text", "text": "what is this"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA"}}]}),
        json!({"role": "assistant", "content": "a cat"}),
    ];
    d.cmd("/rewind 1").await;
    let events = d.events();
    assert_eq!(d.chat.messages(), Vec::<Value>::new());
    assert_eq!(d.chat.pending_images, ["data:image/png;base64,AA"]);
    assert_eq!(prefill(&events).as_deref(), Some("what is this"));
    assert!(notices(&events).contains(&"1 image attached again".to_string()));
}

#[tokio::test]
async fn shell_changes_are_named_as_not_undone() {
    let mut d = direct(ok(0), &[]);
    d.chat.messages = vec![
        json!({"role": "user", "content": "run it"}),
        json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "s1", "type": "function",
             "function": {"name": "run_shell", "arguments": "{\"command\": \"touch x\"}"}}]}),
        json!({"role": "tool", "tool_call_id": "s1", "content": "{\"ok\": 1}"}),
        json!({"role": "assistant", "content": "done"}),
    ];
    d.cmd("/rewind 1").await;
    let said = notices(&d.events());
    assert!(
        said.contains(
            &"those turns also ran 1 run_shell command; what they changed is not undone"
                .to_string()
        ),
        "{said:?}"
    );
}

// ------------------------------------------------- the whole chat drawn

#[test]
fn every_message_and_reply_is_drawn_in_order() {
    let msgs: Vec<Value> = (1..=30).flat_map(turn).collect();
    let items = history_items(&msgs, &[]);
    let expected: Vec<HistoryItem> = (1..=30)
        .flat_map(|n| {
            [
                user(&format!("question {n}")),
                agent(&format!("answer {n}")),
            ]
        })
        .collect();
    assert_eq!(items, expected);
}

#[test]
fn compacted_turns_are_drawn_before_a_marker_and_the_summary_is_not() {
    let earlier: Vec<Value> = turn(1).into_iter().chain([sessions::compacted()]).collect();
    let msgs: Vec<Value> = summary_pair("SECRET SUMMARY")
        .into_iter()
        .chain(turn(2))
        .collect();
    assert_eq!(
        history_items(&msgs, &earlier),
        [
            user("question 1"),
            agent("answer 1"),
            HistoryItem::Compacted,
            user("question 2"),
            agent("answer 2"),
        ]
    );
    // A fork of a compacted chat has no earlier turns, only the marker.
    assert_eq!(history_items(&msgs, &[])[0], HistoryItem::Compacted);
}

#[test]
fn thinking_is_one_item_and_tool_calls_one_each() {
    let msgs = vec![
        json!({"role": "user", "content": [
            {"type": "text", "text": "what is here?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA"}},
            {"type": "text", "text": "[rp chat: mounted /x]"}]}),
        json!({"role": "assistant", "content": null, "thought": true, "tool_calls": [
            {"id": "c1", "type": "function", "function": {
                "name": "run_shell", "arguments": "{\"command\": \"ls\\npwd\"}"}},
            {"id": "c2", "type": "function", "function": {
                "name": "web_search", "arguments": "{\"query\": \"x\"}"}}]}),
        json!({"role": "tool", "tool_call_id": "c1", "content": "TOOL OUTPUT"}),
        json!({"role": "tool", "tool_call_id": "c2", "content": "SEARCH OUTPUT"}),
        json!({"role": "assistant", "content": "Two files.", "thought": true}),
    ];
    assert_eq!(
        history_items(&msgs, &[]),
        [
            user("what is here?\n[image]"),
            HistoryItem::Thought,
            HistoryItem::ToolCall {
                call_id: "c1".into(),
                tool: "run".into(),
                summary: "ls\npwd".into()
            },
            HistoryItem::ToolCall {
                call_id: "c2".into(),
                tool: "search".into(),
                summary: "x".into()
            },
            HistoryItem::Thought,
            agent("Two files."),
        ]
    );
}

// ------------------------------------------------------- /new, /resume

#[tokio::test]
async fn new_draws_an_empty_chat() {
    let mut d = direct(ok(1), &[]);
    d.send("first").await;
    d.events();
    d.cmd("/new").await;
    let events = d.events();
    assert_eq!(rewritten(&events), Some((RewriteReason::New, vec![])));
    assert_eq!(d.chat.messages(), Vec::<Value>::new());
}

#[tokio::test]
async fn resume_with_one_hit_switches_and_draws_the_whole_chat() {
    let mut d = direct(ok(2), &[]);
    d.send("apples are red").await;
    let first = d.id();
    d.cmd("/new").await;
    d.send("pears").await;
    d.events();
    d.cmd("/resume apples").await;
    let events = d.events();
    assert_eq!(d.id(), first);
    assert_eq!(
        rewritten(&events),
        Some((
            RewriteReason::Resume,
            vec![user("apples are red"), agent("ok")]
        ))
    );
    let said = notices(&events);
    assert!(
        said.contains(&format!("resumed 'apples are red'  (2 messages, {first})")),
        "{said:?}"
    );
}

#[tokio::test]
async fn resume_without_a_query_picks_from_the_list() {
    let mut d = direct(ok(2), &[]);
    d.send("one").await;
    let first = d.id();
    d.cmd("/new").await;
    d.send("two").await;
    let second = d.id();
    d.events();
    let (question, events) = d.cmd_answering("/resume", Answer::Pick(1)).await;
    assert_eq!(question.options.len(), 2);
    assert!(
        question.options[0].starts_with("two  just now · 2 msgs · "),
        "{:?}",
        question.options
    );
    assert!(
        question.options[1].starts_with("one  "),
        "{:?}",
        question.options
    );
    assert_eq!(question.default, Answer::Pick(0));
    assert_eq!(d.id(), first);
    assert!(rewritten(&events).is_some());
    // Picking the chat you are in does nothing. Leaving `two` updated
    // it, so it is listed first now.
    let (question, events) = d.cmd_answering("/resume", Answer::Pick(1)).await;
    assert!(
        question.options[0].contains(&second),
        "{:?}",
        question.options
    );
    assert!(
        question.options[1].contains(&first),
        "{:?}",
        question.options
    );
    assert_eq!(d.id(), first);
    assert!(rewritten(&events).is_none());
}

#[tokio::test]
async fn chats_lists_this_projects_chats_newest_first() {
    let mut d = direct(ok(2), &[]);
    d.cmd("/chats").await;
    assert_eq!(
        notices(&d.events()),
        ["no saved chats in this project (--all for every project)"]
    );
    d.send("one").await;
    d.cmd("/new").await;
    d.send("two").await;
    let id = d.id();
    d.events();
    d.cmd("/chats").await;
    let said = notices(&d.events());
    assert_eq!(said.len(), 2, "{said:?}");
    assert_eq!(said[0], format!("*  1. two  just now · 2 msgs · {id}"));
    assert!(said[1].starts_with("   2. one  just now · 2 msgs · "));
    d.cmd("/chats nothing-like-it --all").await;
    assert_eq!(
        notices(&d.events()),
        ["no saved chats matching 'nothing-like-it' anywhere"]
    );
}

// -------------------------------------------------- settings and words

#[tokio::test]
async fn settings_commands_say_what_they_set() {
    let mut d = direct(ok(0), &[]);
    for (line, said) in [
        ("/temp 0.7", "temperature = 0.7"),
        ("/temp 1", "temperature = 1.0"),
        ("/temp hot", "usage: /temp 0.7"),
        ("/maxtok 4096", "max_tokens = 4096"),
        ("/maxtok lots", "usage: /maxtok 4096"),
        ("/effort", "reasoning effort = medium"),
        ("/effort low", "reasoning effort = low"),
        ("/effort max", "usage: /effort xhigh|medium|low"),
        ("/system be brief", "system prompt set"),
        ("/system", "system prompt cleared"),
        ("/images", "none pending"),
        ("/image", "usage: /image <path|url>"),
        (
            "/image /nonexistent/x.png",
            "no such file: /nonexistent/x.png",
        ),
        ("/tools on", "/tools comes with phase 4"),
        // No pod in a chat on a given address (viy dev chat).
        (
            "/up",
            "/up: this chat talks to the address it was given; viy chat runs the pod commands",
        ),
        // rp's transcripts are not kept (kb d082).
        ("/save", "unknown command: /save  (try /help)"),
        ("/load x", "unknown command: /load  (try /help)"),
        ("/frobnicate", "unknown command: /frobnicate  (try /help)"),
        ("/think off", "/think: the chat's screen handles it"),
    ] {
        d.cmd(line).await;
        assert_eq!(notices(&d.events()), [said], "{line}");
    }
    assert_eq!(d.chat.settings.temperature, 1.0);
    assert_eq!(d.chat.settings.max_tokens, 4096);
    assert_eq!(d.chat.settings.reasoning_effort, "low");
    assert_eq!(d.chat.system, None);
    d.cmd("/help").await;
    let help = notices(&d.events());
    assert!(
        help[0].starts_with("  /help              show this\n"),
        "{help:?}"
    );
}

#[tokio::test]
async fn an_attached_image_goes_with_the_next_message() {
    let mut d = direct(ok(1), &[]);
    d.cmd("/image https://example.com/cat.png").await;
    assert_eq!(notices(&d.events()), ["attached (1 pending)"]);
    let content = d.chat.user_content("what is it".into());
    assert_eq!(
        content,
        json!([{"type": "text", "text": "what is it"},
               {"type": "image_url", "image_url": {"url": "https://example.com/cat.png"}}])
    );
    assert!(d.chat.pending_images.is_empty());
    let path = d._scratch.path().join("dot.gif");
    std::fs::write(&path, b"GIF8").unwrap();
    d.cmd(&format!("/image {}", path.display())).await;
    d.cmd("/images").await;
    let said = notices(&d.events());
    assert_eq!(said[1], "  1. data:image/gif;base64,R0lGOA==");
    let _ = &d.mock;
}

impl Direct {
    /// [`Chat::open_session`] with the pick answered `answer`, if it asks:
    /// the question and the events.
    async fn open_answering(
        &mut self,
        open: crate::OpenSession,
        answer: Answer,
    ) -> (Option<Question>, Vec<EventMsg>) {
        let ctx = self.ctx.clone();
        let (mut asked, mut seen) = (None, Vec::new());
        {
            let opening = self.chat.open_session(&ctx, &open);
            tokio::pin!(opening);
            loop {
                tokio::select! {
                    () = &mut opening => break,
                    Some(event) = self.rx.recv() => match event.msg {
                        EventMsg::QuestionRequest(q) => {
                            ctx.answer(&q.question_id, answer);
                            asked = Some(q.question);
                        }
                        other => seen.push(other),
                    },
                }
            }
        }
        seen.extend(self.events());
        (asked, seen)
    }
}

/// `viy resume ID` and `viy fork ID` (kb d083): the chat named opens
/// whole, or a copy of it.
#[tokio::test]
async fn opening_a_saved_chat_resumes_it_or_forks_it() {
    let mut d = direct(ok(1), &[]);
    d.send("apples are red").await;
    let first = d.id();
    d.cmd("/new").await;
    d.events();
    let open = |fork| crate::OpenSession {
        id: Some(first.clone()),
        fork,
        ..crate::OpenSession::default()
    };
    let (asked, events) = d.open_answering(open(false), Answer::No).await;
    assert!(asked.is_none());
    assert_eq!(d.id(), first);
    assert_eq!(
        rewritten(&events),
        Some((
            RewriteReason::Resume,
            vec![user("apples are red"), agent("ok")]
        ))
    );
    d.cmd("/new").await;
    let (_, events) = d.open_answering(open(true), Answer::No).await;
    assert_ne!(d.id(), first);
    assert_eq!(d.chat.messages().len(), 2);
    assert!(rewritten(&events).is_some());
}

/// `viy resume` without a chat named: a pick of this project's chats,
/// one-shot ones left out; none picked, or none to pick, leaves the new
/// chat as it is.
#[tokio::test]
async fn opening_without_a_chat_named_picks_one() {
    let mut d = direct(ok(1), &[]);
    let fresh = d.id();
    let (asked, events) = d
        .open_answering(crate::OpenSession::default(), Answer::Pick(0))
        .await;
    assert!(asked.is_none());
    assert_eq!(
        notices(&events),
        ["no saved chats in this project; this is a new one"]
    );
    assert_eq!(d.id(), fresh);
    d.send("one").await;
    let first = d.id();
    d.cmd("/new").await;
    let mut one_shot = d.store.create(d.chat.project.as_deref(), Some("m"));
    one_shot.mark_exec();
    one_shot
        .sync(None, &[json!({"role": "user", "content": "exec"})], false)
        .unwrap();
    one_shot.close("exec").unwrap();
    let new = d.id();
    let (asked, _) = d
        .open_answering(crate::OpenSession::default(), Answer::No)
        .await;
    assert_eq!(asked.unwrap().options.len(), 1);
    assert_eq!(d.id(), new);
    let (asked, _) = d
        .open_answering(crate::OpenSession::default(), Answer::Pick(0))
        .await;
    assert_eq!(asked.unwrap().options.len(), 1);
    assert_eq!(d.id(), first);
    let with_exec = crate::OpenSession {
        include_exec: true,
        ..crate::OpenSession::default()
    };
    let (asked, _) = d.open_answering(with_exec, Answer::No).await;
    assert_eq!(asked.unwrap().options.len(), 2);
}
