//! Test support shared by the other crates: the oracle client that asks rpctl
//! at the pinned commit (behind the `oracle` feature), the fixture loader, the
//! temp-XDG guard that keeps tests away from real state, a scripted
//! frontend, the scripted model server and the scenarios it serves, and
//! readable snapshots of the model context (docs/implementation-plan.md
//! 3.2-3.6). Never a dependency of non-test code.

pub mod context_snapshot;
pub mod fixtures;
pub mod frontend;
pub mod fuzz;
pub mod home;
pub mod mock_llm;
#[cfg(feature = "oracle")]
pub mod oracle;
pub mod scenario;
pub mod scratch;

mod repo;
