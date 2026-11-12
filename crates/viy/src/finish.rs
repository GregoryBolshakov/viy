//! `viy migrate --finish`: the cutover from rp (ROADMAP phase 8, kb d135).
//! What rp kept that viy left alone until now comes over in one run:
//!
//! - rp's saved chats, copied into viy's sessions folder with what /rewind
//!   restores (`<id>.files/`), each under the lock rp takes on it; a chat
//!   viy has (or has archived) is kept. rp's trash is not copied, and the
//!   listing sidecars are left for viy to rebuild.
//! - rp's global toolbox, tool by tool, a tool viy has kept.
//! - Per project, rp's input history, put before viy's in `.viy/history`,
//!   and the project's toolbox. The projects are those rp's chats were
//!   started in and the current one. `.viy/` is made as a chat makes it,
//!   never in the home folder or above it (kb d118).
//! - The pod state, moved from rp's state folder to viy's, under the pod
//!   lock: from then on viy reads and writes it there
//!   ([`viy_config::Paths::pod_state_dir`]). Lock files and rp's own chat
//!   records stay.
//! - rp's reap timer stopped and removed before the move, so it never sees
//!   a pod without its state, and viy's installed after it.
//!
//! Every copy is new files beside the target, linked or renamed into place,
//! so nothing of viy's is replaced and rp's files are never changed; the
//! pod state is the one thing moved. Each step is recorded or idempotent
//! (Codex's import ledger, `external-agent-migration/src/sessions/
//! ledger.rs`), so a run that stops part way is run again. It is refused,
//! with nothing done, while a chat of either tool is open, an up, stop or
//! down holds the pod lock, a shutdown is scheduled, or the viy on PATH,
//! which the reap timer runs, is not this one. `--dry-run` says what it
//! would do and changes nothing.

use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::fs::TryLockError;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::UNIX_EPOCH;

use viy_agent::sessions::Sessions;
use viy_config::Paths;
use viy_config::Project;
use viy_config::cutover::Record;
use viy_config::migrate::copy_new;
use viy_config::migrate::copy_tree_new;
use viy_config::project::MARKER;
use viy_config::project::RPCTL_MARKER;
use viy_exec::ExecBackend;
use viy_pod::idle;
use viy_pod::lock::PodLock;
use viy_pod::notice::Report;
use viy_pod::shutdown::Chats;
use viy_pod::shutdown::Scheduled;
use viy_time::SystemTimeProvider;

/// This machine, as the cutover sees it.
pub struct Machine<'a> {
    pub paths: &'a Paths,
    pub cwd: &'a Path,
    /// This process's executable.
    pub me: &'a Path,
    /// The viy on PATH, which the reap timer will run.
    pub on_path: Option<&'a Path>,
    pub chats: &'a dyn Chats,
    pub exec: &'a dyn ExecBackend,
    /// Now, Unix seconds.
    pub now: f64,
}

/// What a run would do, read before anything is changed.
#[derive(Debug, Default)]
pub struct Plan {
    record: Record,
    /// rp's chats viy lacks, by id.
    sessions: Vec<String>,
    /// rp's chats viy has already.
    sessions_kept: usize,
    /// rp's global tools viy lacks, and those it has.
    tools: Vec<String>,
    tools_kept: Vec<String>,
    projects: Vec<ProjectPlan>,
    /// (rp's, viy's) for each pod state file there is to move.
    pod_files: Vec<(PathBuf, PathBuf)>,
    rp_timer: bool,
    viy_timer: bool,
    /// Why a run would be refused now.
    blockers: Vec<String>,
}

#[derive(Debug)]
struct ProjectPlan {
    root: PathBuf,
    /// rp's entries to import, stamped with its history file's time;
    /// `None` when there is no history or it is in already.
    history: Option<(Vec<String>, u64)>,
    history_done: bool,
    tools: Vec<String>,
    tools_kept: Vec<String>,
    /// Why nothing is imported here.
    skipped: Option<&'static str>,
}

