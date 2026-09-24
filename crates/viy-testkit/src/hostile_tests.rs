use pretty_assertions::assert_eq;

use super::*;

#[test]
fn the_renderers_own_sequences_are_clean() {
    let frame =
        b"\x1b[?2026h\x1b[?25l\x1b[1;1H\x1b[38;5;6mhi\x1b[0m\x1b[2J\x1b[K\x1b[?1049h\x1b[?2026l";
    assert_eq!(foreign_controls(frame), Vec::<String>::new());
    assert_eq!(
        foreign_controls("␛[2J <U+202E> ❤\u{fe0f}".as_bytes()),
        Vec::<String>::new()
    );
}

/// Sequences shaped like the renderer's (`CSI 2J`, `CSI H`, SGR, modes)
/// pass here; the widget tests catch those by comparing the emulated screen
/// with the frame drawn.
#[test]
fn what_the_renderer_never_writes_is_foreign() {
    for text in [
        "\x1b]0;t\x07",
        "\x1b]52;c;?\x07",
        "\x1bP+q544e\x1b\\",
        "\x1bcreset",
        "\x1b[6n",
        "\x1b[1A",
        "\x1b[5;10r",
        "\x07",
        "a\rb",
        "\u{9b}2J",
        "\u{202e}",
        "\u{2028}",
    ] {
        assert!(!foreign_controls(text.as_bytes()).is_empty(), "{text:?}");
    }
    assert_eq!(foreign_controls(b"\xff"), ["not UTF-8 at 0"]);
    assert_eq!(foreign_controls(b"a\x1b[6n"), ["CSI \"6\" Some('n') at 1"]);
    assert_eq!(
        foreign_controls(b"\x1b[?1;2$p"),
        ["CSI \"?1;2\" Some('$') at 0"]
    );
}

/// A U+FFFD cut between two writes reaches the emulator whole, as its
/// stand-in, so nothing is recorded and the row does not shift.
#[test]
fn a_replacement_char_cut_between_writes_is_fed_whole() {
    let mut parser = crate::screen::parser(2, 10);
    let mut feed = crate::screen::Feed::default();
    feed.process(&mut parser, b"a\xef");
    feed.process(&mut parser, b"\xbf");
    feed.process(&mut parser, b"\xbdb");
    assert_eq!(parser.callbacks().foreign(), Vec::<String>::new());
    assert_eq!(crate::screen::rows(parser.screen())[0], "a\u{fffd}b▮");
}
