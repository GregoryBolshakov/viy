//! `viy migrate --finish` on scratch homes: an rp machine laid out with
//! synthetic chats, tools, histories, pod state and rp's reap timer.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_exec::BoxFuture;
use viy_pod::shutdown::Closed;
use viy_testkit::exec::FakeExec;
use viy_testkit::exec::Reply as Run;
use viy_testkit::scratch::ScratchDir;

use super::*;

const A: &str = "20260719-100000-aaaa";
const B: &str = "20260720-100000-bbbb";
const KEPT: &str = "20260721-100000-cccc";
const TRASHED: &str = "20260722-100000-dddd";

struct Open(Vec<u32>);

impl Chats for Open {
    fn find(&self) -> Vec<u32> {
        self.0.clone()
    }

    fn close<'a>(&'a self, _pids: &'a [u32], _grace: Duration) -> BoxFuture<'a, Closed> {
        Box::pin(async { Closed::default() })
    }
}

/// An rp machine: its home, projects, and viy installed on PATH.
struct Rp {
    dir: ScratchDir,
    paths: Paths,
    /// Started in by chat A, with rp's history and a project tool.
    alpha: PathBuf,
    /// Started in by the trashed chat, with rp's history only.
    beta: PathBuf,
    viy: PathBuf,
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn log(id: &str, project: &Path) -> String {
    format!(
        "{}\n{}\n",
        json!({"t": "meta", "id": id, "created": 1.0, "project": project, "model": "m"}),
        json!({"t": "msg", "message": {"role": "user", "content": format!("hello from {id}")}}),
    )
}

fn tool(dir: &Path, name: &str) {
    write(
        &dir.join(name).join("TOOL.md"),
        &format!("---\nname: {name}\n---\n"),
    );
    let exe = dir.join(name).join(name);
    write(&exe, "#!/bin/sh\necho ok\n");
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
}

fn rp_machine() -> Rp {
    let dir = ScratchDir::new("finish").unwrap();
    let home = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&home);
    let alpha = home.join("work/alpha");
    let beta = home.join("work/beta");

    let sessions = paths.rpctl_data_dir().join("sessions");
    write(&sessions.join(format!("{A}.jsonl")), &log(A, &alpha));
    write(
        &sessions.join(format!("{A}.meta.json")),
        "{\"id\": \"stale\"}",
    );
    write(&sessions.join(format!("{A}.files/1/notes.md")), "before\n");
    write(&sessions.join(format!("{B}.jsonl")), &log(B, &home));
    write(&sessions.join(format!("{KEPT}.jsonl")), &log(KEPT, &alpha));
    write(
        &sessions.join("trash").join(format!("{TRASHED}.jsonl")),
        &log(TRASHED, &beta),
    );
    write(
        &paths.sessions_dir().join(format!("{KEPT}.jsonl")),
        "viy's own\n",
    );

    let rp_tools = paths.rpctl_data_dir().join("toolbox");
    tool(&rp_tools, "grab");
    tool(&rp_tools, "keep");
    tool(&paths.data_dir().join("toolbox"), "keep");

    let rp_alpha = alpha.join(RPCTL_MARKER);
    write(&rp_alpha.join("history"), "ls\nls\n\ngit status\n");
    tool(&rp_alpha.join("toolbox"), "ptool");
    write(&beta.join(RPCTL_MARKER).join("history"), "make\n");
    write(&home.join(RPCTL_MARKER).join("history"), "from home\n");

    let state = paths.rpctl_state_dir();
    write(&state.join("pod.json"), "{\"pod_id\": \"p1\"}");
    write(&state.join("last_activity"), "");
    write(&state.join("known_hosts"), "[host]:22 ssh-ed25519 AAAA\n");
    write(&state.join("boots/b1/record.json"), "{}");
    write(&state.join("pod.lock"), "");
    write(&state.join("alarms.jsonl"), "rp's own\n");

    let units = paths.systemd_user_dir();
    write(&units.join("rpctl-reap.timer"), "[Timer]\n");
    write(&units.join("rpctl-reap.service"), "[Service]\n");

    let viy = home.join("bin/viy");
    write(&viy, "");
    Rp {
        dir,
        paths,
        alpha,
        beta,
        viy,
    }
}