/// Runs `viy migrate --finish`, or says what it would do with `dry_run`.
/// `say` gets each line as it happens. `Err` is the message to exit with,
/// status 1.
pub async fn run(
    m: &Machine<'_>,
    dry_run: bool,
    say: &mut dyn FnMut(String),
) -> Result<(), String> {
    let plan = plan(m).map_err(|e| e.to_string())?;
    if dry_run {
        say("viy migrate --finish --dry-run: nothing is changed; viy migrate --finish would do this:".into());
        for line in describe(m, &plan) {
            say(line);
        }
        if !plan.blockers.is_empty() {
            say("and it would be refused now:".into());
            for b in &plan.blockers {
                say(format!("  - {b}"));
            }
        }
        return Ok(());
    }
    if !plan.blockers.is_empty() {
        let mut text = String::from("viy migrate --finish refused; nothing was changed:");
        for b in &plan.blockers {
            text += &format!("\n  - {b}");
        }
        return Err(text);
    }
    apply(m, plan, say).await
}

// ------------------------------------------------------------------ the plan

fn plan(m: &Machine<'_>) -> io::Result<Plan> {
    let paths = m.paths;
    let mut plan = Plan {
        record: Record::load(&paths.state_dir())?,
        ..Plan::default()
    };

    let rp_sessions = rp_sessions_dir(paths);
    let mut roots = BTreeSet::new();
    for path in jsonl_files(&rp_sessions.join("trash"))? {
        roots.extend(meta_project(&path));
    }
    let archived = archived_dir(paths);
    for path in jsonl_files(&rp_sessions)? {
        roots.extend(meta_project(&path));
        let name = path.file_name().unwrap_or_default();
        if paths.sessions_dir().join(name).exists() || archived.join(name).exists() {
            plan.sessions_kept += 1;
        } else {
            plan.sessions.push(stem(&path));
        }
    }

    (plan.tools, plan.tools_kept) = tool_names(
        &paths.rpctl_data_dir().join("toolbox"),
        &paths.data_dir().join("toolbox"),
    )?;

    if let Ok(here) = Project::find(m.cwd) {
        roots.insert(here.root().to_path_buf());
    }
    // Resolved, so a project reached through a symlink is one project, and
    // recorded as one.
    let roots: BTreeSet<PathBuf> = roots
        .into_iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect();
    let home = fs::canonicalize(&paths.home).unwrap_or_else(|_| paths.home.clone());
    for root in roots {
        let rp = root.join(RPCTL_MARKER);
        if !rp.is_dir() {
            continue;
        }
        let history = rp.join("history");
        let (tools, tools_kept) =
            tool_names(&rp.join("toolbox"), &root.join(MARKER).join("toolbox"))?;
        if !history.is_file() && tools.is_empty() && tools_kept.is_empty() {
            continue;
        }
        let history_done = plan.record.histories.contains(&root);
        let mut project = ProjectPlan {
            root: root.clone(),
            history: None,
            history_done,
            tools,
            tools_kept,
            skipped: None,
        };
        if home.starts_with(&root) {
            project.skipped = Some("viy marks no project in the home folder or above it");
        } else if history.is_file() && !history_done {
            let entries = viy_term::history::readline_entries(&fs::read(&history)?);
            let ts = fs::metadata(&history)?
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            project.history = Some((entries, ts));
        }
        plan.projects.push(project);
    }

    if !paths.pod_state_moved {
        let (rp, viy) = both_sides(paths);
        // Read again under the pod lock when the run moves them
        // ([`move_pod_state`]); a file only at viy's side is one an
        // interrupted run moved.
        for (src, dst) in pod_files(&rp).into_iter().zip(pod_files(&viy)) {
            if src.symlink_metadata().is_err() {
                continue;
            }
            if dst.symlink_metadata().is_ok() {
                plan.blockers.push(already_there(&dst));
            }
            plan.pod_files.push((src, dst));
        }
    }
    plan.rp_timer = idle::rpctl_timer_installed(paths);
    plan.viy_timer = idle::timer_installed(paths);

    let open = m.chats.find();
    if !open.is_empty() {
        let pids: Vec<String> = open.iter().map(u32::to_string).collect();
        plan.blockers.push(format!(
            "a chat is open: close every rp and viy chat first (pid {})",
            pids.join(", ")
        ));
    }
    if let Some(holder) = PodLock::holder(paths) {
        plan.blockers.push(format!(
            "{} {} is running (pid {}): let it finish first",
            holder.tool_name(),
            holder.verb,
            holder.pid
        ));
    }
    if let Some(shutdown) = Scheduled::peek(paths) {
        plan.blockers.push(format!(
            "a shutdown is scheduled (pid {}): cancel it first with viy shutdown --cancel",
            shutdown.pid
        ));
    }
    if !plan.viy_timer {
        match m.on_path {
            None => plan.blockers.push(
                "viy is not on PATH: install this build first (cargo install --path crates/viy)"
                    .into(),
            ),
            Some(on_path) if !same_file(on_path, m.me) => plan.blockers.push(format!(
                "this is {}, but the reap timer will run the viy on PATH, {}: install this \
                 build there and run that one",
                m.me.display(),
                on_path.display()
            )),
            Some(_) => {}
        }
    }
    Ok(plan)
}

