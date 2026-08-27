//! rp and viy take turns on one state directory: rpctl's own `pod.py` and
//! `idle.py` from the pinned snapshot (`rp_pod_state.py`) beside viy's
//! [`crate::lock`] and [`crate::state`]. The gate of implementation plan WP
//! 2.3. Needs python3 and the rpctl checkout: `cargo test -p viy-pod
//! --features oracle`.

use std::io::BufRead;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_config::Paths;
use viy_testkit::oracle::snapshot_dir;
use viy_testkit::scratch::ScratchDir;

use crate::lock::LockError;
use crate::lock::PodLock;
use crate::lock::PreemptNotice;
use crate::lock::tests::holder;
use crate::lock::tests::python_as;
use crate::lock::tests::soon;
use crate::process::Tool;
use crate::state;

/// A home for both tools: rp reads it from the environment, viy from
/// [`Paths`].
struct Home {
    dir: ScratchDir,
    paths: Paths,
    snapshot: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = ScratchDir::new("pod-interop").unwrap();
        let paths = Paths::under(dir.path());
        Self {
            dir,
            paths,
            snapshot: snapshot_dir().unwrap(),
        }
    }

    fn rp(&self, op: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new("python3");
        cmd.args(["-I", "-B"])
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/rp_pod_state.py"))
            .arg(&self.snapshot)
            .arg(op)
            .args(args)
            .current_dir(self.dir.path())
            .env("HOME", &self.paths.home)
            .env("XDG_CONFIG_HOME", &self.paths.config_home)
            .env("XDG_DATA_HOME", &self.paths.data_home)
            .env("XDG_STATE_HOME", &self.paths.state_home)
            .env("XDG_CACHE_HOME", self.dir.path().join(".cache"));
        cmd
    }

    /// rp `op`, run to the end; its stdout's last line.
    fn rp_says(&self, op: &str, args: &[&str]) -> String {
        let out = self.rp(op, args).stdin(Stdio::null()).output().unwrap();
        assert!(
            out.status.success(),
            "rp {op}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        text.lines().last().unwrap_or("").to_string()
    }

    /// `rp <verb>` holding the lock until its stdin is closed.
    fn rp_holding(&self, verb: &str) -> Child {
        let mut child = self
            .rp("hold", &[verb])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, "held\n");
        child
    }

    fn lock_file(&self) -> String {
        std::fs::read_to_string(self.paths.pod_lock_file()).unwrap()
    }
}

fn release(mut child: Child) {
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

#[test]
fn viy_is_refused_while_rp_holds_the_lock() {
    let home = Home::new();
    let rp = home.rp_holding("up");
    let err = PodLock::exclusive(&home.paths, "stop").unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Another rp up (pid {}) is already running. Let it finish, or Ctrl-C it, \
             then re-run viy stop.",
            rp.id()
        )
    );
    assert!(matches!(err, LockError::Busy { holder: Some(h), .. } if h.tool == Some(Tool::Rp)));
    release(rp);
    let _lock = soon(|| PodLock::exclusive(&home.paths, "stop").ok());
    assert_eq!(home.lock_file(), format!("stop {}", std::process::id()));
}

#[test]
fn rp_is_refused_while_viy_holds_the_lock() {
    let home = Home::new();
    let lock = PodLock::exclusive(&home.paths, "up").unwrap();
    assert_eq!(
        home.rp_says("run", &["stop"]),
        format!(
            "Another rp up (pid {}) is already running. Let it finish, or Ctrl-C it, \
             then re-run rp stop.",
            std::process::id()
        )
    );
    drop(lock);
    soon(|| (home.rp_says("run", &["stop"]) == "ran").then_some(()));
}

#[test]
fn rp_reap_waits_while_viy_holds_the_lock_and_viy_while_rp_does() {
    let home = Home::new();
    assert_eq!(home.rp_says("lock_free", &[]), "true");
    let lock = PodLock::exclusive(&home.paths, "up").unwrap();
    assert_eq!(home.rp_says("lock_free", &[]), "false");
    drop(lock);
    soon(|| (home.rp_says("lock_free", &[]) == "true").then_some(()));
    let rp = home.rp_holding("stop");
    assert!(!PodLock::is_free(&home.paths).unwrap());
    release(rp);
    soon(|| PodLock::is_free(&home.paths).unwrap().then_some(()));
}

