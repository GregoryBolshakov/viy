//! CPython 3.10's `shlex`, hand-ported (docs/flags.md F06): `split`,
//! `quote`, `join`, and the lexer with `punctuation_chars` that approve.py
//! and egress.py build. The `shlex` crate has no `punctuation_chars` and
//! quotes differently, and the boot script's `shlex.quote` output must stay
//! byte-identical.
//!
//! Only the configuration rpctl uses is ported: `posix=True` with
//! `whitespace_split=True`, which also makes `wordchars` irrelevant (any
//! character that is not whitespace, a quote, the escape, a comment or a
//! punctuation character extends the word). The state machine is
//! `shlex.read_token` line for line; the oracle tests compare it with CPython.

use std::fmt;

/// `punctuation_chars=True`.
pub const DEFAULT_PUNCTUATION: &str = "();<>|&";

const WHITESPACE: &[char] = &[' ', '\t', '\r', '\n'];
const QUOTES: &[char] = &['\'', '"'];
const ESCAPE: char = '\\';
/// Quotes inside which the escape character works: `"`.
const ESCAPED_QUOTES: &[char] = &['"'];

/// The `ValueError`s shlex raises, with its messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShlexError {
    NoClosingQuotation,
    NoEscapedCharacter,
}

impl fmt::Display for ShlexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ShlexError::NoClosingQuotation => "No closing quotation",
            ShlexError::NoEscapedCharacter => "No escaped character",
        })
    }
}

impl std::error::Error for ShlexError {}

/// `shlex.split(s)`: posix, no comments, no punctuation.
pub fn split(s: &str) -> Result<Vec<String>, ShlexError> {
    tokens(s, "", "")
}

/// `list(lex)` for `lex = shlex.shlex(s, posix=True,
/// punctuation_chars=punctuation)` with `lex.whitespace_split = True` and
/// `lex.commenters = commenters` (shlex's own default is `"#"`).
pub fn tokens(s: &str, punctuation: &str, commenters: &str) -> Result<Vec<String>, ShlexError> {
    let mut lexer = Lexer {
        input: s.chars().collect(),
        pos: 0,
        punctuation: punctuation.chars().collect(),
        commenters: commenters.chars().collect(),
        state: State::Whitespace,
        pushback: Vec::new(),
    };
    let mut out = Vec::new();
    while let Some(token) = lexer.read_token()? {
        out.push(token);
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// `' '`
    Whitespace,
    /// `'a'`: in a word.
    Word,
    /// `'c'`: in a run of punctuation characters.
    Punctuation,
    /// Inside this quote character.
    Quote(char),
    /// After the escape character; the state to return to.
    Escape(Escaped),
    /// `None`: past the end.
    Eof,
}

/// `escapedstate`: where an escape was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escaped {
    Word,
    Quote(char),
}

struct Lexer {
    input: Vec<char>,
    pos: usize,
    punctuation: Vec<char>,
    commenters: Vec<char>,
    state: State,
    /// `_pushback_chars`, a stack.
    pushback: Vec<char>,
}

impl Lexer {
    fn next_char(&mut self) -> Option<char> {
        if !self.punctuation.is_empty()
            && let Some(c) = self.pushback.pop()
        {
            return Some(c);
        }
        let c = self.input.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// `instream.readline()`: through the next newline, or to the end.
    fn skip_line(&mut self) {
        while let Some(&c) = self.input.get(self.pos) {
            self.pos += 1;
            if c == '\n' {
                break;
            }
        }
    }

    /// One token; `Ok(None)` at the end (shlex's `eof` in posix mode).
    fn read_token(&mut self) -> Result<Option<String>, ShlexError> {
        let mut quoted = false;
        let mut token = String::new();
        loop {
            let next = self.next_char();
            match self.state {
                State::Eof => {
                    token.clear();
                    break;
                }
                State::Whitespace => {
                    let Some(c) = next else {
                        self.state = State::Eof;
                        break;
                    };
                    if WHITESPACE.contains(&c) {
                        if !token.is_empty() || quoted {
                            break;
                        }
                    } else if self.commenters.contains(&c) {
                        self.skip_line();
                    } else if c == ESCAPE {
                        self.state = State::Escape(Escaped::Word);
                    } else if self.punctuation.contains(&c) {
                        token.push(c);
                        self.state = State::Punctuation;
                    } else if QUOTES.contains(&c) {
                        self.state = State::Quote(c);
                    } else {
                        // wordchars and whitespace_split alike start a word.
                        token.push(c);
                        self.state = State::Word;
                    }
                }
                State::Quote(q) => {
                    quoted = true;
                    let Some(c) = next else {
                        return Err(ShlexError::NoClosingQuotation);
                    };
                    if c == q {
                        self.state = State::Word;
                    } else if c == ESCAPE && ESCAPED_QUOTES.contains(&q) {
                        self.state = State::Escape(Escaped::Quote(q));
                    } else {
                        token.push(c);
                    }
                }
                State::Escape(escaped) => {
                    let Some(c) = next else {
                        return Err(ShlexError::NoEscapedCharacter);
                    };
                    // In quotes, only the quote itself or the escape may be
                    // escaped; otherwise the backslash stays.
                    if let Escaped::Quote(q) = escaped
                        && c != ESCAPE
                        && c != q
                    {
                        token.push(ESCAPE);
                    }
                    token.push(c);
                    self.state = match escaped {
                        Escaped::Word => State::Word,
                        Escaped::Quote(q) => State::Quote(q),
                    };
                }
                State::Word | State::Punctuation => {
                    let Some(c) = next else {
                        self.state = State::Eof;
                        break;
                    };
                    if WHITESPACE.contains(&c) {
                        self.state = State::Whitespace;
                        if !token.is_empty() || quoted {
                            break;
                        }
                    } else if self.commenters.contains(&c) {
                        self.skip_line();
                        self.state = State::Whitespace;
                        if !token.is_empty() || quoted {
                            break;
                        }
                    } else if self.state == State::Punctuation {
                        if self.punctuation.contains(&c) {
                            token.push(c);
                        } else {
                            self.pushback.push(c);
                            self.state = State::Whitespace;
                            break;
                        }
                    } else if QUOTES.contains(&c) {
                        self.state = State::Quote(c);
                    } else if c == ESCAPE {
                        self.state = State::Escape(Escaped::Word);
                    } else if !self.punctuation.contains(&c) {
                        token.push(c);
                    } else {
                        self.pushback.push(c);
                        self.state = State::Whitespace;
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                }
            }
        }
        if token.is_empty() && !quoted {
            return Ok(None);
        }
        Ok(Some(token))
    }
}

/// `shlex.quote(s)`.
pub fn quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    // re.compile(r'[^\w@%+=:,./-]', re.ASCII): the characters needing quotes.
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if s.chars().all(safe) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// `shlex.join(words)`.
pub fn join<S: AsRef<str>>(words: &[S]) -> String {
    words
        .iter()
        .map(|w| quote(w.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
#[path = "shlex_tests.rs"]
mod tests;