/// What [`plan`] found, as the dry run says it.
fn describe(m: &Machine<'_>, plan: &Plan) -> Vec<String> {
    let paths = m.paths;
    let mut lines = Vec::new();
    lines.push(sessions_line(paths, plan, "would copy"));
    lines.push(toolbox_line(paths, plan, "would copy"));
    for p in &plan.projects {
        lines.extend(project_lines(p, "would import", "would copy"));
    }
    lines.push(match &plan.pod_files[..] {
        [] if paths.pod_state_moved => {
            format!("pod state: in {} already", paths.state_dir().display())
        }
        [] => format!(
            "pod state: nothing to move; would keep it in {} from now on",
            paths.state_dir().display()
        ),
        files => format!(
            "pod state: would move {} from {} to {}",
            names(files),
            paths.rpctl_state_dir().display(),
            paths.state_dir().display()
        ),
    });
    if plan.rp_timer {
        lines.push("rpctl-reap.timer: would stop and remove it, before the move".into());
    }
    lines.push(if plan.viy_timer {
        "viy-reap.timer: installed already".into()
    } else {
        format!(
            "viy-reap.timer: would install it, running {} reap every 5 minutes",
            m.on_path.unwrap_or(Path::new("viy")).display()
        )
    });
    lines
}

fn sessions_line(paths: &Paths, plan: &Plan, verb: &str) -> String {
    let kept = match plan.sessions_kept {
        0 => String::new(),
        n => format!(" ({n} there already, kept)"),
    };
    match plan.sessions.len() {
        0 if plan.sessions_kept == 0 => "sessions: rp has none".into(),
        0 => format!("sessions: viy has all {} of rp's chats", plan.sessions_kept),
        n => format!(
            "sessions: {verb} {n} of rp's chats to {}{kept}",
            paths.sessions_dir().display()
        ),
    }
}

fn toolbox_line(paths: &Paths, plan: &Plan, verb: &str) -> String {
    let kept = kept_note(&plan.tools_kept);
    if plan.tools.is_empty() {
        return if plan.tools_kept.is_empty() {
            "toolbox: rp has no global tools".into()
        } else {
            format!("toolbox: viy has every global tool of rp's{kept}")
        };
    }
    format!(
        "toolbox: {verb} {} to {}{kept}",
        plan.tools.join(", "),
        paths.data_dir().join("toolbox").display()
    )
}

fn project_lines(p: &ProjectPlan, import: &str, copy: &str) -> Vec<String> {
    let root = p.root.display();
    if let Some(why) = p.skipped {
        return vec![format!("{root}: not imported: {why}")];
    }
    let mut lines = Vec::new();
    if p.history_done {
        lines.push(format!("{root}: history imported before"));
    } else if let Some((entries, _)) = &p.history {
        lines.push(format!(
            "{root}: {import} {} history entries into {MARKER}/history",
            entries.len()
        ));
    }
    if !p.tools.is_empty() {
        lines.push(format!(
            "{root}: {copy} tools {} to {MARKER}/toolbox{}",
            p.tools.join(", "),
            kept_note(&p.tools_kept)
        ));
    }
    lines
}

fn kept_note(kept: &[String]) -> String {
    if kept.is_empty() {
        String::new()
    } else {
        format!(" (there already, kept: {})", kept.join(", "))
    }
}

fn names(files: &[(PathBuf, PathBuf)]) -> String {
    files
        .iter()
        .map(|(src, _)| src.file_name().unwrap_or_default().to_string_lossy())
        .collect::<Vec<_>>()
        .join(", ")
}

// ------------------------------------------------------------------ the run

