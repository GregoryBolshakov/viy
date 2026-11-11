//! The model's own git repository (rpctl `sandboxgit.py`, rpctl kb d026): a
//! working, writable .git in the sandbox that is never your real one, kept
//! encrypted on this machine only.
//!
//! Your real .git stays read-only to the model ([`crate::sandbox`]). What is
//! in it runs or is used outside the sandbox the next time *you* run git:
//! hooks, core.fsmonitor (a git-aware prompt runs `git status` on every
//! cd), filters, aliases, the URL your next push goes to. So the model gets
//! a repository of its own instead, one per workspace:
//!
//! - inside the sandbox it is bound over `<workspace>/.git`, so git works
//!   as usual against the real working tree. The real .git is read-only at
//!   [`HOST_GIT`], the remote "host". [`SYNC`], at the first command of
//!   each chat, fetches its branches into `refs/remotes/host/*` and moves
//!   the model's branch forward when it has no commits of its own;
//! - local only: its one remote is a local path, and the sandbox has no
//!   credentials. It lives outside the project;
//! - encrypted at rest: all of them live in one gocryptfs directory
//!   ([`Places::cipher`]) whose password is the tok token [`TOKEN`]. The
//!   plaintext view ([`Places::plain`]) is mounted when a command first
//!   needs it and unmounted when the last viy chat using it exits. While
//!   mounted any process of yours can read it: this protects the disk, not
//!   a live session;
//! - the host never runs git in it. Its config and hooks are the model's,
//!   so every git command on it runs inside the sandbox. Its commits reach
//!   your repository only as a bundle ([`import_branches`]), which is data.
//!
//! viy keeps its own encrypted directory and runtime folder (its data and
//! runtime folders, not rp's) until cutover, under the same tok token
//! (kb d128).

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::future::Future;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use rand::TryRngCore;
use rand::rngs::OsRng;
use sha2::Digest;
use sha2::Sha256;
use tokio::io::AsyncWriteExt;
use viy_config::vault::Tok;
use viy_config::vault::find_executable;

use crate::sandbox;
pub use crate::sandbox::HOST_GIT;
use crate::sandbox::Host;
use crate::sandbox::Jail;
use crate::sandbox::Launcher;
use crate::sandbox::Ran;

/// The tok token whose value opens [`Places::cipher`]; rp's name, shared
/// with rp's own directory.
pub const TOKEN: &str = "rpctl-sandbox-git";
/// Where `/git import` puts the model's branches in your repository.
pub const IMPORT_PREFIX: &str = "refs/remotes/sandbox/";
pub const AUTHOR_EMAIL: &str = "viy-chat@sandbox.invalid";
/// gocryptfs' exit code for a wrong password.
const WRONG_PASSWORD: i32 = 12;

/// Runs in the sandbox with the model's repository at .git. `$1` is
/// [`HOST_GIT`], `$2` and `$3` the author for a repository that has none.
/// A branch the host has follows it while the model's copy has no commits
/// of its own (and, if HEAD is on it, nothing staged); HEAD follows the
/// host's branch while where it stands holds nothing of the model's. A
/// host on a detached HEAD gives a new repository the same detached HEAD,
/// and nothing more. `git reset` (mixed) only resets the index to HEAD: the
/// working tree is the real one and is never touched.
pub const SYNC: &str = r#"
set -e
H=$1
[ -f .git/HEAD ] || git -c init.defaultBranch=main init -q
git config user.name >/dev/null || git config user.name "$2"
git config user.email >/dev/null || git config user.email "$3"
git config remote.host.url "$H"
git config --replace-all remote.host.fetch '+refs/heads/*:refs/remotes/host/*'
git fetch -q --no-tags --prune host
b=$(sed -n 's|^ref: refs/heads/||p' "$H/HEAD")
if [ -z "$b" ]; then
  sha=$(sed -n '1{/^[0-9a-f]\{40,64\}$/p}' "$H/HEAD")
  if [ -n "$sha" ] && ! git rev-parse -q --verify HEAD >/dev/null; then
    git cat-file -e "$sha^{commit}" 2>/dev/null || git fetch -q --no-tags host "$sha" || exit 0
    git update-ref --no-deref HEAD "$sha"
    git reset -q
  fi
  exit 0
