//! Test support shared by the other crates: the oracle client that asks rpctl
//! at the pinned commit (behind the `oracle` feature), the fixture loader, the
//! temp-XDG guard that keeps tests away from real state, config fixture
//! cases laid out on disk, scripted HTTP and process backends and a clock,
//! an sshd of the tests' own, the scripted model server and the scenarios
//! it serves, and readable snapshots of the model context (docs/implementation-plan.md 3.2-3.6). Never a
//! dependency of non-test code.

pub mod config_tree;
pub mod context_snapshot;
pub mod exec;
pub mod fixtures;
pub mod fuzz;
pub mod home;
pub mod http;
pub mod mock_llm;
#[cfg(feature = "oracle")]
pub mod oracle;
pub mod scenario;
pub mod scratch;
pub mod sshd;
pub mod time;

mod repo;
