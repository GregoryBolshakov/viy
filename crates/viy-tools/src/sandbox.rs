//! The bubblewrap sandbox run_shell's commands run in (rpctl sandbox.py
//! `bwrap_argv`, `_protected_existing`, `net_mode`, `sandbox_available`,
//! `run`). The sandbox is the security boundary for run_shell: what the
//! approval layer decides on top of it is about intent; this is what a
//! command physically cannot do.
//!
//! What a sandboxed command sees:
//! - /usr, /etc, /opt read-only; nothing else from the host filesystem;
//! - `$HOME` an empty in-memory folder, so ~/.ssh, ~/.config, token caches
//!   and other projects are absent (the toolbox is bound read-only when the
//!   caller passes it); `/tmp` the chat's own folder when the caller passes
//!   one ([`ChatTmp`]), else an empty in-memory one;
//! - the workspace read-write, except the protected paths (the clients'
//!   code, `.git`, `.venv`, git's core.hooksPath, ...: [`protected_existing`])
//!   bound read-only, and secret files (env files, private keys, credential
//!   files) masked with an empty file;
//! - with `git_dir`, the model's own repository read-write at
//!   `<workspace>/.git`, over the real one, which is read-only at
//!   [`HOST_GIT`];
//! - the folders the user lent with `/mount`, at their own path, read-only
//!   unless the user chose rw;
//! - no /run (no ssh-agent, session bus, keyring or systemd --user sockets);
//!   its own pid namespace and /proc; every process it started dies with it;
//! - an empty environment apart from PATH, HOME, LANG and a few more;
//! - its own network namespace with only a loopback: no network unless the
//!   call was escalated. Escalated ([`Net::Proxy`], the default), the only
//!   way out is the egress proxy on 127.0.0.1:3128 inside, served by the
//!   client; [`Net::Host`] (SANDBOX_NET=host) shares the host's network.
//!   In every mode the seccomp filter ([`seccomp`]) refuses Unix and vsock
//!   sockets.
//!
//! Where rpctl names itself, viy protects both tools' names and code (kb
//! d014); the names a command sees (`RPCTL_SANDBOX`, `/run/rpctl/host.git`,
//! the launcher's `rpctl:` lines) stay rpctl's until cutover (flags F21).

pub mod launch;
pub mod seccomp;
mod tmp;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub use launch::Egress;
pub use launch::NetSetupError;
pub use tmp::ChatTmp;
use viy_exec::sandboxed;

use crate::guard;
use crate::mounts;
use crate::mounts::Mount;
use crate::resolve::resolve;

/// The PATH a sandboxed command starts with, before the toolbox's.
pub const SANDBOX_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Where the egress proxy listens inside a proxied sandbox.
pub const PROXY_HOST: [u8; 4] = [127, 0, 0, 1];
pub const PROXY_PORT: u16 = 3128;
pub const PROXY_URL: &str = "http://127.0.0.1:3128";

/// Where a sandbox with its own repository sees the real `.git`.
pub const HOST_GIT: &str = "/run/rpctl/host.git";

/// What a proxied command's environment says about the proxy
/// (netaccess/stage6-egress-proxy.md section 2): NO_PROXY empty so nothing
/// bypasses it; npm reads its own names first; NODE_USE_ENV_PROXY makes
/// Node's fetch honour the rest.
pub const PROXY_ENV: [(&str, &str); 11] = [
    ("HTTP_PROXY", PROXY_URL),
    ("HTTPS_PROXY", PROXY_URL),
    ("http_proxy", PROXY_URL),
    ("https_proxy", PROXY_URL),
    ("ALL_PROXY", PROXY_URL),
    ("all_proxy", PROXY_URL),
    ("npm_config_proxy", PROXY_URL),
    ("npm_config_https_proxy", PROXY_URL),
    ("NO_PROXY", ""),
    ("no_proxy", ""),
    ("NODE_USE_ENV_PROXY", "1"),
];

/// How many secret files are masked in one tree, as rpctl.
pub const SECRET_LIMIT: usize = 200;

/// A sandboxed command's network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Net {
    /// A loopback only.
    Off,
    /// The egress proxy only.
    Proxy,
    /// The host's network: its loopback and the LAN too.
    Host,
}

/// The network a SANDBOX_NET value asks for (rp's `net_mode`): empty is
/// none, `host` the host's, anything else the proxy, so an unknown value
/// never widens access.
pub fn net_mode(value: &str) -> Net {
    match value {
        "" => Net::Off,
        "host" => Net::Host,
        _ => Net::Proxy,
    }
}

