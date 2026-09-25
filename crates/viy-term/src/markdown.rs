//! The answer's Markdown (flags F34, ui-plan 12): pulldown-cmark's events
//! drawn as rows of a width, in rp's look (`markdown.py`): headings by
//! colour without their `#`, bullets `•◦▪` by depth, task boxes `☑ ☐`,
//! quotes behind `│ `, code between `╭─ lang` and `╰─`, a link's address
//! after its label, LaTeX as Unicode ([`crate::latex`]), and tables boxed,
//! aligned and wrapped inside the width, or written as `header: value`
//! records when the columns cannot fit. A blank row goes where the source
//! has a blank line, as rp prints line by line.
//!
//! rp holds back open inline spans because printed text can't change; the
//! live region redraws instead, so the answer is cut at [`finished`]: every
//! top-level block but the last can no longer change and moves up into
//! the transcript, the last one is drawn again with each delta (Codex's
//! `markdown_stream.rs` commits at newlines the same way).
//!
//! The structure is Codex's `markdown_render.rs` writer (an indent stack,
//! an inline style stack, a table collected then laid out), without its
//! themes, syntax colours, file links and Mermaid.

use std::borrow::Cow;

use pulldown_cmark::Alignment;
use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::Event;
use pulldown_cmark::HeadingLevel;
use pulldown_cmark::LinkType;
use pulldown_cmark::Options;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::latex::latex;
use crate::wrap::text_width;
use crate::wrap::wrap_rows;
use crate::wrap::wrap_spans;

/// A rule is at most this wide (rp: `min(width, 80)`).
const RULE_WIDTH: usize = 80;
/// Columns narrower than this on average make records (rp's `6 * ncol`).
const MIN_COLUMN: usize = 6;

/// One drawn row and where it comes from: a key that grows down the
/// rows, so a reading position survives a new width (the source offset of
/// its line, then its offset in the line).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub line: Line<'static>,
    pub key: (usize, usize),
}

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
}

/// Where the last top-level block of `source` starts: everything before it
/// is drawn the same whatever follows. 0 while there is one block or none.
/// Only whole lines count: a line still arriving can read as another block
/// (a lone `|` ends a table) until it is complete.
pub fn finished(source: &str) -> usize {
    let source = &source[..source.rfind('\n').map_or(0, |p| p + 1)];
    let mut depth = 0usize;
    let mut last = 0;
    let mut blocks = 0;
    for (event, range) in Parser::new_ext(source, options()).into_offset_iter() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    blocks += 1;
                    last = range.start;
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Rule if depth == 0 => {
                blocks += 1;
                last = range.start;
            }
            _ => {}
        }
    }
    if blocks > 1 { last } else { 0 }
}

/// `text` with each carriage return that does not start a CRLF as `␍`:
/// CommonMark ends a line at a lone CR, which would hide it (ui-plan 7
/// shows such chars). A CR at the very end may still be half a CRLF, and
/// stays unless `whole`.
pub fn visible_cr(text: &str, whole: bool) -> Cow<'_, str> {
    if !text.contains('\r') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\r', Some('\n')) => out.push(c),
            ('\r', None) if !whole => out.push(c),
            ('\r', _) => out.push('␍'),
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// `source` drawn as rows at most `width` cells wide.
pub fn render(source: &str, width: usize) -> Vec<Row> {
    let source = visible_cr(source, true);
    let (source, tex) = tex_delimiters(&source);
    let mut writer = Writer::new(&source, width.max(1), tex);
    for (event, range) in Parser::new_ext(&source, options()).into_offset_iter() {
        writer.at = range.start;
        writer.event(event, range.start);
    }
    writer.flush_line();
    writer.rows
}

/// `$...$`, `$$...$$` are pulldown-cmark's; rp also takes `\(...\)` and
/// `\[...\]`, which CommonMark reads as escaped brackets. They are swapped
/// for private-use placeholders before parsing (outside code), so they
/// can't pair with a `$` around them, and drawn back from the table.
const TEX_OPEN: char = '\u{E000}';
const TEX_CLOSE: char = '\u{E001}';