async fn apply(m: &Machine<'_>, mut plan: Plan, say: &mut dyn FnMut(String)) -> Result<(), String> {
    let paths = m.paths;
    let text = |e: io::Error| e.to_string();

    let rp_sessions = rp_sessions_dir(paths);
    if !plan.sessions.is_empty() {
        let dir = paths.sessions_dir();
        fs::create_dir_all(&dir).map_err(text)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(text)?;
        for id in &plan.sessions {
            copy_session(&rp_sessions, &dir, id)?;
        }
    }
    say(sessions_line(paths, &plan, "copied"));

    let (rp_tools, viy_tools) = (
        paths.rpctl_data_dir().join("toolbox"),
        paths.data_dir().join("toolbox"),
    );
    for name in &plan.tools {
        copy_tree_new(&rp_tools.join(name), &viy_tools.join(name)).map_err(text)?;
    }
    say(toolbox_line(paths, &plan, "copied"));

    for p in &plan.projects {
        if p.skipped.is_none() && (p.history.is_some() || !p.tools.is_empty()) {
            let (project, _) = Project::init(&p.root).map_err(text)?;
            let rp = p.root.join(RPCTL_MARKER);
            for name in &p.tools {
                copy_tree_new(
                    &rp.join("toolbox").join(name),
                    &project.dir().join("toolbox").join(name),
                )
                .map_err(text)?;
            }
            if let Some((entries, ts)) = &p.history {
                viy_term::history::import(&project.history_file(), entries, *ts)
                    .map_err(|e| format!("{}: {e}", project.history_file().display()))?;
                plan.record.histories.push(p.root.clone());
                plan.record.save(&paths.state_dir()).map_err(text)?;
            }
        }
        for line in project_lines(p, "imported", "copied") {
            say(line);
        }
    }

    // The pod: from here to the end of the timer swap rp's pod lock is
    // held, so no up, stop or down of either tool runs, and rp reap, which
    // checks the lock first, looks nowhere ("not checking now").
    let lock = PodLock::exclusive(paths, "migrate --finish").map_err(|e| e.to_string())?;
    let told = Told::default();
    let reporter = told.reporter();
    let result = idle::remove_rpctl_timer(paths, m.exec, &reporter).await;
    told.drain(say);
    result.map_err(|e| e.to_string())?;

    if !paths.pod_state_moved {
        let moved = move_pod_state(paths)?;
        plan.record.pod_state_moved = true;
        plan.record.save(&paths.state_dir()).map_err(text)?;
        say(if moved.is_empty() {
            format!(
                "pod state: nothing to move; viy keeps it in {} from now on",
                paths.state_dir().display()
            )
        } else {
            format!(
                "pod state: moved {} from {} to {}",
                names(&moved),
                paths.rpctl_state_dir().display(),
                paths.state_dir().display()
            )
        });
    }

    if !plan.viy_timer {
        let result = idle::install(paths, m.exec, m.on_path, &reporter).await;
        told.drain(say);
        result.map_err(|e| {
            format!(
                "{e}\nno reap timer is installed now (the pod's own watchdog still runs): \
                 run viy migrate --finish again"
            )
        })?;
    }
    drop(lock);

    if plan.record.finished.is_none() {
        plan.record.finished = Some(m.now);
        plan.record.save(&paths.state_dir()).map_err(text)?;
    }
    say(format!(
        "cutover finished: viy keeps the pod state in {}. rp's own files are where they were; \
         use viy, not rp, for the pod from now on.",
        paths.state_dir().display()
    ));
    Ok(())
}

/// Copies rp's chat `id` into `dir`, holding the lock rp takes on its log,
/// so an rp chat cannot write it meanwhile: what /rewind restores first,
/// then the log, which makes the chat appear.
fn copy_session(from: &Path, dir: &Path, id: &str) -> Result<(), String> {
    let src = from.join(format!("{id}.jsonl"));
    let with_path = |path: &Path, e: io::Error| format!("{}: {e}", path.display());
    let held = File::open(&src).map_err(|e| with_path(&src, e))?;
    match held.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(format!(
                "rp's chat {id} is open: close it and run viy migrate --finish again"
            ));
        }
        Err(TryLockError::Error(e)) => return Err(with_path(&src, e)),
    }
    let files = from.join(format!("{id}.files"));
    if files.is_dir() {
        copy_tree_new(&files, &dir.join(format!("{id}.files"))).map_err(|e| e.to_string())?;
    }
    let dst = dir.join(format!("{id}.jsonl"));
    if copy_new(&src, &dst).map_err(|e| e.to_string())? {
        // Kept, so a chat with no stamps in it still lists where it was.
        if let Ok(modified) = held.metadata().and_then(|m| m.modified()) {
            let _ = File::open(&dst).and_then(|f| f.set_modified(modified));
        }
    }
    Ok(())
}

