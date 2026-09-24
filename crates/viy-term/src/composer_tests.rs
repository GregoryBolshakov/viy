use std::time::Duration;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;
use tokio::time::Instant;

use super::*;

/// A composer and a clock: keys come 50 ms apart unless a test types them
/// as a keystroke paste.
struct Typist {
    composer: Composer,
    now: Instant,
}

impl Typist {
    fn new(history: History) -> Self {
        Self {
            composer: Composer::new(history),
            now: Instant::now(),
        }
    }

    fn press(&mut self, code: KeyCode, modifiers: KeyModifiers, gap: Duration) -> Action {
        self.now += gap;
        self.composer.key(KeyEvent::new(code, modifiers), self.now)
    }

    fn key(&mut self, code: KeyCode) -> Action {
        self.press(code, KeyModifiers::NONE, Duration::from_millis(50))
    }

    fn mods(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Action {
        self.press(code, modifiers, Duration::from_millis(50))
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    /// `text` as a terminal without bracketed paste sends it: a key per
    /// char, 1 ms apart, Enter for each newline.
    fn keystroke_paste(&mut self, text: &str) -> Vec<Action> {
        text.chars()
            .map(|c| {
                let code = if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                };
                self.press(code, KeyModifiers::NONE, Duration::from_millis(1))
            })
            .collect()
    }
}

fn typist() -> Typist {
    Typist::new(History::default())
}

#[test]
fn enter_sends_the_trimmed_line_and_a_blank_one_nothing() {
    let mut t = typist();
    t.type_text("  hello  ");
    assert_eq!(t.key(KeyCode::Enter), Action::Submit("hello".into()));
    assert!(t.composer.is_empty());
    t.type_text("   ");
    assert_eq!(t.key(KeyCode::Enter), Action::None);
}

/// rp's readline: Ctrl-J (kb f087), Alt-Enter and a trailing `\` add a
/// line; only Enter sends.
#[test]
fn ctrl_j_alt_enter_and_a_trailing_backslash_add_a_line() {
    let mut t = typist();
    t.type_text("one");
    t.mods(KeyCode::Char('j'), KeyModifiers::CONTROL);
    t.type_text("two\\");
    t.key(KeyCode::Enter);
    t.type_text("three");
    t.mods(KeyCode::Enter, KeyModifiers::ALT);
    t.type_text("four");
    assert_eq!(
        t.key(KeyCode::Enter),
        Action::Submit("one\ntwo\nthree\nfour".into())
    );
}

/// `test_paste.py`: a bracketed paste is one block, its newlines (sent as
/// `\r`) kept, and only a real Enter sends it.
#[test]
fn a_bracketed_paste_is_one_block_that_never_sends() {
    let mut t = typist();
    t.composer.paste("AAAA AAA\rBBBBB");
    assert_eq!(t.composer.text(), "AAAA AAA\nBBBBB");
    assert_eq!(
        t.key(KeyCode::Enter),
        Action::Submit("AAAA AAA\nBBBBB".into())
    );
}

/// Without bracketed paste a paste arrives as keys faster than anyone
/// types: its Enters are newlines, and an Enter typed after it sends.
#[test]
fn a_keystroke_paste_keeps_its_newlines() {
    let mut t = typist();
    let actions = t.keystroke_paste("line one\nline two\nline three");
    assert!(actions.iter().all(|a| *a == Action::None), "{actions:?}");
    assert_eq!(t.composer.text(), "line one\nline two\nline three");
    assert_eq!(
        t.press(
            KeyCode::Enter,
            KeyModifiers::NONE,
            Duration::from_millis(400)
        ),
        Action::Submit("line one\nline two\nline three".into())
    );
}

/// Two quick keys are not a burst, and a burst's window ends 120 ms after
/// its last key.
#[test]
fn typing_fast_is_not_a_paste() {
    let mut t = typist();
    t.press(
        KeyCode::Char('o'),
        KeyModifiers::NONE,
        Duration::from_millis(50),
    );
    t.press(
        KeyCode::Char('k'),
        KeyModifiers::NONE,
        Duration::from_millis(5),
    );
    assert_eq!(
        t.press(KeyCode::Enter, KeyModifiers::NONE, Duration::from_millis(5)),
        Action::Submit("ok".into())
    );
    t.keystroke_paste("abc");
    assert_eq!(
        t.press(
            KeyCode::Enter,
            KeyModifiers::NONE,
            Duration::from_millis(121)
        ),
        Action::Submit("abc".into())
    );
}

#[test]
fn shift_tab_steps_the_mode() {
    let mut t = typist();
    t.type_text("kept");
    assert_eq!(t.key(KeyCode::BackTab), Action::CycleMode);
    assert_eq!(t.composer.text(), "kept");
}

#[test]
fn up_recalls_what_was_sent_and_down_comes_back() {
    let mut t = typist();
    t.type_text("first");
    t.key(KeyCode::Enter);
    t.type_text("second");
    t.key(KeyCode::Enter);
    t.key(KeyCode::Up);
    assert_eq!(t.composer.text(), "second");
    t.key(KeyCode::Up);
    assert_eq!(t.composer.text(), "first");
    t.key(KeyCode::Down);
    t.key(KeyCode::Down);
    assert_eq!(t.composer.text(), "");
    assert_eq!(t.composer.history().entries(), ["first", "second"]);
}

/// With a draft, Up and Down move in it; a draft is never thrown away for
/// a history entry.
#[test]
fn up_in_a_draft_moves_the_cursor() {
    let mut t = typist();
    t.type_text("sent");
    t.key(KeyCode::Enter);
    t.type_text("draft");
    t.mods(KeyCode::Char('j'), KeyModifiers::CONTROL);
    t.type_text("more");
    t.key(KeyCode::Up);
    assert_eq!(t.composer.text(), "draft\nmore");
    t.type_text("!");
    assert_eq!(t.composer.text(), "draf!t\nmore", "the column kept");
}

#[test]
fn the_slash_popup_completes_with_tab_and_closes_with_esc() {
    let mut t = typist();
    t.type_text("/mo");
    let labels: Vec<String> = t.composer.popup().into_iter().map(|c| c.label).collect();
    assert_eq!(
        labels,
        ["/mode [normal|auto-yes]", "/mount [<drive|path> [rw]]"]
    );
    t.key(KeyCode::Tab);
    assert_eq!(t.composer.text(), "/mode ");
    t.key(KeyCode::Down);
    assert_eq!(t.composer.selected(), 1);
    t.key(KeyCode::Tab);
    assert_eq!(t.composer.text(), "/mode auto-yes");
    assert!(t.composer.popup().is_empty());
    t.key(KeyCode::Backspace);
    assert!(!t.composer.popup().is_empty());
    t.key(KeyCode::Esc);
    assert!(t.composer.popup().is_empty());
    assert_eq!(
        t.key(KeyCode::Enter),
        Action::Submit("/mode auto-ye".into())
    );
}

#[test]
fn readline_keys_edit() {
    let mut t = typist();
    t.type_text("git push origin");
    t.mods(KeyCode::Char('w'), KeyModifiers::CONTROL);
    t.mods(KeyCode::Char('a'), KeyModifiers::CONTROL);
    t.mods(KeyCode::Char('k'), KeyModifiers::CONTROL);
    assert!(t.composer.is_empty());
    t.mods(KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert_eq!(t.composer.text(), "git push ");
    t.mods(KeyCode::Char('b'), KeyModifiers::ALT);
    t.mods(KeyCode::Char('d'), KeyModifiers::ALT);
    assert_eq!(t.composer.text(), "git  ");
}
