use pretty_assertions::assert_eq;

use super::*;

#[test]
fn idle_and_spinning_titles() {
    assert_eq!(
        title(Some("Pod costs"), None, Encoding::Unicode),
        "✳ Pod costs"
    );
    assert_eq!(
        title(Some("Pod costs"), Some(2), Encoding::Unicode),
        "⠹ Pod costs"
    );
    assert_eq!(
        title(Some("Pod costs"), Some(12), Encoding::Unicode),
        "⠹ Pod costs"
    );
    assert_eq!(title(None, None, Encoding::Unicode), "✳ viy chat");
    // A name that cleans to nothing is no name.
    assert_eq!(
        title(Some("\u{202e}\x1b "), None, Encoding::Unicode),
        "✳ viy chat"
    );
}

#[test]
fn a_long_name_is_cut_to_48_chars() {
    let name = "word ".repeat(20);
    let shown = title(Some(&name), None, Encoding::Unicode);
    let name = shown.strip_prefix("✳ ").unwrap();
    assert_eq!(name.chars().count(), 48);
    // Cut mid-word, as rp cuts it.
    assert!(name.ends_with(" wo…"), "{name}");
    // A cut that ends on a space leaves no space before the ellipsis.
    let long = format!("{} {}", "x".repeat(46), "y".repeat(20));
    let shown = title(Some(&long), None, Encoding::Unicode);
    assert_eq!(shown, format!("✳ {}…", "x".repeat(46)));
}

/// rp's `test_model_title_cannot_inject_escapes`: a title holding BEL, an
/// OSC 52 clipboard write and C1 controls writes one BEL, its own.
#[test]
fn model_title_cannot_inject_escapes() {
    let shown = title(
        Some("ok\x07\x1b]52;c;ZXZpbA==\x07\u{9d}0;x\u{9c}"),
        None,
        Encoding::Unicode,
    );
    let seq = osc(&shown);
    assert_eq!(seq.matches('\x07').count(), 1);
    assert_eq!(seq.matches('\x1b').count(), 1);
    assert!(!shown.chars().any(char::is_control), "{shown:?}");
    assert_eq!(shown, "✳ ok]52;c;ZXZpbA==0;x");
}

/// Codex's cases.
#[test]
fn controls_whitespace_and_invisible_chars_are_dropped() {
    assert_eq!(
        sanitize(
            "  Project\t|\nWorking\x1b\x07\u{009D}\u{009C} |  Thread  ",
            Encoding::Unicode
        ),
        "Project | Working | Thread"
    );
    assert_eq!(
        sanitize(
            "Pro\u{202E}j\u{2066}e\u{200F}c\u{061C}t\u{200B} \u{FEFF}T\u{2060}itle",
            Encoding::Unicode
        ),
        "Project Title"
    );
    assert_eq!(
        sanitize("a\u{2028}b\u{2029}c\u{E0041}d\u{FE0F}", Encoding::Unicode),
        "a b cd"
    );
}

#[test]
fn screen_gets_printable_ascii_only() {
    assert_eq!(
        title(Some("café ⠇ naïve"), Some(0), Encoding::Screen),
        "| caf? | na?ve"
    );
    assert_eq!(title(None, None, Encoding::Screen), "* viy chat");
    let long = title(Some(&"é".repeat(60)), None, Encoding::Screen);
    assert!(long.is_ascii(), "{long}");
    assert!(long.ends_with("?*"), "{long}");
}

#[test]
fn the_sequences() {
    assert_eq!(osc("✳ x"), "\x1b]0;✳ x\x07");
    assert_eq!(PUSH, "\x1b[22;0t");
    assert_eq!(POP, "\x1b[23;0t");
}
