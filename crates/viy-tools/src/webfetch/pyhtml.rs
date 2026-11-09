//! The tokenizer rpctl's extractor reads pages with: Python 3.10.12's
//! `html.parser.HTMLParser(convert_charrefs=True)` as Ubuntu ships it
//! (3.10.12-1~22.04.18, with the 2025 security backports: comments,
//! `<![CDATA[`, raw text in `<script>`, `<style>`, `<xmp>`, `<iframe>`,
//! `<noembed>`, `<noframes>` and `<plaintext>`, escapable raw text in
//! `<title>` and `<textarea>`, `<noscript>` parsed), and `html.unescape`.
//!
//! The extractor decides what is hidden on these tokens, so they are
//! Python's, not a browser's: a page then hides in viy at least what it
//! hides in rpctl (kb d115). A browser's tokenizer (html5gum, d017's
//! first choice) differed on markup inside attributes, `<!-->`,
//! `<![CDATA[` and `<noscript>`, each a way to show text rpctl hides.
//!
//! rpctl feeds the whole page and closes the parser, which is one pass
//! with the end of input known; the regular expressions are written out
//! as code with the same backtracking outcome.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Captures;
use regex::Regex;

use super::entities::ENTITIES;
use crate::approve::chars::is_py_space;

/// What the parser reports; comments, declarations and processing
/// instructions are dropped, as the extractor drops them.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event<'a> {
    /// `handle_starttag`, or `handle_startendtag` when `self_closing`.
    /// Attributes in order, a repeated one each time; `None` for one
    /// without a value.
    Start {
        tag: String,
        attrs: Vec<(String, Option<String>)>,
        self_closing: bool,
    },
    End(String),
    Data(Cow<'a, str>),
}

const RAWTEXT: [&str; 6] = ["script", "style", "xmp", "iframe", "noembed", "noframes"];
const RCDATA: [&str; 2] = ["textarea", "title"];

/// `[\t\n\r\f ]`: the space characters of the tag patterns.
fn tag_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\r' | b'\x0c' | b' ')
}

/// Python raised, and rpctl's parser stops reading the page there: a
/// decimal reference of more than 4300 digits is past `int()`'s limit
/// (`sys.get_int_max_str_digits()`), the text being decoded included; the
/// extractor stops on a link `urljoin` refuses.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stop;

/// Runs the parser over `html`, calling `on` for each event, until the
/// end or a [`Stop`], the parser's own or `on`'s.
pub(super) fn parse<'a>(html: &'a str, mut on: impl FnMut(Event<'a>) -> Result<(), Stop>) {
    let _ = run(html, &mut on);
}

fn run<'a>(html: &'a str, on: &mut impl FnMut(Event<'a>) -> Result<(), Stop>) -> Result<(), Stop> {
    let s = html.as_bytes();
    let n = s.len();
    // The element whose raw text is being read, and whether references in
    // it are decoded.
    let mut cdata: Option<(&'static str, bool)> = None;
    let mut i = 0;
    let data =
        |text: &'a str, escapable: bool, on: &mut dyn FnMut(Event<'a>) -> Result<(), Stop>| {
            on(Event::Data(if escapable {
                unescape(text)?
            } else {
                Cow::Borrowed(text)
            }))
        };
    while i < n {
        let j = match cdata {
            None => find(s, b"<", i).unwrap_or(n),
            // `<plaintext>`: the rest is its text.
            Some(("plaintext", _)) => n,
            Some((elem, _)) => match cdata_end(s, elem, i) {
                Some(j) => j,
                None => break,
            },
        };
        if i < j {
            data(&html[i..j], cdata.is_none_or(|c| c.1), on)?;
        }
        i = j;
        if i == n {
            break;
        }
        let at = |p: &[u8]| s[j..].starts_with(p);
        let k = if starts_tag(s, i + 1) {
            parse_starttag(html, i, on, &mut cdata)?
        } else if at(b"</") {
            parse_endtag(html, i, on, &mut cdata)?
        } else if at(b"<!--") {
            parse_comment(s, i)
        } else if at(b"<?") {
            find(s, b">", i + 2).map(|p| p + 1)
        } else if at(b"<!") {
            parse_html_declaration(s, i)
        } else {
            on(Event::Data(Cow::Borrowed("<")))?;
            Some(i + 1)
        };
        // Not closed before the end: the rest is that construct, except
        // `</` alone, which is text.
        i = match k {
            Some(k) => k,
            None => {
                if !starts_tag(s, j + 1) && at(b"</") && j + 2 == n {
                    on(Event::Data(Cow::Borrowed("</")))?;
                }
                n
            }
        };
    }
    if i < n {
        data(&html[i..], cdata.is_none_or(|c| c.1), on)?;
    }
    Ok(())
}

