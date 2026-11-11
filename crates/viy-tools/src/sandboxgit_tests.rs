//! rpctl tests/test_sandboxgit.py RepoTests, SyncTests and ImportTests:
//! real runs through bwrap and gocryptfs, on a vault and a repository in a
//! scratch folder. Skipped where bwrap, gocryptfs or FUSE are missing, as
//! rpctl's are.

use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

pub(crate) fn git(ws: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["-c", "user.name=Me", "-c", "user.email=me@x"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub(crate) fn make_repo(ws: &Path, branch: &str) {
    std::fs::create_dir(ws).unwrap();
    git(ws, &["init", "-q", "-b", branch]);
    // No detached auto-gc after a fetch, still writing when the folder goes.
    git(ws, &["config", "maintenance.auto", "false"]);
    git(ws, &["config", "gc.auto", "0"]);
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();
    git(ws, &["add", "a.txt"]);
    git(ws, &["commit", "-qm", "first"]);
}

/// A real gocryptfs vault and a workspace repository in a scratch folder;
/// the mount is taken down when it goes.
struct Vault {
    scratch: ScratchDir,
    places: Places,
    ws: PathBuf,
    tmp: PathBuf,
    host: Host,
}

impl Drop for Vault {
    fn drop(&mut self) {
        if let Some(fm) = fusermount() {
            let _ = std::process::Command::new(fm)
                .args(["-u", "-z"])
                .arg(&self.places.plain)
                .output();
        }
    }
}

async fn vault() -> Option<Vault> {
    let scratch = ScratchDir::new("sandboxgit").unwrap();
    let root = scratch.path().to_path_buf();
    let host = Host {
        home: root.join("home"),
        user: "tester".into(),
        lang: "C.UTF-8".into(),
        clients: Vec::new(),
    };
    std::fs::create_dir(&host.home).unwrap();
    if let Err(why) = sandbox::available(&host).await {
        eprintln!("skipped: bwrap sandbox unavailable: {why}");
        return None;
    }
    if which("gocryptfs").is_none() || fusermount().is_none() || !Path::new("/dev/fuse").exists() {
        eprintln!("skipped: gocryptfs/FUSE unavailable");
        return None;
    }
    let places = Places {
        cipher: root.join("cipher"),
        plain: root.join("plain"),
        users: root.join("users"),
        lock: root.join("lock"),
    };
    std::fs::create_dir(&places.cipher).unwrap();
    let mut init = std::process::Command::new("gocryptfs")
        .args(["-q", "-init"])
        .arg(&places.cipher)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        init.stdin.take().unwrap().write_all(b"pw").unwrap();
    }
    assert!(init.wait().unwrap().success());
    let ws = root.join("ws");
    make_repo(&ws, "main");
    let tmp = root.join("chat-tmp");
    std::fs::create_dir(&tmp).unwrap();
    Some(Vault {
        scratch,
        places,
        ws,
        tmp,
        host,
    })
}

impl Vault {
    fn runner(&self) -> Runner<'_> {
        Runner {
            host: &self.host,
            launcher: Launcher::RPCTL,
            project_dir: &self.ws,
        }
    }

    async fn prepare(&self) -> PathBuf {
        prepare(
            &self.runner(),
            &self.places,
            &self.ws,
            || async { Ok("pw".to_string()) },
            "model (viy chat)",
        )
        .await
        .unwrap()
    }

    async fn sh(&self, repo: &Path, cmd: &str) -> Ran {
        let argv: Vec<OsString> = vec!["bash".into(), "-c".into(), cmd.into()];
        self.runner()
            .in_repo(&argv, &self.ws, repo, None, Duration::from_secs(60))
            .await
            .unwrap()
    }
}

// ---- RepoTests --------------------------------------------------------------------

