//! This machine's backup for the pod's own idle watchdog: rpctl's
//! `idle.py`. So far the limits, which `up` reports with the watchdog's
//! state, and the activity stamp `up` and the chat write; `viy reap` comes
//! with WP 2.9.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use viy_config::Config;
use viy_config::Paths;
use viy_time::TimeProvider;

/// [`touch`] writes at most this often.
const TOUCH_EVERY_S: i64 = 30;

/// The pod's idle limits from the config: how long without tokens, the
/// reaper's grace on top, and the hard cap. Zero means off; a value that is
/// not a number counts as zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub ttl: Duration,
    pub grace: Duration,
    pub max: Duration,
}

pub fn limits(cfg: &Config) -> Limits {
    let seconds = |key: &str, unit: f64| {
        let n = cfg
            .get(key)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(0.0);
        Duration::try_from_secs_f64(n * unit).unwrap_or(Duration::ZERO)
    };
    Limits {
        ttl: seconds("IDLE_TTL_MIN", 60.0),
        grace: seconds("REAP_GRACE_MIN", 60.0),
        max: seconds("MAX_POD_HOURS", 3600.0),
    }
}

/// Records that the model is in use, at most every 30 s per state
/// directory unless `force`: cheap enough per streamed chunk. Best effort.
pub fn touch(paths: &Paths, time: &dyn TimeProvider, force: bool) {
    static LAST: Mutex<BTreeMap<PathBuf, DateTime<Utc>>> = Mutex::new(BTreeMap::new());
    let now = time.now();
    let path = paths.activity_file();
    let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
    if !force
        && last
            .get(&path)
            .is_some_and(|at| (now - *at).num_seconds() < TOUCH_EVERY_S)
    {
        return;
    }
    last.insert(path, now);
    let _ = fs::create_dir_all(paths.pod_state_dir()).and_then(|()| {
        fs::write(
            paths.activity_file(),
            format!("{:.0}\n", now.timestamp_micros() as f64 / 1e6),
        )
    });
}

/// When this machine last streamed, if it ever did.
pub fn last_activity(paths: &Paths) -> Option<DateTime<Utc>> {
    let text = fs::read_to_string(paths.activity_file()).ok()?;
    let seconds: f64 = text.trim().parse().ok()?;
    DateTime::from_timestamp_micros((seconds * 1e6) as i64)
}
