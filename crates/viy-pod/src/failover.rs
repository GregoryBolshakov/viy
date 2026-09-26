//! Creating the pod: rpctl pod.py's `_create_pod` with its `attempt` and
//! `_ask`, and regions.py's `failover` and `_grab`.
//!
//! The success path is the single create request it always was. When
//! DATA_CENTER_ID has no GPU free, the user is asked whether to try the
//! backup volume; when that region is full too, or there is no backup
//! yet, RunPod's A100 stock is sampled and the user asked once whether to
//! move the backup to the regions that showed one free. On yes each of
//! them gets a new volume and a pod request, refusals are retried for
//! FAILOVER_RETRY_MIN (the primary and the old backup too), and the first
//! region to take the pod wins. Volumes no pod used are deleted at once,
//! also on Ctrl-C; the old backup only once `viy up` serves on the new one
//! (`regions::retire_replaced`).
//!
//! Questions go to an [`Asker`]; with nobody to ask, the backup is tried
//! and nothing is made or deleted.

use std::collections::HashSet;
use std::pin::pin;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Value;
use serde_json::json;
use tokio::time::Instant;
use viy_config::Config;
use viy_config::config::gpu_type_list;
use viy_protocol::Answer;
use viy_protocol::Choices;
use viy_protocol::Question;

use crate::api::RunPod;
use crate::api::RunPodError;
use crate::boot::build_pod_body;
use crate::commands::CommandError;
use crate::commands::Env;
use crate::commands::Interrupt;
use crate::commands::alert;
use crate::notice::Asker;
use crate::notice::Reporter;
use crate::notice::Tone;
use crate::notice::say;
use crate::regions;
use crate::regions::RETRY_GAP_S;
use crate::regions::id;
use crate::regions::is_capacity_error;
use crate::regions::short;

/// The pod a region took.
#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub pod: Value,
    pub volume_id: String,
    /// The volume is new and empty: the weights must be downloaded.
    pub volume_is_new: bool,
    /// Where the pod runs: DATA_CENTER_ID, or the backup's region.
    pub region: String,
    /// Time spent on the user's answers and between retries for a free
    /// GPU: the user's or RunPod's, not the boot's.
    pub waited: Duration,
}

/// Why failover ended without a pod.
enum Stop {
    /// The user said no; nothing was created or deleted.
    Declined(String),
    /// No region took the pod.
    NoCapacity(String),
    /// A refusal no other region can fix, as RunPod said it.
    Refused(RunPodError),
    Command(CommandError),
}

impl From<CommandError> for Stop {
    fn from(error: CommandError) -> Self {
        match error {
            CommandError::RunPod(e) => Self::Refused(e),
            other => Self::Command(other),
        }
    }
}

impl From<std::io::Error> for Stop {
    fn from(error: std::io::Error) -> Self {
        Self::Command(error.into())
    }
}

/// What failover won: the pod, its region and volume, whether that volume
/// is new.
struct Won {
    pod: Value,
    region: String,
    volume_id: String,
    created: bool,
}

/// The user's side of a pod creation: what it says, who answers its
/// questions, and Ctrl-C.
#[derive(Clone, Copy)]
pub struct User<'a> {
    pub report: Reporter<'a>,
    pub ask: Asker<'a>,
    pub interrupt: &'a Interrupt,
    /// The Ctrl-C at which `up` ends at once; raised while new volumes
    /// exist, so that they are deleted first.
    pub abort_at: &'a AtomicU32,
}

/// One pod creation by `viy up`.
pub struct Creation<'a> {
    env: &'a Env<'a>,
    cfg: &'a Config,
    rp: &'a RunPod,
    vllm_key: &'a str,
    pubkey: Option<&'a str>,
    user: User<'a>,
    /// DATA_CENTER_ID.
    primary: String,
    /// The pod body, built once; each request sets its region and volume.
    body: Value,
    /// Whether a request was sent, which "Creating pod in" announces.
    tried: bool,
    waited: Duration,
}

