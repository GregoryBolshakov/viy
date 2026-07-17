//! Test support shared by the other crates: the oracle client that asks rpctl
//! at the pinned commit (behind the `oracle` feature), the fixture loader, the
//! temp-XDG guard that keeps tests away from real state, and the fakellm
//! runner (docs/implementation-plan.md 3.2-3.6). Never a dependency of
//! non-test code.

pub mod fixtures;
#[cfg(feature = "oracle")]
pub mod oracle;

mod repo;
