//! The centipede header: the built-in art at each width, which one is
//! drawn and where, and its colours.

use pretty_assertions::assert_eq;

use super::*;

fn text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn header(colors: Colors) -> Header {
    Header {
        colors,
        version: "0.1.0".into(),
        directory: "~/Documents/Projects/viy".into(),
    }
}

fn is_sextant_cell(ch: char) -> bool {
    matches!(ch, ' ' | '\u{2588}' | '\u{258c}' | '\u{2590}')
        || ('\u{1fb00}'..='\u{1fb3b}').contains(&ch)
}

/// Twenty widths, every 12 columns from 80 to 308, the centipede 11 rows
/// high in each, as wide as its width (its tail near the right edge, no
/// row wider than the width less the indent, its head at the left edge),
/// sextant characters only, every block coloured.
#[test]
fn the_art_is_built_in_at_twenty_widths_as_coloured_sextants() {
    assert_eq!(
        widths().collect::<Vec<_>>(),
        (80..=308).step_by(12).collect::<Vec<_>>()
    );
    for width in widths() {
        let (w, art) = pick(width).unwrap();
        assert_eq!(w, width);
        assert_eq!(art.len(), 11, "w{width}");
        assert!(
            art.iter().all(|row| row.len() + INDENT <= width),
            "w{width}"
        );
        assert!(
            art.iter()
                .any(|row| row.first().is_some_and(|c| c.0 != ' ')),
            "w{width}"
        );
        assert!(art.iter().any(|row| row.len() > width * 9 / 10), "w{width}");
        for row in art {
            for &(ch, fg, _) in row {
                assert!(is_sextant_cell(ch), "w{width}: {ch:?}");
                assert!(ch == ' ' || fg.is_some(), "w{width}: {ch:?}");
            }
        }
    }
}

/// The largest that fits, its leftmost block in the column where the
/// title's `>_` starts; nothing below 80.
#[test]
fn the_largest_art_that_fits_starts_where_the_title_does() {
    let h = header(Colors::True);
    let (_, art164) = pick(164).unwrap();
    let plain = |row: &[ArtCell]| row.iter().map(|c| c.0).collect::<String>();
    // 175 columns: the 164 art, two blank columns before it, as the title.
    let lines = h.art(175);
    assert_eq!(lines.len(), art164.len());
    for (line, row) in lines.iter().zip(art164) {
        let want = if row.is_empty() {
            String::new()
        } else {
            format!("  {}", plain(row))
        };
        assert_eq!(text(line), want);
    }
    assert_eq!(pick(91).unwrap().0, 80);
    assert_eq!(pick(92).unwrap().0, 92);
    assert_eq!(pick(175).unwrap().0, 164);
    assert_eq!(pick(400).unwrap().0, 308);
    assert_eq!(h.art(400).len(), 11);
    assert_eq!(h.art(80).len(), 11);
    assert!(h.art(79).is_empty());
    let title = text(&h.lines(175)[art164.len() + 1]);
    assert_eq!(title.find('>'), Some(2));
    for width in [64, 100, 167, 200, 333] {
        assert!(h.lines(width).iter().all(|l| l.width() <= width), "{width}");
    }
}

/// The sextants (1 2 / 3 4 / 5 6, bit n-1) a cell lights: its block's, or
/// all six when it has a background.
fn lit(&(ch, _, bg): &ArtCell) -> u8 {
    if bg.is_some() {
        return 63;
    }
    match ch {
        ' ' => 0,
        '\u{2588}' => 63,
        '\u{258c}' => 21,
        '\u{2590}' => 42,
        _ => {
            // U+1FB00.. counts 1..=62 with 21 and 42 left out.
            let n = ch as u32 - 0x1fb00 + 1;
            (n + u32::from(n >= 21) + u32::from(n >= 41)) as u8
        }
    }
}

/// Drawn from the top of the screen, the art's highest lit pixel is as far
/// below the top edge as its leftmost one is right of the left edge
/// ([`INDENT`] columns in): one row, as two columns, a cell being twice as
/// tall as it is wide.
#[test]
fn the_art_is_as_far_from_the_top_as_from_the_left() {
    for width in widths() {
        let (_, art) = pick(width).unwrap();
        let mut top = usize::MAX;
        let mut left = usize::MAX;
        for (r, row) in art.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let bits = lit(cell);
                for k in 0..6 {
                    if bits >> k & 1 == 1 {
                        top = top.min(r * 3 + k / 2);
                        left = left.min(c * 2 + k % 2);
                    }
                }
            }
        }
        // In a 12 x 24 cell a sextant is 6 wide and 8 tall.
        assert_eq!(top * 8, (INDENT * 2 + left) * 6, "w{width}");
    }
}