#[tokio::test]
async fn model_commits_stay_out_of_the_real_repo() {
    let Some(v) = vault().await else { return };
    std::fs::write(v.ws.join("a.txt"), "one\nmine, uncommitted\n").unwrap();
    let repo = v.prepare().await;
    let r = v
        .sh(
            &repo,
            "git status --short && echo new > b.txt && git add -A && \
             git commit -qm model-work && git log --format='%an|%s'",
        )
        .await;
    assert_eq!(r.exit_code, Some(0), "{}", r.stderr);
    assert!(r.stdout.contains(" M a.txt"), "{}", r.stdout); // the real working tree
    assert!(
        r.stdout.contains("model (viy chat)|model-work"),
        "{}",
        r.stdout
    );
    assert_eq!(
        git(&v.ws, &["log", "--format=%s"])
            .split_whitespace()
            .collect::<Vec<_>>(),
        ["first"]
    );
    assert!(git(&v.ws, &["status", "--short"]).contains("a.txt")); // still unstaged
}

#[tokio::test]
async fn the_real_git_is_read_only_at_host_git() {
    let Some(v) = vault().await else { return };
    let repo = v.prepare().await;
    let r = v
        .sh(
            &repo,
            &format!("git remote get-url host; touch {HOST_GIT}/x"),
        )
        .await;
    assert!(r.stdout.contains(HOST_GIT), "{}", r.stdout);
    assert!(r.stderr.contains("Read-only file system"), "{}", r.stderr);
    let r = v.sh(&repo, "git remote").await;
    assert_eq!(r.stdout.split_whitespace().collect::<Vec<_>>(), ["host"]);
}

#[tokio::test]
async fn plaintext_lives_only_in_the_mount() {
    let Some(v) = vault().await else { return };
    let repo = v.prepare().await;
    assert!(v.places.mounted());
    assert!(repo.join("HEAD").is_file());
    let names: Vec<String> = std::fs::read_dir(&v.places.cipher)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"gocryptfs.conf".to_string()));
    let repo_name = repo.file_name().unwrap().to_string_lossy().into_owned();
    assert!(!names.contains(&repo_name));
    release(&v.places);
    assert!(!v.places.mounted());
    // And it comes back with the same commits.
    v.sh(&v.prepare().await, "git commit -q --allow-empty -m kept")
        .await;
    release(&v.places);
    let r = v.sh(&v.prepare().await, "git log -1 --format=%s").await;
    assert_eq!(r.stdout.trim(), "kept");
}

#[tokio::test]
async fn wrong_password_is_unavailable() {
    let Some(v) = vault().await else { return };
    let got = prepare(
        &v.runner(),
        &v.places,
        &v.ws,
        || async { Ok("wrong".to_string()) },
        "m",
    )
    .await;
    assert!(
        matches!(&got, Err(Unavailable(why)) if why.contains("does not open")),
        "{got:?}"
    );
}

#[tokio::test]
async fn another_live_user_keeps_it_mounted() {
    let Some(v) = vault().await else { return };
    v.prepare().await;
    // SAFETY: getppid has no preconditions.
    let parent = unsafe { libc::getppid() }.to_string();
    std::fs::write(v.places.users.join(&parent), "").unwrap();
    release(&v.places);
    assert!(v.places.mounted());
    std::fs::remove_file(v.places.users.join(&parent)).unwrap();
    release(&v.places);
    assert!(!v.places.mounted());
}

// ---- SyncTests --------------------------------------------------------------------

#[tokio::test]
async fn follows_the_host_until_it_has_commits_of_its_own() {
    let Some(v) = vault().await else { return };
    let repo = v.prepare().await;
    git(&v.ws, &["commit", "-q", "--allow-empty", "-m", "host-2"]);
    v.prepare().await;
    let r = v.sh(&repo, "git log -1 --format=%s").await;
    assert_eq!(r.stdout.trim(), "host-2");
    v.sh(&repo, "git commit -q --allow-empty -m model-1").await;
    git(&v.ws, &["commit", "-q", "--allow-empty", "-m", "host-3"]);
    v.prepare().await;
    let r = v
        .sh(
            &repo,
            "git log -1 --format=%s main; git log -1 --format=%s host/main",
        )
        .await;
    assert_eq!(
        r.stdout.split_whitespace().collect::<Vec<_>>(),
        ["model-1", "host-3"]
    );
}