/// What the sandbox takes from the machine: the same for every command of
/// a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// `$HOME`, empty inside.
    pub home: PathBuf,
    /// `$USER` and `$LANG` inside, from this process's.
    pub user: String,
    pub lang: String,
    /// The clients' own code ([`guard::client_paths`]), bound read-only
    /// where it lies in the workspace.
    pub clients: Vec<PathBuf>,
}

impl Host {
    /// For `home`, with USER and LANG from the environment ("user" and
    /// "C.UTF-8" without them) and the clients' paths.
    pub fn new(home: PathBuf, clients: Vec<PathBuf>) -> Self {
        let var =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
        Self {
            home,
            user: var("USER", "user"),
            lang: var("LANG", "C.UTF-8"),
            clients,
        }
    }
}

/// One command's sandbox.
#[derive(Debug, Clone, Copy)]
pub struct Jail<'a> {
    pub workspace: &'a Path,
    pub project_dir: &'a Path,
    pub net: Net,
    /// The workspace and rw mounts read-only too: the explore helpers
    /// only look.
    pub readonly: bool,
    /// The chat's /tmp ([`ChatTmp`]); an empty one without.
    pub tmp_dir: Option<&'a Path>,
    /// Host folders bound read-only at their own path (the toolbox).
    pub ro_paths: &'a [PathBuf],
    /// Appended to PATH, after the system folders.
    pub path_extra: &'a [PathBuf],
    pub mounts: &'a [Mount],
    /// The model's own repository, over the workspace's `.git`.
    pub git_dir: Option<&'a Path>,
}

impl<'a> Jail<'a> {
    /// A sandbox for `workspace` with nothing else: no network, no mounts.
    pub fn new(workspace: &'a Path, project_dir: &'a Path) -> Self {
        Self {
            workspace,
            project_dir,
            net: Net::Off,
            readonly: false,
            tmp_dir: None,
            ro_paths: &[],
            path_extra: &[],
            mounts: &[],
            git_dir: None,
        }
    }
}

/// The entries of `dir` that are folders, a link to a folder counting as
/// one (as `os.walk` sorts them): (path, name, is a link).
fn subdirs(dir: &Path) -> Vec<(PathBuf, String, bool)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let kind = e.file_type().ok()?;
            let path = e.path();
            // Only a link needs the stat that follows it.
            let link = kind.is_symlink();
            (kind.is_dir() || link && path.is_dir())
                .then(|| (path, e.file_name().to_string_lossy().into_owned(), link))
        })
        .collect()
}

/// The existing paths in workspace `ws` to bind read-only (rp's
/// `_protected_existing`): the clients' code inside it; at the workspace
/// root, and at the project root when that is inside the workspace, every
/// protected name, git's hooks folder and `*.egg-info`; and a protected
/// folder anywhere below (a vendored repository's `.git`, a subproject's
/// venv). Resolved, parents before children.
pub fn protected_existing(ws: &Path, project_dir: &Path, clients: &[PathBuf]) -> Vec<PathBuf> {
    let ws = resolve(ws);
    let project_dir = resolve(project_dir);
    // bwrap refuses a bind whose source is missing, and viy's list names
    // build files a tree may not have.
    let mut out: Vec<PathBuf> = clients
        .iter()
        .filter(|c| c.starts_with(&ws) && c.exists())
        .cloned()
        .collect();
    let mut roots = vec![ws.clone()];
    if project_dir != ws {
        roots.push(project_dir);
    }
    for root in roots.iter().filter(|r| r.starts_with(&ws)) {
        for name in guard::PROTECTED_DIRS
            .iter()
            .chain(&guard::PROTECTED_ROOT_FILES)
        {
            let p = root.join(name);
            if p.exists() {
                out.push(p);
            }
        }
        if let Some(hooks) = guard::git_hooks_dir(root)
            && hooks.exists()
        {
            out.push(hooks);
        }
        // As pathlib's glob: hidden names and broken links match too.
        if let Ok(entries) = fs::read_dir(root) {
            out.extend(
                entries
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".egg-info"))
                    .map(|e| e.path()),
            );
        }
    }
    // Nested protected folders; links to folders are not walked into.
    let mut stack = vec![ws];
    while let Some(dir) = stack.pop() {
        for (path, name, link) in subdirs(&dir) {
            if guard::PROTECTED_DIRS.contains(&name.as_str()) {
                out.push(path);
            } else if !link && !mounts::WALK_SKIP.contains(&name.as_str()) {
                stack.push(path);
            }
        }
    }
    let unique: BTreeSet<PathBuf> = out.iter().map(|p| resolve(p)).collect();
    let mut sorted: Vec<PathBuf> = unique.into_iter().collect();
    sorted.sort_by_key(|p| p.components().count());
    sorted
}

