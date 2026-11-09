//! The tokenizer against Python 3.10.12-1~22.04.18's `HTMLParser` on
//! pages chosen for its edges; the expected events are what a subclass
//! recording each handler printed (2026-07-26). The extractor's oracle
//! fuzz (html_tests.rs) checks the rest through rpctl.

use pretty_assertions::assert_eq;

use super::*;

/// The events as `S tag [k=v k]`, `SE ...`, `E tag`, `D text`.
fn events(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    parse(html, |e| {
        out.push(match e {
            Event::Start {
                tag,
                attrs,
                self_closing,
            } => {
                let attrs: Vec<String> = attrs
                    .iter()
                    .map(|(k, v)| match v {
                        Some(v) => format!("{k}={v}"),
                        None => k.clone(),
                    })
                    .collect();
                let kind = if self_closing { "SE" } else { "S" };
                format!("{kind} {tag} [{}]", attrs.join(" "))
            }
            Event::End(tag) => format!("E {tag}"),
            Event::Data(text) => format!("D {text}"),
        });
        Ok(())
    });
    out
}

#[test]
fn tags_attributes_and_text() {
    let cases: [(&str, &[&str]); 9] = [
        (
            r#"<p class=a id="b" hidden data-x='y'>t&amp;x</p>"#,
            &["S p [class=a id=b hidden data-x=y]", "D t&x", "E p"],
        ),
        ("<a href=x/>t</a>", &["S a [href=x/]", "D t", "E a"]),
        ("<br/><div/>", &["SE br []", "SE div []"]),
        ("<P CLASS=X>y</P>", &["S p [class=X]", "D y", "E p"]),
        (r#"<p "a"=b =c>x"#, &[r#"S p ["a"=b =c]"#, "D x"]),
        ("<p a= 'x>b", &["S p [a= 'x]", "D b"]),
        ("<p a='x>b", &[]),
        ("a<b c", &["D a"]),
        ("a < b & c", &["D a ", "D <", "D  b & c"]),
    ];
    for (html, want) in cases {
        assert_eq!(events(html), want, "{html}");
    }
}

#[test]
fn raw_text_comments_and_declarations() {
    let cases: [(&str, &[&str]); 6] = [
        (
            r#"<script>if (a<b) x="</div>"</script >after"#,
            &[
                "S script []",
                r#"D if (a<b) x="</div>""#,
                "E script",
                "D after",
            ],
        ),
        (
            "<title>a<b>&amp;</title>",
            &["S title []", "D a<b>&", "E title"],
        ),
        ("<!-->x-->y<!---->z<!-- a --!>w", &["D y", "D z", "D w"]),
        (
            "<![CDATA[ a > b ]]>c<!doctype html>d<?pi x>e<!x>f</ x>g</>h",
            &["D c", "D d", "D e", "D f", "D g", "D h"],
        ),
        ("</", &["D </"]),
        ("<plaintext><p>x", &["S plaintext []", "D <p>x"]),
    ];
    for (html, want) in cases {
        assert_eq!(events(html), want, "{html}");
    }
}

#[test]
fn references_as_html_unescape() {
    assert_eq!(
        unescape("&nbspx &amp &copy=2 &#x41;&#0;&#128;&#xD800;&#99999999999;&#11;&notit;&zz;")
            .unwrap(),
        "\u{a0}x & ©=2 A\u{fffd}€\u{fffd}\u{fffd}¬it;&zz;"
    );
    assert_eq!(
        events(r#"<p title="&nbspx &amp &copy=2 &#x41;&#0;&#128;&#xD800;&#99999999999;">"#),
        ["S p [title=\u{a0}x & ©=2 A\u{fffd}€\u{fffd}\u{fffd}]"]
    );
}

/// Past 4300 digits Python's `int()` raises and rpctl's parser stops
/// there, the text it was decoding included; hex has no such limit.
#[test]
fn a_reference_past_int_digits_ends_the_page() {
    let digits = |n: usize| format!("{}65;", "0".repeat(n - 2));
    let at = |n: usize| {
        format!(
            "<p>before</p><p>&#{} LEAK</p><p title=x>more</p>",
            digits(n)
        )
    };
    assert_eq!(
        events(&at(4300)),
        [
            "S p []",
            "D before",
            "E p",
            "S p []",
            "D A LEAK",
            "E p",
            "S p [title=x]",
            "D more",
            "E p"
        ]
    );
    assert_eq!(events(&at(4301)), ["S p []", "D before", "E p", "S p []"]);
    let attr = format!("<p>before</p><p title=\"&#{}\">LEAK</p>", digits(4301));
    assert_eq!(events(&attr), ["S p []", "D before", "E p"]);
    let hex = format!("<p>&#x{}41;</p>", "0".repeat(5000));
    assert_eq!(events(&hex), ["S p []", "D A", "E p"]);
}