#[derive(Debug, Clone)]
struct Tex {
    display: bool,
    body: String,
    /// The source as written, for code, where nothing is converted.
    raw: String,
}

fn tex_delimiters(src: &str) -> (Cow<'_, str>, Vec<Tex>) {
    if !(src.contains("\\(") || src.contains("\\["))
        || src.contains(TEX_OPEN)
        || src.contains(TEX_CLOSE)
    {
        return (Cow::Borrowed(src), Vec::new());
    }
    let mut out = String::with_capacity(src.len());
    let mut tex: Vec<Tex> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    // An open `\[` block: its body so far and its source.
    let mut display: Option<(String, String)> = None;
    for line in src.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some((ch, len)) = fence {
            if fence_marker(trimmed)
                .is_some_and(|(c, n, rest)| c == ch && n >= len && rest.trim().is_empty())
            {
                fence = None;
            }
            out.push_str(line);
            continue;
        }
        if let Some((body, raw)) = &mut display {
            raw.push_str(line);
            match line.rfind("\\]") {
                Some(p) if line[p + 2..].trim().is_empty() => {
                    body.push_str(&line[..p]);
                    let (body, raw) = display.take().unwrap_or_default();
                    out.push_str(&placeholder(&mut tex, true, body, raw));
                    out.push('\n');
                }
                _ => body.push_str(line),
            }
            continue;
        }
        if display.is_none()
            && let Some((ch, len, _)) = fence_marker(trimmed)
        {
            fence = Some((ch, len));
            out.push_str(line);
            continue;
        }
        convert_line(line, &mut out, &mut tex, &mut display);
    }
    if let Some((_, raw)) = display {
        out.push_str(&raw);
    }
    (Cow::Owned(out), tex)
}

/// A fence's char, length and what follows it, if `line` (indent
/// trimmed) opens or closes one.
fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
    let ch = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = line.chars().take_while(|&c| c == ch).count();
    (len >= 3).then(|| (ch, len, &line[len..]))
}

fn placeholder(tex: &mut Vec<Tex>, display: bool, body: String, raw: String) -> String {
    tex.push(Tex { display, body, raw });
    format!("{TEX_OPEN}{}{TEX_CLOSE}", tex.len() - 1)
}

/// One line outside code: `\(..\)` and one-line `\[..\]` become
/// placeholders; a `\[` that opens the line and is not closed on it starts
/// a display block.
fn convert_line(
    line: &str,
    out: &mut String,
    tex: &mut Vec<Tex>,
    display: &mut Option<(String, String)>,
) {
    let mut i = 0;
    let bytes = line.as_bytes();
    while i < line.len() {
        let rest = &line[i..];
        if rest.starts_with('`') {
            let run = rest.bytes().take_while(|&b| b == b'`').count();
            let fence = &rest[..run];
            let close = rest[run..]
                .find(fence)
                .filter(|&p| !rest[run + p + run..].starts_with('`'));
            let end = close.map_or(run, |p| run + p + run);
            out.push_str(&rest[..end]);
            i += end;
            continue;
        }
        if bytes[i] == b'\\' && i + 1 < line.len() {
            let next = bytes[i + 1];
            let closer = match next {
                b'(' => Some("\\)"),
                b'[' => Some("\\]"),
                _ => None,
            };
            if let Some(closer) = closer {
                if let Some(p) = line[i + 2..].find(closer) {
                    let body = &line[i + 2..i + 2 + p];
                    if !body.trim().is_empty() {
                        let raw = &line[i..i + 2 + p + 2];
                        let display = next == b'[';
                        out.push_str(&placeholder(tex, display, body.to_string(), raw.into()));
                        i += 2 + p + 2;
                        continue;
                    }
                } else if next == b'[' && line[..i].trim().is_empty() {
                    *display = Some((line[i + 2..].to_string(), line[i..].to_string()));
                    return;
                }
            }
            // An escape: both chars as they are.
            let len = line[i + 1..].chars().next().map_or(0, char::len_utf8);
            out.push_str(&line[i..i + 1 + len]);
            i += 1 + len;
            continue;
        }
        let len = rest.chars().next().map_or(1, char::len_utf8);
        out.push_str(&rest[..len]);
        i += len;
    }
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn code_style() -> Style {
    Style::new().fg(Color::Green)
}

fn math_style() -> Style {
    Style::new().fg(Color::Yellow)
}

fn link_style() -> Style {
    Style::new()
        .fg(Color::Blue)
        .add_modifier(Modifier::UNDERLINED)
}

/// rp's `_HEAD_STYLE`: bold, coloured by level.
fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => bold().fg(Color::Magenta).add_modifier(Modifier::UNDERLINED),
        HeadingLevel::H2 => bold().fg(Color::Magenta),
        HeadingLevel::H3 => bold().fg(Color::Blue),
        _ => bold(),
    }
}

