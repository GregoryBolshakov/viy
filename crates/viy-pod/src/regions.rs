//! Regions: rpctl's `regions.py`. DATA_CENTER_ID stays the primary; when
//! RunPod refuses a pod there for lack of capacity, `viy up` offers the
//! backup volume `<NETWORK_VOLUME_NAME>-backup` in a second region, and
//! failing that moves the backup to a region where RunPod shows one of our
//! GPU types free, the cheapest first (`crate::failover`). This module
//! keeps what that needs and what follows it: the record of backup volumes
//! (`backups.json`) and of every refused creation (`up_failures.jsonl`),
//! RunPod's stock of our GPU types (`stock.jsonl`) and the regions ranked
//! by it, the deletion of the old
//! backup once its replacement serves, `viy reap`'s expiry of backups no
//! pod has used for BACKUP_IDLE_DAYS, and `viy backup`.
//!
//! The files are rp's, at rp's paths (kb d006): rp and viy each read what
//! the other wrote.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::DateTime;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_config::Config;
use viy_config::Paths;
use viy_config::config::gpu_type_list;
use viy_config::redact::Redactor;
use viy_time::TimeProvider;

use crate::api::RunPod;
use crate::api::RunPodError;
use crate::api::shown;
use crate::api::truthy;
use crate::commands::CommandError;
use crate::commands::Env;
use crate::notice::Reporter;
use crate::notice::Tone;
use crate::notice::say;
use crate::tracked::pod_volume_id;

/// Stock queries before a region is offered, and the pause between them.
pub const SAMPLES: usize = 3;
pub const SAMPLE_GAP: Duration = Duration::from_secs(2);
/// Between rounds of refused creations.
pub const RETRY_GAP_S: u64 = 10;
/// How long a volume made for a pod no region took may wait for `viy up`
/// to delete it before `viy reap` does.
const UNUSED_GRACE_S: f64 = 3600.0;
/// Every allowedCudaVersions value RunPod's REST API accepts (its enum,
/// checked 2026-07-08). It matches them exactly against the host's
/// driver, and hosts have moved on to 13.1 and 13.2, which the REST API
/// cannot ask for.
const REST_CUDA: [&str; 12] = [
    "13.0", "12.9", "12.8", "12.7", "12.6", "12.5", "12.4", "12.3", "12.2", "12.1", "12.0", "11.8",
];
pub const HISTORY_DAYS: u32 = 30;
const DAY_S: f64 = 86400.0;

/// RunPod's wording: "There are no instances currently available", and the
/// older "There are no longer any instances available with the requested
/// specifications". Only this refusal is one another region can fix; a
/// credit or request error would fail anywhere, and must never move the
/// backup.
/// True when RunPod's REST API refuses the pod request for its region:
/// dataCenterIds names a region outside the API's list. GraphQL's
/// dataCenters may still list that region as supporting network volumes
/// (US-NE-1 on 2026-07-29, where a volume was made and the pod refused).
pub fn is_region_rejected(error: &RunPodError) -> bool {
    error.status_code() == Some(400) && error.to_string().contains("dataCenterIds")
}

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

/// The wall clock as rp's files hold it: seconds since the epoch.
pub(crate) fn now(time: &dyn TimeProvider) -> f64 {
    time.now().timestamp_micros() as f64 / 1e6
}

/// A time in rp's form, `2026-07-16T09:30:00+00:00`.
fn iso(at: f64) -> String {
    DateTime::from_timestamp(at.floor() as i64, 0)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string()
}

/// A config number as rp reads one, `float(value or 0)`; 0 when it is
/// not one.
fn number(cfg: &Config, key: &str) -> f64 {
    cfg.get(key)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map_or(Some(0.0), |v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// The first line of an error, for a one-line notice.
pub(crate) fn first_line(error: &RunPodError) -> String {
    error
        .to_string()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

fn text<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key).and_then(Value::as_str)
}

/// The volume's id, `""` for none.
pub(crate) fn id(volume: &Value) -> &str {
    text(volume, "id").unwrap_or_default()
}

/// Appends a line to a log. Best effort: a log that cannot be written
/// changes nothing.
fn append(paths: &Paths, file: &std::path::Path, row: &Value) {
    let write = || -> io::Result<()> {
        fs::create_dir_all(paths.pod_state_dir())?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)?
            .write_all(format!("{row}\n").as_bytes())
    };
    let _ = write();
}

