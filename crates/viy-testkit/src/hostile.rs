//! Hostile strings for the terminal (ui-plan 7, flags F36): text the model,
//! a tool, a file or a web page could carry to clear the screen, move the
//! cursor over a question, retitle the tab, read or write the clipboard,
//! plant a link, ask the terminal to answer into the input, or reorder and
//! hide text. Every widget is tested with all of them; [`foreign_controls`]
//! checks that what reached the terminal holds only the renderer's own
//! sequences.

/// The corpus. Rust strings are valid UTF-8: broken UTF-8 reaches the UI
/// already decoded with U+FFFD, as the last entries have it.
pub const CORPUS: &[&str] = &[
    // The screen and the scrollback.
    "\x1b[2J\x1b[Hcleared",
    "\x1b[3Jscrollback gone",
    "\x1bcfull reset",
    "\x1b#8DECALN",
    "\x1b[5;10rscroll region\x1b[r",
    "\x1b[?1049lleft the alternate screen",
    "\x1b[1;31mred\x1b[0m and \x1b[7minverse",
    // The cursor, to write over a question.
    "\x1b[10;10Hallow? [y/N] y",
    "\x1b[1A\x1b[2Kerased the line above",
    "\x1b7\x1b[1;1Hover the top\x1b8",
    "line\rOVERWRITTEN",
    "abc\x08\x08\x08xyz",
    "\x1b[?25lhidden cursor",
    // Modes the UI relies on.
    "\x1b[?1000l\x1b[?1006l\x1b[?2004lno mouse, no paste",
    "\x1b[?1h\x1b=application keys",
    // Title, clipboard, links, notifications.
    "\x1b]0;pwned title\x07",
    "\x1b]2;pwned title\x1b\\",
    "\x1b]1;icon name\x07",
    "\x1b]52;c;cm0gLXJmIH4K\x07",
    "\x1b]52;c;?\x07",
    "\x1b]8;;https://evil.example/\x1b\\click me\x1b]8;;\x1b\\",
    "\x1b]777;notify;viy;pwned\x07\x1b]9;pwned\x07",
    "\x1b]1337;SetUserVar=x=eQ==\x07",
    // Reports the terminal answers into viy's input.
    "\x1b[6n\x1b[21t\x1b[>c\x1bZ\x1b]10;?\x07",
    // DCS, APC, PM, SOS.
    "\x1bP+q544e\x1b\\",
    "\x1b_Gf=100;AAAA\x1b\\\x1b^privacy\x1b\\\x1bXsos\x1b\\",
    // Huge parameters.
    "\x1b[999999999999;999999999999H\x1b[99999999999999999999m",
    "\x1b[?99999999999999h",
    // Character sets and controls.
    "\x0eshift out\x0f\x1b(0lqqk\x1b(B",
    "\x07\x07bells\x00nul\x7fdel",
    // C1 controls as chars: CSI, OSC, ST, DCS.
    "\u{9b}2J\u{9d}0;c1 title\u{9c}\u{90}dcs\u{9c}",
    // Bidi and invisible chars.
    "invoice_\u{202e}fdp.exe",
    "\u{2066}isolate\u{2069} \u{2067}rtl\u{2069} \u{200e}\u{200f}\u{61c}",
    "pa\u{200b}ss\u{200c}wo\u{200d}rd\u{2060}\u{feff}\u{ad}",
    "line\u{2028}separator\u{2029}paragraph",
    "smuggled\u{e0049}\u{e0067}\u{e006e}\u{e006f}\u{e0072}\u{e0065}",
    "\u{3164}\u{115f}\u{ffa0}hangul fillers",
    // A lone ESC at the end, to join the next string's text.
    "trailing escape \x1b",
    "[2Jafter a split escape",
    // Broken UTF-8, decoded.
    "\u{fffd}\u{fffd}\x1b[2J\u{fffd}",
    "\t\ttabs\tand\ttext",
];

/// What in `bytes`, written to a terminal, is not the renderer's: any
/// escape other than a CSI that moves the cursor (`H`), sets colours
/// (`m`), clears (`J`, `K`) or sets a private mode (`?n h`/`l`); any other
/// C0 control; a C1 control; a bidi control; bytes that are not UTF-8.
/// Each finding is described with its offset; empty when clean. A leaked
/// sequence shaped like the renderer's passes: comparing the emulated
/// screen with the frame drawn catches those.
pub fn foreign_controls(bytes: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => return vec![format!("not UTF-8 at {}", e.valid_up_to())],
    };
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '\x1b' => {
                if chars.next_if(|&(_, c)| c == '[').is_none() {
                    found.push(format!("escape that is not a CSI at {at}"));
                    continue;
                }
                let mut seq = String::new();
                while let Some((_, c)) = chars.next_if(|&(_, c)| matches!(c, '0'..='9' | ';' | '?'))
                {
                    seq.push(c);
                }
                let last = chars.next().map(|(_, c)| c);
                let ok = match last {
                    Some('H' | 'm' | 'J' | 'K') => !seq.starts_with('?'),
                    Some('h' | 'l') => {
                        seq.starts_with('?') && seq[1..].chars().all(|c| c.is_ascii_digit())
                    }
                    _ => false,
                };
                if !ok {
                    found.push(format!("CSI {seq:?} {last:?} at {at}"));
                }
            }
            '\0'..='\x1f' | '\x7f'..='\u{9f}' => found.push(format!("control {c:?} at {at}")),
            '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => {
                found.push(format!("bidi {c:?} at {at}"));
            }
            _ => {}
        }
    }
    found
}

#[cfg(test)]
#[path = "hostile_tests.rs"]
mod tests;
