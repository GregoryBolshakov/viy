//! The per-boot timing report, checked against the boot budget: rpctl's
//! `bootreport.py`. Every fresh `viy up` ends with a stage-by-stage
//! breakdown read from the pod's platform log, appends it to `boots.jsonl`
//! and leaves a record of the boot in the boots directory, shared with rp
//! (kb d006): `<boot_id>.json` (stages, facts, `up`'s own timeline, the
//! boot-relevant config, the file list), and the logs a detached process
//! collects afterwards, `<boot_id>.platform.log` and `<boot_id>.vllm.log`
//! (`crate::wait::capture_boot`). Secrets are masked before anything is
//! written, and recording never adds boot time.

use std::fs;
use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::sync::LazyLock;

use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::Utc;
use regex::Regex;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_config::Config;
use viy_config::Paths;
use viy_time::TimeProvider;

use crate::api::LogEntry;
use crate::api::shown;
use crate::compile_regex;

/// Pod creation to a local /health 200, image pull included, in seconds. A
/// hard limit (kb d004).
pub const BUDGET_S: u64 = 180;

/// How many boots keep their record and logs; `boots.jsonl` keeps them all.
pub const KEEP_RECORDS: usize = 50;

/// Config keys that shape a boot, copied into each record. No secrets.
pub const BOOT_KEYS: [&str; 25] = [
    "VLLM_IMAGE",
    "MODEL_REPO",
    "MAX_MODEL_LEN",
    "GPU_MEMORY_UTILIZATION",
    "MAX_NUM_SEQS",
    "MAX_IMAGES_PER_PROMPT",
    "KV_CACHE_DTYPE",
    "MTP_SPECULATIVE_TOKENS",
    "SAFETENSORS_LOAD_STRATEGY",
    "WEIGHTS_PREWARM",
    "MM_WARMUP",
    "PREFIX_CACHING",
    "STARTUP_CACHE",
    "MODEL_CACHE_MODE",
    "GPU_TYPE_IDS",
    "GPU_TYPE_PRIORITY",
    "DATA_CENTER_ID",
    "CLOUD_TYPE",
    "TUNNEL",
    "SEARCH_ENABLED",
    "TOOLS_ENABLED",
    "TOOL_CALL_PARSER",
    "CUDA_VERSIONS",
    "IDLE_TTL_MIN",
    "MAX_POD_HOURS",
];

/// (key, label, the log text that ends the stage). Each stage runs from the
/// end of the previous one; a missing marker folds its time into the next
/// stage, so a vLLM that rewords a line shows as one stage absorbing
/// another.
pub const STAGES: [(&str, &str, &str); 9] = [
    ("pull", "GPU allocation + image pull", "=== boot "),
    (
        "prepare",
        "boot script (caches, sshd)",
        "RPCTL_PHASE starting vLLM",
    ),
    ("imports", "vLLM CLI imports", "non-default args"),
    ("api_init", "config, processor, engine spawn", "world_size="),
    (
        "engine_init",
        "engine: CUDA + processor init",
        "Starting to load model",
    ),
    ("weights", "weights to GPU", "Model loading took"),
    (
        "compile",
        "encoder profiling + compile",
        "torch.compile took",
    ),
    ("engine", "KV cache + CUDA graphs", "init engine (profile"),
    ("api", "API server warmup", "HTTP server started"),
];

/// The last stage, measured on this machine: up to the local 200.
pub const LOCAL_STAGE: (&str, &str) = ("endpoint", "SSH tunnel / local endpoint");

/// What to check when a stage is the biggest share of an overrun.
fn hint(key: &str) -> &'static str {
    match key {
        "pull" => {
            "the host had to download the image; keep VLLM_IMAGE on the stock Docker Hub \
             vllm/vllm-openai image"
        }
        "prepare" => "something in the boot script ran in the foreground before vLLM",
        "imports" => "vLLM's Python imports; check for a new image or new vLLM arguments",
        "api_init" => "API-server config/processor load; check for new vLLM arguments",
        "engine_init" => "engine CUDA and processor init",
        "weights" => "network-volume read; check the 'weights prewarmed' line in viy logs",
        "compile" => "compile cache missed; the first boot on a new GPU type compiles once",
        "engine" => "KV-cache profiling or CUDA graph capture (warmup after a compile miss)",
        "api" => "API-server warmup; check MM_WARMUP is 0",
        "endpoint" => "sshd or the tunnel came up late",
        _ => "",
    }
}

