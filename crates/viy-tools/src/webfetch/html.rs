//! A page's HTML as the text a reader sees (rpctl webfetch.py `_Extractor`,
//! `_tidy`, `extract_html`): Markdown-ish text without what a reader never
//! sees (scripts, styles, templates, comments, hidden and aria-hidden
//! elements, inline-hidden styles, screen-reader-only text, image alt
//! text) or the page's furniture (nav, footer, aside, forms), only the
//! `<main>`/`<article>` when that holds real content, and links as `[n]`
//! markers whose URLs are listed apart.
//!
//! The page is read with rpctl's tokenizer, Python's `HTMLParser`
//! ([`pyhtml`](super::pyhtml)), and this is rpctl's extractor on it,
//! stopping where rpctl's stops (a link `urljoin` refuses, a reference
//! `int()` refuses): what it hides is what rpctl hides (kb d115, d116).
//! Pages where rpctl shows text a browser hides (a self-closed
//! `<div hidden/>`, an attribute given twice, a hidden element inside one
//! an optional end tag ends early) are rpctl bugs, reported, to be fixed
//! there first and then here.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::pyhtml;
use super::pyhtml::Event;
use super::pyhtml::Stop;
use crate::approve::chars::is_py_space;
use crate::approve::chars::py_strip;
use crate::netguard::MAX_URL;
use crate::netguard::url;
use crate::untrusted;

/// Elements whose content is dropped.
const SKIP: [&str; 21] = [
    "script", "style", "noscript", "template", "svg", "math", "canvas", "iframe", "object",
    "embed", "head", "select", "textarea", "button", "datalist", "dialog", "nav", "footer",
    "aside", "form", "menu",
];
const SKIP_ROLES: [&str; 9] = [
    "navigation",
    "contentinfo",
    "complementary",
    "search",
    "dialog",
    "alertdialog",
    "menu",
    "menubar",
    "banner",
];
const VOID: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
const BLOCK: [&str; 22] = [
    "p",
    "div",
    "section",
    "article",
    "main",
    "header",
    "ul",
    "ol",
    "dl",
    "dt",
    "dd",
    "blockquote",
    "figure",
    "figcaption",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "details",
    "summary",
    "address",
    "center",
];
const HEADINGS: [&str; 6] = ["h1", "h2", "h3", "h4", "h5", "h6"];
/// What may sit in `<head>`: anything else means the page left `</head>`
/// out.
const HEAD_TAGS: [&str; 8] = [
    "title", "meta", "link", "style", "script", "base", "noscript", "template",
];

/// The containers whose end closes an element with an optional end tag:
/// a hidden `<li>` left open must not swallow the rest of the list, so a
/// sibling start (or the container's end) ends it.
fn optional_end(tag: &str) -> Option<&'static [&'static str]> {
    Some(match tag {
        "li" => &["ul", "ol"],
        "p" => &["div", "section", "article", "main", "body"],
        "dt" | "dd" => &["dl"],
        "tr" => &["table", "tbody", "thead", "tfoot"],
        "td" | "th" => &["tr", "table"],
        _ => return None,
    })
}
/// Python's `\s` (`[\s\x1c-\x1f]` here) in the patterns below, so a style
/// rpctl reads as hidden is hidden here too. rpctl's `(?<![-\w])` before
/// `width` is `(^|[^-A-Za-z0-9_])`: ASCII word characters only, so any
/// character Python's `\w` takes as a boundary is one here too. The value
/// goes through [`fold`] first.
static HIDDEN_STYLE: LazyLock<Regex> = LazyLock::new(|| {
    rx(
        r"(?i)display[\s\x1c-\x1f]*:[\s\x1c-\x1f]*none|visibility[\s\x1c-\x1f]*:[\s\x1c-\x1f]*(hidden|collapse)|opacity[\s\x1c-\x1f]*:[\s\x1c-\x1f]*0(\.0+)?[\s\x1c-\x1f]*(;|$)|font-size[\s\x1c-\x1f]*:[\s\x1c-\x1f]*0+(\.0+)?(px|em|rem|pt|%)?[\s\x1c-\x1f]*(;|$)|(left|top|text-indent)[\s\x1c-\x1f]*:[\s\x1c-\x1f]*-\d{3,}|clip[\s\x1c-\x1f]*:[\s\x1c-\x1f]*rect\([\s\x1c-\x1f]*0|(^|[^-A-Za-z0-9_])(width|height|max-height|max-width)[\s\x1c-\x1f]*:[\s\x1c-\x1f]*0+(px)?[\s\x1c-\x1f]*(;|$)",
    )
});
static HIDDEN_CLASS: LazyLock<Regex> = LazyLock::new(|| {
    rx(
        r"(?i)(^|[\s\x1c-\x1f])(sr-only|visually-hidden|screen-reader-text|hidden|d-none|is-hidden|u-hidden|hide)([\s\x1c-\x1f]|$)",
    )
});
/// "hidden md:block", "d-none d-lg-block": hidden on phones, shown on
/// desktops.
static SHOWN_LATER: LazyLock<Regex> = LazyLock::new(|| {
    rx(
        r"(?i)(^|[\s\x1c-\x1f])([a-z0-9]+:(block|flex|grid|inline[^\s\x1c-\x1f]*|table[^\s\x1c-\x1f]*)|d-[a-z]{2,3}-(block|flex|grid|inline[^\s\x1c-\x1f]*|table[^\s\x1c-\x1f]*))([\s\x1c-\x1f]|$)",
    )
});
static SPACE_RUN: LazyLock<Regex> = LazyLock::new(|| rx(r"[\s\x1c-\x1f]+"));
static BLANKS: LazyLock<Regex> = LazyLock::new(|| rx(r"[ \t]+"));
/// A link marker. ASCII digits only: Python's `\d` and `int()` also take
/// other scripts' digits, which only a page's own text can hold.
static MARKER: LazyLock<Regex> = LazyLock::new(|| rx(r"\[([0-9]+)\]"));

