//! The terminal tab's title (rpctl `termtitle.py`, ui-plan 10): a spinner
//! while a turn runs and `✳` while the chat waits for you, before the
//! chat's name, so a chat in a background tab shows at a glance whether it
//! is still going. Only the chat's terminal UI sets it, never the plain
//! renderer or a scripted run, and only when `CHAT_TAB_TITLE` is `1`, its
//! default (rp's test).
//!
//! The shell's title is saved on xterm's title stack when the chat takes
//! the terminal and given back on every restore path ([`crate::tui`]), as
//! rp does; a terminal without the stack ignores both.
//!
//! The name comes from the model (an automatic title) or the user, so it
//! is cleaned as Codex's `terminal_title.rs` cleans titles: controls,
//! bidi and invisible format chars are dropped, whitespace runs become one
//! space, and under GNU Screen, which cuts a title's chars to one byte,
//! only printable ASCII is written. That is stricter than rp's
//! `untrusted.terminal_safe`, which keeps bidi and format chars.

/// The spinner's frames, rp's and Codex's.
pub const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Before the name while nothing runs.
pub const IDLE: char = '✳';
/// The name's length at most, in chars (rp's `LABEL_CHARS`).
pub const LABEL_CHARS: usize = 48;
/// The name when the chat has none yet (rp's "rp chat").
pub const UNNAMED: &str = "viy chat";

/// How a title's chars reach the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Unicode,
    /// Inside GNU Screen (`STY` set; `TERM=screen` is tmux too): printable
    /// ASCII only, the spinner as `|/-\`.
    Screen,
}

impl Encoding {
    /// From the environment, as Codex decides it.
    pub fn detect() -> Self {
        if std::env::var_os("STY").is_some() {
            Self::Screen
        } else {
            Self::Unicode
        }
    }
}

/// The title for `name`: the spinner's `frame` while busy, else [`IDLE`].
pub fn title(name: Option<&str>, busy: Option<usize>, encoding: Encoding) -> String {
    let glyph = busy.map_or(IDLE, |frame| FRAMES[frame % FRAMES.len()]);
    let name = label(name.unwrap_or_default());
    let name = if name.is_empty() {
        UNNAMED.to_string()
    } else {
        name
    };
    sanitize(&format!("{glyph} {name}"), encoding)
}

/// The sequence that sets the window and tab title to `title`, which must
/// come from [`title`]: OSC 0 ended by BEL, as rp and Codex end it.
pub fn osc(title: &str) -> String {
    format!("\x1b]0;{title}\x07")
}

/// Saves the shell's title on the title stack.
pub const PUSH: &str = "\x1b[22;0t";
/// Gives the saved title back.
pub const POP: &str = "\x1b[23;0t";

/// `name` on one line of at most [`LABEL_CHARS`] chars, cut with `…`.
fn label(name: &str) -> String {
    let clean = sanitize(name, Encoding::Unicode);
    if clean.chars().count() <= LABEL_CHARS {
        return clean;
    }
    let mut cut: String = clean.chars().take(LABEL_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// Untrusted text as one title line (Codex's `sanitize_terminal_title`):
/// disallowed chars dropped, whitespace runs one space, no space at
/// either end.
pub fn sanitize(text: &str, encoding: Encoding) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if disallowed(c) {
            continue;
        }
        let c = match (encoding, c) {
            (Encoding::Screen, '⠋' | '⠼' | '⠇') => '|',
            (Encoding::Screen, '⠙' | '⠴' | '⠏') => '/',
            (Encoding::Screen, '⠹' | '⠦') => '-',
            (Encoding::Screen, '⠸' | '⠧') => '\\',
            (Encoding::Screen, '✳' | '…') => '*',
            (Encoding::Screen, c) if !c.is_ascii() => '?',
            (_, c) => c,
        };
        if std::mem::take(&mut space) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Controls, and the format chars that reorder or hide text (Codex's
/// list, with the rest of the bidi isolates and the line separators).
fn disallowed(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E0FFF}'
        )
}

#[cfg(test)]
#[path = "tabtitle_tests.rs"]
mod tests;