fn find(s: &[u8], what: &[u8], from: usize) -> Option<usize> {
    let mut at = from;
    loop {
        let p = at + s.get(at..)?.iter().position(|&b| b == what[0])?;
        if s[p..].starts_with(what) {
            return Some(p);
        }
        at = p + 1;
    }
}

/// `[a-zA-Z]` at `i`.
fn starts_tag(s: &[u8], i: usize) -> bool {
    s.get(i).is_some_and(u8::is_ascii_alphabetic)
}

/// `</elem(?=[\t\n\r\f />])`, ASCII case ignored, from `i`.
fn cdata_end(s: &[u8], elem: &str, i: usize) -> Option<usize> {
    let e = elem.as_bytes();
    let mut from = i;
    while let Some(p) = find(s, b"</", from) {
        let name = p + 2;
        if s.len() > name + e.len()
            && s[name..name + e.len()].eq_ignore_ascii_case(e)
            && (tag_space(s[name + e.len()]) || matches!(s[name + e.len()], b'/' | b'>'))
        {
            return Some(p);
        }
        from = p + 1;
    }
    None
}

/// `tagfind_tolerant` at `i`: the name's end and where the match ends.
fn tagfind(s: &[u8], i: usize) -> (usize, usize) {
    let mut k = i + 1;
    while k < s.len() && !tag_space(s[k]) && !matches!(s[k], b'/' | b'>') {
        k += 1;
    }
    let name_end = k;
    while k < s.len() && (tag_space(s[k]) || (s[k] == b'/' && s.get(k + 1) != Some(&b'>'))) {
        k += 1;
    }
    (name_end, k)
}

/// Where an attribute's name ends, its value's span (quotes included)
/// if it has one, and where the attribute ends.
type Attribute = (usize, Option<(usize, usize)>, usize);

/// An attribute at `k`, as `attrfind_tolerant` and `locatetagend` read
/// one: where its name ends, its value's span (quotes included) when it
/// has one, and where the value ends. `None` when no attribute starts at
/// `k` (the character before must be a quote, a space or `/`).
fn attribute(s: &[u8], k: usize) -> Option<Attribute> {
    if k == 0 || k >= s.len() || !(tag_space(s[k - 1]) || matches!(s[k - 1], b'\'' | b'"' | b'/')) {
        return None;
    }
    if tag_space(s[k]) || matches!(s[k], b'/' | b'>') {
        return None;
    }
    let mut p = k + 1;
    while p < s.len() && !tag_space(s[p]) && !matches!(s[p], b'/' | b'=' | b'>') {
        p += 1;
    }
    let name_end = p;
    // `[\t\n\r\f ]*=[\t\n\r\f ]*` and a value; the whole group is optional.
    let mut q = p;
    while q < s.len() && tag_space(s[q]) {
        q += 1;
    }
    if s.get(q) != Some(&b'=') {
        return Some((name_end, None, name_end));
    }
    let r = q + 1;
    let mut t = r;
    while t < s.len() && tag_space(s[t]) {
        t += 1;
    }
    let bare = |t: usize| {
        let mut e = t;
        while e < s.len() && !tag_space(s[e]) && s[e] != b'>' {
            e += 1;
        }
        e
    };
    match s.get(t) {
        Some(&quote @ (b'\'' | b'"')) => match find(s, &[quote], t + 1) {
            Some(close) => Some((name_end, Some((t, close + 1)), close + 1)),
            // Unclosed: backtracking leaves one space before it as an
            // empty bare value, or no value at all.
            None if t > r => Some((name_end, Some((t - 1, t - 1)), t - 1)),
            None => Some((name_end, None, name_end)),
        },
        _ => {
            let e = bare(t);
            Some((name_end, Some((t, e)), e))
        }
    }
}