/// One stage of a boot and how long it took.
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    pub key: &'static str,
    pub label: &'static str,
    pub seconds: f64,
}

/// vLLM's own timings and the GPU, read from the log; the last of each.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Details {
    pub weights_read_s: Option<f64>,
    pub compile_s: Option<f64>,
    pub prewarm_s: Option<f64>,
    pub gpu: Option<String>,
}

impl Details {
    fn insert_into(&self, row: &mut Map<String, Value>) {
        for (key, value) in [
            ("weights_read_s", self.weights_read_s),
            ("compile_s", self.compile_s),
            ("prewarm_s", self.prewarm_s),
        ] {
            if let Some(value) = value {
                row.insert(key.to_string(), json!(value));
            }
        }
        if let Some(gpu) = &self.gpu {
            row.insert("gpu".to_string(), json!(gpu));
        }
    }
}

/// How the boot ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ready,
    Failed,
    Interrupted,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

/// A platform-log timestamp: RFC 3339 in UTC, up to nanoseconds.
pub fn parse_ts(text: &str) -> Option<DateTime<Utc>> {
    static TS: LazyLock<Regex> = LazyLock::new(|| {
        compile_regex(r"^([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2})(?:\.([0-9]+))?Z?$")
    });
    let found = TS.captures(text)?;
    let whole = NaiveDateTime::parse_from_str(&found[1], "%Y-%m-%dT%H:%M:%S").ok()?;
    let digits = found.get(2).map_or("", |m| m.as_str());
    let nanos: u32 = format!("{:0<9}", &digits[..digits.len().min(9)])
        .parse()
        .ok()?;
    Some(whole.and_utc() + chrono::TimeDelta::nanoseconds(i64::from(nanos)))
}

/// The log entries from the most recent container boot on.
pub fn latest_boot(entries: &[LogEntry]) -> &[LogEntry] {
    let start = entries
        .iter()
        .rposition(|e| e.line.contains("=== boot "))
        .unwrap_or(0);
    &entries[start..]
}

/// The boot's stages, from pod creation (`created`) to the local 200
/// (`ready`), and vLLM's own timings.
pub fn measure(
    entries: &[LogEntry],
    created: DateTime<Utc>,
    ready: DateTime<Utc>,
) -> (Vec<Stage>, Details) {
    static NUMBERS: LazyLock<[Regex; 3]> = LazyLock::new(|| {
        [
            r"Loading weights took ([0-9.]+)",
            r"torch\.compile took ([0-9.]+)",
            r"weights prewarmed: [0-9.]+ GiB in ([0-9.]+)s",
        ]
        .map(compile_regex)
    });
    let mut marks: [Option<DateTime<Utc>>; STAGES.len()] = [None; STAGES.len()];
    let mut details = Details::default();
    for entry in entries {
        let Some(ts) = entry.ts.as_deref().and_then(parse_ts) else {
            continue;
        };
        let line = entry.line.as_str();
        for (mark, (_, _, marker)) in marks.iter_mut().zip(STAGES) {
            if mark.is_none() && line.contains(marker) {
                *mark = Some(ts);
            }
        }
        let slots = [
            &mut details.weights_read_s,
            &mut details.compile_s,
            &mut details.prewarm_s,
        ];
        for (pattern, slot) in NUMBERS.iter().zip(slots) {
            if let Some(n) = pattern.captures(line).and_then(|c| c[1].parse().ok()) {
                *slot = Some(n);
            }
        }
        if let Some((_, gpu)) = line.split_once("RPCTL_PHASE gpu ") {
            details.gpu = Some(gpu.trim().to_string());
        }
    }
    let mut stages = Vec::new();
    let mut previous = created;
    for (mark, (key, label, _)) in marks.into_iter().zip(STAGES) {
        if let Some(at) = mark
            && at >= previous
        {
            stages.push(Stage {
                key,
                label,
                seconds: (at - previous).as_seconds_f64(),
            });
            previous = at;
        }
    }
    stages.push(Stage {
        key: LOCAL_STAGE.0,
        label: LOCAL_STAGE.1,
        seconds: (ready - previous).as_seconds_f64().max(0.0),
    });
    (stages, details)
}