/// Moves the pod state files from rp's folder to viy's, read now, under
/// the pod lock the caller holds, so a file an up wrote after the plan
/// moves too; a file viy's folder has is never replaced. Returns what
/// moved.
fn move_pod_state(paths: &Paths) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let (rp, viy) = both_sides(paths);
    fs::create_dir_all(paths.state_dir()).map_err(|e| e.to_string())?;
    let mut moved = Vec::new();
    for (src, dst) in pod_files(&rp).into_iter().zip(pod_files(&viy)) {
        if src.symlink_metadata().is_err() {
            continue;
        }
        if dst.symlink_metadata().is_ok() {
            return Err(already_there(&dst));
        }
        fs::rename(&src, &dst)
            .map_err(|e| format!("{} -> {}: {e}", src.display(), dst.display()))?;
        moved.push((src, dst));
    }
    Ok(moved)
}

fn already_there(dst: &Path) -> String {
    format!(
        "{} is there already: viy's state folder holds pod state before the move",
        dst.display()
    )
}

// ------------------------------------------------------------------ helpers

/// rp's sessions folder.
fn rp_sessions_dir(paths: &Paths) -> PathBuf {
    paths.rpctl_data_dir().join("sessions")
}

/// viy's archived chats.
fn archived_dir(paths: &Paths) -> PathBuf {
    Sessions::new(paths.sessions_dir(), Arc::new(SystemTimeProvider))
        .archived()
        .dir()
        .to_path_buf()
}

/// `paths` with the pod state at rp's and at viy's.
fn both_sides(paths: &Paths) -> (Paths, Paths) {
    let mut rp = paths.clone();
    rp.pod_state_moved = false;
    let mut viy = paths.clone();
    viy.pod_state_moved = true;
    (rp, viy)
}

/// The pod state files that move, wherever `paths` puts them: d006's and
/// d049's shared files, but the lock files (`pod.lock`, `pod.json.lock`,
/// `locks/`), which hold nothing, and `shutdown.json`, which a run refuses
/// while it is live.
fn pod_files(paths: &Paths) -> Vec<PathBuf> {
    vec![
        paths.pod_state_file(),
        paths.activity_file(),
        paths.known_hosts_file(),
        paths.reap_state_file(),
        paths.backups_file(),
        paths.stock_file(),
        paths.up_failures_file(),
        paths.boots_history_file(),
        paths.boots_dir(),
        paths.tunnel_log_file(),
        paths.local_state_file(),
        paths.local_log_file(),
        paths.shutdown_log_file(),
    ]
}

/// The `*.jsonl` files directly in `dir`, sorted; none when it is missing.
fn jsonl_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", dir.display()))),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|x| x == "jsonl") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// The project a chat was started in: its log's first line, the `meta`
/// event, and nothing more of it.
fn meta_project(path: &Path) -> Option<PathBuf> {
    let mut line = String::new();
    BufReader::new(File::open(path).ok()?)
        .read_line(&mut line)
        .ok()?;
    let meta: serde_json::Value = serde_json::from_str(&line).ok()?;
    if meta.get("t")?.as_str()? != "meta" {
        return None;
    }
    let project = meta.get("project")?.as_str()?;
    Path::new(project)
        .is_absolute()
        .then(|| PathBuf::from(project))
}

/// The tool folders in `from`, split into those `to` lacks and those it
/// has, sorted.
fn tool_names(from: &Path, to: &Path) -> io::Result<(Vec<String>, Vec<String>)> {
    let entries = match fs::read_dir(from) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", from.display()))),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        // A linked tool counts, as the toolbox's own scan follows links.
        if entry.path().is_dir()
            && let Some(name) = entry.file_name().to_str()
            && !name.starts_with('.')
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names
        .into_iter()
        .partition(|name| to.join(name).symlink_metadata().is_err()))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The notices of the reap timer calls, said in order once each returns.
#[derive(Default)]
struct Told(Arc<Mutex<Vec<String>>>);

impl Told {
    fn reporter(&self) -> impl Fn(Report) + Send + Sync + 'static {
        let told = Arc::clone(&self.0);
        move |r| {
            if let Report::Notice(n) = r {
                told.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(n.text);
            }
        }
    }

    fn drain(&self, say: &mut dyn FnMut(String)) {
        let lines: Vec<String> = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect();
        for line in lines {
            say(line);
        }
    }
}

#[cfg(test)]
#[path = "finish_tests.rs"]
mod tests;