fn rx(pattern: &str) -> Regex {
    // The patterns are constants, checked by every test that extracts.
    Regex::new(pattern).unwrap_or_else(|e| panic!("bad pattern {pattern}: {e}"))
}

/// A value as Python's `re.IGNORECASE` reads it against the ASCII
/// letters of the patterns: it also matches `ı` and `İ` to `i`, which
/// Rust's case folding does not (`ſ` and the Kelvin sign it folds alike).
fn fold(v: &str) -> std::borrow::Cow<'_, str> {
    if v.contains(['ı', 'İ']) {
        v.replace(['ı', 'İ'], "i").into()
    } else {
        v.into()
    }
}

/// A page's title, text and links: `links[n - 1]` is `[n]`'s URL, `None`
/// when the text kept has no `[n]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub title: String,
    pub text: String,
    pub links: Vec<Option<String>>,
}

/// Python's `int()` takes at most this many decimal digits
/// (`sys.get_int_max_str_digits()` on rpctl's 3.10.12).
pub(super) const INT_MAX_DIGITS: usize = 4300;

/// The error rpctl's `int()` raises on a link marker past
/// [`INT_MAX_DIGITS`].
pub(super) fn too_many_digits(digits: usize) -> String {
    format!(
        "Exceeds the limit ({INT_MAX_DIGITS}) for integer string conversion: value has \
         {digits} digits; use sys.set_int_max_str_digits() to increase the limit"
    )
}

/// The numbers of the `[n]` markers in `text`, or the error rpctl raises
/// on one past Python's `int()` digit limit. ASCII digits only: Python's
/// `\d` and `int()` also take other scripts' digits, which only a page's
/// own text holds, and a marker left out keeps a link unlisted.
pub(super) fn markers(text: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for c in MARKER.captures_iter(text) {
        let digits = c[1].len();
        if digits > INT_MAX_DIGITS {
            return Err(too_many_digits(digits));
        }
        // More digits than usize holds: no link has that number.
        out.push(c[1].parse().unwrap_or(usize::MAX));
    }
    Ok(out)
}

/// `extract_html(html, base_url)`; an error where rpctl's raises out of it.
pub fn extract(html: &str, base_url: &str) -> Result<Extracted, String> {
    let mut ex = Extractor::new(base_url);
    pyhtml::parse(html, |event| match event {
        Event::Start {
            tag,
            attrs,
            self_closing,
        } => ex.start(&tag, &attrs, self_closing),
        Event::End(tag) => {
            ex.end(&tag);
            Ok(())
        }
        Event::Data(text) => {
            ex.data(&text);
            Ok(())
        }
    });
    let whole = tidy(&ex.out);
    let main = tidy(&ex.main);
    let text = if main.chars().count() >= 500 {
        main
    } else {
        whole
    };
    let used: HashSet<usize> = markers(&text)?.into_iter().collect();
    let links = ex
        .links
        .into_iter()
        .enumerate()
        .map(|(i, url)| used.contains(&(i + 1)).then_some(url))
        .collect();
    Ok(Extracted {
        title: untrusted::terminal_safe(&ex.title, Some(300)),
        text,
        links,
    })
}