/// What the platform log says about what usually explains a slow boot:
/// was the image on the host, did the caches restore and hit, what the
/// pod's idle watchdog reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// `downloaded` or `already on host`.
    pub image: Option<&'static str>,
    pub layers_already_on_host: Option<u64>,
    pub layers_downloaded: Option<u64>,
    pub layer_retries: Option<u64>,
    pub model_metadata: Option<&'static str>,
    pub compile_archive: Option<String>,
    pub compile_cache: Option<&'static str>,
    /// `armed`, `off`, or `CANNOT STOP THIS POD...`.
    pub watchdog: Option<String>,
    pub error: Option<String>,
}

impl Facts {
    pub fn to_json(&self) -> Map<String, Value> {
        let mut map = Map::new();
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                map.insert(key.to_string(), value);
            }
        };
        put("image", self.image.map(Value::from));
        put(
            "layers_already_on_host",
            self.layers_already_on_host.map(Value::from),
        );
        put("layers_downloaded", self.layers_downloaded.map(Value::from));
        put("layer_retries", self.layer_retries.map(Value::from));
        put("model_metadata", self.model_metadata.map(Value::from));
        put(
            "compile_archive",
            self.compile_archive.clone().map(Value::from),
        );
        put("compile_cache", self.compile_cache.map(Value::from));
        put("watchdog", self.watchdog.clone().map(Value::from));
        put("error", self.error.clone().map(Value::from));
        map
    }
}

/// The first `n` characters of `text`.
fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

pub fn facts(entries: &[LogEntry]) -> Facts {
    let mut found = Facts::default();
    let (mut on_host, mut downloaded, mut retries) = (0, 0, 0);
    for entry in entries {
        let line = entry.line.as_str();
        if line.contains("Already exists") {
            on_host += 1;
        } else if line.contains("Pull complete") {
            downloaded += 1;
        } else if line.contains("Retrying in 1 second") {
            // One per retry; the countdown repeats.
            retries += 1;
        }
        // RunPod pulls twice: "Downloaded newer image" and then, seconds
        // later, "Image is up to date". The download is the fact.
        if line.contains("Image is up to date") {
            found.image.get_or_insert("already on host");
        } else if line.contains("Downloaded newer image") {
            found.image = Some("downloaded");
        }
        if line.contains("RPCTL_PHASE verified model cache") {
            found.model_metadata = Some("offline (verified cache)");
        } else if line.contains("RPCTL_PHASE checking model cache with Hugging Face") {
            found.model_metadata = Some("online (Hugging Face check)");
        }
        if line.contains("RPCTL_PHASE compilation caches restored") {
            found.compile_archive = Some("restored".to_string());
        } else if line.contains("RPCTL_PHASE first compilation") {
            found.compile_archive = Some("none for this GPU".to_string());
        } else if line.contains("RPCTL_PHASE cache restore unavailable") {
            let why = line
                .split_once("compiling normally")
                .map_or(line, |(_, r)| r);
            found.compile_archive = Some(format!("restore failed: {}", why.trim()));
        }
        if line.contains("Directly load AOT compilation") {
            found.compile_cache = Some("hit");
        } else if line.contains("Compiling a graph for compile range") {
            found.compile_cache = Some("miss (compiled)");
        }
        if line.contains("RPCTL_PHASE watchdog armed") {
            found.watchdog = Some("armed".to_string());
        } else if let Some((_, rest)) = line.split_once("RPCTL_PHASE WATCHDOG CANNOT STOP THIS POD")
        {
            found.watchdog = Some(format!("CANNOT STOP THIS POD{}", head(rest, 160)));
        } else if line.contains("RPCTL_PHASE watchdog off") {
            found.watchdog = Some("off".to_string());
        }
        if let Some((_, rest)) = line.split_once("RPCTL_ERROR") {
            found.error = Some(head(rest.trim(), 200));
        }
    }
    if downloaded > 0 || on_host > 0 {
        found.layers_already_on_host = Some(on_host);
        found.layers_downloaded = Some(downloaded);
        found.layer_retries = Some(retries);
        found.image.get_or_insert(if downloaded > 0 {
            "downloaded"
        } else {
            "already on host"
        });
    } else if retries > 0 {
        found.layer_retries = Some(retries);
    }
    found
}