#[test]
fn viy_down_kills_rp_up_and_takes_the_lock() {
    let home = Home::new();
    let mut rp = home.rp_holding("up");
    let mut notices = Vec::new();
    let done = PodLock::preempt(&home.paths, |n| notices.push(n)).unwrap();
    assert!(!rp.wait().unwrap().success());
    assert_eq!(done.killed, ["up"]);
    assert!(done.lock.is_some());
    assert_eq!(home.lock_file(), format!("down {}", std::process::id()));
    assert!(
        matches!(&notices[..], [PreemptNotice::Killing(h)] if h.tool == Some(Tool::Rp) && h.pid == rp.id())
    );
}

/// F44, fixed in rpctl 8665dfa: `rp down` kills a running `viy up` as it
/// kills an `rp up`, and takes the lock.
#[test]
fn rp_down_kills_viy_up_and_takes_the_lock() {
    let home = Home::new();
    let viy = python_as(home.dir.path(), "viy");
    let mut child = holder(&viy, &home.paths.pod_lock_file(), "up", &[]);
    let out: Value = serde_json::from_str(&home.rp_says("preempt", &[])).unwrap();
    assert!(!child.wait().unwrap().success());
    assert_eq!(out["killed"], json!(["up"]));
    assert_eq!(out["lock_file"], format!("down {}", out["pid"]));
}

#[test]
fn pod_json_reads_the_same_whoever_wrote_it() {
    let home = Home::new();
    let first = json!({"pod_id": "abc123", "created_at": 1761650000.25,
                       "cost_per_hr": 0.69, "tunnel_only": true, "volume_id": "vol9"});
    let second = json!({"tunnel_pid": 4242, "tunnel_local_port": "8000", "pod_id": "def456"});
    let third = json!({"tunnel_pid": null, "last_startup_seconds": 87.3});
    let saves = [&first, &second, &third];
    let viy_save = |v: &Value| {
        let map = v.as_object().unwrap();
        state::save(
            &home.paths,
            map.iter().map(|(k, v)| (k.as_str(), v.clone())),
        )
        .unwrap();
    };
    let rp_save = |v: &Value| {
        home.rp_says("save", &[&v.to_string()]);
    };
    let file = || std::fs::read_to_string(home.paths.pod_state_file()).unwrap();

    for v in saves {
        rp_save(v);
    }
    let rp_wrote = file();
    state::clear(&home.paths).unwrap();
    assert_eq!(home.rp_says("load", &[]), "{}");
    for v in saves {
        viy_save(v);
    }
    assert_eq!(file(), rp_wrote);
    home.rp_says("clear", &[]);
    assert_eq!(
        state::load(&home.paths).unwrap(),
        state::PodState::default()
    );
    viy_save(&first);
    rp_save(&second);
    viy_save(&third);
    assert_eq!(file(), rp_wrote);
    let loaded: Value = serde_json::from_str(&home.rp_says("load", &[])).unwrap();
    assert_eq!(
        &loaded,
        &Value::Object(state::load(&home.paths).unwrap().as_map().clone())
    );
    assert_eq!(loaded["pod_id"], "def456");
}

#[test]
fn rp_and_viy_saving_at_once_lose_nothing() {
    let home = Home::new();
    let n = 300;
    let mut rp = home
        .rp("save_many", &["rp", &n.to_string()])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    // viy saves for as long as rp does, so the two keep meeting on the lock.
    let mut i = 0;
    while rp.try_wait().unwrap().is_none() {
        let k = format!("viy{i}");
        state::save(&home.paths, [(k.as_str(), json!(i))]).unwrap();
        i += 1;
    }
    assert!(rp.wait().unwrap().success());
    let saved = state::load(&home.paths).unwrap();
    assert!(i > 50, "viy saved only {i} times while rp ran");
    assert_eq!(saved.as_map().len(), n + i);
}
