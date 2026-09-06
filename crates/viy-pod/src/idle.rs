//! This machine's backup for the pod's own idle watchdog: rpctl's
//! `idle.py`. Only `limits` so far, which `up` reports with the watchdog's
//! state; the activity stamp and `viy reap` come with their work package.

use std::time::Duration;

use viy_config::Config;

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