/// The breakdown as printed, and whether the boot went over `budget`.
pub fn render(
    stages: &[Stage],
    details: &Details,
    total: f64,
    budget: u64,
    outcome: Outcome,
) -> (String, bool) {
    let title = match outcome {
        Outcome::Ready => "Boot breakdown".to_string(),
        other => format!("Boot breakdown ({}, so far)", other.as_str().to_uppercase()),
    };
    let mut lines = vec![format!("{title} (budget {budget}s):")];
    for stage in stages {
        let extra = match (stage.key, details) {
            (
                "weights",
                Details {
                    weights_read_s: Some(read),
                    prewarm_s,
                    ..
                },
            ) => match prewarm_s {
                Some(prewarm) => format!("  (read {read:.0}s, prewarm {prewarm:.0}s)"),
                None => format!("  (read {read:.0}s)"),
            },
            (
                "compile",
                Details {
                    compile_s: Some(compile),
                    ..
                },
            ) => format!("  (torch.compile {compile:.0}s)"),
            _ => String::new(),
        };
        lines.push(format!(
            "  {:<34} {:6.0}s{extra}",
            stage.label, stage.seconds
        ));
    }
    lines.push(format!("  {:<34} {total:6.0}s", "total"));
    let over = total - budget as f64;
    if over > 0.0
        && let Some(largest) = stages
            .iter()
            .reduce(|best, s| if s.seconds > best.seconds { s } else { best })
    {
        lines.push(format!(
            "OVER BUDGET by {over:.0}s. Largest stage: {} ({:.0}s) -- {}.",
            largest.label,
            largest.seconds,
            hint(largest.key)
        ));
    }
    (lines.join("\n"), over > 0.0)
}

/// `x` to one decimal, as the records keep seconds.
fn tenths(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// `up`'s own milestones (pod requested, SSH reachable, ...), kept in the
/// boot's record.
#[derive(Debug)]
pub struct Timeline<'a> {
    time: &'a dyn TimeProvider,
    marks: Vec<(DateTime<Utc>, Map<String, Value>)>,
}

impl<'a> Timeline<'a> {
    /// Begins with "viy up started".
    pub fn start(time: &'a dyn TimeProvider) -> Self {
        let mut timeline = Self {
            time,
            marks: Vec::new(),
        };
        timeline.mark("viy up started", []);
        timeline
    }

    /// Notes `event`, with what else is worth keeping about it.
    pub fn mark<'k>(&mut self, event: &str, info: impl IntoIterator<Item = (&'k str, Value)>) {
        let mut entry = Map::new();
        entry.insert("event".to_string(), json!(event));
        entry.extend(info.into_iter().map(|(k, v)| (k.to_string(), v)));
        self.marks.push((self.time.now(), entry));
    }

    /// Each milestone with `t`, its seconds from the start to 0.01.
    pub fn rows(&self) -> Vec<Value> {
        let Some((start, _)) = self.marks.first() else {
            return Vec::new();
        };
        self.marks
            .iter()
            .map(|(at, entry)| {
                let t = ((*at - *start).as_seconds_f64() * 100.0).round() / 100.0;
                let mut row = Map::new();
                row.insert("t".to_string(), json!(t));
                row.extend(entry.clone());
                Value::Object(row)
            })
            .collect()
    }
}

/// A finished (or failed) boot, measured.
#[derive(Debug, Clone, Copy)]
pub struct Boot<'a> {
    pub pod_id: &'a str,
    pub stages: &'a [Stage],
    pub details: &'a Details,
    pub total: f64,
    pub outcome: Outcome,
    pub facts: &'a Facts,
}

/// The boot history and records: `boots.jsonl` and the boots directory.
#[derive(Debug, Clone, Copy)]
pub struct Records<'a> {
    paths: &'a Paths,
    keep: usize,
}

impl<'a> Records<'a> {
    pub fn new(paths: &'a Paths) -> Self {
        Self::keeping(paths, KEEP_RECORDS)
    }

    /// Keeping the files of only the newest `keep` boots.
    pub fn keeping(paths: &'a Paths, keep: usize) -> Self {
        Self { paths, keep }
    }

    /// `<boots dir>/<boot_id>.<suffix>`.
    pub fn file(&self, boot_id: &str, suffix: &str) -> PathBuf {
        self.paths.boots_dir().join(format!("{boot_id}.{suffix}"))
    }

    pub fn record_path(&self, boot_id: &str) -> PathBuf {
        self.file(boot_id, "json")
    }