/// bwrap's command line up to the `--` before the command (rp's
/// `bwrap_argv`). `seccomp_fd` is the descriptor bwrap reads the filter
/// from.
pub fn bwrap_argv(host: &Host, jail: &Jail<'_>, seccomp_fd: i32) -> Vec<OsString> {
    let ws = resolve(jail.workspace);
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |items: &[&dyn AsRef<std::ffi::OsStr>]| {
        a.extend(items.iter().map(|s| s.as_ref().to_os_string()));
    };
    let fd = seccomp_fd.to_string();
    push(&[
        &"bwrap",
        &"--unshare-all",
        &"--die-with-parent",
        &"--new-session",
        &"--seccomp",
        &fd,
        &"--ro-bind",
        &"/usr",
        &"/usr",
        &"--symlink",
        &"usr/bin",
        &"/bin",
        &"--symlink",
        &"usr/sbin",
        &"/sbin",
        &"--symlink",
        &"usr/lib",
        &"/lib",
        &"--symlink",
        &"usr/lib64",
        &"/lib64",
        &"--ro-bind",
        &"/etc",
        &"/etc",
        &"--ro-bind-try",
        &"/opt",
        &"/opt",
        &"--proc",
        &"/proc",
        &"--dev",
        &"/dev",
    ]);
    match jail.tmp_dir {
        Some(tmp) => push(&[&"--bind", &tmp, &"/tmp"]),
        None => push(&[&"--tmpfs", &"/tmp"]),
    }
    // The root is bwrap's own tmpfs: only what is mounted exists.
    push(&[&"--tmpfs", &"/var/tmp", &"--dir", &host.home]);
    if jail.net == Net::Host {
        // /etc/resolv.conf points at systemd-resolved's stub under /run.
        push(&[
            &"--share-net",
            &"--ro-bind-try",
            &"/run/systemd/resolve",
            &"/run/systemd/resolve",
        ]);
    }
    let bind = |rw: bool| if rw { "--bind" } else { "--ro-bind" };
    push(&[&bind(!jail.readonly), &ws, &ws]);
    for p in protected_existing(&ws, jail.project_dir, &host.clients) {
        push(&[&"--ro-bind", &p, &p]);
    }
    for p in mounts::secret_files(&ws, SECRET_LIMIT) {
        push(&[&"--ro-bind", &"/dev/null", &p]);
    }
    if let Some(git) = jail.git_dir {
        // After the protected binds, so it covers the real .git's.
        let real = ws.join(".git");
        push(&[
            &bind(!jail.readonly),
            &git,
            &real,
            &"--ro-bind",
            &real,
            &HOST_GIT,
        ]);
    }
    for p in jail.ro_paths {
        push(&[&"--ro-bind-try", p, p]);
    }
    for m in jail.mounts {
        let rw = m.rw && !jail.readonly;
        push(&[&bind(rw), &m.path, &m.path]);
        if rw {
            for p in protected_existing(&m.path, &m.path, &host.clients) {
                push(&[&"--ro-bind", &p, &p]);
            }
            for p in mounts::secret_files(&m.path, SECRET_LIMIT) {
                push(&[&"--ro-bind", &"/dev/null", &p]);
            }
        } else {
            for p in m.secrets.iter().filter(|p| p.exists()) {
                push(&[&"--ro-bind", &"/dev/null", p]);
            }
        }
    }
    push(&[&"--chdir", &ws, &"--clearenv"]);
    let mut path = OsString::from(SANDBOX_PATH);
    for p in jail.path_extra {
        path.push(":");
        path.push(p);
    }
    let home = host.home.as_os_str().to_os_string();
    let mut env: Vec<(&str, OsString)> = vec![
        ("PATH", path),
        ("HOME", home),
        ("USER", host.user.clone().into()),
        ("LANG", host.lang.clone().into()),
        ("TERM", "dumb".into()),
        ("TMPDIR", "/tmp".into()),
        ("RPCTL_SANDBOX", "1".into()),
    ];
    if jail.net == Net::Proxy {
        env.extend(PROXY_ENV.iter().map(|(k, v)| (*k, OsString::from(v))));
    }
    for (k, v) in &env {
        push(&[&"--setenv", k, v]);
    }
    a
}