/// What goes before the rows of a quote or a list item.
#[derive(Debug, Clone)]
struct Indent {
    /// Before its first row: a list marker, a quote bar.
    first: Vec<Span<'static>>,
    /// Before the rows after it.
    rest: Vec<Span<'static>>,
    /// The first row has been drawn.
    used: bool,
    quote: bool,
    /// A bullet, which a task box replaces.
    bullet: bool,
}

/// A line of the source being filled, drawn when it ends.
struct Logical {
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
    spans: Vec<Span<'static>>,
    key: usize,
    /// Cut between graphemes, not words.
    code: bool,
}

#[derive(Default)]
struct TableCell {
    /// Its lines (a `<br>` starts one).
    lines: Vec<Vec<Span<'static>>>,
}

impl TableCell {
    fn push(&mut self, span: Span<'static>) {
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        if let Some(line) = self.lines.last_mut() {
            line.push(span);
        }
    }

    fn width(&self) -> usize {
        self.lines.iter().map(|l| spans_width(l)).max().unwrap_or(0)
    }

    fn plain(&self) -> String {
        let lines: Vec<String> = self
            .lines
            .iter()
            .map(|l| l.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        lines.join(" ")
    }
}

struct Table {
    start: usize,
    aligns: Vec<Alignment>,
    head: Vec<TableCell>,
    body: Vec<Vec<TableCell>>,
    in_head: bool,
}

struct Link {
    dest: String,
    label: String,
    kind: LinkType,
}

struct Writer<'a> {
    width: usize,
    rows: Vec<Row>,
    line: Option<Logical>,
    indents: Vec<Indent>,
    styles: Vec<Style>,
    /// The next number of each enclosing list; `None` for bullets.
    lists: Vec<Option<u64>>,
    link: Option<Link>,
    /// Inside an image: its address and the alt text so far.
    image: Option<(String, String)>,
    table: Option<Table>,
    /// Inside a code block: its text so far, where it starts, and whether
    /// it is fenced.
    code: Option<(String, usize, bool)>,
    tex: Vec<Tex>,
    src: &'a str,
    /// The source offset of the event being drawn.
    at: usize,
}