    /// Appends the boot to the history and writes its record. Returns the
    /// history row; its `boot_id` names the record's files.
    pub fn record(
        &self,
        time: &dyn TimeProvider,
        cfg: &Config,
        boot: Boot<'_>,
        timeline: &Timeline<'_>,
    ) -> io::Result<Map<String, Value>> {
        let now = time.now();
        let boot_id = format!("{}-{}", now.format("%Y%m%dT%H%M%SZ"), boot.pod_id);
        let mut row = Map::new();
        row.insert(
            "at".to_string(),
            json!(now.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()),
        );
        row.insert("boot_id".to_string(), json!(boot_id));
        row.insert("pod_id".to_string(), json!(boot.pod_id));
        row.insert("outcome".to_string(), json!(boot.outcome.as_str()));
        row.insert("image".to_string(), json!(cfg.get("VLLM_IMAGE")));
        row.insert("total_s".to_string(), json!(tenths(boot.total)));
        row.insert("budget_s".to_string(), json!(BUDGET_S));
        let stages: Map<String, Value> = boot
            .stages
            .iter()
            .map(|s| (s.key.to_string(), json!(tenths(s.seconds))))
            .collect();
        row.insert("stages".to_string(), Value::Object(stages));
        boot.details.insert_into(&mut row);

        fs::create_dir_all(self.paths.pod_state_dir())?;
        let mut history = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.paths.boots_history_file())?;
        history.write_all(format!("{}\n", Value::Object(row.clone())).as_bytes())?;

        let mut full = row.clone();
        full.insert("facts".to_string(), Value::Object(boot.facts.to_json()));
        full.insert("timeline".to_string(), Value::Array(timeline.rows()));
        let config: Map<String, Value> = BOOT_KEYS
            .iter()
            .map(|k| (k.to_string(), json!(cfg.get(k))))
            .collect();
        full.insert("config".to_string(), Value::Object(config));
        full.insert("files".to_string(), json!({}));
        self.write(&boot_id, &Value::Object(full))?;
        self.prune();
        Ok(row)
    }

    /// Replaces a boot's record, whole.
    pub fn write(&self, boot_id: &str, data: &Value) -> io::Result<()> {
        fs::create_dir_all(self.paths.boots_dir())?;
        let temp = self.file(boot_id, "tmp");
        let text = serde_json::to_string_pretty(data).map_err(io::Error::other)?;
        fs::write(&temp, text + "\n")?;
        fs::rename(&temp, self.record_path(boot_id))
    }

    /// A boot's record; none if it is missing or unreadable.
    pub fn read(&self, boot_id: &str) -> Option<Value> {
        let text = fs::read_to_string(self.record_path(boot_id)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// The newest boots' files go; their history rows stay.
    fn prune(&self) {
        let Ok(dir) = fs::read_dir(self.paths.boots_dir()) else {
            return;
        };
        let names: Vec<String> = dir
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        let mut records: Vec<&str> = names
            .iter()
            .filter_map(|n| n.strip_suffix(".json"))
            .collect();
        records.sort_unstable();
        let old = records.len().saturating_sub(self.keep);
        for stem in &records[..old] {
            let prefix = format!("{stem}.");
            for name in names.iter().filter(|n| n.starts_with(&prefix)) {
                let _ = fs::remove_file(self.paths.boots_dir().join(name));
            }
        }
    }

    /// The last `limit` history rows; none if any line is unreadable.
    pub fn history(&self, limit: usize) -> Vec<Value> {
        let Ok(text) = fs::read_to_string(self.paths.boots_history_file()) else {
            return Vec::new();
        };
        let rows: Result<Vec<Value>, _> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str)
            .collect();
        let rows = rows.unwrap_or_default();
        rows[rows.len().saturating_sub(limit)..].to_vec()
    }
}

