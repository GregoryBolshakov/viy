//! The slash commands (WP 3.12): rpctl's RewindTests and
//! ResumeShowsWholeChatTests (test_sessions.py) where they run without
//! tools, and each command's words.

use std::path::PathBuf;

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

// ------------------------------------------- /rewind and write_file

/// A workspace of its own for `d`'s file tools.
fn workspace(d: &mut Direct) -> (viy_testkit::scratch::ScratchDir, PathBuf) {
    let scratch = viy_testkit::scratch::ScratchDir::new("rewind-ws").unwrap();
    let ws = scratch.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    d.chat.files.workspace = ws.clone();
    d.chat.files.project_dir = ws.clone();
    d.chat.files.locks_dir = scratch.path().join("locks");
    d.chat.files.quarantine_dir = scratch.path().join("quarantine");
    let ws = viy_tools::resolve::resolve(&ws);
    (scratch, ws)
}

/// A chat whose session log is off: the file history lives in memory.
fn unsaved(d: &mut Direct) {
    *lock(&d.chat.session) = None;
    d.chat.sessions = None;
}

fn call(id: &str, name: &str, args: &Value) -> Value {
    json!({"id": id, "type": "function",
           "function": {"name": name, "arguments": args.to_string()}})
}

impl Direct {
    /// A turn in which the model reads `path` (unless `read` is false) and
    /// writes `content` there, as the tool loop runs them (rp's
    /// `write_turn`): call `cid` writes, `cid`+"r" reads.
    async fn write_turn(&mut self, text: &str, path: &str, content: &str, read: bool, cid: &str) {
        self.chat
            .messages
            .push(json!({"role": "user", "content": text}));
        if read {
            let args = json!({"path": path});
            let rid = format!("{cid}r");
            self.chat
                .messages
                .push(json!({"role": "assistant", "content": null,
                "tool_calls": [call(&rid, "read_file", &args)]}));
            let out = self
                .chat
                .files
                .read_file(args.as_object().unwrap(), &self.ctx)
                .await;
            self.chat
                .messages
                .push(json!({"role": "tool", "tool_call_id": rid,
                "content": out.result.to_string()}));
        }
        let args = json!({"path": path, "content": content});
        self.chat
            .messages
            .push(json!({"role": "assistant", "content": null,
            "tool_calls": [call(cid, "write_file", &args)]}));
        let ctx = self.ctx.clone();
        let out = self
            .chat
            .write_file(&ctx, cid, args.as_object().unwrap(), "stamp")
            .await;
        assert_eq!(out.result["ok"], true, "{}", out.result);
        self.chat
            .messages
            .push(json!({"role": "tool", "tool_call_id": cid,
            "content": out.result.to_string()}));
        self.chat
            .messages
            .push(json!({"role": "assistant", "content": "done"}));
        self.chat.checkpoint(&ctx);
    }

    /// `line` when it must ask nothing.
    async fn cmd_silent(&mut self, line: &str) -> Vec<EventMsg> {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.cmd(line))
            .await
            .expect("asked a question");
        let events = self.events();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, EventMsg::QuestionRequest(_))),
            "asked a question"
        );
        events
    }

    async fn write(&mut self, path: &str, content: &str) -> Value {
        let ctx = self.ctx.clone();
        let args = json!({"path": path, "content": content});
        self.chat
            .write_file(&ctx, "later", args.as_object().unwrap(), "stamp")
            .await
            .result
    }
}

async fn restores_what_write_file_changed(saved: bool) {
    let mut d = direct(ok(0), &[]);
    if !saved {
        unsaved(&mut d);
    }
    let (_s, ws) = workspace(&mut d);
    std::fs::write(ws.join("a.txt"), "v1").unwrap();
    d.write_turn("edit a", "a.txt", "v2", true, "w1").await;
    d.write_turn("make b", "b.txt", "new", false, "w2").await;
    d.write_turn("edit a again", "a.txt", "v3", true, "w3")
        .await;
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "v3");
    d.events();
    let (question, events) = d.cmd_answering("/rewind 2", Answer::Yes).await;
    assert_eq!(
        format!("{} {}", question.prompt, question.hint()),
        "put them back as they were? [Y/n, q = cancel]"
    );
    assert_eq!(question.kind, None, "no chat mode answers it");
    assert_eq!(question.unattended, Answer::Cancel);
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "v2");
    assert!(!ws.join("b.txt").exists());
    let said = notices(&events);
    assert_eq!(
        said[0],
        "write_file changed these files since then:\n  b.txt  (created then; would be deleted)\n  a.txt"
    );
    assert!(said.contains(&"restored a.txt".to_string()), "{said:?}");
    assert!(said.contains(&"deleted b.txt".to_string()), "{said:?}");
    assert_eq!(prefill(&events).as_deref(), Some("make b"));
    // a.txt is known as v2 again: the model may write it without a read.
    assert_eq!(d.write("a.txt", "v4").await["ok"], true);
}

