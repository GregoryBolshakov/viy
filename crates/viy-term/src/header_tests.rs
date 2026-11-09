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
        ghost: false,
        art: true,
        dissolve: 0,
    }
}

fn is_sextant_cell(ch: char) -> bool {
    matches!(ch, ' ' | '\u{2588}' | '\u{258c}' | '\u{2590}')
        || ('\u{1fb00}'..='\u{1fb3b}').contains(&ch)
}

/// 25 widths, all 11 rows high: the larva at 32; then the centipede every
/// 10 columns from 40 to 70 and every 12 from 80 to 308, as wide as its
/// width (its tail near the right edge). No row is wider than
/// the width less the indent, every art's head is at its left edge, and
/// they hold sextant characters only, every block coloured.
#[test]
fn the_art_is_built_in_at_25_widths_as_coloured_sextants() {
    assert_eq!(
        widths().collect::<Vec<_>>(),
        std::iter::once(32)
            .chain((40..=70).step_by(10))
            .chain((80..=308).step_by(12))
            .collect::<Vec<_>>()
    );
    for width in widths() {
        let (w, art) = pick(width).unwrap();
        assert_eq!(w, width);
        let larva = width < 40;
        assert_eq!(art.len(), 11, "w{width}");
        assert!(
            art.iter().all(|row| row.len() + INDENT <= width),
            "w{width}"
        );
        // The centipede's antenna reaches the left edge; the larva's,
        // shorter, ends 3 columns in, its head where the centipede's is.
        let first = art
            .iter()
            .filter_map(|row| row.iter().position(|c| c.0 != ' ' || c.2.is_some()))
            .min();
        assert_eq!(first, Some(if larva { 3 } else { 0 }), "w{width}");
        if !larva {
            assert!(art.iter().any(|row| row.len() > width * 9 / 10), "w{width}");
        }
        for row in art {
            for &(ch, fg, _) in row {
                assert!(is_sextant_cell(ch), "w{width}: {ch:?}");
                assert!(ch == ' ' || fg.is_some(), "w{width}: {ch:?}");
            }
        }
    }
}

/// The largest that fits, its leftmost block in the column where the
/// title's `>_` starts; nothing below 32.
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
    assert_eq!(pick(32).unwrap().0, 32);
    assert_eq!(pick(39).unwrap().0, 32);
    assert_eq!(h.art(39).len(), 11);
    assert_eq!(pick(40).unwrap().0, 40);
    assert_eq!(pick(49).unwrap().0, 40);
    assert_eq!(pick(50).unwrap().0, 50);
    assert_eq!(pick(79).unwrap().0, 70);
    assert_eq!(pick(91).unwrap().0, 80);
    assert_eq!(pick(92).unwrap().0, 92);
    assert_eq!(pick(175).unwrap().0, 164);
    assert_eq!(pick(400).unwrap().0, 308);
    assert_eq!(h.art(400).len(), 11);
    assert_eq!(h.art(80).len(), 11);
    assert_eq!(h.art(79).len(), 11);
    assert!(h.art(31).is_empty());
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

