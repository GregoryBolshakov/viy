//! At most 30 frames a second (ui-plan 11): whatever changes between two
//! frames, any number of deltas included, is drawn by the next one (Codex's
//! `tui/frame_rate_limiter.rs`, at 30 rather than 120 frames a second).

use std::time::Duration;

use tokio::time::Instant;

pub const FRAME_INTERVAL: Duration = Duration::from_nanos(33_333_334);

#[derive(Debug, Default)]
pub struct FrameLimiter {
    last: Option<Instant>,
}

impl FrameLimiter {
    /// When a frame asked for at `requested` may be drawn.
    pub fn deadline(&self, requested: Instant) -> Instant {
        match self.last {
            Some(last) => requested.max(last + FRAME_INTERVAL),
            None => requested,
        }
    }

    pub fn drawn(&mut self, at: Instant) {
        self.last = Some(at);
    }
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
