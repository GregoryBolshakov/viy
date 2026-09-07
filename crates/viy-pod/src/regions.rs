//! Regions: rpctl's `regions.py`. So far what `up` needs on its success
//! path and for a refused creation: whether a refusal is for capacity,
//! whether failover is on, and the log of refusals. The backup region,
//! failover with its questions, stock and expiry come with WP 2.8.

use std::fs;
use std::io;
use std::io::Write;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::json;
use viy_config::Config;
use viy_config::Paths;
use viy_config::config::gpu_type_list;
use viy_config::redact::Redactor;
use viy_time::TimeProvider;

use crate::api::RunPodError;

/// RunPod's wording: "There are no instances currently available", and the
/// older "There are no longer any instances available with the requested
/// specifications". Only this refusal is one another region can fix; a
/// credit or request error would fail anywhere.
pub fn is_capacity_error(error: &RunPodError) -> bool {
    static CAPACITY: LazyLock<Regex> = LazyLock::new(|| {
        crate::compile_regex(
            r"(?i)no (?:longer any )?instances|instances? currently available|not enough (?:free )?gpus|no gpus? (?:are )?available|out of capacity",
        )
    });
    CAPACITY.is_match(&error.to_string())
}

pub fn failover_enabled(cfg: &Config) -> bool {
    !matches!(
        cfg.get("FAILOVER").unwrap_or("").trim(),
        "" | "0" | "false" | "no"
    )
}

/// One line in `up_failures.jsonl` per refused pod creation, secrets
/// masked. Best effort: a log that cannot be written changes nothing.
pub fn log_failure(
    paths: &Paths,
    time: &dyn TimeProvider,
    cfg: &Config,
    region: &str,
    role: &str,
    volume_id: &str,
    error: &RunPodError,
) {
    let text: String = Redactor::from_config(cfg)
        .text(&error.to_string())
        .chars()
        .take(1000)
        .collect();
    let row = json!({
        "at": time.now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string(),
        "region": region,
        "role": role,
        "volume_id": volume_id,
        "pod_name": cfg.get("POD_NAME"),
        "gpu_types": gpu_type_list(cfg),
        "status_code": error.status_code(),
        "capacity": is_capacity_error(error),
        "error": text,
    });
    let append = || -> io::Result<()> {
        fs::create_dir_all(paths.pod_state_dir())?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.up_failures_file())?
            .write_all(format!("{row}\n").as_bytes())
    };
    let _ = append();
}
