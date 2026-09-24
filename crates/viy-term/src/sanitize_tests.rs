use pretty_assertions::assert_eq;

use super::*;

fn sanitize(text: &str) -> Cow<'_, str> {
    sanitize_after(None, text)
}

fn sanitize_line(text: &str) -> Cow<'_, str> {
    sanitize_line_after(None, text)
}
use viy_testkit::hostile::CORPUS;

#[test]
fn plain_text_is_borrowed_unchanged() {
    for text in ["", "hello", "日本語 ćafé", "tab\there\nnewline", "❤️ 👨‍👩‍👧 1️⃣"]
    {
        assert!(
            matches!(sanitize(text), Cow::Borrowed(t) if t == text),
            "{text:?}"
        );
    }
}

#[test]
fn c0_controls_and_del_become_control_pictures() {
    assert_eq!(sanitize("a\x1b[2Jb\x07\x7f\r\0"), "a␛[2Jb␇␡␍␀");
    assert_eq!(sanitize_line("a\nb\tc"), "a␊b␉c");
}

#[test]
fn c1_controls_bidi_and_invisible_chars_show_their_code_point() {
    assert_eq!(sanitize("\u{9b}31m"), "<U+009B>31m");
    assert_eq!(sanitize("\u{9d}0;t\u{9c}"), "<U+009D>0;t<U+009C>");
    assert_eq!(
        sanitize("admin\u{202e}gnp.exe"),
        "admin<U+202E>gnp.exe",
        "Trojan Source reordering"
    );
    assert_eq!(sanitize("\u{2066}x\u{2069}"), "<U+2066>x<U+2069>");
    assert_eq!(
        sanitize("a\u{200e}b\u{200f}c\u{61c}"),
        "a<U+200E>b<U+200F>c<U+061C>"
    );
    assert_eq!(sanitize("pass\u{200b}word"), "pass<U+200B>word");
    assert_eq!(
        sanitize("\u{feff}x\u{2060}y\u{ad}"),
        "<U+FEFF>x<U+2060>y<U+00AD>"
    );
    assert_eq!(sanitize("a\u{2028}b\u{2029}"), "a<U+2028>b<U+2029>");
    assert_eq!(
        sanitize("\u{3164}\u{115f}\u{ffa0}"),
        "<U+3164><U+115F><U+FFA0>"
    );
    // ASCII smuggled in tag chars, and the rest of Default_Ignorable.
    assert_eq!(sanitize("ok\u{e0041}\u{e0042}"), "ok<U+E0041><U+E0042>");
    assert_eq!(
        sanitize("a\u{e0080}\u{e0fff}\u{fff0}\u{fff8}\u{fffb}b"),
        "a<U+E0080><U+E0FFF><U+FFF0><U+FFF8><U+FFFB>b"
    );
    assert_eq!(sanitize("\u{e0080}\u{200d}"), "<U+E0080><U+200D>");
}

#[test]
fn joiners_and_selectors_pass_inside_emoji_and_scripts_only() {
    for text in [
        "❤\u{fe0f}",              // ❤️
        "❤\u{fe0f}\u{200d}🔥",    // ❤️‍🔥
        "👨\u{200d}👩\u{200d}👧", // family
        "1\u{fe0f}\u{20e3}",      // keycap
        "می\u{200c}خواهم",        // Persian ZWNJ
        "क\u{94d}\u{200d}ष",      // Devanagari ZWJ
        "葛\u{e0100}",            // ideographic variation
    ] {
        assert_eq!(sanitize(text), text, "{text:?}");
    }
    assert_eq!(sanitize("a\u{200d}b"), "a<U+200D>b", "after ASCII");
    assert_eq!(sanitize("\u{200d}x"), "<U+200D>x", "at the start");
    assert_eq!(
        sanitize("é\u{200d}\u{200d}"),
        "é\u{200d}<U+200D>",
        "one at a time"
    );
    assert_eq!(sanitize("a\u{fe0f}"), "a<U+FE0F>");
    assert_eq!(sanitize("❤\u{fe0f}\u{fe0f}"), "❤\u{fe0f}<U+FE0F>");
    assert_eq!(
        sanitize("\x1b\u{fe0f}"),
        "␛\u{fe0f}",
        "after a picture: visible"
    );
}

#[test]
fn a_stream_continues_after_its_last_char() {
    assert_eq!(sanitize_after(Some('👨'), "\u{200d}👩"), "\u{200d}👩");
    assert_eq!(sanitize_after(Some('a'), "\u{200d}"), "<U+200D>");
}

#[test]
fn sanitizing_twice_changes_nothing_more() {
    for text in CORPUS {
        let once = sanitize_line(text).into_owned();
        assert_eq!(sanitize_line(&once), once, "{text:?}");
        assert!(!unsafe_in_cell(&once), "{text:?}");
        let once = sanitize(text).into_owned();
        assert_eq!(sanitize(&once), once, "{text:?}");
    }
}

#[test]
fn unsafe_in_cell_finds_controls_and_bidi() {
    assert!(unsafe_in_cell("\x1b"));
    assert!(unsafe_in_cell("a\u{9b}"));
    assert!(unsafe_in_cell("\u{202e}"));
    assert!(unsafe_in_cell("\u{2028}"));
    assert!(!unsafe_in_cell("❤\u{fe0f}"));
    assert!(!unsafe_in_cell("␛"));
}

#[test]
fn glyphs_are_graphemes_measured_as_ratatui_measures_them() {
    let got: Vec<(String, usize, usize, usize)> =
        glyphs("a❤\u{fe0f}1\u{fe0f}\u{20e3}\u{202e}\tb", None)
            .into_iter()
            .map(|g| (g.text, g.width, g.chars, g.at))
            .collect();
    let mut want = vec![
        ("a".to_string(), 1, 1, 0),
        ("❤\u{fe0f}".to_string(), 2, 2, 1),
        ("1\u{fe0f}\u{20e3}".to_string(), 2, 3, 7),
    ];
    for (i, c) in "<U+202E>".chars().enumerate() {
        want.push((c.to_string(), 1, usize::from(i == 0), 14));
    }
    want.push(("\t".to_string(), 0, 1, 17));
    want.push(("b".to_string(), 1, 1, 18));
    assert_eq!(got, want);
}
