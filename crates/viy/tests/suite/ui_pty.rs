//! The chat's terminal UI under a pseudo-terminal (ui-plan 13): the real
//! binary (`viy dev chat`) against the scripted model server, keys in,
//! screen out through a recording vt100 emulator.

use std::process::Command;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_testkit::hostile::CORPUS;
use viy_testkit::hostile::foreign_controls;
use viy_testkit::mock_llm::MockLlm;
use viy_testkit::pty::Pty;
use viy_testkit::pty::wait_exit;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::screen::dump;

const ROWS: u16 = 40;
const COLS: u16 = 100;

/// A paste holding OSC 52 and a bidi override, sent; an answer and its
/// reasoning holding the whole corpus, and a hostile title: all of it is on
/// the screen as visible symbols, the terminal saw only viy's own
/// sequences, nothing set the title or touched the clipboard, the model got
/// the paste unchanged, and the terminal is given back at the end.
#[test]
fn hostile_text_from_the_paste_and_the_model_is_shown_and_does_nothing() {
    let answer = format!("{}\n\nend of the answer", CORPUS.concat());
    let script = json!({
        "name": "hostile-text",
        "max_model_len": 65536,
        "responses": [
            {"stream": [
                {"reasoning": CORPUS.concat()},
                {"content": answer},
            ]},
            {"message": "Hostile \u{1b}]0;retitled\u{7} chat \u{202e}txt.exe"},
        ],
    });
    let mock = MockLlm::start(&script).unwrap();
    let scratch = ScratchDir::new("ui-pty").unwrap();
    let pty = Pty::open(ROWS, COLS).unwrap();
    let mut child = pty
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_viy"))
                .args(["dev", "chat", "--url", &mock.uri(), "--dir"])
                .arg(scratch.path())
                .env("VLLM_API_KEY", "fake-key"),
        )
        .unwrap();
    pty.wait_for("the input box", |s| {
        s.alternate_screen() && s.contents().contains("› ")
    });

    let pasted = "look \u{1b}]52;c;aGk=\u{7} at invoice_\u{202e}fdp.exe";
    pty.send(format!("\x1b[200~{pasted}\x1b[201~").as_bytes())
        .unwrap();
    pty.wait_for("the paste in the input", |s| {
        s.contents()
            .contains("› look ␛]52;c;aGk=␇ at invoice_<U+202E>fdp.exe")
    });
    pty.send(b"\r").unwrap();
    pty.wait_for("the answer and the title", |s| {
        let text = s.contents();
        text.contains("end of the answer") && text.contains("Hostile ␛]0;retitled␇ chat")
    });
    pty.with_screen(|parser| {
        let screen = parser.screen();
        let shown = dump(screen);
        assert_eq!(
            parser.callbacks().foreign(),
            Vec::<String>::new(),
            "{shown}"
        );
        let text = screen.contents();
        assert!(
            text.contains("you › look ␛]52;c;aGk=␇ at invoice_<U+202E>fdp.exe"),
            "{shown}"
        );
        assert!(
            screen.alternate_screen() && screen.bracketed_paste(),
            "{shown}"
        );
    });
    assert_eq!(foreign_controls(&pty.bytes()), Vec::<String>::new());
    // The engine sent what was pasted, not what the screen shows.
    let asked = mock.requests();
    let last_user = asked
        .iter()
        .filter_map(|r| r.body.as_ref())
        .find(|b| b["stream"] == Value::Bool(true))
        .and_then(|b| b["messages"].as_array())
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
        .map(|m| m["content"].clone());
    assert_eq!(last_user, Some(Value::String(pasted.into())));

    // Ctrl-D closes the chat and gives the terminal back.
    pty.send(b"\x04").unwrap();
    let status = wait_exit(&mut child).unwrap();
    assert!(status.success(), "{status}");
    // The child has exited, but the reader thread may not have read its
    // last bytes yet.
    pty.wait_for("the terminal given back", |s| {
        !s.alternate_screen()
            && !s.bracketed_paste()
            && s.mouse_protocol_mode() == vt100::MouseProtocolMode::None
    });
    pty.with_screen(|parser| {
        assert_eq!(parser.callbacks().foreign(), Vec::<String>::new());
    });
    assert!(pty.cooked());
}

/// `viy dev chat` on `mock`'s server in `dir`, under `pty`, with the input
/// box drawn.
fn open_chat(pty: &Pty, mock: &MockLlm, dir: &std::path::Path) -> viy_testkit::pty::Spawned {
    let child = pty
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_viy"))
                .args(["dev", "chat", "--url", &mock.uri(), "--dir"])
                .arg(dir)
                .env("VLLM_API_KEY", "fake-key"),
        )
        .unwrap();
    pty.wait_for("the input box", |s| {
        s.alternate_screen() && s.contents().contains("› ")
    });
    child
}