impl<'a> Creation<'a> {
    pub fn new(
        env: &'a Env<'a>,
        cfg: &'a Config,
        rp: &'a RunPod,
        vllm_key: &'a str,
        pubkey: Option<&'a str>,
        user: User<'a>,
    ) -> Self {
        Self {
            env,
            cfg,
            rp,
            vllm_key,
            pubkey,
            user,
            primary: cfg.get("DATA_CENTER_ID").unwrap_or("").to_string(),
            body: Value::Null,
            tried: false,
            waited: Duration::ZERO,
        }
    }

    fn say(&self, tone: Tone, text: impl Into<String>) {
        say(self.user.report, tone, text);
    }

    /// Says a failed write of backups.json and goes on: once a pod exists,
    /// or while cleaning up, the record is not worth losing the pod over.
    /// What it missed, `viy reap` and the next `viy up` mend.
    fn keep(&self, written: std::io::Result<()>) {
        if let Err(error) = written {
            self.say(
                Tone::Dim,
                format!("(backups.json not updated: {:?})", error.kind()),
            );
        }
    }

    /// Creates the pod in DATA_CENTER_ID, and when that region has no GPU
    /// free offers the backup region. Ends with everything said when no
    /// region takes the pod.
    pub async fn run(
        mut self,
        volume_id: &str,
        volume_is_new: bool,
    ) -> Result<Created, CommandError> {
        let primary = self.primary.clone();
        // Only dataCenterIds and networkVolumeId differ between requests.
        self.body = build_pod_body(self.cfg, self.vllm_key, volume_id, self.pubkey)
            .map_err(|e| CommandError::Exit(e.to_string()))?;
        let refused = match self.attempt(&primary, volume_id, "primary", false).await {
            Ok(pod) => return Ok(self.created(pod, volume_id, volume_is_new, &primary)),
            Err(error) => error,
        };
        let (message, capacity) =
            if is_capacity_error(&refused) && regions::failover_enabled(self.cfg) {
                match self.failover(volume_id).await {
                    Ok(won) if won.region == primary => {
                        self.say(
                            Tone::Ok,
                            format!(
                                "  pod created in {} on the primary volume after all",
                                won.region
                            ),
                        );
                        return Ok(self.created(won.pod, &won.volume_id, volume_is_new, &primary));
                    }
                    Ok(won) => {
                        // The boot record shows where this boot ran; the next
                        // `viy up` starts from the primary again.
                        self.say(
                            Tone::Ok,
                            format!(
                                "  pod created in {} on the backup volume {}",
                                won.region, won.volume_id
                            ),
                        );
                        return Ok(self.created(won.pod, &won.volume_id, won.created, &won.region));
                    }
                    Err(Stop::Declined(message)) => {
                        self.say(
                            Tone::Dim,
                            format!("{message} No pod was created; nothing is billing."),
                        );
                        return Err(CommandError::Failed);
                    }
                    Err(Stop::Command(error)) => return Err(error),
                    Err(Stop::NoCapacity(message)) => (message, true),
                    Err(Stop::Refused(error)) => (error.to_string(), is_capacity_error(&error)),
                }
            } else {
                (refused.to_string(), is_capacity_error(&refused))
            };
        self.say(Tone::Error, message);
        let hint = if capacity {
            let first = if regions::failover_enabled(self.cfg) {
                "No region took the pod".to_string()
            } else {
                format!("No GPU free in {primary} (FAILOVER=0: no backup offered)")
            };
            format!(
                "{first}. Retry in a minute (stock flickers); viy backup shows RunPod's A100 stock."
            )
        } else {
            "Common causes: insufficient RunPod credit, or a request RunPod rejects (see the \
             message above)."
                .to_string()
        };
        self.say(
            Tone::Warn,
            format!(
                "\n{hint}\nEvery refused pod creation is logged in {}",
                self.env.paths.up_failures_file().display()
            ),
        );
        Err(CommandError::Failed)
    }