/// `locatetagend` from `i` (a letter): where it ends.
fn locate_tag_end(s: &[u8], i: usize) -> usize {
    let (name_end, _) = tagfind(s, i);
    let mut k = name_end;
    let skip = |mut k: usize| {
        while k < s.len() && (tag_space(s[k]) || s[k] == b'/') {
            k += 1;
        }
        k
    };
    k = skip(k);
    while let Some((_, _, end)) = attribute(s, k) {
        k = skip(end);
    }
    if s.get(k) == Some(&b'>') { k + 1 } else { k }
}

fn parse_starttag<'a>(
    html: &'a str,
    i: usize,
    on: &mut impl FnMut(Event<'a>) -> Result<(), Stop>,
    cdata: &mut Option<(&'static str, bool)>,
) -> Result<Option<usize>, Stop> {
    let s = html.as_bytes();
    let endpos = locate_tag_end(s, i + 1);
    if s[endpos - 1] != b'>' {
        return Ok(None);
    }
    let (name_end, mut k) = tagfind(s, i + 1);
    let tag = html[i + 1..name_end].to_lowercase();
    let mut attrs = Vec::new();
    while k < endpos {
        let Some((name_end, value, end)) = attribute(s, k) else {
            break;
        };
        let value = value.map(|(a, b)| -> Result<String, Stop> {
            let v = &html[a..b];
            let v = if v.len() >= 2
                && (v.starts_with('\'') && v.ends_with('\'')
                    || v.starts_with('"') && v.ends_with('"'))
            {
                &v[1..v.len() - 1]
            } else {
                v
            };
            Ok(unescape(v)?.into_owned())
        });
        attrs.push((html[k..name_end].to_lowercase(), value.transpose()?));
        k = end;
        while k < s.len() && (tag_space(s[k]) || (s[k] == b'/' && s.get(k + 1) != Some(&b'>'))) {
            k += 1;
        }
    }
    let end = html[k.min(endpos)..endpos].trim_matches(is_py_space);
    if end != ">" && end != "/>" {
        on(Event::Data(Cow::Borrowed(&html[i..endpos])))?;
        return Ok(Some(endpos));
    }
    let self_closing = end == "/>";
    if !self_closing {
        if let Some(raw) = RAWTEXT
            .iter()
            .chain(["plaintext"].iter())
            .find(|e| **e == tag)
        {
            *cdata = Some((raw, false));
        } else if let Some(rc) = RCDATA.iter().find(|e| **e == tag) {
            *cdata = Some((rc, true));
        }
    }
    on(Event::Start {
        tag,
        attrs,
        self_closing,
    })?;
    Ok(Some(endpos))
}

fn parse_endtag<'a>(
    html: &'a str,
    i: usize,
    on: &mut impl FnMut(Event<'a>) -> Result<(), Stop>,
    cdata: &mut Option<(&'static str, bool)>,
) -> Result<Option<usize>, Stop> {
    let s = html.as_bytes();
    if find(s, b">", i + 2).is_none() {
        return Ok(None);
    }
    if !starts_tag(s, i + 2) {
        if s.get(i + 2) == Some(&b'>') {
            return Ok(Some(i + 3)); // `</>` is ignored
        }
        return Ok(find(s, b">", i + 2).map(|p| p + 1)); // a bogus comment
    }
    let j = locate_tag_end(s, i + 2);
    if s[j - 1] != b'>' {
        return Ok(None);
    }
    let (name_end, _) = tagfind(s, i + 2);
    on(Event::End(html[i + 2..name_end].to_lowercase()))?;
    *cdata = None;
    Ok(Some(j))
}

/// `<!--`: to `-->` or `--!>`, else `<!-->` or `<!--->` closes at once.
fn parse_comment(s: &[u8], i: usize) -> Option<usize> {
    let mut from = i + 4;
    while let Some(p) = find(s, b"--", from) {
        if s.get(p + 2) == Some(&b'>') {
            return Some(p + 3);
        }
        if s.get(p + 2) == Some(&b'!') && s.get(p + 3) == Some(&b'>') {
            return Some(p + 4);
        }
        from = p + 1;
    }
    match (s.get(i + 4), s.get(i + 5)) {
        (Some(b'>'), _) => Some(i + 5),
        (Some(b'-'), Some(b'>')) => Some(i + 6),
        _ => None,
    }
}

fn parse_html_declaration(s: &[u8], i: usize) -> Option<usize> {
    if s[i..].starts_with(b"<![CDATA[") {
        return find(s, b"]]>", i + 9).map(|j| j + 3);
    }
    if s.len() >= i + 9 && s[i..i + 9].eq_ignore_ascii_case(b"<!doctype") {
        return find(s, b">", i + 9).map(|j| j + 1);
    }
    if s[i..].starts_with(b"<![") {
        return find(s, b">", i + 3).map(|j| j + 1);
    }
    find(s, b">", i + 2).map(|j| j + 1)
}

static CHARREF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"&(#[0-9]+;?|#[xX][0-9a-fA-F]+;?|[^\t\n\x0c <&#;]{1,32};?)")
        .unwrap_or_else(|e| panic!("bad pattern: {e}"))
});

