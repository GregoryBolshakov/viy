//! A [`TimeProvider`] for tests (kb d057): a fixed time of day that moves
//! with tokio's clock, so a test on paused time sees the wall clock advance
//! exactly as far as its sleeps and timeouts do, and further when told.

use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use tokio::time::Instant;
use viy_time::TimeProvider;

#[derive(Debug)]
pub struct FakeTime {
    start: DateTime<Utc>,
    origin: Instant,
    skipped: Mutex<Duration>,
}

impl FakeTime {
    /// It is `start` now. Inside a tokio runtime: it reads tokio's clock.
    pub fn at(start: DateTime<Utc>) -> Self {
        Self {
            start,
            origin: Instant::now(),
            skipped: Mutex::default(),
        }
    }

    /// Moves the time of day on by `by`, tokio's clock staying put.
    pub fn advance(&self, by: Duration) {
        *self.skipped.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }
}

impl TimeProvider for FakeTime {
    fn now(&self) -> DateTime<Utc> {
        let skipped = *self.skipped.lock().unwrap_or_else(PoisonError::into_inner);
        let since = self.origin.elapsed() + skipped;
        self.start + chrono::TimeDelta::from_std(since).unwrap_or(chrono::TimeDelta::MAX)
    }
}