fi
new=$(git rev-parse -q --verify "refs/remotes/host/$b^{commit}") || exit 0
cur=$(git symbolic-ref -q --short HEAD) || cur=
here=$(git rev-parse -q --verify HEAD) || here=
clean=yes
if [ -n "$here" ] && ! git diff --cached --quiet; then clean=; fi
old=$(git rev-parse -q --verify "refs/heads/$b") || old=
if [ -z "$old" ] || { [ "$old" != "$new" ] && git merge-base --is-ancestor "$old" "$new"; }; then
  if [ "$cur" != "$b" ] || [ -n "$clean" ]; then
    git update-ref "refs/heads/$b" "$new"
    if [ "$cur" = "$b" ]; then git reset -q; fi
  fi
fi
if [ -n "$cur" ] && [ "$cur" != "$b" ] && [ -n "$clean" ]; then
  mine=$(git rev-parse -q --verify "refs/heads/$cur") || mine=
  theirs=$(git rev-parse -q --verify "refs/remotes/host/$cur") || theirs=
  if [ -z "$mine" ] || [ "$mine" = "$theirs" ]; then
    git symbolic-ref HEAD "refs/heads/$b"
    git reset -q
  fi
fi
"#;

/// `<commits the host lacks> <branch>` per branch.
pub const BRANCHES: &str = r#"
for r in $(git for-each-ref --format='%(refname:short)' refs/heads); do
  printf '%s %s\n' "$(git rev-list --count "refs/heads/$r" --not --remotes=host)" "$r"
done
"#;