/// `_tidy`: cleaned, runs of spaces collapsed outside code blocks, at most
/// one blank line in a row.
pub fn tidy(text: &str) -> String {
    let text = untrusted::clean(text);
    let mut out: Vec<String> = Vec::new();
    let mut blank = 0;
    let mut in_code = false;
    for line in text.split('\n') {
        let mut line = line.trim_end_matches(is_py_space).to_string();
        let fence = py_strip(&line).starts_with("```");
        if !in_code && !fence {
            let indent = if line.trim_start_matches(is_py_space).starts_with("- ") {
                line.len() - line.trim_start_matches(' ').len()
            } else {
                0
            };
            let collapsed = BLANKS.replace_all(&line, " ");
            line = format!("{}{}", " ".repeat(indent), py_strip(&collapsed));
            if matches!(py_strip(&line), "|" | "-") {
                line.clear();
            }
        }
        if fence {
            in_code = !in_code;
        }
        if py_strip(&line).is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push(line);
    }
    py_strip(&out.join("\n")).to_string()
}

type Attrs = [(String, Option<String>)];

/// `dict(attrs).get(name)`: the last value given.
fn last<'a>(attrs: &'a Attrs, name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_deref().unwrap_or_default())
}

/// `_hidden`, on `dict(attrs)`: the last value of each attribute.
fn hidden(tag: &str, attrs: &Attrs) -> bool {
    if SKIP.contains(&tag) || tag == "input" {
        return true;
    }
    if attrs.iter().any(|(k, _)| k == "hidden") {
        return true;
    }
    let lower = |name| last(attrs, name).unwrap_or_default().to_lowercase();
    if lower("aria-hidden") == "true" || SKIP_ROLES.contains(&lower("role").as_str()) {
        return true;
    }
    if HIDDEN_STYLE.is_match(&fold(last(attrs, "style").unwrap_or_default())) {
        return true;
    }
    let class = fold(last(attrs, "class").unwrap_or_default());
    HIDDEN_CLASS.is_match(&class) && !SHOWN_LATER.is_match(&class)
}

struct Extractor<'a> {
    base: &'a str,
    /// The tag names whose content is dropped.
    skip: Vec<String>,
    /// The whole page.
    out: String,
    /// Only what is inside `<main>`/`<article>`.
    main: String,
    main_depth: usize,
    pre: usize,
    title: String,
    in_title: bool,
    links: Vec<String>,
    link_ix: HashMap<String, usize>,
    /// The link number of each open `<a>`, or `None`.
    href: Vec<Option<usize>>,
    list_depth: usize,
}

impl<'a> Extractor<'a> {
    fn new(base: &'a str) -> Self {
        Self {
            base,
            skip: Vec::new(),
            out: String::new(),
            main: String::new(),
            main_depth: 0,
            pre: 0,
            title: String::new(),
            in_title: false,
            links: Vec::new(),
            link_ix: HashMap::new(),
            href: Vec::new(),
            list_depth: 0,
        }
    }

    fn emit(&mut self, s: &str) {
        self.out.push_str(s);
        if self.main_depth > 0 {
            self.main.push_str(s);
        }
    }

    fn skipping_only(&self, tag: &str) -> bool {
        self.skip.len() == 1 && self.skip[0] == tag
    }

    /// `handle_startendtag` passes on only void elements (and `<br/>`).
    fn start(&mut self, tag: &str, attrs: &Attrs, self_closing: bool) -> Result<(), Stop> {
        if self_closing && !VOID.contains(&tag) {
            return Ok(());
        }
        self.start_tag(tag, attrs)
    }