/// A server that answers one turn with `answer` and titles the chat.
fn one_turn(answer: &str) -> MockLlm {
    MockLlm::start(&json!({
        "name": "one-turn",
        "max_model_len": 65536,
        "responses": [
            {"stream": [{"content": answer}]},
            {"message": "A title"},
        ],
    }))
    .unwrap()
}

/// The last user message of the first streamed request `mock` saw.
fn sent(mock: &MockLlm) -> Option<String> {
    mock.requests()
        .iter()
        .filter_map(|r| r.body.as_ref())
        .find(|b| b["stream"] == Value::Bool(true))
        .and_then(|b| b["messages"].as_array().cloned())
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "user").cloned())
        .and_then(|m| m["content"].as_str().map(str::to_string))
}

/// Types `keys` as one write, then Enter 300 ms later (past the paste-burst
/// window), and waits for the answer; returns what the model got.
fn send_and_answer(pty: &Pty, mock: &MockLlm, keys: &[u8]) -> Option<String> {
    pty.send(keys).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.wait_for("the answer", |s| s.contents().contains("the answer"));
    sent(mock)
}

/// Ctrl-D at the empty input closes the chat and gives the terminal back.
fn close(pty: &Pty, mut child: viy_testkit::pty::Spawned) {
    pty.send(b"\x04").unwrap();
    let status = wait_exit(&mut child).unwrap();
    assert!(status.success(), "{status}");
    pty.wait_for("the terminal given back", |s| !s.alternate_screen());
}

/// rp's `test_paste.py::test_multiline_paste_is_one_input`: a pasted "A\nB"
/// (newlines sent as `\r`) is one message, sent by the Enter after it.
#[test]
fn a_bracketed_multiline_paste_is_one_message() {
    let mock = one_turn("the answer");
    let dir = ScratchDir::new("ui-paste").unwrap();
    let pty = Pty::open(ROWS, COLS).unwrap();
    let child = open_chat(&pty, &mock, dir.path());
    let got = send_and_answer(&pty, &mock, b"\x1b[200~AAAA AAA\rBBBBB\x1b[201~");
    assert_eq!(got.as_deref(), Some("AAAA AAA\nBBBBB"));
    close(&pty, child);
}

/// `test_paste.py::test_ctrl_j_inserts_newline`: Ctrl-J (`\n`) adds a line;
/// only Enter (`\r`) sends.
#[test]
fn ctrl_j_inserts_a_newline() {
    let mock = one_turn("the answer");
    let dir = ScratchDir::new("ui-ctrl-j").unwrap();
    let pty = Pty::open(ROWS, COLS).unwrap();
    let child = open_chat(&pty, &mock, dir.path());
    let got = send_and_answer(&pty, &mock, b"AAAA\nBBBB");
    assert_eq!(got.as_deref(), Some("AAAA\nBBBB"));
    close(&pty, child);
}

/// A terminal without bracketed paste types a paste as keys: its `\r`s
/// arrive as Enter within the burst and stay newlines.
#[test]
fn a_keystroke_paste_is_one_message() {
    let mock = one_turn("the answer");
    let dir = ScratchDir::new("ui-burst").unwrap();
    let pty = Pty::open(ROWS, COLS).unwrap();
    let child = open_chat(&pty, &mock, dir.path());
    let got = send_and_answer(&pty, &mock, b"line one\rline two\rline three");
    assert_eq!(got.as_deref(), Some("line one\nline two\nline three"));
    close(&pty, child);
}

/// What was sent is in the project's history after the chat closes: the
/// next chat in that project brings it back with Up.
#[test]
fn the_history_outlives_the_chat() {
    let dir = ScratchDir::new("ui-history").unwrap();
    let mock = one_turn("the answer");
    let pty = Pty::open(ROWS, COLS).unwrap();
    let child = open_chat(&pty, &mock, dir.path());
    send_and_answer(&pty, &mock, b"remember this");
    close(&pty, child);

    let mock = one_turn("unused");
    let pty = Pty::open(ROWS, COLS).unwrap();
    let child = open_chat(&pty, &mock, dir.path());
    pty.send(b"\x1b[A").unwrap();
    pty.wait_for("the recalled message", |s| {
        s.contents().contains("› remember this")
    });
    pty.send(b"\x03").unwrap();
    close(&pty, child);
}
