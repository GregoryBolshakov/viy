//! The literal view of a command: its text cut into tokens at whitespace and
//! shell punctuation, quotes and escapes ignored, which is how rpctl's
//! patterns see it. Text the shell hands to another program as data
//! (`bash -c '...'`, `ssh host '...'`, `xargs rm -rf`, `find -exec`) is
//! checked here, where the argv view sees one quoted word.
//!
//! Each rule is the token form of one rpctl pattern, with the same anchors
//! (what may precede a name, whitespace between words, the end of the line
//! or list), so on ASCII text it decides as rpctl does. Where Python's
//! character classes differ from Rust's, the classes in `chars` make it
//! match at least where rpctl matches.

use super::chars::is_ascii_word;
use super::chars::is_broad_word;
use super::chars::is_py_space;

/// Characters that end a token: whitespace and every shell character that
/// can sit next to a command name. None of them occurs inside a token a
/// rule looks for (`-rf`, `--hard`, `mkfs.ext4`, `/usr/bin/rp`).
fn is_sep(c: char) -> bool {
    is_py_space(c) || ";&|()'\"`\\$<>{}".contains(c)
}

struct Token {
    start: usize,
    end: usize,
    text: Vec<char>,
}

impl Token {
    fn is(&self, word: &str) -> bool {
        self.text.iter().copied().eq(word.chars())
    }

    fn starts_with(&self, word: &str) -> bool {
        let w: Vec<char> = word.chars().collect();
        self.text.starts_with(&w)
    }

    fn ends_with(&self, word: &str) -> bool {
        let w: Vec<char> = word.chars().collect();
        self.text.ends_with(&w)
    }

    /// `\b<word>\b` inside the token: the offsets where it starts.
    fn word_at(&self, word: &str) -> Option<usize> {
        let w: Vec<char> = word.chars().collect();
        (0..=self.text.len().saturating_sub(w.len())).find(|&k| {
            self.text[k..].starts_with(&w)
                && (k == 0 || !is_ascii_word(self.text[k - 1]))
                && self
                    .text
                    .get(k + w.len())
                    .is_none_or(|c| !is_ascii_word(*c))
        })
    }

    /// `\b<word>` at the end of the token.
    fn ends_with_word(&self, word: &str) -> bool {
        let n = word.chars().count();
        self.ends_with(word)
            && (self.text.len() == n || !is_ascii_word(self.text[self.text.len() - n - 1]))
    }

    /// `<prefix>\b` at the start of the token.
    fn starts_with_word(&self, prefix: &str) -> bool {
        let n = prefix.chars().count();
        self.starts_with(prefix) && self.text.get(n).is_none_or(|c| !is_ascii_word(*c))
    }
}

pub(super) struct Literal {
    chars: Vec<char>,
    tokens: Vec<Token>,
}

impl Literal {
    pub(super) fn new(cmd: &str) -> Self {
        let chars: Vec<char> = cmd.chars().collect();
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if is_sep(chars[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < chars.len() && !is_sep(chars[i]) {
                i += 1;
            }
            tokens.push(Token {
                start,
                end: i,
                text: chars[start..i].to_vec(),
            });
        }
        Self { chars, tokens }
    }

    fn prev(&self, t: &Token) -> Option<char> {
        t.start.checked_sub(1).map(|i| self.chars[i])
    }

    fn next(&self, t: &Token) -> Option<char> {
        self.chars.get(t.end).copied()
    }

    /// The token is followed by whitespace (`\s`).
    fn space_after(&self, t: &Token) -> bool {
        self.next(t).is_some_and(is_py_space)
    }

    /// The token is followed by whitespace or ends the text (`(\s|$)`).
    fn space_or_end_after(&self, t: &Token) -> bool {
        self.next(t).is_none_or(is_py_space)
    }

    /// The token starts the text or follows whitespace or one of `set`.
    fn after(&self, t: &Token, set: &str) -> bool {
        self.prev(t)
            .is_none_or(|c| is_py_space(c) || set.contains(c))
    }

    /// Token `i + 1`, when only whitespace separates it from token `i`
    /// (`\s+`); token `i` must itself end at whitespace.
    fn word_after(&self, i: usize) -> Option<&Token> {
        let t = &self.tokens[i];
        let u = self.tokens.get(i + 1)?;
        (self.space_after(t) && self.chars[t.end..u.start].iter().all(|c| is_py_space(*c)))
            .then_some(u)
    }

    // ---------------------------------------------------------- DESTRUCTIVE

    /// `(^|[;&|(\s'"\\/])rm\s+([^;&|\n]*\s)?(-\w*[rRf]|--recursive|--force)`
    pub(super) fn rm(&self) -> bool {
        (0..self.tokens.len()).any(|i| {
            let t = &self.tokens[i];
            ((t.is("rm") && self.after(t, ";&|('\"\\")) || t.ends_with("/rm"))
                && self.later_in_list(i, rm_flag)
        })
    }

    /// `\bgit\s+(-[Cc]\s+\S+\s+|-[^\sCc]\S*\s+)*(clean|reset\s+--hard|restore\b|stash\s+(drop|clear)|checkout\s+…|switch\s+…)`
    pub(super) fn git(&self) -> bool {
        (0..self.tokens.len()).any(|i| {
            let t = &self.tokens[i];
            if !t.ends_with_word("git") || !self.space_after(t) {
                return false;
            }
            let p = self.past_git_options(self.skip_space(t.end));
            let Some(j) = self.tokens.iter().position(|u| u.start == p) else {
                return false;
            };
            let sub = &self.tokens[j];
            let then = |w: &str| self.word_after(j).is_some_and(|u| u.starts_with(w));
            let flag_in = |words: &'static [&'static str]| {
                self.later_in_list(j, |u: &Token| {
                    words.iter().any(|w| u.is(w)) && self.space_or_end_after(u)
                })
            };
            (sub.starts_with("clean"))
                || (sub.is("reset") && then("--hard"))
                || sub.starts_with_word("restore")
                || (sub.is("stash") && (then("drop") || then("clear")))
                || (sub.is("checkout") && flag_in(&["--", "-f", "--force", "."]))
                || (sub.is("switch") && flag_in(&["-f", "--force", "--discard-changes"]))
        })
    }