impl<'a> Writer<'a> {
    fn new(src: &'a str, width: usize, tex: Vec<Tex>) -> Self {
        Self {
            width,
            rows: Vec::new(),
            line: None,
            indents: Vec::new(),
            styles: Vec::new(),
            lists: Vec::new(),
            link: None,
            image: None,
            table: None,
            code: None,
            tex,
            src,
            at: 0,
        }
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::new(), |style, s| style.patch(*s))
    }

    fn event(&mut self, event: Event<'_>, start: usize) {
        match event {
            Event::Start(tag) => self.start(tag, start),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => {
                let code = self.restore_raw(&code);
                self.inline(Span::styled(code, self.style().patch(code_style())));
            }
            Event::InlineMath(math) => {
                self.inline(Span::styled(latex(&math), self.style().patch(math_style())));
            }
            Event::DisplayMath(math) => self.display_math(&math),
            Event::SoftBreak | Event::HardBreak => self.line_break(),
            Event::Rule => {
                self.block(start);
                let width = self.room().min(RULE_WIDTH);
                self.inline(Span::styled("─".repeat(width), dim()));
                self.flush_line();
            }
            Event::Html(html) => {
                for (i, line) in html.lines().enumerate() {
                    if i > 0 {
                        self.flush_line();
                    }
                    self.inline(Span::raw(line.to_string()));
                }
                self.flush_line();
            }
            Event::InlineHtml(html) => {
                if is_br(&html) {
                    self.line_break();
                } else {
                    self.inline(Span::styled(html.to_string(), self.style()));
                }
            }
            Event::FootnoteReference(name) => {
                self.inline(Span::styled(format!("[^{name}]"), self.style()));
            }
            Event::TaskListMarker(checked) => self.task(checked),
        }
    }

    fn start(&mut self, tag: Tag<'_>, start: usize) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.block(start),
            Tag::Heading { level, .. } => {
                self.block(start);
                self.styles.push(heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.block(start);
                let bar = vec![Span::styled("│ ", dim())];
                self.indents.push(Indent {
                    first: bar.clone(),
                    rest: bar,
                    used: false,
                    quote: true,
                    bullet: false,
                });
                self.styles
                    .push(Style::new().add_modifier(Modifier::ITALIC));
            }
            Tag::CodeBlock(kind) => {
                self.block(start);
                let fenced = matches!(kind, CodeBlockKind::Fenced(_));
                if let CodeBlockKind::Fenced(info) = kind {
                    let lang = info.split_whitespace().next().unwrap_or_default();
                    let head = if lang.is_empty() {
                        "╭─".to_string()
                    } else {
                        format!("╭─ {lang}")
                    };
                    self.inline(Span::styled(head, dim()));
                    self.flush_line();
                }
                self.code = Some((String::new(), start, fenced));
            }
            Tag::List(first) => {
                self.block(start);
                self.lists.push(first);
            }
            Tag::Item => self.item(start),
            Tag::Table(aligns) => {
                self.block(start);
                self.table = Some(Table {
                    start,
                    aligns,
                    head: Vec::new(),
                    body: Vec::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.body.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    let row = if table.in_head {
                        &mut table.head
                    } else if let Some(row) = table.body.last_mut() {
                        row
                    } else {
                        return;
                    };
                    row.push(TableCell::default());
                }
            }
            Tag::Emphasis => self
                .styles
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(bold()),
            Tag::Strikethrough => self
                .styles
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                self.styles.push(link_style());
                self.link = Some(Link {
                    dest: dest_url.to_string(),
                    label: String::new(),
                    kind: link_type,
                });
            }
            Tag::Image { dest_url, .. } => {
                self.image = Some((dest_url.to_string(), String::new()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.flush_line(),
            TagEnd::Heading(_) => {
                self.flush_line();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.indents.pop();
                self.styles.pop();
            }
            TagEnd::CodeBlock => self.end_code(),
            TagEnd::List(_) => {
                self.flush_line();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush_line();
                self.indents.pop();
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = false;
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.draw_table(table);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some(link) = self.link.take() {
                    let shown = !link.dest.is_empty()
                        && link.dest != link.label
                        && format!("<{}>", link.dest) != link.label
                        && !matches!(link.kind, LinkType::Autolink | LinkType::Email);
                    if shown {
                        self.inline(Span::styled(format!(" ({})", link.dest), dim()));
                    }
                }
            }
            TagEnd::Image => {
                if let Some((dest, alt)) = self.image.take() {
                    let label = if alt.is_empty() { &dest } else { &alt };
                    self.inline(Span::styled(format!("[image: {label}]"), dim()));
                    self.inline(Span::styled(format!(" {dest}"), dim()));
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if let Some((_, alt)) = &mut self.image {
            alt.push_str(text);
            return;
        }
        if let Some((code, _, _)) = &mut self.code {
            let raw = restore(text, &self.tex);
            code.push_str(&raw);
            return;
        }
        if let Some(link) = &mut self.link {
            link.label.push_str(text);
        }
        // A display formula alone in its paragraph is a block of its own.
        if let Some(tex) = self.lone_display(text) {
            self.display_math(&tex);
            return;
        }
        let style = self.style();
        for (piece, math) in split_tex(text, &self.tex) {
            let span = if math {
                Span::styled(piece, style.patch(math_style()))
            } else {
                Span::styled(piece, style)
            };
            self.inline(span);
        }
    }

    /// The body of the display placeholder `text` is, when nothing else is
    /// on its line.
    fn lone_display(&self, text: &str) -> Option<String> {
        let inner = text
            .trim()
            .strip_prefix(TEX_OPEN)?
            .strip_suffix(TEX_CLOSE)?;
        let tex = self.tex.get(inner.parse::<usize>().ok()?)?;
        let empty = self.line.as_ref().is_none_or(|l| l.spans.is_empty());
        (tex.display && empty && self.table.is_none()).then(|| tex.body.clone())
    }

    fn restore_raw(&self, code: &str) -> String {
        restore(code, &self.tex)
    }

    /// `$$...$$`: its lines indented, as rp draws display math; inline when
    /// text precedes it on the line.
    fn display_math(&mut self, math: &str) {
        let empty = self.line.as_ref().is_none_or(|l| l.spans.is_empty());
        if !empty || self.table.is_some() {
            let span = Span::styled(latex(math.trim()), self.style().patch(math_style()));
            self.inline(span);
            return;
        }
        for line in math.lines().map(str::trim).filter(|l| !l.is_empty()) {
            self.inline(Span::styled(format!("    {}", latex(line)), math_style()));
            self.flush_line();
        }
    }

    fn task(&mut self, checked: bool) {
        let mark = if checked { "☑" } else { "☐" };
        if let Some(indent) = self.indents.last_mut()
            && !indent.used
            && !indent.quote
        {
            if indent.bullet {
                indent.first = vec![Span::raw(format!("{mark} "))];
            } else {
                indent.first.push(Span::raw(format!("{mark} ")));
                indent.rest.push(Span::raw("  "));
            }
            return;
        }
        self.inline(Span::raw(format!("{mark} ")));
    }

    fn item(&mut self, start: usize) {
        self.block(start);
        let depth = self.lists.len().saturating_sub(1);
        let marker = match self.lists.last_mut() {
            Some(Some(n)) => {
                let number = *n;
                *n += 1;
                let sep = self.src[start..]
                    .trim_start()
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .chars()
                    .next()
                    .filter(|c| matches!(c, '.' | ')'))
                    .unwrap_or('.');
                format!("{number}{sep}")
            }
            _ => ["•", "◦", "▪"][depth % 3].to_string(),
        };
        let bullet = !marker.ends_with(['.', ')']);
        let pad = " ".repeat(text_width(&marker) + 1);
        self.indents.push(Indent {
            first: vec![Span::styled(marker, bold()), Span::raw(" ")],
            rest: vec![Span::raw(pad)],
            used: false,
            quote: false,
            bullet,
        });
    }

    fn end_code(&mut self) {
        let Some((text, start, fenced)) = self.code.take() else {
            return;
        };
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let mut offset = start;
        if !(text.is_empty() && fenced) {
            for line in text.split('\n') {
                self.at = offset;
                offset += line.len() + 1;
                let line = if fenced {
                    line.to_string()
                } else {
                    format!("    {line}")
                };
                self.new_line(true);
                self.inline(Span::styled(line, code_style()));
                self.flush_line();
            }
        }
        if fenced {
            self.at = offset;
            self.inline(Span::styled("╰─", dim()));
            self.flush_line();
        }
    }

    /// A block starts at `start`: the line before it ends, and a blank row
    /// goes in where the source has a blank line before it.
    fn block(&mut self, start: usize) {
        self.flush_line();
        let blank =
            self.rows.last().is_some_and(|r| !is_blank_row(r)) && blank_before(self.src, start);
        if blank {
            let bars: Vec<Span<'static>> = self
                .indents
                .iter()
                .filter(|i| i.quote)
                .flat_map(|i| i.rest.clone())
                .collect();
            self.push_row(Line::from(bars), (start, 0));
        }
    }

    fn line_break(&mut self) {
        if let Some(cell) = self.cell() {
            cell.lines.push(Vec::new());
            return;
        }
        if self.line.is_none() {
            self.new_line(false);
        }
        self.flush_line();
    }

    /// The table cell being filled, if any.
    fn cell(&mut self) -> Option<&mut TableCell> {
        let table = self.table.as_mut()?;
        if table.in_head {
            table.head.last_mut()
        } else {
            table.body.last_mut()?.last_mut()
        }
    }

    fn inline(&mut self, span: Span<'static>) {
        if let Some(cell) = self.cell() {
            cell.push(span);
            return;
        }
        if self.line.is_none() {
            self.new_line(false);
        }
        if let Some(line) = &mut self.line {
            line.spans.push(span);
        }
    }

    fn new_line(&mut self, code: bool) {
        self.flush_line();
        let mut first = Vec::new();
        let mut rest = Vec::new();
        for indent in &mut self.indents {
            if indent.used {
                first.extend(indent.rest.iter().cloned());
            } else {
                first.extend(indent.first.iter().cloned());
                indent.used = true;
            }
            rest.extend(indent.rest.iter().cloned());
        }
        self.line = Some(Logical {
            first,
            rest,
            spans: Vec::new(),
            key: self.at,
            code,
        });
    }

    /// Cells left after the indents.
    fn room(&self) -> usize {
        let used: usize = self.indents.iter().map(|i| spans_width(&i.rest)).sum();
        self.width.saturating_sub(used).max(1)
    }

    fn flush_line(&mut self) {
        let Some(line) = self.line.take() else {
            return;
        };
        let room = self.width.saturating_sub(spans_width(&line.first)).max(1);
        let pieces: Vec<(Vec<Span<'static>>, usize)> = if line.code {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let style = line.spans.first().map(|s| s.style).unwrap_or_default();
            let mut offset = 0;
            wrap_rows(&Line::from(Span::styled(text, style)), room)
                .into_iter()
                .map(|(row, chars)| {
                    let at = offset;
                    offset += chars;
                    (row.spans, at)
                })
                .collect()
        } else {
            wrap_spans(&line.spans, room)
                .into_iter()
                .map(|row| (row.spans, row.start))
                .collect()
        };
        for (i, (spans, at)) in pieces.into_iter().enumerate() {
            let mut all = if i == 0 {
                line.first.clone()
            } else {
                line.rest.clone()
            };
            all.extend(spans);
            self.push_row(Line::from(all), (line.key, at));
        }
    }

    fn push_row(&mut self, line: Line<'static>, mut key: (usize, usize)) {
        if let Some(last) = self.rows.last()
            && key <= last.key
        {
            key = (last.key.0, last.key.1 + 1);
        }
        self.rows.push(Row { line, key });
    }

    /// A finished table: boxed, or as records when it can't fit.
    fn draw_table(&mut self, table: Table) {
        let mut first = Vec::new();
        let mut rest = Vec::new();
        for indent in &mut self.indents {
            first.extend(if indent.used {
                indent.rest.clone()
            } else {
                indent.first.clone()
            });
            indent.used = true;
            rest.extend(indent.rest.clone());
        }
        let width = self.width.saturating_sub(spans_width(&rest)).max(1);
        let (lines, boxed) = table_lines(table.head, table.body, &table.aligns, width);
        let mut n = 0;
        for line in lines {
            // A box fits by construction; a record wraps like prose.
            let rows = if boxed {
                vec![line.spans]
            } else {
                wrap_spans(&line.spans, width)
                    .into_iter()
                    .map(|r| r.spans)
                    .collect()
            };
            for spans in rows {
                let mut all = if n == 0 { first.clone() } else { rest.clone() };
                all.extend(spans);
                self.push_row(Line::from(all), (table.start, n));
                n += 1;
            }
        }
    }
}

/// `text` cut at its placeholders: plain pieces, and the formulas.
fn pieces<'t>(text: &str, tex: &'t [Tex]) -> Vec<(String, Option<&'t Tex>)> {
    let mut out: Vec<(String, Option<&Tex>)> = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while let Some(open) = rest.find(TEX_OPEN) {
        let after = &rest[open + TEX_OPEN.len_utf8()..];
        let found = after.find(TEX_CLOSE).and_then(|close| {
            let index: usize = after[..close].parse().ok()?;
            Some((close, tex.get(index)?))
        });
        match found {
            Some((close, t)) => {
                plain.push_str(&rest[..open]);
                if !plain.is_empty() {
                    out.push((std::mem::take(&mut plain), None));
                }
                out.push((String::new(), Some(t)));
                rest = &after[close + TEX_CLOSE.len_utf8()..];
            }
            None => {
                plain.push_str(&rest[..open + TEX_OPEN.len_utf8()]);
                rest = after;
            }
        }
    }
    plain.push_str(rest);
    if !plain.is_empty() {
        out.push((plain, None));
    }
    out
}

/// `text` with its formulas as the source wrote them (code).
fn restore(text: &str, tex: &[Tex]) -> String {
    if tex.is_empty() || !text.contains(TEX_OPEN) {
        return text.to_string();
    }
    pieces(text, tex)
        .into_iter()
        .map(|(plain, t)| t.map_or(plain, |t| t.raw.clone()))
        .collect()
}

/// `text` cut into plain pieces and converted formulas (true).
fn split_tex(text: &str, tex: &[Tex]) -> Vec<(String, bool)> {
    pieces(text, tex)
        .into_iter()
        .map(|(plain, t)| match t {
            Some(t) => (latex(t.body.trim()), true),
            None => (plain, false),
        })
        .collect()
}

fn is_br(html: &str) -> bool {
    let s = html.trim().to_ascii_lowercase();
    let Some(inner) = s.strip_prefix("<br") else {
        return false;
    };
    let inner = inner.trim_end_matches('>').trim().trim_end_matches('/');
    inner.trim().is_empty() && s.ends_with('>')
}

fn is_blank_row(row: &Row) -> bool {
    row.line.spans.iter().all(|s| {
        s.content
            .trim_matches(|c: char| c.is_whitespace() || c == '│')
            .is_empty()
    })
}

/// The source line before the one holding `start` is blank (or only quote
/// markers).
fn blank_before(src: &str, start: usize) -> bool {
    let Some(line_start) = src[..start.min(src.len())].rfind('\n') else {
        return false;
    };
    let prev = &src[..line_start];
    let prev = &prev[prev.rfind('\n').map_or(0, |p| p + 1)..];
    prev.trim_matches(|c: char| c.is_whitespace() || c == '>')
        .is_empty()
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    text_width(&text)
}

/// A table cell's row: its spans and the cells they take.
type CellRow = (Vec<Span<'static>>, usize);

/// rp's `render_table`: column widths from the widest cell (at least 3),
/// shrunk from the column with the most to spare (down to 6) until the
/// table fits; records (false) when even that can't fit.
fn table_lines(
    head: Vec<TableCell>,
    body: Vec<Vec<TableCell>>,
    aligns: &[Alignment],
    width: usize,
) -> (Vec<Line<'static>>, bool) {
    let ncol = body
        .iter()
        .map(Vec::len)
        .chain([head.len(), aligns.len()])
        .max()
        .unwrap_or(0);
    let pad = |mut row: Vec<TableCell>| {
        row.resize_with(ncol, TableCell::default);
        row
    };
    let mut head = pad(head);
    for cell in &mut head {
        for line in &mut cell.lines {
            for span in line.iter_mut() {
                span.style = span.style.patch(bold());
            }
        }
    }
    let body: Vec<Vec<TableCell>> = body.into_iter().map(pad).collect();
    if ncol == 0 {
        return (Vec::new(), true);
    }
    let avail = width.saturating_sub(3 * ncol + 1);
    if avail < MIN_COLUMN * ncol {
        return (records(&head, &body), false);
    }
    let natural: Vec<usize> = (0..ncol)
        .map(|j| {
            std::iter::once(&head)
                .chain(&body)
                .map(|row| row[j].width())
                .max()
                .unwrap_or(0)
                .max(3)
        })
        .collect();
    let floor: Vec<usize> = natural.iter().map(|&n| n.min(MIN_COLUMN)).collect();
    let mut cols = natural;
    while cols.iter().sum::<usize>() > avail {
        let spare = |k: usize| cols[k] - floor[k];
        let Some(j) = (0..ncol).reduce(|best, k| if spare(k) > spare(best) { k } else { best })
        else {
            break;
        };
        if cols[j] <= floor[j] {
            break;
        }
        cols[j] -= 1;
    }
    let wrap = |cell: &TableCell, width: usize| -> Vec<CellRow> {
        let mut rows = Vec::new();
        for line in &cell.lines {
            for row in wrap_spans(line, width) {
                rows.push((row.spans, row.width));
            }
        }
        rows
    };
    let wrapped: Vec<Vec<Vec<CellRow>>> = std::iter::once(&head)
        .chain(&body)
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(j, c)| wrap(c, cols[j]))
                .collect()
        })
        .collect();
    let multi = wrapped[1..]
        .iter()
        .any(|row| row.iter().any(|cell| cell.len() > 1));
    let border = |l: &str, m: &str, r: &str, fill: &str| {
        let parts: Vec<String> = cols.iter().map(|w| fill.repeat(w + 2)).collect();
        Line::from(Span::styled(format!("{l}{}{r}", parts.join(m)), dim()))
    };
    let mut out = vec![border("┌", "┬", "┐", "─")];
    let last = wrapped.len() - 1;
    for (ri, row) in wrapped.iter().enumerate() {
        let height = row.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for k in 0..height {
            let mut spans = vec![Span::styled("│", dim())];
            for (j, cell) in row.iter().enumerate() {
                let (text, used) = cell.get(k).cloned().unwrap_or_default();
                let gap = cols[j].saturating_sub(used);
                let align = aligns.get(j).copied().unwrap_or(Alignment::None);
                let (left, right) = match align {
                    Alignment::Right => (gap, 0),
                    Alignment::Center => (gap / 2, gap - gap / 2),
                    _ => (0, gap),
                };
                spans.push(Span::raw(" ".repeat(left + 1)));
                spans.extend(text);
                spans.push(Span::raw(" ".repeat(right + 1)));
                spans.push(Span::styled("│", dim()));
            }
            out.push(Line::from(spans));
        }
        if ri == 0 {
            out.push(border("╞", "╪", "╡", "═"));
        } else if multi && ri < last {
            out.push(border("├", "┼", "┤", "─"));
        }
    }
    out.push(border("└", "┴", "┘", "─"));
    (out, true)
}

/// rp's `_render_records`: one block per row, `header: value`, a blank
/// line between rows.
fn records(head: &[TableCell], body: &[Vec<TableCell>]) -> Vec<Line<'static>> {
    let names: Vec<String> = head.iter().map(TableCell::plain).collect();
    let mut out = Vec::new();
    for row in body {
        for (j, cell) in row.iter().enumerate() {
            let name = names
                .get(j)
                .cloned()
                .unwrap_or_else(|| format!("col {}", j + 1));
            let mut spans = vec![Span::styled(name, bold()), Span::raw(": ")];
            for (k, line) in cell.lines.iter().enumerate() {
                if k > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.extend(line.iter().cloned());
            }
            out.push(Line::from(spans));
        }
        out.push(Line::default());
    }
    out.pop();
    out
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