/// `$1` the bundle to write, `$2` non-empty for a full bundle, then branch
/// names (none: every branch with commits the host lacks).
pub const BUNDLE: &str = r#"
set -e
f=$1; full=$2; shift 2
if [ $# -eq 0 ]; then
  for r in $(git for-each-ref --format='%(refname)' refs/heads); do
    [ "$(git rev-list --count "$r" --not --remotes=host)" = 0 ] || set -- "$@" "$r"
  done
  [ $# -gt 0 ] || { echo "no branch has commits your repository lacks" >&2; exit 3; }
else
  n=$#
  while [ "$n" -gt 0 ]; do set -- "$@" "refs/heads/$1"; shift; n=$((n - 1)); done
fi
if [ -n "$full" ]; then git bundle create -q "$f" "$@"
else git bundle create -q "$f" "$@" --not --remotes=host; fi
"#;

/// The model's repository cannot be used; .git stays read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

fn unavailable(text: impl Into<String>) -> Unavailable {
    Unavailable(text.into())
}

/// Where the encrypted repositories and their mount live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Places {
    /// The gocryptfs directory: ciphertext only.
    pub cipher: PathBuf,
    /// Where it is mounted, in plaintext.
    pub plain: PathBuf,
    /// One file per viy process using the mount.
    pub users: PathBuf,
    pub lock: PathBuf,
}

impl Places {
    /// viy's: `<data>/sandbox-git`, mounted under `$XDG_RUNTIME_DIR/viy`
    /// (else `<state>/run`), as rp's are under its own folders.
    pub fn new(data_dir: &Path, runtime_dir: Option<&Path>, state_dir: &Path) -> Self {
        let run = runtime_dir.map_or_else(|| state_dir.join("run"), |r| r.join("viy"));
        Self {
            cipher: data_dir.join("sandbox-git"),
            plain: run.join("sandbox-git"),
            users: run.join("sandbox-git.users"),
            lock: run.join("sandbox-git.lock"),
        }
    }

    /// Whether the encrypted directory has been set up (`/git init`).
    pub fn initialized(&self) -> bool {
        self.cipher.join("gocryptfs.conf").is_file()
    }

    /// Whether the plaintext view is mounted (Python's `os.path.ismount`:
    /// another device than its parent's, or the same inode).
    pub fn mounted(&self) -> bool {
        let (Ok(me), Some(Ok(up))) = (
            std::fs::symlink_metadata(&self.plain),
            self.plain.parent().map(std::fs::symlink_metadata),
        ) else {
            return false;
        };
        !me.file_type().is_symlink() && (me.dev() != up.dev() || me.ino() == up.ino())
    }

    /// The model's repository for `ws`, inside the mount. Not named .git,
    /// so git with `safe.bareRepository=explicit` does not pick it up by
    /// cd.
    pub fn repo_dir(&self, ws: &Path) -> PathBuf {
        let ws = std::fs::canonicalize(ws).unwrap_or_else(|_| ws.to_path_buf());
        let name = ws
            .file_name()
            .map_or_else(|| "root".into(), |n| n.to_string_lossy().into_owned());
        let digest = Sha256::digest(ws.as_os_str().as_encoded_bytes());
        self.plain.join(format!("{name}-{}", hex(&digest[..6])))
    }
}

/// Whether the workspace has a real .git directory to stand in for.
pub fn applies(ws: &Path) -> bool {
    std::fs::symlink_metadata(ws.join(".git")).is_ok_and(|m| m.is_dir())
}

fn which(name: &str) -> Option<PathBuf> {
    find_executable(std::env::var_os("PATH").as_deref(), name)
}

pub fn fusermount() -> Option<PathBuf> {
    which("fusermount3").or_else(|| which("fusermount"))
}

/// [`TOKEN`]'s tier: `Some(true)` open, `Some(false)` PIN-gated, `None`
/// when there is no such token (or no tok).
pub fn token_state(tok: &Tok) -> Option<bool> {
    tok.clone().timed(Duration::from_secs(30)).state(TOKEN)
}

/// Why the workspace cannot get its own repository, or `None`.
pub fn problem(ws: &Path, places: &Places, tok: &Tok) -> Option<String> {
    if !applies(ws) {
        return Some("the workspace has no .git directory".into());
    }
    let mut missing: Vec<&str> = ["gocryptfs", "git"]
        .into_iter()
        .filter(|t| which(t).is_none())
        .collect();
    if fusermount().is_none() {
        missing.push("fusermount");
    }
    if !missing.is_empty() {
        return Some(format!("{} not installed", missing.join(", ")));
    }
    if !places.initialized() || token_state(tok).is_none() {
        return Some("not set up yet (/git init)".into());
    }
    None
}

/// [`TOKEN`]'s value from tok, the PIN in tok's environment for a
/// PIN-gated one. Never printed; it goes to gocryptfs' stdin.
pub fn read_password(tok: &Tok, pin: Option<&str>) -> Result<String, Unavailable> {
    tok.clone()
        .timed(Duration::from_secs(60))
        .get(TOKEN, pin)
        .map_err(|e| unavailable(format!("tok could not read {TOKEN}: {e}")))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `n` bytes from the system's randomness, as hex.
fn random_hex(n: usize) -> Result<String, Unavailable> {
    let mut bytes = vec![0u8; n];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| unavailable(format!("no randomness: {e}")))?;
    Ok(hex(&bytes))
}

/// The last non-empty line of `stderr`, else the exit code.
fn last(stderr: &str, code: Option<i32>) -> String {
    stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(
            || match code {
                Some(c) => format!("exit {c}"),
                None => "timed out".into(),
            },
            str::to_string,
        )
}

/// `argv` with `input` on stdin, within `timeout`: (exit code, stderr);
/// the code is `None` when it ran over and was killed.
async fn run_input(
    argv: &[&std::ffi::OsStr],
    input: &str,
    timeout: Duration,
) -> std::io::Result<(Option<i32>, String)> {
    let mut child = tokio::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes()).await;
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(out) => {
            let out = out?;
            Ok((
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ))
        }
        Err(_) => Ok((None, String::new())),
    }
}