    /// Past `(-[Cc]\s+\S+\s+|-[^\sCc]\S*\s+)*` from `p`: git's own options.
    fn past_git_options(&self, mut p: usize) -> usize {
        let at = |k: usize| self.chars.get(k).copied();
        while at(p) == Some('-') {
            let end = match at(p + 1) {
                Some('C' | 'c') => {
                    let value = self.skip_space(p + 2);
                    let end = self.skip_word(value);
                    if value == p + 2 || end == value {
                        break;
                    }
                    end
                }
                Some(c) if !is_py_space(c) => self.skip_word(p + 1),
                _ => break,
            };
            let next = self.skip_space(end);
            if next == end {
                break;
            }
            p = next;
        }
        p
    }

    fn skip_space(&self, mut p: usize) -> usize {
        while self.chars.get(p).is_some_and(|c| is_py_space(*c)) {
            p += 1;
        }
        p
    }

    /// Past a run of non-whitespace (`\S*`).
    fn skip_word(&self, mut p: usize) -> usize {
        while self.chars.get(p).is_some_and(|c| !is_py_space(*c)) {
            p += 1;
        }
        p
    }

    /// `<token i>\s+([^;&|\n]*\s)?X` for a token X that `is_x` accepts.
    fn later_in_list(&self, i: usize, is_x: impl Fn(&Token) -> bool) -> bool {
        let t = &self.tokens[i];
        if !self.space_after(t) {
            return false;
        }
        let from = self.skip_space(t.end);
        // The run may not cross a list operator or a newline, but the one
        // whitespace character that ends it may be a newline.
        self.tokens[i + 1..]
            .iter()
            .take_while(|u| {
                u.start == from
                    || !self.chars[from..u.start - 1]
                        .iter()
                        .any(|c| ";&|\n".contains(*c))
            })
            .any(|u| (u.start == from || self.prev(u).is_some_and(is_py_space)) && is_x(u))
    }

    /// `\bfind\b.*\s-delete\b`
    pub(super) fn find_delete(&self) -> bool {
        self.tokens.iter().enumerate().any(|(i, t)| {
            let Some(k) = t.word_at("find") else {
                return false;
            };
            let end = t.start + k + 4;
            self.tokens[i + 1..].iter().any(|u| {
                u.starts_with_word("-delete")
                    && self.prev(u).is_some_and(is_py_space)
                    && !self.chars[end..u.start - 1].contains(&'\n')
            })
        })
    }

    /// `(^|[;&|(\s])(shred|truncate|mkfs\S*|dd)\s`
    pub(super) fn overwrite(&self) -> bool {
        self.tokens.iter().any(|t| {
            self.after(t, ";&|(")
                && (((t.is("shred") || t.is("truncate") || t.is("dd")) && self.space_after(t))
                    || (t.starts_with("mkfs")
                        && self.chars[t.start + 4..].iter().any(|c| is_py_space(*c))))
        })
    }

    /// `\bchmod\s+-R\b|\bchown\b`
    pub(super) fn bulk_permissions(&self) -> bool {
        (0..self.tokens.len()).any(|i| {
            let t = &self.tokens[i];
            t.word_at("chown").is_some()
                || (t.ends_with_word("chmod")
                    && self.word_after(i).is_some_and(|u| u.starts_with_word("-R")))
        })
    }

    // -------------------------------------------------------- NET_ALWAYS_ASK

    /// `(^|[;&|(\s/])(<name>)(\s|$)` for each of `names`.
    pub(super) fn program(&self, names: &[&str]) -> bool {
        self.tokens.iter().any(|t| {
            self.space_or_end_after(t)
                && names.iter().any(|name| {
                    (t.is(name) && self.after(t, ";&|(")) || t.ends_with(&format!("/{name}"))
                })
        })
    }

    /// `\b(<name>)\b` for each of `names`.
    pub(super) fn word(&self, names: &[&str]) -> bool {
        self.tokens
            .iter()
            .any(|t| names.iter().any(|name| t.word_at(name).is_some()))
    }
}

/// `-\w*[rRf]|--recursive|--force` at the start of a word.
fn rm_flag(t: &Token) -> bool {
    if t.starts_with("--") {
        t.starts_with("--recursive") || t.starts_with("--force")
    } else {
        t.starts_with("-")
            && t.text[1..]
                .iter()
                .take_while(|c| is_broad_word(**c))
                .any(|c| matches!(c, 'r' | 'R' | 'f'))
    }
}