/// A log's lines that are JSON; none when it cannot be read.
fn rows(file: &std::path::Path) -> Vec<Value> {
    fs::read_to_string(file)
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// One line in `up_failures.jsonl` per refused pod creation, secrets
/// masked.
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
        "at": iso(now(time)),
        "region": region,
        "role": role,
        "volume_id": volume_id,
        "pod_name": cfg.get("POD_NAME"),
        "gpu_types": gpu_type_list(cfg),
        "status_code": error.status_code(),
        "capacity": is_capacity_error(error),
        "error": text,
    });
    append(paths, &paths.up_failures_file(), &row);
}

// ------------------------------------------------------------ backup volume

pub fn backup_name(cfg: &Config) -> String {
    format!("{}-backup", cfg.get("NETWORK_VOLUME_NAME").unwrap_or(""))
}

/// `backups.json`: volume id to its name, region and last use.
fn load_backups(paths: &Paths) -> Map<String, Value> {
    match fs::read_to_string(paths.backups_file())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
    {
        Some(Value::Object(data)) => data,
        _ => Map::new(),
    }
}

/// Replaces `backups.json` whole, as rp does: a temporary file renamed over it.
fn save_backups(paths: &Paths, data: &Map<String, Value>) -> io::Result<()> {
    fs::create_dir_all(paths.pod_state_dir())?;
    let file = paths.backups_file();
    let tmp = file.with_file_name(format!(".backups.json.{}", std::process::id()));
    let text = serde_json::to_string_pretty(data).map_err(io::Error::other)?;
    fs::write(&tmp, text + "\n")?;
    fs::rename(&tmp, &file)
}

fn last_used(record: Option<&Value>) -> Option<f64> {
    record
        .and_then(|r| r.get("last_used"))
        .and_then(Value::as_f64)
}

/// Records that a pod is using this backup volume, which restarts its
/// BACKUP_IDLE_DAYS countdown.
pub fn touch(paths: &Paths, volume: &Value, now: f64) -> io::Result<()> {
    let mut data = load_backups(paths);
    data.insert(
        id(volume).to_string(),
        json!({"name": volume.get("name"), "region": volume.get("dataCenterId"), "last_used": now}),
    );
    save_backups(paths, &data)
}

pub fn forget(paths: &Paths, volume_id: &str) -> io::Result<()> {
    let mut data = load_backups(paths);
    if data.shift_remove(volume_id).is_some() {
        save_backups(paths, &data)?;
    }
    Ok(())
}

/// Called with a live pod: if it runs on a backup volume, that backup is
/// in use.
pub fn note_pod(paths: &Paths, pod: &Value, now: f64) -> io::Result<()> {
    let mut data = load_backups(paths);
    let Some(record) = pod_volume_id(pod).and_then(|vid| data.get_mut(vid)) else {
        return Ok(());
    };
    if let Some(record) = record.as_object_mut() {
        record.insert("last_used".into(), json!(now));
    }
    save_backups(paths, &data)
}

/// Marks the new backup `volume_id` as replacing `old_id`, which goes once
/// `viy up` serves on the new one ([`retire_replaced`]).
pub fn mark_replaces(paths: &Paths, volume_id: &str, old_id: &str) -> io::Result<()> {
    let mut data = load_backups(paths);
    if let Some(record) = data.get_mut(volume_id).and_then(Value::as_object_mut) {
        record.insert("replaces".into(), json!(old_id));
        save_backups(paths, &data)?;
    }
    Ok(())
}

/// Until a pod uses it, a new volume is recorded as unused, so that
/// `viy reap` deletes it if `viy up` dies before it can.
pub fn record_unused(paths: &Paths, volume: &Value, now: f64) -> io::Result<()> {
    let mut data = load_backups(paths);
    data.insert(
        id(volume).to_string(),
        json!({"name": volume.get("name"), "region": volume.get("dataCenterId"),
               "last_used": now, "unused": true}),
    );
    save_backups(paths, &data)
}