#[test]
fn colours_are_the_arts_or_the_nearest_of_256() {
    let (_, art) = pick(160).unwrap();
    let (row, column, fg, bg) = art
        .iter()
        .enumerate()
        .find_map(|(r, cells)| {
            cells
                .iter()
                .enumerate()
                .find_map(|(c, &(ch, fg, bg))| match (ch, fg, bg) {
                    (ch, Some(fg), Some(bg)) if ch != ' ' => Some((r, c, fg, bg)),
                    _ => None,
                })
        })
        .expect("a block with both colours");
    let style_at = |colors: Colors| {
        let line = &header(colors).art(160)[row];
        let mut x = 0;
        line.spans
            .iter()
            .find(|s| {
                let n = s.content.chars().count();
                let hit = (x..x + n).contains(&(column + INDENT));
                x += n;
                hit
            })
            .map(|s| (s.style.fg, s.style.bg))
            .unwrap()
    };
    let rgb = |(r, g, b): Rgb| Color::Rgb(r, g, b);
    let indexed = |(r, g, b): Rgb| Color::Indexed(xterm256(r, g, b));
    assert_eq!(style_at(Colors::True), (Some(rgb(fg)), Some(rgb(bg))));
    assert_eq!(
        style_at(Colors::Indexed),
        (Some(indexed(fg)), Some(indexed(bg)))
    );
    assert_eq!(xterm256(18, 13, 22), 232);
    assert_eq!(xterm256(170, 48, 44), 130);
    assert_eq!(xterm256(255, 255, 255), 231);
}

#[test]
fn only_colour_sequences_are_read_from_the_art() {
    let cells = parse(
        "\x1b[38;2;1;2;3m\u{1fb00}\x1b[48;2;4;5;6m \x1b[39m\u{1fb01}\x1b[49m\x1b[5;31m\u{2588}\x1b[38;2;7;8;9;48;2;1;1;1mx\x1b[0m  \n",
    );
    assert_eq!(
        cells,
        vec![vec![
            ('\u{1fb00}', Some((1, 2, 3)), None),
            (' ', Some((1, 2, 3)), Some((4, 5, 6))),
            ('\u{1fb01}', None, Some((4, 5, 6))),
            ('\u{2588}', None, None),
            ('x', Some((7, 8, 9)), Some((1, 1, 1))),
        ]]
    );
}

/// Under the art, as Codex's session header: a blank row, `  >_ viy (vX)`
/// (the prompt sign in cyan, the name bold, the version dim) and the
/// directory, dim, five columns in. Narrower than the art, they remain.
#[test]
fn the_title_and_directory_are_under_the_art() {
    let h = header(Colors::True);
    let lines = h.lines(168);
    let art = h.art(168).len();
    assert_eq!(lines.len(), art + 3);
    let texts: Vec<String> = lines[art..].iter().map(text).collect();
    assert_eq!(
        texts,
        ["", "  >_ viy (v0.1.0)", "     ~/Documents/Projects/viy"]
    );
    let title = &lines[art + 1].spans;
    assert_eq!(title[1].style.fg, Some(Color::Cyan));
    assert!(title[2].style.add_modifier.contains(Modifier::BOLD));
    assert!(title[3].style.add_modifier.contains(Modifier::DIM));
    assert!(
        lines[art + 2].spans[1]
            .style
            .add_modifier
            .contains(Modifier::DIM)
    );
    let narrow: Vec<String> = h.lines(40).iter().map(text).collect();
    assert_eq!(
        narrow,
        ["", "  >_ viy (v0.1.0)", "     ~/Documents/Projects/viy"]
    );
}

/// A directory too long for the row keeps its first and last parts.
#[test]
fn a_long_directory_is_cut_in_the_middle() {
    assert_eq!(
        fit_path("~/Documents/Projects/viy", 30),
        "~/Documents/Projects/viy"
    );
    assert_eq!(fit_path("~/Documents/Projects/viy", 20), "~/…/Projects/viy");
    assert_eq!(fit_path("~/Documents/Projects/viy", 8), "~/…/viy");
    assert_eq!(fit_path("~/Documents/Projects/viy", 5), "…/viy");
    assert_eq!(fit_path("/a-very-long-folder-name", 10), "…lder-name");
    assert_eq!(fit_path("~", 0), "");
}