/// Creates the encrypted directory. Uses [`TOKEN`] when tok has it
/// (`have`, from [`token_state`]; read with `pin` when PIN-gated), else adds
/// a random one (`open`: readable without the PIN), which needs `pin`. tok
/// runs off the async runtime: each call can take seconds of scrypt.
pub async fn init_vault(
    places: &Places,
    tok: &Tok,
    have: bool,
    pin: Option<&str>,
    open: bool,
) -> Result<(), Unavailable> {
    if places.initialized() {
        return Err(unavailable(format!(
            "{} is already set up",
            places.cipher.display()
        )));
    }
    if std::fs::read_dir(&places.cipher).is_ok_and(|mut d| d.next().is_some()) {
        return Err(unavailable(format!(
            "{} exists and is not empty",
            places.cipher.display()
        )));
    }
    let pin = pin.map(str::to_string);
    let pw = if have {
        let (tok, pin) = (tok.clone(), pin.clone());
        tokio::task::spawn_blocking(move || read_password(&tok, pin.as_deref()))
            .await
            .unwrap_or_else(|e| Err(unavailable(e.to_string())))?
    } else {
        random_hex(32)?
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&places.cipher)
        .map_err(|e| unavailable(format!("{}: {e}", places.cipher.display())))?;
    let gocryptfs = which("gocryptfs").unwrap_or_else(|| "gocryptfs".into());
    let argv = [
        gocryptfs.as_os_str(),
        "-q".as_ref(),
        "-init".as_ref(),
        places.cipher.as_os_str(),
    ];
    let ran = run_input(&argv, &pw, Duration::from_secs(120)).await;
    match ran {
        Ok((Some(0), _)) => {}
        Ok((code, err)) => {
            let _ = std::fs::remove_dir_all(&places.cipher);
            return Err(unavailable(format!(
                "gocryptfs -init failed: {}",
                last(&err, code)
            )));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&places.cipher);
            return Err(unavailable(format!("gocryptfs -init failed: {e}")));
        }
    }
    if have {
        return Ok(());
    }
    let added = match pin {
        Some(pin) => {
            let tok = tok.clone().timed(Duration::from_secs(60));
            tokio::task::spawn_blocking(move || tok.add(TOKEN, &pw, open, &pin))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        }
        None => Err("adding a token needs the PIN".into()),
    };
    added.map_err(|e| {
        // Without the key the new directory is useless: start over next time.
        let _ = std::fs::remove_dir_all(&places.cipher);
        unavailable(format!("tok add failed: {e}"))
    })
}

/// The mount's lock file, open; `lock_file` or `try_lock` takes the flock.
fn lock_file(places: &Places) -> Result<File, Unavailable> {
    let err = |e: std::io::Error| unavailable(format!("{}: {e}", places.lock.display()));
    if let Some(dir) = places.lock.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(err)?;
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&places.lock)
        .map_err(err)?;
    Ok(f)
}

/// The mount's lock (flock), held while the mount and its users change;
/// waited for off the async runtime, as another chat may hold it.
async fn lock(places: &Places) -> Result<File, Unavailable> {
    let places = places.clone();
    tokio::task::spawn_blocking(move || {
        let f = lock_file(&places)?;
        f.lock()
            .map_err(|e| unavailable(format!("{}: {e}", places.lock.display())))?;
        Ok(f)
    })
    .await
    .unwrap_or_else(|e| Err(unavailable(e.to_string())))
}

/// Mounts the plaintext view unless it is, and counts this process as its
/// user. `password` is asked only when a mount is needed.
///
/// The password is asked without the lock held (a person may take a while
/// to type a PIN, and another chat would wait for them); the lock is taken
/// again to mount, unless another chat has mounted it meanwhile.
pub async fn mount<F, Fut>(places: &Places, password: F) -> Result<(), Unavailable>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<String, Unavailable>>,
{
    let mut password = Some(password);
    let mut pw: Option<String> = None;
    let _held = loop {
        let held = lock(places).await?;
        if places.mounted() {
            break held;
        }
        let Some(pw) = pw.take() else {
            drop(held);
            let ask = password
                .take()
                .ok_or_else(|| unavailable("the password was asked already"))?;
            pw = Some(ask().await?);
            continue;
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&places.plain)
            .map_err(|e| unavailable(format!("{}: {e}", places.plain.display())))?;
        let gocryptfs = which("gocryptfs").unwrap_or_else(|| "gocryptfs".into());
        let argv = [
            gocryptfs.as_os_str(),
            "-q".as_ref(),
            places.cipher.as_os_str(),
            places.plain.as_os_str(),
        ];
        match run_input(&argv, &pw, Duration::from_secs(60)).await {
            Ok((Some(0), _)) => {}
            Ok((Some(WRONG_PASSWORD), _)) => {
                return Err(unavailable(format!(
                    "{TOKEN} does not open {}",
                    places.cipher.display()
                )));
            }
            Ok((code, err)) => {
                return Err(unavailable(format!(
                    "gocryptfs failed: {}",
                    last(&err, code)
                )));
            }
            Err(e) => return Err(unavailable(format!("gocryptfs failed: {e}"))),
        }
        break held;
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&places.users)
        .and_then(|()| File::create(places.users.join(std::process::id().to_string())).map(drop))
        .map_err(|e| unavailable(format!("{}: {e}", places.users.display())))
}