/// This model's backup volume, found on RunPod by name; the local record
/// only says when it was last used. `None` when there is none.
pub async fn find_backup(
    rp: &RunPod,
    paths: &Paths,
    cfg: &Config,
    now: f64,
) -> Result<Option<Value>, CommandError> {
    let known = load_backups(paths);
    let name = backup_name(cfg);
    // A volume made for a pod that no region took is not a backup.
    let mut volumes: Vec<Value> = rp
        .list_volumes()
        .await?
        .into_iter()
        .filter(|v| {
            text(v, "name") == Some(name.as_str())
                && !known
                    .get(id(v))
                    .and_then(|r| r.get("unused"))
                    .is_some_and(truthy)
        })
        .collect();
    // Two only while an old backup's deletion is pending: the newest.
    let rank = |v: &Value| {
        let record = known.get(id(v));
        (record.is_some(), last_used(record).unwrap_or(0.0))
    };
    volumes.sort_by(|a, b| {
        let ((ka, ua), (kb, ub)) = (rank(a), rank(b));
        kb.cmp(&ka).then(ub.total_cmp(&ua))
    });
    let Some(backup) = volumes.into_iter().next() else {
        return Ok(None);
    };
    if !known.contains_key(id(&backup)) {
        // No record of its last use: the countdown starts now.
        touch(paths, &backup, now)?;
    }
    Ok(Some(backup))
}

// -------------------------------------------------------------------- stock

/// Per region, the first of our GPU types RunPod shows free there, or
/// `None`.
pub type Snapshot = IndexMap<String, Option<String>>;

/// Our GPU types, in GPU_TYPE_IDS order: the cost policy, cheapest first
/// (rpctl's `stock_types`). Until rpctl 03ec13a only the A100s among them
/// counted, so a region with only an RTX PRO 6000 or an H100 free was
/// never offered.
pub fn stock_types(cfg: &Config) -> Vec<String> {
    gpu_type_list(cfg).into_iter().map(str::to_string).collect()
}

fn version(v: &str) -> Vec<u64> {
    v.split('.')
        .map(|x| x.trim().parse().unwrap_or(0))
        .collect()
}

/// The oldest host CUDA version CUDA_VERSIONS allows, or `None` for any.
pub fn cuda_floor(cfg: &Config) -> Option<String> {
    cfg.get("CUDA_VERSIONS")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .min_by_key(|v| version(v))
        .map(str::to_string)
}

fn secure(cfg: &Config) -> bool {
    let cloud = cfg.get("CLOUD_TYPE").filter(|c| !c.is_empty());
    cloud.unwrap_or("SECURE").to_uppercase() != "COMMUNITY"
}

/// The first of our GPU types RunPod shows free, for every listed region
/// that supports network volumes; each snapshot kept in `stock.jsonl`.
///
/// dataCenters.gpuAvailability ignores the host's CUDA version: on
/// 2026-07-08 it listed A100s free in four regions that all refused the
/// pod, their free hosts being on 13.2 drivers or older than 13.0. With
/// CUDA_VERSIONS set, a second query asks for hosts at or above its floor.
pub async fn stock_snapshot(
    rp: &RunPod,
    paths: &Paths,
    time: &dyn TimeProvider,
    cfg: &Config,
) -> Result<Snapshot, RunPodError> {
    let wanted = stock_types(cfg);
    let mut snap = Snapshot::new();
    for dc in rp.data_centers().await? {
        let usable =
            dc.get("storageSupport").is_some_and(truthy) && dc.get("listed").is_some_and(truthy);
        let Some(region) = text(&dc, "id").filter(|_| usable) else {
            continue;
        };
        let free: Vec<&str> = dc
            .get("gpuAvailability")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|g| g.get("available").is_some_and(truthy))
            .filter_map(|g| text(g, "gpuTypeId"))
            .collect();
        let first = wanted.iter().find(|w| free.contains(&w.as_str())).cloned();
        snap.insert(region.to_string(), first);
    }
    if let Some(floor) = cuda_floor(cfg)
        && !snap.is_empty()
        && !wanted.is_empty()
    {
        let regions: Vec<String> = snap.keys().cloned().collect();
        let stock = rp
            .cuda_stock(&regions, &wanted, &floor, &[], secure(cfg))
            .await?;
        for (region, free) in &mut snap {
            *free = wanted
                .iter()
                .find(|g| {
                    stock
                        .get(&(region.clone(), (*g).clone()))
                        .and_then(|s| s.fit.as_deref())
                        .is_some_and(|s| !s.is_empty())
                })
                .cloned();
        }
    }
    append(
        paths,
        &paths.stock_file(),
        &json!({"at": iso(now(time)), "free": Map::from_iter(
            snap.iter().map(|(region, free)| (region.clone(), json!(free)))
        )}),
    );
    Ok(snap)
}