    /// `handle_starttag`.
    fn start_tag(&mut self, tag: &str, attrs: &Attrs) -> Result<(), Stop> {
        if tag == "title" && (self.skip.is_empty() || self.skipping_only("head")) {
            self.in_title = true;
            return Ok(());
        }
        if self.skipping_only("head") && !HEAD_TAGS.contains(&tag) {
            self.skip.pop(); // </head> was left out
        }
        if let Some(top) = self.skip.last() {
            if self.skip.len() == 1 && top == tag && optional_end(tag).is_some() {
                self.skip.pop(); // a sibling: the hidden one ended here
            } else {
                if top == tag && !VOID.contains(&tag) {
                    self.skip.push(tag.into());
                }
                return Ok(());
            }
        }
        if tag == "title" {
            return Ok(());
        }
        if hidden(tag, attrs) {
            if !VOID.contains(&tag) {
                self.skip.push(tag.into());
            }
            return Ok(());
        }
        if tag == "main" || tag == "article" {
            self.main_depth += 1;
        }
        if let Some(level) = HEADINGS.iter().position(|h| *h == tag) {
            self.emit(&format!("\n\n{} ", "#".repeat(level + 1)));
        } else if tag == "li" {
            let indent = "  ".repeat(self.list_depth.saturating_sub(1));
            self.emit(&format!("\n{indent}- "));
        } else if tag == "ul" || tag == "ol" {
            self.list_depth += 1;
            self.emit("\n");
        } else if tag == "pre" {
            self.pre += 1;
            self.emit("\n\n```\n");
        } else if tag == "code" && self.pre == 0 {
            self.emit("`");
        } else if tag == "br" || tag == "tr" {
            self.emit("\n");
        } else if tag == "hr" {
            self.emit("\n\n---\n\n");
        } else if tag == "td" || tag == "th" {
            self.emit(" | ");
        } else if BLOCK.contains(&tag) {
            self.emit("\n\n");
        }
        if tag == "a" {
            let n = self.link(last(attrs, "href").unwrap_or_default())?;
            self.href.push(n);
        }
        Ok(())
    }

    /// `handle_endtag`.
    fn end(&mut self, tag: &str) {
        if tag == "title" {
            self.in_title = false;
            return;
        }
        if let Some(top) = self.skip.last() {
            if top == tag {
                self.skip.pop();
                return;
            }
            let closes = optional_end(&self.skip[0]).is_some_and(|c| c.contains(&tag));
            if self.skip.len() == 1 && closes {
                self.skip.pop(); // the container closed an open hidden child
            } else {
                return;
            }
        }
        if (tag == "main" || tag == "article") && self.main_depth > 0 {
            self.main_depth -= 1;
        }
        if tag == "a" && !self.href.is_empty() {
            if let Some(Some(n)) = self.href.pop() {
                self.emit(&format!(" [{n}]"));
            }
        } else if tag == "pre" && self.pre > 0 {
            self.pre -= 1;
            self.emit("\n```\n\n");
        } else if tag == "code" && self.pre == 0 {
            self.emit("`");
        } else if (tag == "ul" || tag == "ol") && self.list_depth > 0 {
            self.list_depth -= 1;
            self.emit("\n");
        } else if HEADINGS.contains(&tag) || BLOCK.contains(&tag) {
            self.emit("\n\n");
        }
    }

    /// `handle_data`.
    fn data(&mut self, data: &str) {
        if self.in_title {
            self.title.push_str(data);
            return;
        }
        if !self.skip.is_empty() {
            return;
        }
        if self.pre > 0 {
            self.emit(data);
        } else {
            let data = SPACE_RUN.replace_all(data, " ");
            self.emit(&data);
        }
    }

    /// `_link`: the link's number, `None` for an anchor, a non-http(s)
    /// URL or one longer than [`MAX_URL`]; a URL Python's `urljoin` or
    /// `urldefrag` refuses stops the page, as rpctl's parser stops there.
    fn link(&mut self, href: &str) -> Result<Option<usize>, Stop> {
        let href = py_strip(href);
        if href.is_empty() || href.starts_with('#') {
            return Ok(None);
        }
        let url = url::join(self.base, href)
            .and_then(|u| url::defrag(&u))
            .map_err(|_| Stop)?;
        if !(url.starts_with("http://") || url.starts_with("https://"))
            || url.chars().count() > MAX_URL
        {
            return Ok(None);
        }
        if let Some(n) = self.link_ix.get(&url) {
            return Ok(Some(*n));
        }
        self.links.push(url.clone());
        self.link_ix.insert(url, self.links.len());
        Ok(Some(self.links.len()))
    }
}

#[cfg(test)]
#[path = "html_tests.rs"]
mod tests;
