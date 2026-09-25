use pretty_assertions::assert_eq;
use ratatui::style::Color;
use ratatui::style::Modifier;

use super::Row;
use super::finished;
use super::render;
use crate::wrap::text_width;

/// rpctl tests/test_markdown.py `SAMPLE`.
const SAMPLE: &str = r"# Title

Some **bold**, *italic*, `code`, ~~gone~~, a [link](https://x.org), $x^2$
and money: $5 or $10. snake_case_name stays, 2 * 3 * 4 too.

| GPU | VRAM | Notes |
|:----|-----:|:-----:|
| **A100** | 80 GB | fast<br>enough |
| 4090 | 24 GB | 中文 ✅ |

1. one
   - [x] done
> quoted

---

$$
\frac{a}{b} \le \sqrt{c}
$$

```python
x = 2 ** 3  # *not italic*
```
Last *unclosed line";

fn plain(row: &Row) -> String {
    row.line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn lines(source: &str, width: usize) -> Vec<String> {
    render(source, width).iter().map(plain).collect()
}

/// rp's `test_chunking_does_not_change_output`, for a renderer that redraws:
/// at every point of the stream, what has moved up into the transcript is
/// drawn exactly as the whole answer draws it.
#[test]
fn what_is_finished_never_changes() {
    let whole = render(SAMPLE, 80);
    let mut ends: Vec<usize> = SAMPLE.char_indices().map(|(i, _)| i).collect();
    ends.push(SAMPLE.len());
    let mut moved = 0;
    for end in ends {
        let so_far = &SAMPLE[..end];
        let cut = finished(so_far);
        assert!(cut >= moved, "the cut went back at {end}");
        moved = cut;
        let head = render(&so_far[..cut], 80);
        assert!(head.len() <= whole.len(), "at {end}");
        assert_eq!(
            head[..],
            whole[..head.len()],
            "at {end}: {:?}",
            &so_far[..cut]
        );
    }
    assert!(moved > SAMPLE.len() / 2, "most of it moved up");
}

/// rp's `test_table_is_drawn_and_aligned`.
#[test]
fn the_table_is_drawn_and_aligned() {
    let lines = lines(SAMPLE, 80);
    let top = lines.iter().position(|l| l.starts_with('┌')).unwrap();
    let bottom = lines.iter().position(|l| l.starts_with('└')).unwrap();
    let table = &lines[top..=bottom];
    assert_eq!(table.len(), 8, "{table:#?}");
    let widths: std::collections::BTreeSet<usize> = table.iter().map(|l| text_width(l)).collect();
    assert_eq!(widths.len(), 1, "{table:#?}");
    assert!(
        table[3].replace("  ", " ").contains("│ A100 │"),
        "{}",
        table[3]
    );
    assert!(table[3].contains("80 GB │"), "{}", table[3]);
}

/// rp's `test_inline_and_blocks`.
#[test]
fn inline_styles_and_blocks() {
    let out = lines(SAMPLE, 80).join("\n");
    for want in [
        "Some bold, italic, code, gone, a link (https://x.org), x²",
        "$5 or $10",
        "snake_case_name stays, 2 * 3 * 4 too.",
        "☑ done",
        "│ quoted",
        "a/b ≤ √c",
        "x = 2 ** 3  # *not italic*",
        "Last *unclosed line",
    ] {
        assert!(out.contains(want), "{want:?} not in\n{out}");
    }
    assert!(!out.replace("2 ** 3", "").contains("**"), "{out}");
}

/// rp's `test_narrow_table_wraps_inside_width`.
#[test]
fn a_narrow_table_wraps_inside_the_width() {
    let source = format!("| a | b |\n|---|---|\n| short | {}|\n", "word ".repeat(30));
    let rows = lines(&source, 40);
    assert!(rows.len() > 5, "{rows:#?}");
    for row in rows {
        assert!(text_width(&row) <= 40, "{row}");
    }
}

/// rp's `test_too_many_columns_become_records`.
#[test]
fn too_many_columns_become_records() {
    let head: Vec<String> = (0..12).map(|i| format!("h{i}")).collect();
    let source = format!(
        "|{}|\n|{}\n|{}|\n",
        head.join("|"),
        "---|".repeat(12),
        ["v"; 12].join("|")
    );
    let rows = lines(&source, 40);
    assert_eq!(rows[0], "h0: v");
    assert_eq!(rows[11], "h11: v");
}

#[test]
fn every_row_fits_the_width() {
    for width in [20, 40, 60, 80] {
        for row in lines(SAMPLE, width) {
            assert!(text_width(&row) <= width, "{width}: {row:?}");
        }
    }
}

#[test]
fn blank_rows_follow_the_source() {
    assert_eq!(
        lines("# Head\ntext\n\nmore", 80),
        ["Head", "text", "", "more"]
    );
    assert_eq!(lines("- a\n- b\n\n- c", 80), ["• a", "• b", "", "• c"]);
}

#[test]
fn headings_are_coloured_without_their_marks() {
    let rows = render("# One\n## Two\n### Three\n#### Four", 80);
    let style = |i: usize| rows[i].line.spans[0].style;
    assert_eq!(plain(&rows[0]), "One");
    assert_eq!(style(0).fg, Some(Color::Magenta));
    assert!(style(0).add_modifier.contains(Modifier::UNDERLINED));
    assert_eq!(style(2).fg, Some(Color::Blue));
    assert!(style(3).add_modifier.contains(Modifier::BOLD));
}

#[test]
fn lists_nest_with_hanging_indents() {
    let source = "1. first item that is long enough to wrap\n   - inner\n     - deeper\n2) two";
    assert_eq!(
        lines(source, 24),
        [
            "1. first item that is",
            "   long enough to wrap",
            "   ◦ inner",
            "     ▪ deeper",
            "2) two",
        ]
    );
}

#[test]
fn quotes_code_and_rules() {
    assert_eq!(lines("> a\n>\n> b", 80), ["│ a", "│ ", "│ b"]);
    assert_eq!(
        lines("```rust\nfn main() {}\n\n```", 80),
        ["╭─ rust", "fn main() {}", "", "╰─"]
    );
    assert_eq!(lines("```\nopen", 80), ["╭─", "open", "╰─"]);
    assert_eq!(lines("---", 10), ["──────────"]);
}

#[test]
fn links_images_and_autolinks() {
    assert_eq!(lines("[x](https://a.b)", 80), ["x (https://a.b)"]);
    assert_eq!(lines("[https://a.b](https://a.b)", 80), ["https://a.b"]);
    assert_eq!(lines("<https://a.b>", 80), ["https://a.b"]);
    assert_eq!(lines("![cat](c.png)", 80), ["[image: cat] c.png"]);
    assert_eq!(lines("a<br>b", 80), ["a", "b"]);
}

/// rp takes `\(...\)` and `\[...\]` as math too; CommonMark reads them as
/// escaped brackets.
#[test]
fn backslash_delimiters_are_math() {
    assert_eq!(
        lines(r"so \( \alpha_1 \) and \[x^2\]", 80),
        ["so α₁ and x²"]
    );
    assert_eq!(lines("\\[\n\\frac{a}{b}\n\\]", 80), ["    a/b"]);
    assert_eq!(lines(r"costs $5 and \(x\) now", 80), ["costs $5 and x now"]);
    assert_eq!(lines(r"`\(x\)` stays", 80), [r"\(x\) stays"]);
    assert_eq!(lines("```\n\\(x\\)\n```", 80), ["╭─", r"\(x\)", "╰─"]);
    assert_eq!(lines(r"just \( open", 80), [r"just ( open"]);
}

#[test]
fn keys_grow_down_the_rows() {
    for width in [12, 30, 80] {
        let rows = render(SAMPLE, width);
        for pair in rows.windows(2) {
            assert!(pair[0].key < pair[1].key, "{width}: {pair:?}");
        }
    }
}

#[test]
fn finished_is_where_the_last_block_starts() {
    assert_eq!(finished(""), 0);
    assert_eq!(finished("one paragraph\nstill"), 0);
    assert_eq!(finished("one\n\ntwo"), 0, "a line still arriving");
    assert_eq!(finished("one\n\ntwo\n"), 5);
    assert_eq!(finished("one\n\n```\ncode\n\nmore\n"), 5);
    assert_eq!(
        finished("| a |\n|---|\n| b |\n|"),
        0,
        "a row still arriving"
    );
    assert_eq!(finished("- a\n\n- b"), 0);
}

#[test]
fn escapes_in_the_text_are_shown_not_applied() {
    let source =
        "a [l\x1b]8;;x\x07](https://e.x/\x1b[2J) | b\n\n| h |\n|---|\n| \x1b]52;c;QQ==\x07 |";
    for row in lines(source, 80) {
        assert!(!row.contains('\x1b') && !row.contains('\x07'), "{row:?}");
    }
}

/// The sample as rows, for the eye and the snapshot.
#[test]
fn the_sample_at_60_columns() {
    insta::assert_snapshot!(lines(SAMPLE, 60).join("\n"));
}

#[test]
fn a_lone_carriage_return_is_shown() {
    assert_eq!(lines("line\rOVER", 80), ["line␍OVER"]);
    assert_eq!(lines("a\r\nb", 80), ["a", "b"]);
    assert_eq!(super::visible_cr("a\r", false), "a\r");
    assert_eq!(super::visible_cr("a\r", true), "a␍");
}