/// True when RunPod shows one of our GPU types free in `region` on a
/// driver at or above the CUDA floor, and none free on an older one. Only
/// then may `viy up` drop allowedCudaVersions, which REST matches exactly
/// and caps at 13.0, so the pod can land on a 13.1 or 13.2 host. One
/// GraphQL request; any doubt (no floor, an error) is a no.
pub async fn any_cuda_is_safe(rp: &RunPod, cfg: &Config, region: &str) -> bool {
    let Some(floor) = cuda_floor(cfg) else {
        return false;
    };
    let older: Vec<String> = REST_CUDA
        .iter()
        .filter(|v| version(v) < version(&floor))
        .map(|v| (*v).to_string())
        .collect();
    let gpus: Vec<String> = gpu_type_list(cfg).into_iter().map(str::to_string).collect();
    let Ok(stock) = rp
        .cuda_stock(&[region.to_string()], &gpus, &floor, &older, secure(cfg))
        .await
    else {
        return false;
    };
    let free = |s: &Option<String>| s.as_deref().is_some_and(|s| !s.is_empty());
    let fit = stock.values().any(|s| free(&s.fit));
    let old = stock.values().any(|s| free(&s.old));
    fit && !old
}

/// Share of the stored samples in which each region had one of our GPU
/// types free (an A100, in rows written before rpctl 03ec13a), over the
/// `days` before `now`.
pub fn history(paths: &Paths, now: f64, days: u32) -> HashMap<String, f64> {
    let cutoff = iso(now - f64::from(days) * DAY_S);
    let mut seen: HashMap<String, (u32, u32)> = HashMap::new();
    for row in rows(&paths.stock_file()) {
        if text(&row, "at").unwrap_or("") < cutoff.as_str() {
            continue;
        }
        // rp's `row.get("free") or row.get("a100_free")`: an empty map
        // falls through too.
        let snap = ["free", "a100_free"]
            .iter()
            .filter_map(|k| row.get(*k).and_then(Value::as_object))
            .find(|m| !m.is_empty());
        for (region, free) in snap.into_iter().flatten() {
            let counts = seen.entry(region.clone()).or_default();
            counts.0 += 1;
            counts.1 += u32::from(truthy(free));
        }
    }
    seen.into_iter()
        .map(|(region, (n, free))| (region, f64::from(free) / f64::from(n)))
        .collect()
}

/// Regions to move the backup to, best first. Only a region that showed one
/// of our GPU types free in these samples qualifies, ordered by the
/// cheapest type it showed (its place in `types`, GPU_TYPE_IDS), then by
/// how often it showed one; the past only breaks ties, then the name.
pub fn rank(
    samples: &[Snapshot],
    exclude: &HashSet<&str>,
    past: &HashMap<String, f64>,
    types: &[String],
) -> Vec<String> {
    let mut counts: IndexMap<&str, (u32, usize)> = IndexMap::new();
    for snap in samples {
        for (region, free) in snap {
            let entry = counts.entry(region).or_insert((0, usize::MAX));
            if let Some(gpu) = free {
                entry.0 += 1;
                entry.1 = entry.1.min(place(gpu, types));
            }
        }
    }
    let mut live: Vec<(&str, (u32, usize))> = counts
        .into_iter()
        .filter(|(region, (n, _))| *n > 0 && !exclude.contains(region))
        .collect();
    let past = |r: &str| past.get(r).copied().unwrap_or(0.0);
    live.sort_by(|(ra, (na, ca)), (rb, (nb, cb))| {
        ca.cmp(cb)
            .then(nb.cmp(na))
            .then(past(rb).total_cmp(&past(ra)))
            .then(ra.cmp(rb))
    });
    live.into_iter().map(|(r, _)| r.to_string()).collect()
}

