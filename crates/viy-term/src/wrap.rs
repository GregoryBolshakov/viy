//! Cutting styled lines into screen rows.

use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

/// Cuts `line` into rows at most `width` cells wide, breaking between
/// characters, with controls made visible (ui-plan 7: only the renderer
/// writes escape sequences). The line's style is kept on every row.
pub fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in &line.spans {
        let mut text = String::new();
        for c in span.content.chars().map(visible) {
            let w = c.width().unwrap_or(0);
            if used + w > width && used > 0 {
                spans.push(Span::styled(std::mem::take(&mut text), span.style));
                rows.push(Line::from(std::mem::take(&mut spans)).style(line.style));
                used = 0;
            }
            text.push(c);
            used += w;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    rows.push(Line::from(spans).style(line.style));
    rows
}

/// C0 controls and DEL as their Control Pictures (`␛`), C1 controls as
/// U+FFFD.
fn visible(c: char) -> char {
    match c {
        '\0'..='\x1f' => char::from_u32(0x2400 + u32::from(c)).unwrap_or('\u{fffd}'),
        '\x7f' => '\u{2421}',
        '\u{80}'..='\u{9f}' => '\u{fffd}',
        _ => c,
    }
}

#[cfg(test)]
#[path = "wrap_tests.rs"]
mod tests;