/// The head is in the same place at every width (the larva's, the same
/// picture tinted, with shorter antennae and tusks, has the same top), so
/// resizing a terminal changes only the body. Drawn from the top of the
/// screen, from 60 up the legs reach highest, as far below the top edge as
/// the art's leftmost pixel is right of the left edge ([`INDENT`] columns
/// in): one row, as two columns, a cell being twice as tall as it is wide.
/// Below 60 (the larva, 40, 50) the head is highest, a sextant row lower.
#[test]
fn the_art_is_as_far_from_the_top_as_from_the_left() {
    let head = |art: &[Vec<ArtCell>]| -> Vec<Vec<u8>> {
        // The head's corner: 16 columns of the first 8 rows, as lit sextants.
        art[..8]
            .iter()
            .map(|row| (0..16).map(|c| row.get(c).map_or(0, lit)).collect())
            .collect()
    };
    let (_, art80) = pick(80).unwrap();
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
        // In a 12 x 24 cell a sextant is 6 wide and 8 tall. The larva's
        // shorter antenna ends 6 sextants (3 columns) further in.
        let lower = if width < 60 { 8 } else { 0 };
        let shorter = if width < 40 { 6 } else { 0 };
        assert_eq!(
            top * 8,
            (INDENT * 2 + left - shorter) * 6 + lower,
            "w{width}"
        );
        // The larva's head is the same picture, tinted, so its blocks differ.
        if width >= 40 {
            assert_eq!(head(art), head(art80), "w{width}");
        }
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
/// directory, dim, five columns in. Narrower than the narrowest art, they
/// remain, at the top.
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
    // Below the narrowest art, the title and the directory, cut to fit.
    let narrow: Vec<String> = h.lines(31).iter().map(text).collect();
    assert_eq!(
        narrow,
        ["  >_ viy (v0.1.0)", "     ~/Documents/Projects/viy"]
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

/// The ghost (CHAT_HEADER=1): Braille cells with a foreground only, never
/// a background, so whatever is behind the terminal shows between the
/// dots; built in at the coloured art's widths (the larva's at 32); no
/// row wider than the width less the indent; none below [`MIN_WIDTH`].
#[test]
fn the_ghost_is_braille_on_the_terminals_own_background() {
    let h = Header {
        ghost: true,
        ..header(Colors::True)
    };
    assert!(h.art(MIN_WIDTH - 1).is_empty());
    assert_eq!(
        GHOSTS.iter().map(|(w, _)| *w).collect::<Vec<_>>(),
        widths().collect::<Vec<_>>()
    );
    for &(w, _) in GHOSTS {
        let art = h.art(w);
        assert_eq!(art.len(), 11, "g{w}");
        for line in &art {
            assert!(line.width() <= w, "g{w}");
            for span in &line.spans {
                assert_eq!(span.style.bg, None, "g{w}");
                assert!(
                    span.content
                        .chars()
                        .all(|c| c == ' ' || ('\u{2801}'..='\u{28ff}').contains(&c)),
                    "g{w}"
                );
            }
        }
    }
}

/// Its ink is a faint step from the terminal's background toward the
/// tone: on a dark terminal a little lighter, on a light one a little
/// darker, never the tone itself.
#[test]
fn the_ghosts_ink_is_close_to_the_background() {
    let dark = crate::palette::DefaultColors {
        fg: (210, 214, 220),
        bg: (24, 24, 30),
    };
    let light = crate::palette::DefaultColors {
        fg: (40, 40, 46),
        bg: (250, 249, 246),
    };
    let tone = (139, 91, 97);
    let ink = ghost_ink(tone, dark, 1.0);
    assert!(ink.0 > dark.bg.0 && ink.0 < 90, "{ink:?}");
    let ink = ghost_ink(tone, light, 1.0);
    assert!(ink.0 < light.bg.0 && ink.0 > 180, "{ink:?}");
    // A cell's own opacity takes it nearer the background still.
    let faint = ghost_ink(tone, dark, 0.5);
    assert!(
        faint.0 > dark.bg.0 && faint.0 < ghost_ink(tone, dark, 1.0).0,
        "{faint:?}"
    );
}

/// With the ghost the title and the directory come first, at the very
/// top, then a blank row and the ghost under them; below the art's
/// narrowest width only the two lines.
#[test]
fn the_ghost_sits_under_the_title_and_directory() {
    let h = Header {
        ghost: true,
        ..header(Colors::True)
    };
    let lines = h.lines(140);
    assert_eq!(text(&lines[0]), "  >_ viy (v0.1.0)");
    assert_eq!(text(&lines[1]), "     ~/Documents/Projects/viy");
    assert_eq!(text(&lines[2]), "");
    assert_eq!(lines.len(), 3 + 11);
    assert_eq!(&lines[3..], h.art(140).as_slice());
    assert_eq!(h.lines(MIN_WIDTH - 1).len(), 2);
}

/// Braille dots in `lines`.
fn dots(lines: &[Line<'_>]) -> u32 {
    lines
        .iter()
        .flat_map(|l| l.spans.iter().flat_map(|s| s.content.chars()))
        .filter_map(|c| u32::from(c).checked_sub(0x2800).filter(|&d| d <= 0xff))
        .map(u32::count_ones)
        .sum()
}

/// Dissolving, the ghost's dots drop out a few at a time, each gone for
/// good once it has, until none is left; the rows stay, and a cell with
/// no dot left is a space.
#[test]
fn the_ghost_dissolves_dot_by_dot() {
    let at = |dissolve| Header {
        ghost: true,
        dissolve,
        ..header(Colors::True)
    };
    let whole = dots(&at(0).art(140));
    let mut last = whole;
    for step in (100..=1000).step_by(100) {
        let art = at(step).art(140);
        assert_eq!(art.len(), 11);
        let now = dots(&art);
        assert!(now <= last, "{step}: {now} after {last}");
        last = now;
        for line in &art {
            for span in &line.spans {
                assert!(!span.content.contains('\u{2800}'), "{step}");
            }
        }
    }
    let half = dots(&at(500).art(140));
    assert!(
        half > whole / 3 && half < whole * 2 / 3,
        "{half} of {whole}"
    );
    assert_eq!(last, 0);
}

/// The coloured art's cells fade into the terminal's background, each at
/// its own moment: halfway, some are gone and some still in their own
/// colours; at the end every one is the background.
#[test]
fn the_coloured_art_fades_cell_by_cell() {
    let at = |dissolve| Header {
        dissolve,
        ..header(Colors::True)
    };
    let bg = crate::palette::default_colors().unwrap_or(FALLBACK).bg;
    let background = Color::Rgb(bg.0, bg.1, bg.2);
    let colours = |dissolve| -> Vec<Option<Color>> {
        at(dissolve)
            .art(140)
            .iter()
            .flat_map(|l| {
                l.spans
                    .iter()
                    .flat_map(|s| s.content.chars().map(|_| s.style.bg))
            })
            .collect()
    };
    let whole = colours(0);
    let half = colours(500);
    let end = colours(1000);
    assert_eq!(whole.len(), end.len());
    let lit = whole.iter().filter(|c| c.is_some()).count();
    let faded = half.iter().filter(|&&c| c == Some(background)).count();
    let kept = whole
        .iter()
        .zip(&half)
        .filter(|(a, b)| a.is_some() && a == b)
        .count();
    assert!(faded > 0 && kept > 0, "{faded} faded, {kept} kept of {lit}");
    assert!(end.iter().all(|c| c.is_none() || *c == Some(background)));
}

/// Dissolving only takes a colour toward the terminal's background, in
/// every channel, frame after frame: no pixel ever turns darker or lighter
/// than the line from its colour to the background, at any width, ghost
/// or coloured.
#[test]
fn the_dissolve_only_moves_colours_toward_the_background() {
    let bg = FALLBACK.bg;
    let between = |now: u8, was: u8, b: u8| now >= was.min(b) && now <= was.max(b);
    for ghost in [true, false] {
        for &(w, _) in ARTS {
            let mut last: std::collections::HashMap<(usize, usize, bool), Rgb> =
                std::collections::HashMap::new();
            for dissolve in (0..=1000).step_by(10) {
                let h = Header {
                    ghost,
                    dissolve,
                    ..header(Colors::True)
                };
                for (y, line) in h.art(w).iter().enumerate() {
                    let cells = line
                        .spans
                        .iter()
                        .flat_map(|s| s.content.chars().map(move |_| s.style));
                    for (x, style) in cells.enumerate() {
                        for (is_bg, colour) in [(false, style.fg), (true, style.bg)] {
                            let Some(Color::Rgb(r, g, b)) = colour else {
                                continue;
                            };
                            if let Some(&(pr, pg, pb)) = last.get(&(y, x, is_bg)) {
                                assert!(
                                    between(r, pr, bg.0)
                                        && between(g, pg, bg.1)
                                        && between(b, pb, bg.2),
                                    "ghost {ghost}, w{w}, {dissolve}: ({y}, {x}) went from {:?} to {:?}",
                                    (pr, pg, pb),
                                    (r, g, b)
                                );
                            }
                            last.insert((y, x, is_bg), (r, g, b));
                        }
                    }
                }
            }
        }
    }
}