/// `gpu`'s place in `types`; one past the end when it is not there.
fn place(gpu: &str, types: &[String]) -> usize {
    types.iter().position(|t| t == gpu).unwrap_or(types.len())
}

/// The cheapest of our GPU types `region` showed free in `samples`
/// (rpctl's `cheapest_seen`).
pub fn cheapest_seen<'a>(
    samples: &'a [Snapshot],
    region: &str,
    types: &[String],
) -> Option<&'a str> {
    samples
        .iter()
        .filter_map(|s| s.get(region).and_then(Option::as_deref))
        .min_by_key(|g| place(g, types))
}

/// How many of `samples` showed one of our types free in `region`.
pub fn free_in(samples: &[Snapshot], region: &str) -> usize {
    samples
        .iter()
        .filter(|s| s.get(region).is_some_and(Option::is_some))
        .count()
}

/// `n` stock snapshots [`SAMPLE_GAP`] apart: stock flickers.
pub async fn sample_stock(
    rp: &RunPod,
    paths: &Paths,
    time: &dyn TimeProvider,
    cfg: &Config,
    n: usize,
    gap: Duration,
) -> Result<Vec<Snapshot>, RunPodError> {
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        if i > 0 {
            tokio::time::sleep(gap).await;
        }
        samples.push(stock_snapshot(rp, paths, time, cfg).await?);
    }
    Ok(samples)
}

/// FAILOVER_RETRY_MIN: how long refused creations are retried; 0 when it
/// is not a number.
pub fn grab_minutes(cfg: &Config) -> f64 {
    number(cfg, "FAILOVER_RETRY_MIN").max(0.0)
}

/// BACKUP_IDLE_DAYS: 0 keeps backups, and so does a value that is not a
/// number.
pub fn idle_days(cfg: &Config) -> f64 {
    number(cfg, "BACKUP_IDLE_DAYS")
}

/// A number in a notice, as rp shows one with `:g`: `1`, `0.5`, `15`.
pub(crate) fn short(x: f64) -> String {
    format!("{x}")
}

/// Once vLLM serves on a new backup volume, deletes the backup it
/// replaced, and says so. No API call unless one is pending. The old
/// volume, if RunPod still had it.
pub async fn retire_replaced(
    rp: &RunPod,
    paths: &Paths,
    cfg: &Config,
    volume_id: &str,
    say: &mut (dyn FnMut(String) + Send),
) -> io::Result<Option<Value>> {
    let data = load_backups(paths);
    let Some(old) = data
        .get(volume_id)
        .and_then(|r| r.get("replaces"))
        .filter(|old| truthy(old))
        .map(|old| shown(Some(old)))
    else {
        return Ok(None);
    };
    let volume = match rp.get_volume(&old).await {
        Ok(volume) => volume,
        Err(error) if error.status_code() == Some(404) => None,
        Err(error) => {
            say(format!(
                "Could not check the old backup volume {old} ({}); the next viy up on this \
                 backup retries.",
                first_line(&error)
            ));
            return Ok(None);
        }
    };
    if let Some(volume) = volume
        .as_ref()
        .filter(|v| text(v, "name") == Some(backup_name(cfg).as_str()))
    {
        if let Err(error) = rp.delete_volume(&old).await
            && error.status_code() != Some(404)
        {
            say(format!(
                "Could not delete the old backup volume {old} ({}); the next viy up on this \
                 backup retries.",
                first_line(&error)
            ));
            return Ok(None);
        }
        say(format!(
            "Deleted the old backup volume {old} in {}.",
            shown(volume.get("dataCenterId"))
        ));
    }
    forget(paths, &old)?;
    let mut data = load_backups(paths);
    if let Some(record) = data.get_mut(volume_id).and_then(Value::as_object_mut) {
        record.shift_remove("replaces");
        save_backups(paths, &data)?;
    }
    Ok(volume)
}