/// The users still alive; a file left by a chat that crashed is removed.
fn live_users(places: &Places) -> usize {
    let Ok(entries) = std::fs::read_dir(&places.users) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.filter_map(Result::ok) {
        let Some(pid) = e
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<libc::pid_t>().ok())
        else {
            let _ = std::fs::remove_file(e.path());
            continue;
        };
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if alive {
            n += 1;
        } else {
            let _ = std::fs::remove_file(e.path());
        }
    }
    n
}

/// This process is done with the mount: unmounts it unless another live
/// process still uses it. Never fails; a busy mount stays. It runs where a
/// chat ends (its Drop) and so waits at most a second for the lock: a chat
/// holding it longer is mounting, and will be a user of the mount.
pub fn release(places: &Places) {
    let _ = std::fs::remove_file(places.users.join(std::process::id().to_string()));
    let Ok(held) = lock_file(places) else {
        return;
    };
    // Briefly: a child forked elsewhere in this process shares an open lock
    // until it execs.
    let mut locked = false;
    for _ in 0..100 {
        if held.try_lock().is_ok() {
            locked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !locked {
        return;
    }
    if places.mounted()
        && live_users(places) == 0
        && let Some(fm) = fusermount()
    {
        let child = std::process::Command::new(fm)
            .arg("-u")
            .arg(&places.plain)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(child) = child {
            let _ = viy_config::vault::wait_with_timeout(child, Duration::from_secs(30));
        }
    }
}

/// How to run a command in the sandbox for the chat: its host, launcher
/// and project folder.
#[derive(Debug, Clone, Copy)]
pub struct Runner<'a> {
    pub host: &'a Host,
    pub launcher: Launcher<'a>,
    pub project_dir: &'a Path,
}

impl Runner<'_> {
    /// `argv` in the sandbox with the model's repository `repo` as .git.
    pub async fn in_repo(
        &self,
        argv: &[OsString],
        ws: &Path,
        repo: &Path,
        tmp_dir: Option<&Path>,
        timeout: Duration,
    ) -> Result<Ran, Unavailable> {
        let jail = Jail {
            tmp_dir,
            git_dir: Some(repo),
            ..Jail::new(ws, self.project_dir)
        };
        sandbox::run_with(self.host, &jail, argv, timeout, None, self.launcher)
            .await
            .map_err(|e| unavailable(format!("the sandbox failed: {e}")))
    }
}

fn sh(script: &str, name: &str, args: &[&std::ffi::OsStr]) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec!["sh".into(), "-c".into(), script.into(), name.into()];
    argv.extend(args.iter().map(|a| a.to_os_string()));
    argv
}

const IN_REPO_TIMEOUT: Duration = Duration::from_secs(120);

/// Mounts, creates and syncs the model's repository for `ws`; its path,
/// for the sandbox's `git_dir`.
pub async fn prepare<F, Fut>(
    run: &Runner<'_>,
    places: &Places,
    ws: &Path,
    password: F,
    author: &str,
) -> Result<PathBuf, Unavailable>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<String, Unavailable>>,
{
    let ws = std::fs::canonicalize(ws).unwrap_or_else(|_| ws.to_path_buf());
    if !applies(&ws) {
        return Err(unavailable("the workspace has no .git directory"));
    }
    mount(places, password).await?;
    let repo = places.repo_dir(&ws);
    match std::fs::create_dir(&repo) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(unavailable(format!("{}: {e}", repo.display()))),
    }
    let argv = sh(
        SYNC,
        "sync",
        &[HOST_GIT.as_ref(), author.as_ref(), AUTHOR_EMAIL.as_ref()],
    );
    let r = run
        .in_repo(&argv, &ws, &repo, None, IN_REPO_TIMEOUT)
        .await?;
    if r.exit_code != Some(0) {
        return Err(unavailable(format!(
            "syncing it with your repository failed: {}",
            last(&r.stderr, r.exit_code)
        )));
    }
    Ok(repo)
}