async fn finish(
    rp: &Rp,
    exec: &FakeExec,
    chats: &[u32],
    dry: bool,
) -> (Result<(), String>, Vec<String>) {
    let open = Open(chats.to_vec());
    let machine = Machine {
        paths: &rp.paths,
        cwd: &rp.alpha,
        me: &rp.viy,
        on_path: Some(&rp.viy),
        chats: &open,
        exec,
        now: 1_762_900_000.0,
    };
    let mut lines = Vec::new();
    let result = run(&machine, dry, &mut |line| lines.push(line)).await;
    (result, lines)
}

/// Every file under the home with its contents, for "nothing changed".
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut all = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
                all.insert(path, Vec::new());
            } else {
                all.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    all
}

fn the_paths_after(rp: &Rp) -> Paths {
    let mut paths = rp.paths.clone();
    paths.pod_state_moved = Record::load(&paths.state_dir()).unwrap().pod_state_moved;
    paths
}

#[tokio::test]
async fn the_dry_run_says_what_it_would_do_and_changes_nothing() {
    let rp = rp_machine();
    let before = tree(rp.dir.path());
    let exec = FakeExec::default();
    let (result, lines) = finish(&rp, &exec, &[], true).await;
    result.unwrap();
    let p = &rp.paths;
    assert_eq!(
        lines,
        [
            "viy migrate --finish --dry-run: nothing is changed; viy migrate --finish would do this:".to_string(),
            format!(
                "sessions: would copy 2 of rp's chats to {} (1 there already, kept)",
                p.sessions_dir().display()
            ),
            format!(
                "toolbox: would copy grab to {} (there already, kept: keep)",
                p.data_dir().join("toolbox").display()
            ),
            format!("{}: not imported: viy marks no project in the home folder or above it", p.home.display()),
            format!("{}: would import 2 history entries into .viy/history", rp.alpha.display()),
            format!("{}: would copy tools ptool to .viy/toolbox", rp.alpha.display()),
            format!("{}: would import 1 history entries into .viy/history", rp.beta.display()),
            format!(
                "pod state: would move pod.json, last_activity, known_hosts, boots from {} to {}",
                p.rpctl_state_dir().display(),
                p.state_dir().display()
            ),
            "rpctl-reap.timer: would stop and remove it, before the move".to_string(),
            format!(
                "viy-reap.timer: would install it, running {} reap every 5 minutes",
                rp.viy.display()
            ),
        ]
    );
    assert!(exec.sent().is_empty());
    assert_eq!(tree(rp.dir.path()), before);
}