/// Deletes a volume made for a pod that was then refused, which never held
/// anything, and its record; one RunPod would not delete stays recorded
/// as unused for `viy reap`.
pub(crate) async fn delete_empty(
    rp: &RunPod,
    paths: &Paths,
    volume: &Value,
    report: Reporter<'_>,
) -> io::Result<()> {
    if let Err(error) = rp.delete_volume(id(volume)).await
        && error.status_code() != Some(404)
    {
        say(
            report,
            Tone::Warn,
            format!(
                "  could not delete the unused volume {} in {}: {}; viy reap retries",
                id(volume),
                shown(volume.get("dataCenterId")),
                first_line(&error)
            ),
        );
        return Ok(());
    }
    forget(paths, id(volume))
}

// ------------------------------------------------------------------- expiry

/// Deletes backup volumes no pod has used for BACKUP_IDLE_DAYS (0 keeps
/// them), and after [`UNUSED_GRACE_S`] any volume `viy up` made for a pod
/// that no region took but could not delete itself. Covers every model's
/// backups, not only the active profile's. Calls RunPod only when one is
/// due, so `viy reap`'s 5-minute timer stays free. The volumes deleted.
pub async fn expire(
    rp: &RunPod,
    paths: &Paths,
    cfg: &Config,
    now: f64,
    say: &mut (dyn FnMut(String) + Send),
) -> Result<Vec<Value>, CommandError> {
    let days = idle_days(cfg);
    let data = load_backups(paths);
    let idle_for = |record: &Value| now - last_used(Some(record)).unwrap_or(now);
    let due: Vec<(&String, &Value)> = data
        .iter()
        .filter(|(_, r)| {
            (r.get("unused").is_some_and(truthy) && idle_for(r) >= UNUSED_GRACE_S)
                || (days != 0.0 && idle_for(r) >= days * DAY_S)
        })
        .collect();
    if due.is_empty() {
        return Ok(Vec::new());
    }
    let attached: HashSet<String> = rp
        .list_pods()
        .await?
        .iter()
        .filter(|p| text(p, "desiredStatus") != Some("TERMINATED"))
        .filter_map(|p| pod_volume_id(p).map(str::to_string))
        .collect();
    let mut deleted = Vec::new();
    for (vid, record) in due {
        if attached.contains(vid.as_str()) {
            note_pod(paths, &json!({"networkVolumeId": vid}), now)?;
            continue;
        }
        let volume = match rp.get_volume(vid).await {
            Ok(volume) => volume,
            Err(error) => {
                if error.status_code() == Some(404) {
                    forget(paths, vid)?;
                }
                continue;
            }
        };
        // Only ever a volume this module made: never the primary, whatever
        // the record says.
        let Some(volume) = volume.filter(|v| {
            let name = v.get("name");
            shown(name).ends_with("-backup") && name == record.get("name")
        }) else {
            forget(paths, vid)?;
            continue;
        };
        if let Err(error) = rp.delete_volume(vid).await
            && error.status_code() != Some(404)
        {
            say(format!(
                "could not delete idle backup volume {vid}: {}",
                first_line(&error)
            ));
            continue;
        }
        forget(paths, vid)?;
        let (name, region) = (shown(volume.get("name")), shown(volume.get("dataCenterId")));
        say(if record.get("unused").is_some_and(truthy) {
            format!("deleted volume {vid} ({name}, {region}): made for a pod no region took")
        } else {
            format!(
                "deleted backup volume {vid} ({name}, {region}): unused for {:.0} days",
                idle_for(record) / DAY_S
            )
        });
        deleted.push(volume);
    }
    Ok(deleted)
}

// ------------------------------------------------------------------ command

fn ago(at: f64, now: f64) -> String {
    let days = (now - at) / DAY_S;
    if days < 1.0 {
        "today".to_string()
    } else {
        format!("{days:.0} days ago")
    }
}