/// What a sandboxed command left (rp's result dict).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// `None` when it timed out or did not run.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    /// Why a proxied command did not run: its network could not be set up,
    /// and there is no fallback to the host's.
    pub net_error: Option<String>,
}

/// The launcher a proxied sandbox runs first: rpctl's, or a test's.
#[derive(Debug, Clone, Copy)]
pub struct Launcher<'a> {
    pub argv: &'a [&'a str],
    pub code: &'a str,
}

impl Launcher<'static> {
    pub const RPCTL: Self = Self {
        argv: &launch::LAUNCHER_ARGV,
        code: launch::LAUNCHER,
    };
}

/// A pipe holding the seccomp program, its read end for bwrap.
fn seccomp_pipe() -> io::Result<OwnedFd> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills two descriptors, which are taken at once.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both are fresh descriptors this function owns.
    let (r, w) = unsafe {
        use std::os::fd::FromRawFd;
        (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
    };
    // 8 bytes an instruction, far below the pipe's buffer.
    fs::File::from(w).write_all(&seccomp::program())?;
    Ok(r)
}

/// Runs `argv` in the sandbox `jail` describes, for at most `timeout`
/// (rp's `run`). A proxied jail's port is served by `proxy` for exactly as
/// long as the command runs; if the handover fails the command does not
/// run and [`Ran::net_error`] says why. Fails only when the sandbox could
/// not be started at all.
pub async fn run(
    host: &Host,
    jail: &Jail<'_>,
    argv: &[OsString],
    timeout: Duration,
    proxy: Option<Arc<dyn Egress>>,
) -> io::Result<Ran> {
    run_with(host, jail, argv, timeout, proxy, Launcher::RPCTL).await
}

/// [`run`] with another `launcher`.
pub async fn run_with(
    host: &Host,
    jail: &Jail<'_>,
    argv: &[OsString],
    timeout: Duration,
    proxy: Option<Arc<dyn Egress>>,
    launcher: Launcher<'_>,
) -> io::Result<Ran> {
    let proxy = match (jail.net, proxy) {
        (Net::Proxy, Some(p)) => Some(p),
        (Net::Proxy, None) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a proxied sandbox needs an egress proxy",
            ));
        }
        _ => None,
    };
    // Closes the proxy however this ends, Ctrl-C (a dropped future)
    // included, as rpctl's `finally`.
    let _closing = proxy.clone().map(Closing::new);
    let seccomp = seccomp_pipe()?;
    // The walks and `git config` behind the argv block: off the runtime.
    let (owned, fd) = (OwnedJail::of(jail), seccomp.as_raw_fd());
    let host_ = host.clone();
    let mut args = tokio::task::spawn_blocking(move || bwrap_argv(&host_, &owned.jail(), fd))
        .await
        .map_err(io::Error::other)?;
    let program = args.remove(0);
    args.push("--".into());
    let mut pass_fds = vec![seccomp];
    let mut ours = None;
    if proxy.is_some() {
        let (mine, theirs) = std::os::unix::net::UnixStream::pair()?;
        args.extend(launcher.argv.iter().map(OsString::from));
        args.push(launcher.code.into());
        args.push(theirs.as_raw_fd().to_string().into());
        args.push("--".into());
        pass_fds.push(OwnedFd::from(theirs));
        ours = Some(mine);
    }
    args.extend(argv.iter().cloned());
    let path = std::env::var_os("PATH").unwrap_or_else(|| SANDBOX_PATH.into());
    let running = sandboxed::spawn(sandboxed::Spec {
        program,
        args,
        cwd: resolve(jail.workspace),
        env: vec![("PATH".into(), path)],
        pass_fds,
    })?;
    if let (Some(proxy), Some(mut sock)) = (proxy, ours) {
        let serving = Arc::clone(&proxy);
        let gone = _closing.as_ref().map(|c| Arc::clone(&c.gone));
        let handed = tokio::task::spawn_blocking(move || {
            let r = launch::take_listener(&mut sock, serving.as_ref());
            // The caller went away while this ran: whatever it served goes.
            if gone.is_some_and(|g| g.load(Ordering::SeqCst)) {
                serving.close();
            }
            (r, sock)
        })
        .await;
        // The launcher's end stays open until the command ends.
        let (result, _sock) = match handed {
            Ok((r, sock)) => (r, Some(sock)),
            Err(e) => (
                Err(NetSetupError(format!("the handover failed ({e})"))),
                None,
            ),
        };
        let ran = match result {
            Err(NetSetupError(mut why)) => {
                let (out, err) = running.abandon().await;
                if let Some(last) = err.lines().rev().find(|l| !l.trim().is_empty()) {
                    why.push_str(&format!(" ({})", last.trim()));
                }
                why.push_str(
                    "; the command did not run. Network commands need python3 in /usr for \
                     the proxy launcher; there is no fallback to the host network",
                );
                Ran {
                    exit_code: None,
                    timed_out: false,
                    stdout: out,
                    stderr: err,
                    net_error: Some(why),
                }
            }
            Ok(()) => finished(running.finish(timeout, true).await),
        };
        // The listener and every connection (and again by `_closing`).
        proxy.close();
        return Ok(ran);
    }
    Ok(finished(running.finish(timeout, true).await))
}

