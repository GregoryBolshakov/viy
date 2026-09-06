//! The wall clock behind a seam (kb d037, d057): the [`TimeProvider`] trait
//! after Codex's (core/src/current_time.rs), its system implementation
//! [`SystemTimeProvider`], and in viy-testkit a fake. Below viy-pod and
//! viy-agent.
//!
//! Only the time of day goes through it: what lands in records and is
//! compared with the pod's log timestamps. Elapsed time and sleeps are
//! tokio's, which a test pauses and advances (`start_paused`), as Codex's
//! tests do.

use std::fmt;

use chrono::DateTime;
use chrono::Utc;

/// Tells the time of day.
pub trait TimeProvider: Send + Sync + fmt::Debug {
    fn now(&self) -> DateTime<Utc>;
}

/// This machine's clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTimeProvider;

impl TimeProvider for SystemTimeProvider {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
