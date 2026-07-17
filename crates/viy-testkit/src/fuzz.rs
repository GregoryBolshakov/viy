//! Generated inputs for the oracle tests: a seeded splitmix64 generator and
//! strings built from pieces chosen to hit the edges of a Python semantic
//! (whitespace classes, non-ASCII digits, number syntax, astral chars).
//!
//! Runs are reproducible: the seed is fixed unless `VIY_FUZZ_SEED` is set, and
//! every failure message should print it. `VIY_FUZZ_CASES` scales the number
//! of cases per property (implementation plan 3.2; proptest is not used: it
//! is not in the cargo cache, and Codex does without it).

/// The seed when `VIY_FUZZ_SEED` is not set.
pub const DEFAULT_SEED: u64 = 0x0005_eed0_f797;

/// A splitmix64 generator.
#[derive(Debug, Clone)]
pub struct Gen {
    state: u64,
}

impl Gen {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Seeded from `VIY_FUZZ_SEED` (decimal or 0x-hex), else [`DEFAULT_SEED`].
    pub fn from_env() -> Self {
        Self::new(seed())
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `0..n`; `n` must not be 0.
    pub fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        usize::try_from(self.next_u64() % n).unwrap_or(0)
    }

    /// One of `items`, which must not be empty.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// Up to `max_pieces` of `pieces`, concatenated.
    pub fn string(&mut self, pieces: &[&str], max_pieces: usize) -> String {
        let n = self.below(max_pieces + 1);
        (0..n).map(|_| *self.pick(pieces)).collect()
    }
}

/// The seed in use: `VIY_FUZZ_SEED` or [`DEFAULT_SEED`].
pub fn seed() -> u64 {
    std::env::var("VIY_FUZZ_SEED")
        .ok()
        .and_then(|s| match s.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => s.parse().ok(),
        })
        .unwrap_or(DEFAULT_SEED)
}

/// Cases per property: `VIY_FUZZ_CASES` or `default`.
pub fn cases(default: usize) -> usize {
    std::env::var("VIY_FUZZ_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Pieces for Python string semantics: every whitespace and line-break class
/// boundary, decimal and non-decimal digits of several scripts, number
/// syntax, and ordinary text in Cyrillic, CJK and emoji.
pub const PY_STR_PIECES: &[&str] = &[
    // whitespace and line breaks, ASCII and not, plus near misses
    " ", "  ", "\t", "\n", "\r", "\r\n", "\x0b", "\x0c", "\x1c", "\x1d", "\x1e", "\x1f", "\u{85}",
    "\u{a0}", "\u{1680}", "\u{2000}", "\u{200a}", "\u{200b}", "\u{2028}", "\u{2029}", "\u{202f}",
    "\u{205f}", "\u{3000}", "\u{180e}", "\u{feff}", "\0", "\x7f",
    // digits: ASCII, decimal in other scripts, digits that are not decimal
    "0", "1", "5", "9", "00", "١", "٥", "۹", "०", "৯", "๓", "９", "𝟘", "𝟗", "²", "①", "Ⅷ", "½",
    // number syntax
    "+", "-", "_", "__", ".", "e", "E", "e-", "inf", "Infinity", "nan", "NaN", "x", "0x",
    // text
    "a", "Z", "ы", "Ключ", "日本", "😀", "é", "e\u{301}",
];

/// Pieces for `int()` and `float()`: mostly what a number is made of, so that
/// a fair share of the inputs parse, with the separators and near misses that
/// decide the edge cases.
pub const PY_NUMBER_PIECES: &[&str] = &[
    "0", "1", "2", "7", "9", "00", "١", "٥", "９", "𝟗", "²", "½", "+", "-", "_", ".", "e", "E",
    "e-", "inf", "nan", " ", "\t", "\x1c", "\u{a0}", "\u{3000}", "\0", "x", "ы",
];

/// Pieces for shell-ish commands: quotes, escapes, every punctuation and
/// comment character shlex knows, its four whitespace characters and near
/// misses, and words.
pub const SHELL_PIECES: &[&str] = &[
    " ", "  ", "\t", "\n", "\r", "\x0b", "\x1c", "\u{a0}", "\u{3000}", "'", "\"", "\\", "\\\"",
    "''", "\"\"", "&", "&&", "|", "||", ";", ";;", "(", ")", "<", ">", ">&", "2>", "#", "$", "$(",
    "`", "*", "?", "~", "=", "-", "--", ".", "/", "/tmp", "rm", "-rf", "git", "curl", "x.com", "a",
    "b1", "ы", "😀", "ß", "é",
];

#[cfg(test)]
#[path = "fuzz_tests.rs"]
mod tests;
