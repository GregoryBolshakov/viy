//! The pod: RunPod REST and GraphQL, pod tracking by `POD_NAME`, the boot
//! script and create-pod body built byte for byte like rpctl's, the embedded
//! `startup.py`, the SSH tunnel with its pinned host key, readiness waits, the
//! boot report, regions and backup, the network volume, idle stamp, reap and
//! shutdown, and the `runpod`, `openai` and `local` backends (ROADMAP
//! phase 2). The pod side itself is not ported (kb d003).

pub mod api;
pub mod boot;
pub mod lock;
pub mod process;
pub mod state;

/// rp, from the pinned snapshot, and viy on one state directory.
#[cfg(test)]
#[cfg(feature = "oracle")]
#[path = "interop_tests.rs"]
mod interop_tests;