/// `viy backup`: this model's regions, the backup's idle countdown, and
/// RunPod's stock of our GPU types now, `samples` queries 5 s apart.
pub async fn backup(
    env: &Env<'_>,
    cfg: &Config,
    samples: u32,
    report: Reporter<'_>,
) -> Result<(), CommandError> {
    let rp = crate::commands::connect(env, cfg)?;
    let print = |line: String| say(report, Tone::Plain, line);
    let now = now(env.time);
    let primary = cfg.get("DATA_CENTER_ID").unwrap_or("");
    let volume_id = cfg.get("NETWORK_VOLUME_ID").filter(|v| !v.is_empty());
    print(format!(
        "  primary:   {primary}  volume {} ({})",
        volume_id.unwrap_or("?"),
        cfg.get("NETWORK_VOLUME_NAME").unwrap_or("")
    ));
    let backup = find_backup(&rp, env.paths, cfg, now).await?;
    let backup_region = backup.as_ref().map(|b| shown(b.get("dataCenterId")));
    if let Some(backup) = &backup {
        let record = load_backups(env.paths).get(id(backup)).cloned();
        let mut line = format!(
            "  backup:    {}  volume {} ({})",
            shown(backup.get("dataCenterId")),
            id(backup),
            shown(backup.get("name"))
        );
        if let Some(used) = last_used(record.as_ref()).filter(|u| *u != 0.0) {
            line += &format!(", last used {}", ago(used, now));
            let days = idle_days(cfg);
            if days != 0.0 {
                line += &format!(
                    "; deleted after {} unless used",
                    &iso(used + days * DAY_S)[..10]
                );
            }
        }
        print(line);
    } else {
        print(format!(
            "  backup:    none; viy up makes one when {primary} has no GPU free"
        ));
    }
    if !failover_enabled(cfg) {
        print("  failover:  off (FAILOVER=0)".to_string());
    }

    let past = history(env.paths, now, HISTORY_DAYS);
    let n = samples.max(1) as usize;
    let taken = sample_stock(&rp, env.paths, env.time, cfg, n, Duration::from_secs(5)).await?;
    let mut exclude = HashSet::from([primary]);
    exclude.extend(backup_region.as_deref());
    let types = stock_types(cfg);
    let best = rank(&taken, &exclude, &past, &types);
    print(format!(
        "\n  Our GPU types free, regions with network volumes ({n} sample{}; past {HISTORY_DAYS} \
         days in brackets; the cheapest type seen):",
        if n > 1 { "s" } else { "" }
    ));
    let mut regions: Vec<&String> = taken.last().map(|s| s.keys().collect()).unwrap_or_default();
    regions.sort();
    for region in regions {
        let free = free_in(&taken, region);
        let tag = if region == primary {
            " primary"
        } else if backup_region.as_ref() == Some(region) {
            " backup"
        } else if best.first() == Some(region) {
            " <- next backup"
        } else {
            ""
        };
        let old = past
            .get(region.as_str())
            .map_or("[-]".to_string(), |p| format!("[{:.0}%]", p * 100.0));
        let gpu = cheapest_seen(&taken, region, &types)
            .map(|g| format!("  {g}"))
            .unwrap_or_default();
        print(format!("    {region:<10} {free}/{n}  {old:>6}{tag}{gpu}"));
    }

    let month = iso(now - f64::from(HISTORY_DAYS) * DAY_S);
    let refused: Vec<Value> = rows(&env.paths.up_failures_file())
        .into_iter()
        .filter(|r| text(r, "at").unwrap_or("") >= month.as_str())
        .collect();
    if !refused.is_empty() {
        let mut by: IndexMap<String, usize> = IndexMap::new();
        for row in refused
            .iter()
            .filter(|r| r.get("capacity").is_some_and(truthy))
        {
            *by.entry(shown(row.get("region"))).or_default() += 1;
        }
        // Counter.most_common: by count, first seen first among equals.
        by.sort_by(|_, a, _, b| b.cmp(a));
        let regions: Vec<String> = by.iter().map(|(r, n)| format!("{r} {n}")).collect();
        let regions = if regions.is_empty() {
            "none for capacity".to_string()
        } else {
            regions.join(", ")
        };
        print(format!(
            "\n  pod creations refused in {HISTORY_DAYS} days: {} ({regions})  {}",
            refused.len(),
            env.paths.up_failures_file().display()
        ));
    }
    Ok(())
}
