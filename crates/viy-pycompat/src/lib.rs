//! Python semantics that viy's behaviour rests on, in one place: string
//! length and slicing in code points (`pystr`), `json.dumps` output
//! (`pyjson`), `posixpath` and `Path.resolve()`, `shlex`, `re`, and `repr()`
//! (docs/flags.md F01, F05-F08, F22). Every function is checked against rpctl
//! through the oracle (WP 0.9).
//!
//! The workspace's `clippy.toml` forbids `str::trim`, `str::lines` and
//! friends elsewhere, because Python's whitespace and line breaks are not
//! Rust's; this crate is where the replacements live.

pub mod pyjson;
pub mod pyrepr;
pub mod pystr;
mod pystr_tables;
pub mod shlex;
