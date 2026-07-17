// Single integration test binary that aggregates all test modules.
// The submodules live in `tests/suite/`. They ask CPython through the oracle,
// so they exist only with `--features oracle`.
#![cfg(feature = "oracle")]
// Test helpers outside #[test] fns may panic too (Codex does the same).
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod suite;