    fn created(&self, pod: Value, volume_id: &str, volume_is_new: bool, region: &str) -> Created {
        Created {
            pod,
            volume_id: volume_id.to_string(),
            volume_is_new,
            region: region.to_string(),
            waited: self.waited,
        }
    }

    /// One pod request in `region` on `volume_id`, logged when refused.
    /// A capacity refusal where the free GPUs run only newer CUDA drivers
    /// than CUDA_VERSIONS lists is asked again without the exact version.
    /// `quiet` leaves out the per-region notices of a retry round.
    async fn attempt(
        &mut self,
        region: &str,
        volume_id: &str,
        role: &str,
        quiet: bool,
    ) -> Result<Value, RunPodError> {
        let mut body = self.body.clone();
        body["dataCenterIds"] = json!([region]);
        body["networkVolumeId"] = json!(volume_id);
        if !self.tried {
            let gpus: Vec<&str> = gpu_type_list(self.cfg).into_iter().take(3).collect();
            self.say(
                Tone::Dim,
                format!(
                    "Creating pod in {region} ({}x from: {}...)",
                    self.cfg.get("GPU_COUNT").unwrap_or(""),
                    gpus.join(", ")
                ),
            );
        }
        self.tried = true;
        let result = self
            .request(region, volume_id, role, quiet, &mut body)
            .await;
        if let Err(error) = &result
            && is_capacity_error(error)
            && !quiet
        {
            self.say(Tone::Warn, format!("  {region} ({role}): no GPU free"));
        }
        result
    }