#[tokio::test]
async fn head_follows_your_branch_switch() {
    let Some(v) = vault().await else { return };
    let repo = v.prepare().await;
    git(&v.ws, &["checkout", "-q", "-b", "feature"]);
    git(
        &v.ws,
        &["commit", "-q", "--allow-empty", "-m", "on-feature"],
    );
    v.prepare().await;
    let r = v
        .sh(
            &repo,
            "git symbolic-ref --short HEAD; git log -1 --format=%s",
        )
        .await;
    assert_eq!(
        r.stdout.split_whitespace().collect::<Vec<_>>(),
        ["feature", "on-feature"]
    );
}

#[tokio::test]
async fn a_detached_head_seeds_the_same_commit() {
    let Some(v) = vault().await else { return };
    git(&v.ws, &["commit", "-q", "--allow-empty", "-m", "second"]);
    git(&v.ws, &["checkout", "-q", "--detach", "HEAD~1"]);
    git(&v.ws, &["branch", "-qf", "main", "HEAD"]); // the tip is on no branch
    git(&v.ws, &["commit", "-q", "--allow-empty", "-m", "loose"]);
    let repo = v.prepare().await;
    let r = v
        .sh(&repo, "git log -1 --format=%s; git status --porcelain")
        .await;
    assert_eq!(r.stdout.split_whitespace().collect::<Vec<_>>(), ["loose"]); // nothing untracked
}

#[tokio::test]
async fn a_repository_without_commits_starts_empty() {
    let Some(v) = vault().await else { return };
    std::fs::remove_dir_all(v.ws.join(".git")).unwrap();
    git(&v.ws, &["init", "-q"]);
    let repo = v.prepare().await;
    let r = v
        .sh(
            &repo,
            "git add -A && git commit -qm model-first && git log --format=%s",
        )
        .await;
    assert_eq!(
        r.stdout.split_whitespace().collect::<Vec<_>>(),
        ["model-first"]
    );
}

// ---- ImportTests ------------------------------------------------------------------

#[tokio::test]
async fn import_brings_branches_in_as_sandbox_refs() {
    let Some(v) = vault().await else { return };
    let repo = v.prepare().await;
    v.sh(
        &repo,
        "git switch -qc idea && echo x > x.txt && git add x.txt && git commit -qm idea-1",
    )
    .await;
    assert_eq!(
        branches(&v.runner(), &v.ws, &repo).await.unwrap(),
        [(1, "idea".to_string()), (0, "main".to_string())]
    );
    let report = import_branches(&v.runner(), &v.ws, &repo, &v.tmp, &[])
        .await
        .unwrap();
    assert!(report.contains("sandbox/idea"), "{report}");
    assert!(!report.contains("sandbox/main"), "{report}"); // nothing new on it
    assert_eq!(
        git(&v.ws, &["log", "-1", "--format=%s", "sandbox/idea"]).trim(),
        "idea-1"
    );
    let bundles: Vec<String> = std::fs::read_dir(&v.tmp)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".bundle"))
        .collect();
    assert_eq!(bundles, Vec::<String>::new()); // the bundle is gone
    // nothing left to import
    let again = import_branches(&v.runner(), &v.ws, &repo, &v.tmp, &["main".into()]).await;
    assert!(again.is_err(), "{again:?}");
}