#[tokio::test]
async fn rewind_restores_what_write_file_changed() {
    restores_what_write_file_changed(true).await;
}

#[tokio::test]
async fn rewind_restores_without_a_session_log() {
    restores_what_write_file_changed(false).await;
}

#[tokio::test]
async fn conversation_only_keeps_the_files_and_the_guard_asks_for_a_read() {
    let mut d = direct(ok(0), &[]);
    let (_s, ws) = workspace(&mut d);
    std::fs::write(ws.join("a.txt"), "v1").unwrap();
    d.write_turn("edit a", "a.txt", "v2", true, "w").await;
    let (question, _) = d.cmd_answering("/rewind 1", Answer::No).await;
    assert_eq!(question.prompt, "put it back as it was?");
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "v2");
    assert_eq!(d.chat.messages(), Vec::<Value>::new());
    d.chat.messages = vec![json!({"role": "user", "content": "x"})];
    let r = d.write("a.txt", "v9").await;
    assert!(r.get("error").is_some(), "{r}");
}

#[tokio::test]
async fn a_file_changed_since_is_left_alone() {
    let mut d = direct(ok(0), &[]);
    let (_s, ws) = workspace(&mut d);
    std::fs::write(ws.join("a.txt"), "v1").unwrap();
    d.write_turn("edit a", "a.txt", "v2", true, "w").await;
    std::fs::write(ws.join("a.txt"), "edited by hand").unwrap();
    d.events();
    // Nothing to restore: no question.
    let said = notices(&d.cmd_silent("/rewind 1").await);
    assert!(said[0].contains("changed since"), "{said:?}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "edited by hand"
    );
    assert_eq!(d.chat.messages(), Vec::<Value>::new());
}

#[tokio::test]
async fn cancel_at_the_file_question_keeps_everything() {
    let mut d = direct(ok(0), &[]);
    let (_s, ws) = workspace(&mut d);
    std::fs::write(ws.join("a.txt"), "v1").unwrap();
    d.write_turn("edit a", "a.txt", "v2", true, "w").await;
    let (_, events) = d.cmd_answering("/rewind 1", Answer::Cancel).await;
    assert_eq!(d.chat.messages().len(), 6);
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "v2");
    assert_eq!(prefill(&events), None);
    assert_eq!(rewritten(&events), None);
}

#[tokio::test]
async fn a_fork_can_still_restore() {
    let mut d = direct(ok(0), &[]);
    let (_s, ws) = workspace(&mut d);
    std::fs::write(ws.join("a.txt"), "v1").unwrap();
    d.write_turn("edit a", "a.txt", "v2", true, "w").await;
    let first = d.id();
    d.cmd("/fork").await;
    assert_ne!(d.id(), first);
    d.cmd_answering("/rewind 1", Answer::Yes).await;
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "v1");
}

#[tokio::test]
async fn archiving_takes_the_file_history_along() {
    // rp's test_trash_takes_the_file_history_along; viy archives (kb d083).
    let mut d = direct(ok(0), &[]);
    let (_s, ws) = workspace(&mut d);
    std::fs::write(ws.join("a.txt"), "v1").unwrap();
    d.write_turn("edit a", "a.txt", "v2", true, "w").await;
    let id = d.id();
    let ctx = d.ctx.clone();
    d.chat.end_session(&ctx, "exit").await;
    let dir = sessions::files_dir(&d.store.path_of(&id));
    assert!(dir.is_dir());
    d.store.archive(&id).unwrap();
    assert!(!dir.exists());
    assert!(sessions::files_dir(&d.store.archived().path_of(&id)).is_dir());
}