/// Closes the proxy when dropped, and says so to a handover still running.
struct Closing {
    proxy: Arc<dyn Egress>,
    gone: Arc<AtomicBool>,
}

impl Closing {
    fn new(proxy: Arc<dyn Egress>) -> Self {
        Self {
            proxy,
            gone: Arc::default(),
        }
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        self.gone.store(true, Ordering::SeqCst);
        self.proxy.close();
    }
}

/// A [`Jail`] that owns its paths, to build the argv on another thread.
struct OwnedJail {
    workspace: PathBuf,
    project_dir: PathBuf,
    net: Net,
    readonly: bool,
    tmp_dir: Option<PathBuf>,
    ro_paths: Vec<PathBuf>,
    path_extra: Vec<PathBuf>,
    mounts: Vec<Mount>,
    git_dir: Option<PathBuf>,
}

impl OwnedJail {
    fn of(j: &Jail<'_>) -> Self {
        Self {
            workspace: j.workspace.to_path_buf(),
            project_dir: j.project_dir.to_path_buf(),
            net: j.net,
            readonly: j.readonly,
            tmp_dir: j.tmp_dir.map(Path::to_path_buf),
            ro_paths: j.ro_paths.to_vec(),
            path_extra: j.path_extra.to_vec(),
            mounts: j.mounts.to_vec(),
            git_dir: j.git_dir.map(Path::to_path_buf),
        }
    }

    fn jail(&self) -> Jail<'_> {
        Jail {
            workspace: &self.workspace,
            project_dir: &self.project_dir,
            net: self.net,
            readonly: self.readonly,
            tmp_dir: self.tmp_dir.as_deref(),
            ro_paths: &self.ro_paths,
            path_extra: &self.path_extra,
            mounts: &self.mounts,
            git_dir: self.git_dir.as_deref(),
        }
    }
}

fn finished(done: sandboxed::Finished) -> Ran {
    Ran {
        exit_code: done.exit_code,
        timed_out: done.timed_out,
        stdout: done.stdout,
        stderr: done.stderr,
        net_error: None,
    }
}

/// `program` on PATH, executable, as `shutil.which` finds it.
fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .filter(|d| d.is_absolute())
            .filter_map(|d| fs::metadata(d.join(program)).ok())
            .any(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// `Ok(())` when commands can be sandboxed here, else why not (rp's
/// `sandbox_available`): x86_64, bwrap on PATH, and `true` running in it.
pub async fn available(host: &Host) -> Result<(), String> {
    let arch = std::env::consts::ARCH;
    if arch != "x86_64" {
        return Err(format!(
            "the seccomp filter is x86_64-only (this is {arch})"
        ));
    }
    if !on_path("bwrap") {
        return Err("bubblewrap (bwrap) is not installed".into());
    }
    let dir = tmp::scratch_dir().map_err(|e| format!("bwrap failed: {e}"))?;
    let jail = Jail::new(dir.path(), dir.path());
    let ran = run(host, &jail, &["true".into()], Duration::from_secs(15), None).await;
    match ran {
        Ok(r) if r.exit_code == Some(0) => Ok(()),
        Ok(r) => Err(format!(
            "bwrap failed: {}",
            r.stderr.trim().lines().last().map_or_else(
                || format!("exit {}", r.exit_code.unwrap_or(-1)),
                str::to_string
            )
        )),
        Err(e) => Err(format!("bwrap failed: {e}")),
    }
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "sandbox_live_tests.rs"]
mod live_tests;