#[tokio::test]
async fn the_model_repos_hooks_and_config_never_run_on_the_host() {
    let Some(v) = vault().await else { return };
    let repo = v.prepare().await;
    let canary = v.scratch.path().join("ran");
    let hook = format!("#!/bin/sh\ntouch {}\n", canary.display());
    std::fs::create_dir_all(repo.join("hooks")).unwrap();
    for name in [
        "reference-transaction",
        "post-checkout",
        "post-merge",
        "fsmonitor",
    ] {
        let p = repo.join("hooks").join(name);
        std::fs::write(&p, &hook).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    v.sh(
        &repo,
        "git config core.fsmonitor .git/hooks/fsmonitor && git commit -q --allow-empty -m hooked",
    )
    .await;
    let _ = std::fs::remove_file(&canary); // in the sandbox the hooks may run; that is fine
    import_branches(&v.runner(), &v.ws, &repo, &v.tmp, &[])
        .await
        .unwrap();
    git(&v.ws, &["status"]);
    assert!(!canary.exists());
}

#[tokio::test]
async fn thin_bundle_falls_back_to_full() {
    let Some(v) = vault().await else { return };
    let repo = v.prepare().await;
    v.sh(&repo, "git commit -q --allow-empty -m model-1").await;
    // Your history moves on and drops what the model's branch was cut from.
    git(&v.ws, &["checkout", "-q", "--orphan", "fresh"]);
    git(&v.ws, &["commit", "-q", "-m", "rewritten"]);
    git(&v.ws, &["branch", "-D", "main"]);
    git(&v.ws, &["reflog", "expire", "--expire=now", "--all"]);
    git(&v.ws, &["gc", "-q", "--prune=now"]);
    let report = import_branches(&v.runner(), &v.ws, &repo, &v.tmp, &["main".into()])
        .await
        .unwrap();
    assert!(report.contains("sandbox/main"), "{report}");
}

#[test]
fn places_follow_the_runtime_folder() {
    let p = Places::new(
        Path::new("/d/viy"),
        Some(Path::new("/run/user/1")),
        Path::new("/s/viy"),
    );
    assert_eq!(p.cipher, Path::new("/d/viy/sandbox-git"));
    assert_eq!(p.plain, Path::new("/run/user/1/viy/sandbox-git"));
    assert_eq!(p.users, Path::new("/run/user/1/viy/sandbox-git.users"));
    let p = Places::new(Path::new("/d/viy"), None, Path::new("/s/viy"));
    assert_eq!(p.lock, Path::new("/s/viy/run/sandbox-git.lock"));
}

#[test]
fn repo_dirs_are_named_for_the_workspace_and_its_hash() {
    let p = Places::new(Path::new("/d"), None, Path::new("/s"));
    let a = p.repo_dir(Path::new("/nonexistent/proj"));
    let name = a.file_name().unwrap().to_string_lossy().into_owned();
    // rpctl: <name>-<sha256(path)[:12]>
    assert_eq!(name, "proj-be0a9c7890bb");
    assert_eq!(a.parent().unwrap(), p.plain);
}

// ---- BindTests ----------------------------------------------------------------------

/// `(flag, from, to)` for each bind in `argv`.
fn binds(argv: &[OsString]) -> Vec<(String, String, String)> {
    let s: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    (0..s.len().saturating_sub(2))
        .filter(|&i| s[i] == "--bind" || s[i] == "--ro-bind")
        .map(|i| (s[i].clone(), s[i + 1].clone(), s[i + 2].clone()))
        .collect()
}

struct Binds {
    _scratch: ScratchDir,
    ws: PathBuf,
    repo: PathBuf,
    host: Host,
}

fn bind_fixture() -> Binds {
    let scratch = ScratchDir::new("sandboxgit-binds").unwrap();
    let ws = scratch.path().join("ws");
    make_repo(&ws, "main");
    let repo = scratch.path().join("model-repo");
    std::fs::create_dir(&repo).unwrap();
    let host = Host {
        home: scratch.path().join("home"),
        user: "tester".into(),
        lang: "C.UTF-8".into(),
        clients: Vec::new(),
    };
    Binds {
        _scratch: scratch,
        ws,
        repo,
        host,
    }
}

#[test]
fn model_repo_covers_the_real_one_which_moves_to_host_git() {
    let f = bind_fixture();
    let jail = Jail {
        git_dir: Some(&f.repo),
        ..Jail::new(&f.ws, &f.ws)
    };
    let b = binds(&sandbox::bwrap_argv(&f.host, &jail, 3));
    let dot_git = std::fs::canonicalize(&f.ws)
        .unwrap()
        .join(".git")
        .display()
        .to_string();
    let repo = f.repo.display().to_string();
    let real = b
        .iter()
        .position(|x| *x == ("--ro-bind".into(), dot_git.clone(), dot_git.clone()))
        .unwrap();
    let mine = b
        .iter()
        .position(|x| *x == ("--bind".into(), repo.clone(), dot_git.clone()))
        .unwrap();
    assert!(real < mine); // later binds win
    assert!(b.contains(&("--ro-bind".into(), dot_git, HOST_GIT.into())));
}

#[test]
fn readonly_helpers_get_it_read_only() {
    let f = bind_fixture();
    let jail = Jail {
        readonly: true,
        git_dir: Some(&f.repo),
        ..Jail::new(&f.ws, &f.ws)
    };
    let b = binds(&sandbox::bwrap_argv(&f.host, &jail, 3));
    let dot_git = std::fs::canonicalize(&f.ws)
        .unwrap()
        .join(".git")
        .display()
        .to_string();
    assert!(b.contains(&("--ro-bind".into(), f.repo.display().to_string(), dot_git)));
}

#[test]
fn without_it_nothing_changes() {
    let f = bind_fixture();
    let a = sandbox::bwrap_argv(&f.host, &Jail::new(&f.ws, &f.ws), 3);
    assert!(!a.iter().any(|x| x == HOST_GIT));
}

// ---- HooksPathTests -----------------------------------------------------------------
// core.hooksPath inside the workspace (this project's .githooks) runs on
// the host like .git/hooks, so it gets the same protection.

#[test]
fn no_hooks_path() {
    let f = bind_fixture();
    assert_eq!(crate::guard::git_hooks_dir(&f.ws), None);
}

#[test]
fn hooks_path_in_the_workspace_is_protected() {
    let f = bind_fixture();
    let ws = std::fs::canonicalize(&f.ws).unwrap();
    git(&ws, &["config", "core.hooksPath", ".githooks"]);
    let hooks = ws.join(".githooks");
    assert_eq!(crate::guard::git_hooks_dir(&ws), Some(hooks.clone()));
    let why = crate::guard::protected_reason(&hooks.join("pre-commit"), &ws, &ws, &[]);
    assert!(why.is_some_and(|w| w.contains("hooks")));
    assert_eq!(
        crate::guard::protected_reason(&ws.join("a.txt"), &ws, &ws, &[]),
        None
    );
    // Missing: a command that creates it is quarantined.
    assert!(!crate::quarantine::autorun_paths(&ws).contains(&hooks));
    std::fs::create_dir(&hooks).unwrap();
    assert!(crate::quarantine::autorun_paths(&ws).contains(&hooks));
    assert!(sandbox::protected_existing(&ws, &ws, &[]).contains(&hooks));
}

#[test]
fn hooks_path_outside_the_workspace_is_not_ours() {
    let f = bind_fixture();
    let elsewhere = f._scratch.path().join("elsewhere");
    git(
        &f.ws,
        &["config", "core.hooksPath", &elsewhere.display().to_string()],
    );
    assert_eq!(crate::guard::git_hooks_dir(&f.ws), None);
}

// From the WP 8.1 review: the lock is not held while the password is asked
// (a person typing a PIN must not freeze another chat waiting for it).
#[tokio::test]
async fn the_lock_is_free_while_the_password_is_asked() {
    let Some(v) = vault().await else { return };
    let lock = v.places.lock.clone();
    let repo = prepare(
        &v.runner(),
        &v.places,
        &v.ws,
        || async move {
            let f = std::fs::File::open(&lock).unwrap();
            assert!(f.try_lock().is_ok(), "the lock is held while asking");
            drop(f);
            Ok("pw".to_string())
        },
        "m",
    )
    .await
    .unwrap();
    assert!(repo.join("HEAD").is_file());
    // a second prepare asks nothing: it is mounted
    prepare(
        &v.runner(),
        &v.places,
        &v.ws,
        || async { panic!("asked again") },
        "m",
    )
    .await
    .unwrap();
}
