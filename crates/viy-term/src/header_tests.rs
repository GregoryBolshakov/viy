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

/// Every width from 64 to 320 in steps of 8, the scene whole in each: no
/// row wider than its width, the height in proportion (26 rows at 320, as
/// the script's master), sextant characters only, every block coloured.
#[test]
fn the_art_is_built_in_at_every_eighth_width_as_coloured_sextants() {
    assert_eq!(
        widths().collect::<Vec<_>>(),
        (64..=320).step_by(8).collect::<Vec<_>>()
    );
    for width in widths() {
        let (w, art) = pick(width).unwrap();
        assert_eq!(w, width);
        // The generator's Python round(): halves to even.
        let rows = ((width * 104) as f64 / 320.0 / 4.0).round_ties_even() as usize;
        assert_eq!(art.len(), rows.max(1), "w{width}");
        assert!(art.iter().all(|row| row.len() <= width), "w{width}");
        assert!(art.iter().any(|row| row.len() > width * 9 / 10), "w{width}");
        for row in art {
            for &(ch, fg, _) in row {
                assert!(is_sextant_cell(ch), "w{width}: {ch:?}");
                assert!(ch == ' ' || fg.is_some(), "w{width}: {ch:?}");
            }
        }
    }
    assert_eq!(pick(320).unwrap().1.len(), 26);
}

/// The largest that fits, centred with blank margins; nothing below 64.
#[test]
fn the_largest_art_that_fits_is_drawn_centred() {
    let h = header(Colors::True);
    let (_, art168) = pick(168).unwrap();
    let plain = |row: &[ArtCell]| row.iter().map(|c| c.0).collect::<String>();
    // 175 columns: the 168 art, (175 - 168) / 2 = 3 blank columns before it.
    let lines = h.art(175);
    assert_eq!(lines.len(), art168.len());
    for (line, row) in lines.iter().zip(art168) {
        let want = if row.is_empty() {
            String::new()
        } else {
            format!("   {}", plain(row))
        };
        assert_eq!(text(line), want);
    }
    assert_eq!(h.art(168).len(), 14);
    assert_eq!(h.art(400).len(), 26);
    assert_eq!(h.art(64).len(), pick(64).unwrap().1.len());
    assert!(h.art(63).is_empty());
    for width in [64, 100, 167, 200, 333] {
        assert!(h.lines(width).iter().all(|l| l.width() <= width), "{width}");
    }
}

#[test]
fn colours_are_the_arts_or_the_nearest_of_256() {
    let (_, art) = pick(168).unwrap();
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
        let line = &header(colors).art(168)[row];
        let mut x = 0;
        line.spans
            .iter()
            .find(|s| {
                let n = s.content.chars().count();
                let hit = (x..x + n).contains(&column);
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