/// `viy boots`: the last boots, one row each with its stages.
pub fn history_table(records: &Records<'_>, limit: usize) -> String {
    let rows = records.history(limit);
    if rows.is_empty() {
        return format!(
            "no boots recorded yet ({})",
            records.paths.boots_history_file().display()
        );
    }
    let keys: Vec<&str> = STAGES
        .iter()
        .map(|(k, _, _)| *k)
        .chain([LOCAL_STAGE.0])
        .collect();
    let mut lines = vec![format!(
        "{:<20} {:>6}  {}  gpu",
        "when (UTC)",
        "total",
        keys.iter()
            .map(|k| format!("{:>9}", &k[..k.len().min(9)]))
            .collect::<Vec<_>>()
            .join(" ")
    )];
    for row in &rows {
        let total = row.get("total_s").and_then(Value::as_f64).unwrap_or(0.0);
        let budget = row
            .get("budget_s")
            .and_then(Value::as_f64)
            .unwrap_or(BUDGET_S as f64);
        let mark = if row
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("ready")
            != "ready"
        {
            "x"
        } else if total > budget {
            "!"
        } else {
            " "
        };
        let at: String = row
            .get("at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(19)
            .collect();
        let stages = row.get("stages");
        let cells: Vec<String> = keys
            .iter()
            .map(
                |k| match stages.and_then(|s| s.get(*k)).and_then(Value::as_f64) {
                    Some(seconds) => format!("{seconds:>9.0}"),
                    None => format!("{:>9}", "-"),
                },
            )
            .collect();
        lines.push(format!(
            "{at:<20} {total:>5.0}{mark}  {}  {}",
            cells.join(" "),
            row.get("gpu").map_or("?".to_string(), |g| shown(Some(g)))
        ));
    }
    lines.push(format!(
        "budget {BUDGET_S}s; ! = over budget, x = failed or interrupted. Details: viy boots show \
         [boot_id]"
    ));
    lines.join("\n")
}

/// `viy boots show`: one boot's record, the latest unless `boot_id`
/// (or a part of one) is given. Err is the message to exit with.
pub fn show(records: &Records<'_>, boot_id: Option<&str>) -> Result<String, String> {
    let ids: Vec<String> = records
        .history(200)
        .iter()
        .filter_map(|r| r.get("boot_id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let boot_id = match boot_id {
        None | Some("latest") => ids
            .last()
            .ok_or("no boot records yet; they start with the next viy up")?
            .clone(),
        Some(part) => ids
            .iter()
            .rfind(|id| id.contains(part))
            .ok_or_else(|| format!("no boot record matching {part}"))?
            .clone(),
    };
    let data = records.read(&boot_id).ok_or_else(|| {
        format!(
            "record {} is missing",
            records.record_path(&boot_id).display()
        )
    })?;
    let mut lines = vec![format!(
        "boot {boot_id}  outcome: {}  total: {}s (budget {}s)  gpu: {}",
        shown(data.get("outcome")),
        shown(data.get("total_s")),
        shown(data.get("budget_s")),
        data.get("gpu").map_or("?".to_string(), |g| shown(Some(g)))
    )];
    lines.push("stages:".to_string());
    for (key, seconds) in data
        .get("stages")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let label = STAGES
            .iter()
            .map(|(k, l, _)| (*k, *l))
            .chain([LOCAL_STAGE])
            .find(|(k, _)| k == key)
            .map_or(key.as_str(), |(_, l)| l);
        lines.push(format!(
            "  {label:<34} {:6.1}s",
            seconds.as_f64().unwrap_or(0.0)
        ));
    }
    for name in ["weights_read_s", "prewarm_s", "compile_s"] {
        if let Some(seconds) = data.get(name).and_then(Value::as_f64) {
            lines.push(format!("  {name:<34} {seconds:6.1}s"));
        }
    }
    lines.push("facts:".to_string());
    let mut facts = data
        .get("facts")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(host) = data.get("host").and_then(Value::as_object) {
        facts.extend(host.clone());
    }
    for (key, value) in &facts {
        lines.push(format!("  {key:<34} {}", shown(Some(value))));
    }
    lines.push("viy up timeline (seconds from start):".to_string());
    for entry in data
        .get("timeline")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let t = entry.get("t").and_then(Value::as_f64).unwrap_or(0.0);
        let mut extra = entry.as_object().cloned().unwrap_or_default();
        extra.shift_remove("t");
        let event = extra.shift_remove("event");
        let extra = if extra.is_empty() {
            String::new()
        } else {
            format!("  {}", Value::Object(extra))
        };
        lines.push(format!("  {t:7.1}  {}{extra}", shown(event.as_ref())));
    }
    lines.push("files:".to_string());
    lines.push(format!(
        "  record        {}",
        records.record_path(&boot_id).display()
    ));
    let files = data.get("files").and_then(Value::as_object);
    for (name, path) in files.into_iter().flatten() {
        lines.push(format!("  {name:<13} {}", shown(Some(path))));
    }
    if files.is_none_or(Map::is_empty) {
        lines.push(format!(
            "  (logs are still being collected, or collection failed: see {})",
            records.file(&boot_id, "capture.err").display()
        ));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
#[path = "bootreport_tests.rs"]
pub(crate) mod tests;