#[tokio::test]
async fn the_run_brings_everything_over_and_a_second_finds_nothing_to_do() {
    let rp = rp_machine();
    let exec = FakeExec::new([
        Run::stdout(""),
        Run::stdout(""),
        Run::stdout(""),
        Run::stdout(""),
    ]);
    let (result, lines) = finish(&rp, &exec, &[], false).await;
    result.unwrap();
    let p = &rp.paths;
    let units = p.systemd_user_dir();
    assert_eq!(
        lines,
        [
            format!(
                "sessions: copied 2 of rp's chats to {} (1 there already, kept)",
                p.sessions_dir().display()
            ),
            format!(
                "toolbox: copied grab to {} (there already, kept: keep)",
                p.data_dir().join("toolbox").display()
            ),
            format!(
                "{}: not imported: viy marks no project in the home folder or above it",
                p.home.display()
            ),
            format!(
                "{}: imported 2 history entries into .viy/history",
                rp.alpha.display()
            ),
            format!("{}: copied tools ptool to .viy/toolbox", rp.alpha.display()),
            format!(
                "{}: imported 1 history entries into .viy/history",
                rp.beta.display()
            ),
            "stopped and removed rpctl-reap.timer: rp reap no longer runs".to_string(),
            format!(
                "pod state: moved pod.json, last_activity, known_hosts, boots from {} to {}",
                p.rpctl_state_dir().display(),
                p.state_dir().display()
            ),
            format!(
                "installed {}/viy-reap.timer: viy reap runs every 5 minutes",
                units.display()
            ),
            "check it with:  systemctl --user list-timers viy-reap.timer".to_string(),
            format!(
                "cutover finished: viy keeps the pod state in {}. rp's own files are where they \
                 were; use viy, not rp, for the pod from now on.",
                p.state_dir().display()
            ),
        ]
    );
    let argv: Vec<Vec<String>> = exec.sent().iter().map(viy_exec::Command::argv).collect();
    assert_eq!(
        argv,
        [
            vec![
                "systemctl",
                "--user",
                "disable",
                "--now",
                "rpctl-reap.timer"
            ],
            vec!["systemctl", "--user", "daemon-reload"],
            vec!["systemctl", "--user", "daemon-reload"],
            vec!["systemctl", "--user", "enable", "--now", "viy-reap.timer"],
        ]
    );

    // The chats: rp's logs as they were, at 0600, with what /rewind
    // restores; no sidecar copied; viy's own kept; the trashed one left.
    let rp_sessions = p.rpctl_data_dir().join("sessions");
    for id in [A, B] {
        let copy = p.sessions_dir().join(format!("{id}.jsonl"));
        assert_eq!(
            fs::read(&copy).unwrap(),
            fs::read(rp_sessions.join(format!("{id}.jsonl"))).unwrap()
        );
        assert_eq!(
            fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&copy).unwrap().modified().unwrap(),
            fs::metadata(rp_sessions.join(format!("{id}.jsonl")))
                .unwrap()
                .modified()
                .unwrap()
        );
    }
    assert_eq!(
        fs::read_to_string(p.sessions_dir().join(format!("{A}.files/1/notes.md"))).unwrap(),
        "before\n"
    );
    assert!(!p.sessions_dir().join(format!("{A}.meta.json")).exists());
    assert_eq!(
        fs::read_to_string(p.sessions_dir().join(format!("{KEPT}.jsonl"))).unwrap(),
        "viy's own\n"
    );
    assert!(!p.sessions_dir().join(format!("{TRASHED}.jsonl")).exists());
    assert_eq!(
        fs::metadata(p.sessions_dir()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // ...and viy reads them as its own.
    let store = viy_agent::sessions::Sessions::new(
        p.sessions_dir(),
        Arc::new(viy_time::SystemTimeProvider),
    );
    let (_, state) = store.open(A).unwrap();
    assert_eq!(state.messages.len(), 1);

    // The toolboxes, executables still executable.
    let grab = p.data_dir().join("toolbox/grab/grab");
    assert_eq!(
        fs::metadata(&grab).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let ptool = rp.alpha.join(".viy/toolbox/ptool/ptool");
    assert_eq!(
        fs::metadata(&ptool).unwrap().permissions().mode() & 0o777,
        0o755
    );

    // The histories, in projects marked as a chat marks them; none at home.
    let entries = |root: &Path| {
        viy_term::history::History::load(root.join(".viy/history"))
            .entries()
            .to_vec()
    };
    assert_eq!(entries(&rp.alpha), ["ls", "git status"]);
    assert_eq!(entries(&rp.beta), ["make"]);
    assert_eq!(
        fs::read_to_string(rp.alpha.join(".viy/.gitignore")).unwrap(),
        "history\n"
    );
    assert!(!p.home.join(".viy").exists());

    // The pod state moved; the lock and rp's own records stayed.
    let after = the_paths_after(&rp);
    assert_eq!(after.pod_state_dir(), p.state_dir());
    assert_eq!(
        fs::read_to_string(after.pod_state_file()).unwrap(),
        "{\"pod_id\": \"p1\"}"
    );
    assert!(after.boots_dir().join("b1/record.json").exists());
    assert!(after.known_hosts_file().exists());
    let rp_state = p.rpctl_state_dir();
    for gone in ["pod.json", "last_activity", "known_hosts", "boots"] {
        assert!(!rp_state.join(gone).exists(), "{gone}");
    }
    assert!(rp_state.join("pod.lock").exists());
    assert_eq!(
        fs::read_to_string(rp_state.join("alarms.jsonl")).unwrap(),
        "rp's own\n"
    );

    // The timers swapped.
    assert!(!units.join("rpctl-reap.timer").exists());
    assert!(!units.join("rpctl-reap.service").exists());
    assert!(units.join("viy-reap.timer").exists());

    // rp's files untouched.
    assert_eq!(
        fs::read_to_string(rp.alpha.join(RPCTL_MARKER).join("history")).unwrap(),
        "ls\nls\n\ngit status\n"
    );
    assert!(rp_sessions.join(format!("{A}.jsonl")).exists());

    let record = Record::load(&p.state_dir()).unwrap();
    assert_eq!(record.histories, [rp.alpha.clone(), rp.beta.clone()]);
    assert_eq!(record.finished, Some(1_762_900_000.0));

    // Again, with the pod state where it now is: nothing left to do.
    let rp2 = Rp { paths: after, ..rp };
    let exec = FakeExec::default();
    let (result, lines) = finish(&rp2, &exec, &[], false).await;
    result.unwrap();
    let p = &rp2.paths;
    assert_eq!(
        lines,
        [
            "sessions: viy has all 3 of rp's chats".to_string(),
            "toolbox: viy has every global tool of rp's (there already, kept: grab, keep)"
                .to_string(),
            format!(
                "{}: not imported: viy marks no project in the home folder or above it",
                p.home.display()
            ),
            format!("{}: history imported before", rp2.alpha.display()),
            format!("{}: history imported before", rp2.beta.display()),
            format!(
                "cutover finished: viy keeps the pod state in {}. rp's own files are where they \
                 were; use viy, not rp, for the pod from now on.",
                p.state_dir().display()
            ),
        ]
    );
    assert!(exec.sent().is_empty());
    assert_eq!(entries(&rp2.alpha), ["ls", "git status"]);
    assert_eq!(
        Record::load(&p.state_dir()).unwrap().finished,
        Some(1_762_900_000.0)
    );
}

#[tokio::test]
async fn it_is_refused_with_nothing_done_while_anything_could_race_it() {
    let rp = rp_machine();
    write(&rp.paths.state_dir().join("pod.json"), "{}");
    let before = tree(rp.dir.path());
    let exec = FakeExec::default();
    let (result, _) = finish(&rp, &exec, &[4242, 4343], false).await;
    assert_eq!(
        result.unwrap_err(),
        format!(
            "viy migrate --finish refused; nothing was changed:\n  \
             - {} is there already: viy's state folder holds pod state before the move\n  \
             - a chat is open: close every rp and viy chat first (pid 4242, 4343)",
            rp.paths.state_dir().join("pod.json").display()
        )
    );
    assert_eq!(tree(rp.dir.path()), before);
    assert!(exec.sent().is_empty());

    // The dry run says the same after its plan.
    let (result, lines) = finish(&rp, &exec, &[4242], true).await;
    result.unwrap();
    assert_eq!(
        lines[lines.len() - 3..],
        [
            "and it would be refused now:".to_string(),
            format!(
                "  - {} is there already: viy's state folder holds pod state before the move",
                rp.paths.state_dir().join("pod.json").display()
            ),
            "  - a chat is open: close every rp and viy chat first (pid 4242)".to_string(),
        ]
    );
}

#[tokio::test]
async fn the_reap_timer_must_run_this_viy() {
    let rp = rp_machine();
    let exec = FakeExec::default();
    let open = Open(Vec::new());
    let other = rp.dir.path().join("elsewhere/viy");
    write(&other, "");
    for (on_path, want) in [
        (
            None,
            "viy is not on PATH: install this build first (cargo install --path crates/viy)"
                .to_string(),
        ),
        (
            Some(other.as_path()),
            format!(
                "this is {}, but the reap timer will run the viy on PATH, {}: install this \
                 build there and run that one",
                rp.viy.display(),
                other.display()
            ),
        ),
    ] {
        let machine = Machine {
            paths: &rp.paths,
            cwd: &rp.alpha,
            me: &rp.viy,
            on_path,
            chats: &open,
            exec: &exec,
            now: 0.0,
        };
        let error = run(&machine, false, &mut |_| {}).await.unwrap_err();
        assert_eq!(
            error,
            format!("viy migrate --finish refused; nothing was changed:\n  - {want}")
        );
    }
}

/// A chat rp holds open, or an up holding the pod lock, stops the run at
/// that step; what was done stays done and the next run carries on.
#[tokio::test]
async fn a_lock_held_stops_the_run_at_its_step() {
    let rp = rp_machine();
    let held = File::open(
        rp.paths
            .rpctl_data_dir()
            .join(format!("sessions/{B}.jsonl")),
    )
    .unwrap();
    held.lock().unwrap();
    let exec = FakeExec::default();
    let (result, lines) = finish(&rp, &exec, &[], false).await;
    assert_eq!(
        result.unwrap_err(),
        format!("rp's chat {B} is open: close it and run viy migrate --finish again")
    );
    assert!(lines.is_empty());
    assert!(rp.paths.sessions_dir().join(format!("{A}.jsonl")).exists());
    drop(held);

    // The pod lock is taken before rp's timer goes: with an up running,
    // both stay as they were.
    let lock = PodLock::exclusive(&rp.paths, "up").unwrap();
    let exec = FakeExec::default();
    let (result, _) = finish(&rp, &exec, &[], false).await;
    // The test process is neither rp nor viy, so the holder is called rp.
    assert_eq!(
        result.unwrap_err(),
        format!(
            "Another rp up (pid {}) is already running. Let it finish, or Ctrl-C it, then \
             re-run viy migrate --finish.",
            std::process::id()
        )
    );
    assert!(exec.sent().is_empty());
    assert!(idle::rpctl_timer_installed(&rp.paths));
    assert!(rp.paths.pod_state_file().exists());
    assert!(!Record::load(&rp.paths.state_dir()).unwrap().pod_state_moved);
    drop(lock);
    // A child another test forks in the meantime holds the lock's open
    // file until it execs: wait for it to go.
    for _ in 0..100 {
        if PodLock::is_free(&rp.paths).unwrap() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // A file an earlier run moved already: no refusal, and the rest moves.
    fs::create_dir_all(rp.paths.state_dir()).unwrap();
    fs::rename(
        rp.paths.known_hosts_file(),
        rp.paths.state_dir().join("known_hosts"),
    )
    .unwrap();
    let exec = FakeExec::new([
        Run::stdout(""),
        Run::stdout(""),
        Run::stdout(""),
        Run::stdout(""),
    ]);
    let (result, lines) = finish(&rp, &exec, &[], false).await;
    result.unwrap();
    let want = format!(
        "pod state: moved pod.json, last_activity, boots from {} to {}",
        rp.paths.rpctl_state_dir().display(),
        rp.paths.state_dir().display()
    );
    assert!(lines.contains(&want), "{lines:?}");
    let after = the_paths_after(&rp);
    assert!(after.pod_state_file().exists());
    assert!(after.known_hosts_file().exists());
    assert_eq!(
        viy_term::history::History::load(rp.alpha.join(".viy/history")).entries(),
        ["ls", "git status"]
    );
}

#[tokio::test]
async fn with_no_pod_state_yet_viys_folder_is_where_it_goes() {
    let rp = rp_machine();
    let state = rp.paths.rpctl_state_dir();
    for name in ["pod.json", "last_activity", "known_hosts"] {
        fs::remove_file(state.join(name)).unwrap();
    }
    fs::remove_dir_all(state.join("boots")).unwrap();
    let (result, lines) = finish(&rp, &FakeExec::default(), &[], true).await;
    result.unwrap();
    let want = format!(
        "pod state: nothing to move; would keep it in {} from now on",
        rp.paths.state_dir().display()
    );
    assert!(lines.contains(&want), "{lines:?}");
    let exec = FakeExec::new([
        Run::stdout(""),
        Run::stdout(""),
        Run::stdout(""),
        Run::stdout(""),
    ]);
    let (result, lines) = finish(&rp, &exec, &[], false).await;
    result.unwrap();
    let want = format!(
        "pod state: nothing to move; viy keeps it in {} from now on",
        rp.paths.state_dir().display()
    );
    assert!(lines.contains(&want), "{lines:?}");
    assert!(Record::load(&rp.paths.state_dir()).unwrap().pod_state_moved);
}

/// A tool linked into rp's toolbox is copied as a folder.
#[tokio::test]
async fn a_linked_tool_comes_over() {
    let rp = rp_machine();
    let elsewhere = rp.dir.path().join("dev");
    tool(&elsewhere, "linked");
    std::os::unix::fs::symlink(
        elsewhere.join("linked"),
        rp.paths.rpctl_data_dir().join("toolbox/linked"),
    )
    .unwrap();
    let exec = FakeExec::new([
        Run::stdout(""),
        Run::stdout(""),
        Run::stdout(""),
        Run::stdout(""),
    ]);
    let (result, _) = finish(&rp, &exec, &[], false).await;
    result.unwrap();
    let copy = rp.paths.data_dir().join("toolbox/linked");
    assert!(
        !fs::symlink_metadata(&copy)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(copy.join("linked").is_file());
}

/// A project reached through a symlink in a chat's record and directly
/// from the working directory is one project: its history comes once.
#[tokio::test]
async fn a_project_under_two_names_is_imported_once() {
    let rp = rp_machine();
    let link = rp.dir.path().join("alias");
    std::os::unix::fs::symlink(&rp.alpha, &link).unwrap();
    write(
        &rp.paths
            .rpctl_data_dir()
            .join("sessions/20260723-100000-eeee.jsonl"),
        &log("20260723-100000-eeee", &link),
    );
    let exec = FakeExec::new([
        Run::stdout(""),
        Run::stdout(""),
        Run::stdout(""),
        Run::stdout(""),
    ]);
    let (result, _) = finish(&rp, &exec, &[], false).await;
    result.unwrap();
    assert_eq!(
        viy_term::history::History::load(rp.alpha.join(".viy/history")).entries(),
        ["ls", "git status"]
    );
}