#[tokio::test]
async fn the_pick_counts_each_messages_writes() {
    let mut d = direct(ok(0), &[]);
    let (_s, ws) = workspace(&mut d);
    std::fs::write(ws.join("a.txt"), "v1").unwrap();
    d.write_turn("edit a", "a.txt", "v2", true, "w1").await;
    d.chat
        .messages
        .push(json!({"role": "user", "content": "thanks"}));
    d.chat
        .messages
        .push(json!({"role": "assistant", "content": "ok"}));
    let (question, _) = d.cmd_answering("/rewind", Answer::No).await;
    assert_eq!(question.options, ["edit a  (1 write)", "thanks"]);
}

#[tokio::test]
async fn a_link_put_in_a_written_path_is_left_alone() {
    // rp's RewindTests.test_a_link_put_in_a_written_path_is_left_alone
    // (d9d678f): undoing never writes through a link made since.
    let mut d = direct(ok(0), &[]);
    let (s, ws) = workspace(&mut d);
    let outside = s.path().join("outside.txt");
    std::fs::write(&outside, "v2").unwrap();
    std::fs::write(ws.join("a.txt"), "v1").unwrap();
    d.write_turn("edit a", "a.txt", "v2", true, "w").await;
    std::fs::remove_file(ws.join("a.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join("a.txt")).unwrap();
    let said = notices(&d.cmd_silent("/rewind 1").await);
    assert_eq!(
        said[0],
        "write_file changed these files since then:\n  a.txt  (changed since by something else; stays)"
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "v2");
}

#[tokio::test]
async fn a_rewound_mount_note_goes_out_again() {
    let mut d = direct(ok(0), &[]);
    let note = viy_tools::mounts::note(&[], &[]);
    d.chat.messages = vec![
        json!({"role": "user", "content": [
            {"type": "text", "text": note},
            {"type": "text", "text": "look"}]}),
        json!({"role": "assistant", "content": "ok"}),
    ];
    d.chat.mount_note = None;
    d.cmd("/rewind 1").await;
    assert_eq!(d.chat.mount_note.as_deref(), Some(note.as_str()));
    assert_eq!(prefill(&d.events()).as_deref(), Some("look"));
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
        // nobody at a terminal to answer for each command (rp's no TTY)
        (
            "/ssh host",
            "not lent: /ssh needs you at a terminal to answer for each command",
        ),
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

/// rpctl test_modes CarouselTests.test_mode_command: `/mode <name>` picks
/// one (case and spaces ignored), an unknown name changes nothing, `/mode`
/// lists them with the current one marked.
#[tokio::test]
async fn mode_command() {
    let mut d = direct(ok(0), &[]);
    d.cmd("/mode auto-yes").await;
    assert_eq!(d.ctx.mode().name, "auto-yes");
    let events = d.events();
    assert!(
        events.contains(&EventMsg::ModeChanged(viy_protocol::ModeChangedEvent {
            mode: "auto-yes".into(),
            label: "⏵⏵ auto-yes".into(),
        }))
    );
    assert!(notices(&events)[0].starts_with("⏵⏵ auto-yes -- says yes to destructive commands"));

    d.cmd("/mode nope").await;
    assert_eq!(d.ctx.mode().name, "auto-yes");
    assert_eq!(
        notices(&d.events()),
        ["no mode nope; the modes are normal, auto-yes"]
    );

    d.cmd("/mode").await;
    let listed = notices(&d.events());
    assert_eq!(listed.len(), 3);
    assert_eq!(
        listed[0],
        "  normal     every question waits for your answer"
    );
    assert!(listed[1].starts_with("* auto-yes   says yes to"));
    assert_eq!(
        listed[2],
        "Shift+Tab cycles them; CHAT_MODE sets the one viy chat starts in"
    );

    d.cmd("/mode Normal ").await;
    assert_eq!(d.ctx.mode().name, "normal");
}

// ------------------------------------------------------------ /mount
// rpctl tests/test_mounts.py ChatTests: the command, the note to the model
// and the taint (the file tools' half is files_tests.rs).

struct Drive {
    _scratch: viy_testkit::scratch::ScratchDir,
    path: std::path::PathBuf,
}

fn drive() -> Drive {
    let scratch = viy_testkit::scratch::ScratchDir::new("mount-drive").unwrap();
    let path = scratch.path().join("drive");
    std::fs::create_dir_all(path.join("docs")).unwrap();
    std::fs::write(path.join("docs/a.txt"), "hello from the drive\n").unwrap();
    std::fs::write(path.join(".env"), "KEY=secret\n").unwrap();
    Drive {
        _scratch: scratch,
        path,
    }
}

#[tokio::test]
async fn read_only_mount_for_the_file_tools() {
    let mut d = direct(ok(0), &[]);
    let drive = drive();
    d.cmd(&format!("/mount {}", drive.path.display())).await;
    assert_eq!(d.chat.files.mounts.len(), 1);
    assert!(!d.chat.files.mounts[0].rw);
    assert_eq!(d.chat.files.mounts[0].secrets, [drive.path.join(".env")]);
    let said = notices(&d.events());
    assert!(said[0].starts_with(&format!("mounted {} read-only", drive.path.display())));
    assert!(said.iter().any(|l| l.contains("1 secret file(s)")));
    // The chat holds outside text now; the note waits for the next message.
    assert!(d.chat.web_taint().unwrap().contains("mounted"));
    let content = d.chat.user_content("look at it".into());
    assert!(viy_tools::mounts::is_note(
        content[0]["text"].as_str().unwrap()
    ));
    assert_eq!(content[1]["text"], "look at it");
    assert_eq!(d.chat.user_content("again".into()), "again");
}

#[tokio::test]
async fn refused_mounts_change_nothing() {
    let mut d = direct(ok(0), &[]);
    let drive = drive();
    d.cmd("/mount /etc").await;
    assert!(notices(&d.events())[0].starts_with("not mounted: /etc is a system directory"));
    d.cmd("/mount /").await;
    assert!(d.chat.files.mounts.is_empty());
    assert_eq!(d.chat.web_taint(), None);
    let (q, _) = d
        .cmd_answering(&format!("/mount {} rw", drive.path.display()), Answer::No)
        .await;
    assert_eq!(q.prompt, "  mount it read-write?");
    assert_eq!(q.kind, None, "only the user answers it, whatever the mode");
    assert!(d.chat.files.mounts.is_empty());
}

#[tokio::test]
async fn rw_mount_asks_and_keeps_the_workspace_rules() {
    let mut d = direct(ok(0), &[]);
    let drive = drive();
    let (_, events) = d
        .cmd_answering(&format!("/mount {} rw", drive.path.display()), Answer::Yes)
        .await;
    assert!(d.chat.files.mounts[0].rw);
    assert!(notices(&events).iter().any(|l| l.contains("read-write")));
}

#[tokio::test]
async fn unmount_and_new_chat() {
    let mut d = direct(ok(0), &[]);
    let drive = drive();
    d.cmd(&format!("/mount {}", drive.path.display())).await;
    d.chat.mount_note = None;
    d.cmd("/new").await;
    assert!(d.chat.mount_note.as_deref().unwrap().contains("read-only"));
    assert!(d.chat.web_taint().is_some());
    d.events();
    d.cmd("/unmount").await;
    assert!(d.chat.files.mounts.is_empty());
    assert!(d.chat.mount_note.as_deref().unwrap().contains("unmounted"));
    assert!(d.chat.web_taint().is_some(), "what it read stays");
    assert!(notices(&d.events())[0].starts_with("unmounted "));
    d.cmd("/unmount").await;
    assert_eq!(notices(&d.events()), ["nothing is mounted"]);
}

#[tokio::test]
async fn resumed_chat_with_mount_reads_is_tainted() {
    let mut d = direct(ok(0), &[]);
    let args = json!({"path": "/media/me/USB/a.txt"}).to_string();
    d.chat
        .reset_web(&[json!({"role": "assistant", "content": "", "tool_calls": [
        {"id": "1", "function": {"name": "read_file", "arguments": args}}]})]);
    assert!(d.chat.web_taint().unwrap().contains("mounted folder"));
    d.chat
        .reset_web(&[json!({"role": "assistant", "content": "", "tool_calls": [
        {"id": "1", "function": {"name": "run_shell",
         "arguments": "{\"command\": \"ssh host ls\"}"}}]})]);
    assert!(d.chat.web_taint().unwrap().contains("lent server"));
    d.chat
        .reset_web(&[json!({"role": "tool", "content": "x <<<UNTRUSTED abc y"})]);
    assert_eq!(d.chat.web_taint(), Some("web results earlier in this chat"));
    d.chat.reset_web(&[]);
    assert_eq!(d.chat.web_taint(), None);
}
