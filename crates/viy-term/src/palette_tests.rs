//! The terminal's default colours, from its answers to OSC 10 and 11.

use pretty_assertions::assert_eq;

use super::*;

#[test]
fn colours_are_read_from_either_terminator() {
    let buf = b"\x1b]10;rgb:d0d0/d6d6/dcdc\x1b\\\x1b]11;rgb:18/18/1e\x07\x1b[?65;1;9c";
    assert!(answered(buf));
    assert_eq!(osc_color(buf, b"10"), Some((0xd0, 0xd6, 0xdc)));
    assert_eq!(osc_color(buf, b"11"), Some((0x18, 0x18, 0x1e)));
}

#[test]
fn no_answer_is_no_colour() {
    let buf = b"\x1b[?62;22c";
    assert!(answered(buf));
    assert_eq!(osc_color(buf, b"11"), None);
    assert!(!answered(b"\x1b]11;rgb:0/0/0\x07"));
}