/// `html.unescape`.
pub(super) fn unescape(s: &str) -> Result<Cow<'_, str>, Stop> {
    if !s.contains('&') {
        return Ok(Cow::Borrowed(s));
    }
    // Every `&#` and its digits is one match of the pattern.
    for (at, _) in s.match_indices("&#") {
        let digits = s.as_bytes()[at + 2..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits > 4300 {
            return Err(Stop);
        }
    }
    Ok(CHARREF.replace_all(s, |c: &Captures<'_>| reference(&c[1])))
}

fn entity(name: &str) -> Option<&'static str> {
    ENTITIES
        .binary_search_by(|(k, _)| (*k).cmp(name))
        .ok()
        .map(|i| ENTITIES[i].1)
}

/// `_replace_charref` for `&` + `s`.
fn reference(s: &str) -> String {
    if let Some(num) = s.strip_prefix('#') {
        let num = num.trim_end_matches(';');
        let value = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16),
            None => num.parse::<u32>(),
        }
        // Too many digits for u32 is past U+10FFFF too.
        .unwrap_or(u32::MAX);
        return match value {
            0 => "\u{fffd}".into(),
            13 => "\r".into(),
            0x80..=0x9f => {
                const C1: [char; 32] = [
                    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}',
                    'Ž', '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›',
                    'œ', '\u{9d}', 'ž', 'Ÿ',
                ];
                C1[(value - 0x80) as usize].to_string()
            }
            0xd800..=0xdfff | 0x11_0000.. => "\u{fffd}".into(),
            1..=8 | 11 | 14..=31 | 127 | 0xfdd0..=0xfdef => String::new(),
            v if v & 0xfffe == 0xfffe => String::new(),
            v => char::from_u32(v).map(String::from).unwrap_or_default(),
        };
    }
    if let Some(text) = entity(s) {
        return text.into();
    }
    // The longest name the reference starts with, two characters or more.
    let ends: Vec<usize> = s.char_indices().map(|(i, _)| i).skip(2).collect();
    for &end in ends.iter().rev() {
        if let Some(text) = entity(&s[..end]) {
            return format!("{text}{}", &s[end..]);
        }
    }
    format!("&{s}")
}

#[cfg(test)]
#[path = "pyhtml_tests.rs"]
mod tests;