/// `[(commits the host lacks, branch)]` in the model's repository. The
/// names are the model's: untrusted text.
pub async fn branches(
    run: &Runner<'_>,
    ws: &Path,
    repo: &Path,
) -> Result<Vec<(u64, String)>, Unavailable> {
    let r = run
        .in_repo(
            &sh(BRANCHES, "branches", &[]),
            ws,
            repo,
            None,
            IN_REPO_TIMEOUT,
        )
        .await?;
    if r.exit_code != Some(0) {
        return Err(unavailable(last(&r.stderr, r.exit_code)));
    }
    Ok(r.stdout
        .lines()
        .filter_map(|line| {
            let (n, name) = line.split_once(' ')?;
            let digits = !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
            (digits && !name.is_empty()).then(|| Some((n.parse().ok()?, name.to_string())))?
        })
        .collect())
}

/// Brings the model's branches (`names`, or every one with commits your
/// repository lacks) into your repository as `refs/remotes/sandbox/*`:
/// git fetch's report (untrusted names in it).
///
/// The bundle is made in the sandbox; your repository only ever reads it as
/// a file, with object checks on and your hooks off for the fetch.
pub async fn import_branches(
    run: &Runner<'_>,
    ws: &Path,
    repo: &Path,
    tmp_dir: &Path,
    names: &[String],
) -> Result<String, Unavailable> {
    let name = format!("viy-import-{}.bundle", random_hex(6)?);
    let host_bundle = tmp_dir.join(&name);
    let out = async {
        for full in ["", "full"] {
            let mut args: Vec<&std::ffi::OsStr> = Vec::new();
            let inside = format!("/tmp/{name}");
            args.push(inside.as_ref());
            args.push(full.as_ref());
            args.extend(names.iter().map(|n| std::ffi::OsStr::new(n.as_str())));
            let r = run
                .in_repo(
                    &sh(BUNDLE, "bundle", &args),
                    ws,
                    repo,
                    Some(tmp_dir),
                    IN_REPO_TIMEOUT,
                )
                .await?;
            if r.exit_code != Some(0) {
                return Err(unavailable(last(&r.stderr, r.exit_code)));
            }
            let (code, err) = fetch_bundle(ws, &host_bundle).await;
            if code == Some(0) {
                return Ok(err);
            }
            // A thin bundle needs the commits it was cut from; your history
            // may have moved on since the last sync. Try once in full.
            if !full.is_empty() || !err.contains("prerequisite") {
                return Err(unavailable(format!(
                    "git fetch failed: {}",
                    last(&err, code)
                )));
            }
        }
        Err(unavailable("git fetch failed"))
    }
    .await;
    let _ = std::fs::remove_file(&host_bundle);
    out
}

/// Your repository fetches the bundle file: object checks on, hooks off.
async fn fetch_bundle(ws: &Path, bundle: &Path) -> (Option<i32>, String) {
    let refspec = format!("+refs/heads/*:{IMPORT_PREFIX}*");
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(ws)
        .args([
            "-c",
            "transfer.fsckObjects=true",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(["fetch", "--no-tags", "--no-write-fetch-head"])
        .arg(bundle)
        .arg(refspec)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let ran = tokio::task::spawn_blocking(move || {
        let child = cmd.spawn()?;
        viy_config::vault::wait_with_timeout(child, Duration::from_secs(300))
    })
    .await;
    match ran {
        Ok(Ok(out)) => (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Ok(Err(e)) => (Some(-1), e.to_string()),
        Err(e) => (Some(-1), e.to_string()),
    }
}

/// Deletes the model's repository for `ws` (mounted); the next prepare
/// seeds a new one from yours.
pub fn discard(places: &Places, ws: &Path) -> std::io::Result<()> {
    let repo = places.repo_dir(ws);
    if repo.exists() {
        std::fs::remove_dir_all(repo)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "sandboxgit_tests.rs"]
mod tests;