    async fn request(
        &mut self,
        region: &str,
        volume_id: &str,
        role: &str,
        quiet: bool,
        body: &mut Value,
    ) -> Result<Value, RunPodError> {
        let error = match create(self.rp, body.clone()).await {
            Ok(pod) => return Ok(pod),
            Err(error) => error,
        };
        let env = self.env;
        regions::log_failure(
            env.paths, env.time, self.cfg, region, role, volume_id, &error,
        );
        let filtered = body
            .get("allowedCudaVersions")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty());
        if !(is_capacity_error(&error)
            && filtered
            && regions::any_cuda_is_safe(self.rp, self.cfg, region).await)
        {
            return Err(error);
        }
        // The free GPUs here are on drivers newer than CUDA_VERSIONS lists
        // (13.1 and 13.2 cannot be requested over REST) and none on older
        // ones, so the exact-version filter is all that refuses the pod.
        if !quiet {
            self.say(
                Tone::Dim,
                format!(
                    "  {region} ({role}): its free GPUs run newer CUDA drivers; asking without \
                     the exact version"
                ),
            );
        }
        if let Some(body) = body.as_object_mut() {
            body.shift_remove("allowedCudaVersions");
        }
        create(self.rp, body.clone()).await.inspect_err(|error| {
            let role = format!("{role}, any cuda");
            regions::log_failure(
                env.paths, env.time, self.cfg, region, &role, volume_id, error,
            );
        })
    }

    /// Asks a yes/no question, or takes `default` with nobody to ask.
    async fn ask(&mut self, prompt: String, default: bool) -> bool {
        let answer = if default { Answer::Yes } else { Answer::No };
        let question = Question {
            kind: None,
            prompt,
            choices: Choices::YesNo,
            default: answer,
            unattended: answer,
            reason: Vec::new(),
        };
        let Some(asking) = (self.user.ask)(&question) else {
            let shown = if default { "yes" } else { "no" };
            self.say(
                Tone::Dim,
                format!("{} -> {shown} (no terminal to ask)", question.prompt),
            );
            return default;
        };
        alert(
            self.user.report,
            "viy up needs an answer",
            &question.prompt,
            false,
        );
        let asked = Instant::now();
        let answer = asking.await;
        self.waited += asked.elapsed();
        answer != Answer::No
    }

    /// regions.py's `failover`: called once DATA_CENTER_ID refused the
    /// pod for lack of capacity.
    async fn failover(&mut self, primary_volume: &str) -> Result<Won, Stop> {
        let (paths, time) = (self.env.paths, self.env.time);
        let primary = self.primary.clone();
        let backup = regions::find_backup(self.rp, paths, self.cfg, regions::now(time)).await?;
        let backup_region = backup
            .as_ref()
            .map(|b| crate::api::shown(b.get("dataCenterId")));
        if let (Some(backup), Some(region)) = (&backup, &backup_region) {
            let question = format!("{primary} has no GPU free. Try the backup volume in {region}?");
            if !self.ask(question, true).await {
                return Err(Stop::Declined(format!(
                    "Not trying the backup in {region}."
                )));
            }
            match self.attempt(region, id(backup), "backup", false).await {
                Ok(pod) => {
                    self.keep(regions::touch(paths, backup, regions::now(time)));
                    return Ok(Won {
                        pod,
                        region: region.clone(),
                        volume_id: id(backup).to_string(),
                        created: false,
                    });
                }
                Err(error) if !is_capacity_error(&error) => return Err(Stop::Refused(error)),
                Err(_) => self.say(
                    Tone::Warn,
                    format!("The backup region {region} has no GPU free either."),
                ),
            }
        } else {
            self.say(
                Tone::Warn,
                format!("{primary} has no GPU free, and there is no backup volume yet."),
            );
        }

        // Read before these samples join it.
        let past = regions::history(paths, regions::now(time), regions::HISTORY_DAYS);
        self.say(
            Tone::Warn,
            "Checking RunPod's A100 stock in every region with network volumes...",
        );
        let samples = regions::sample_stock(
            self.rp,
            paths,
            time,
            self.cfg,
            regions::SAMPLES,
            regions::SAMPLE_GAP,
        )
        .await
        .map_err(|error| {
            let first = regions::first_line(&error);
            Stop::NoCapacity(format!("Could not read RunPod's stock: {first}"))
        })?;
        let mut exclude = HashSet::from([primary.as_str()]);
        exclude.extend(backup_region.as_deref());
        let candidates = regions::rank(&samples, &exclude, &past);
        if candidates.is_empty() {
            let kept = backup_region
                .as_ref()
                .map(|r| format!(" The backup in {r} was kept."))
                .unwrap_or_default();
            return Err(Stop::NoCapacity(format!(
                "No other region with network volumes shows an A100 free right now.{kept}"
            )));
        }
        let shown: Vec<String> = candidates
            .iter()
            .map(|r| {
                let free = samples
                    .iter()
                    .filter(|s| s.get(r).copied().unwrap_or(false))
                    .count();
                format!("{r} {free}/{}", samples.len())
            })
            .collect();
        self.say(
            Tone::Warn,
            format!(
                "A100 free now (of {} samples): {}",
                samples.len(),
                shown.join(", ")
            ),
        );
        let minutes = regions::grab_minutes(self.cfg);
        let place = if let [only] = candidates.as_slice() {
            only.clone()
        } else {
            format!("the first of {} that takes the pod", candidates.join(", "))
        };
        let also: Vec<&str> = [Some(primary.as_str()), backup_region.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        let retry = if minutes != 0.0 {
            format!(
                ", retrying refusals there and in {} for up to {} min; new volumes left unused \
                 are deleted",
                also.join(" and "),
                short(minutes)
            )
        } else {
            String::new()
        };
        let question = match (&backup, &backup_region) {
            (Some(backup), Some(region)) => format!(
                "Switch the backup to {place}? It makes a new volume in each region it tries and \
                 boots cold (~5 min: weights download, first compile){retry}. The old backup {} \
                 in {region} is deleted once viy is up on the new one.",
                id(backup)
            ),
            _ => format!(
                "Make a backup volume in {place}? It makes a new volume in each region it tries \
                 and boots cold (~5 min: weights download, first compile){retry}. ~$7/month, \
                 deleted after {:.0} days unused.",
                regions::idle_days(self.cfg)
            ),
        };
        if !self.ask(question, false).await {
            let what = if backup.is_some() {
                "switched."
            } else {
                "made."
            };
            return Err(Stop::Declined(format!("The backup was not {what}")));
        }
        // New volumes exist from here on: a Ctrl-C deletes them first.
        self.user.abort_at.store(u32::MAX, Ordering::SeqCst);
        let won = self
            .grab(
                &candidates,
                backup.as_ref().zip(backup_region.as_deref()),
                primary_volume,
                minutes,
            )
            .await;
        let next = self.user.interrupt.count() + 1;
        self.user.abort_at.store(next, Ordering::SeqCst);
        won
    }

    /// regions.py's `_grab`: the first region that accepts the pod wins,
    /// each candidate on a new volume of its own, and from the second
    /// round on also the primary and the old backup, which need no new
    /// volume and boot warm. Stock flickers, so refusals are retried every
    /// [`RETRY_GAP_S`] until `minutes` run out.
    async fn grab(
        &mut self,
        candidates: &[String],
        backup: Option<(&Value, &str)>,
        primary_volume: &str,
        minutes: f64,
    ) -> Result<Won, Stop> {
        let paths = self.env.paths;
        let mut warm: Vec<(String, String, &str)> = Vec::new();
        if !primary_volume.is_empty() {
            warm.push((self.primary.clone(), primary_volume.to_string(), "primary"));
        }
        if let Some((backup, region)) = backup {
            warm.push((region.to_string(), id(backup).to_string(), "backup"));
        }
        // Region to the new volume made there, empty until a pod uses it.
        let mut made: IndexMap<String, Value> = IndexMap::new();
        let deadline = Duration::try_from_secs_f64(minutes * 60.0)
            .ok()
            .and_then(|d| Instant::now().checked_add(d));
        let mut stopped = pin!(self.user.interrupt.next());
        let mut rounds = 0u32;
        let mut won: Option<Won> = None;
        let mut failed: Option<Stop> = None;
        let mut interrupted = false;
        'rounds: loop {
            let fresh = candidates
                .iter()
                .map(|r| (r.clone(), String::new(), "new backup"));
            let targets: Vec<(String, String, &str)> = if rounds > 0 {
                warm.iter().cloned().chain(fresh).collect()
            } else {
                fresh.collect()
            };
            for (region, mut volume_id, role) in targets {
                if volume_id.is_empty() {
                    if !made.contains_key(&region) {
                        let made_now = tokio::select! {
                            biased;
                            () = &mut stopped => { interrupted = true; break 'rounds; }
                            made_now = self.new_volume(&region) => made_now,
                        };
                        match made_now {
                            Ok(volume) => {
                                made.insert(region.clone(), volume);
                            }
                            Err(stop) => {
                                failed = Some(stop);
                                break 'rounds;
                            }
                        }
                    }
                    volume_id = made.get(&region).map(id).unwrap_or_default().to_string();
                }
                let attempt = tokio::select! {
                    biased;
                    () = &mut stopped => { interrupted = true; break 'rounds; }
                    attempt = self.attempt(&region, &volume_id, role, rounds > 0) => attempt,
                };
                match attempt {
                    Ok(pod) => {
                        let created = made.get(&region).is_some_and(|v| id(v) == volume_id);
                        won = Some(Won {
                            pod,
                            region,
                            volume_id,
                            created,
                        });
                        break 'rounds;
                    }
                    Err(error) if !is_capacity_error(&error) => {
                        failed = Some(Stop::Refused(error));
                        break 'rounds;
                    }
                    Err(_) => {}
                }
            }
            rounds += 1;
            let left = deadline.map_or(f64::INFINITY, |d| {
                d.saturating_duration_since(Instant::now()).as_secs_f64()
            });
            if left <= RETRY_GAP_S as f64 {
                break;
            }
            if rounds == 1 {
                self.say(
                    Tone::Warn,
                    format!(
                        "No region took the pod; retrying every {RETRY_GAP_S}s for {} min \
                         (Ctrl-C stops and deletes the new volumes).",
                        short(minutes)
                    ),
                );
            } else if rounds.is_multiple_of(6) {
                self.say(
                    Tone::Warn,
                    format!(
                        "  still no GPU free; {:.0} min of retries left",
                        left / 60.0
                    ),
                );
            }
            let gap = Duration::from_secs(RETRY_GAP_S);
            tokio::select! {
                biased;
                () = &mut stopped => { interrupted = true; break; }
                () = tokio::time::sleep(gap) => self.waited += gap,
            }
        }

        // Whatever ended the rounds, the new volumes no pod took go now. A
        // further Ctrl-C skips the one being deleted, which stays recorded
        // as unused for `viy reap`, and goes on with the rest.
        let winner = won.as_ref().map(|w| w.volume_id.as_str());
        for volume in made.values().filter(|v| Some(id(v)) != winner) {
            let deleted = tokio::select! {
                biased;
                () = self.user.interrupt.next() => continue,
                deleted = regions::delete_empty(self.rp, paths, volume, self.user.report) => deleted,
            };
            self.keep(deleted);
        }
        if interrupted {
            return Err(Stop::Command(CommandError::Interrupted));
        }
        if let Some(stop) = failed {
            return Err(stop);
        }
        let Some(won) = won else {
            let mut tried = candidates.to_vec();
            if minutes != 0.0 {
                tried.extend(warm.iter().map(|(r, _, _)| r.clone()));
            }
            let during = if minutes != 0.0 {
                format!(" in {} min of retries", short(minutes))
            } else {
                String::new()
            };
            let kept = if backup.is_some() {
                ", the old backup kept."
            } else {
                "."
            };
            return Err(Stop::NoCapacity(format!(
                "No region took the pod{during} ({}). The volumes made for it were deleted{kept}",
                tried.join(", ")
            )));
        };
        // A pod exists now: backups.json failing must not lose it.
        let now = regions::now(self.env.time);
        if won.created {
            if let Some(volume) = made.get(&won.region) {
                self.keep(regions::touch(paths, volume, now));
            }
            if let Some((backup, _)) = backup {
                // The old backup goes only once `viy up` serves on the new
                // one: a boot that fails now leaves it in place.
                self.keep(regions::mark_replaces(paths, &won.volume_id, id(backup)));
            }
        } else if let Some((backup, _)) = backup.filter(|(b, _)| id(b) == won.volume_id) {
            self.keep(regions::touch(paths, backup, now));
        }
        Ok(won)
    }

    /// A new volume for the backup in `region`, recorded as unused until
    /// a pod takes it.
    async fn new_volume(&mut self, region: &str) -> Result<Value, Stop> {
        let size = self.cfg.get("NETWORK_VOLUME_SIZE_GB").unwrap_or("");
        let size_gb: u64 = size.trim().parse().map_err(|_| {
            Stop::Command(CommandError::Exit(format!(
                "NETWORK_VOLUME_SIZE_GB must be a whole number, not {size:?}"
            )))
        })?;
        let volume = self
            .rp
            .create_volume(&regions::backup_name(self.cfg), size_gb, region)
            .await
            .map_err(Stop::Refused)?
            .filter(|v| !id(v).is_empty())
            .ok_or_else(|| Stop::Refused(RunPodError::new("RunPod created no volume", None)))?;
        regions::record_unused(self.env.paths, &volume, regions::now(self.env.time))?;
        Ok(volume)
    }
}

/// One create request: the pod, or why there is none.
async fn create(rp: &RunPod, body: Value) -> Result<Value, RunPodError> {
    match rp.create_pod(body).await? {
        Some(pod) if pod.get("id").and_then(Value::as_str).is_some() => Ok(pod),
        _ => Err(RunPodError::new("RunPod created no pod", None)),
    }
}

#[cfg(test)]
#[path = "failover_tests.rs"]
mod tests;
