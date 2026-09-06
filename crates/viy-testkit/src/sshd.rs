//! An OpenSSH server of the test's own on 127.0.0.1, for the tunnel tests
//! (docs/implementation-plan.md WP 2.5): unprivileged, on a free port, with
//! a fresh ed25519 host key and a client key in a scratch directory, so the
//! real `ssh`, `ssh-keyscan` and `ssh-keygen` run against it as against a
//! pod.
//!
//! viy logs in as `root`, which an unprivileged sshd cannot serve, and
//! `ssh` reads the user's real `~/.ssh` whatever `HOME` says.
//! [`LocalSshd::exec`] therefore rewrites each `ssh` command line on its way
//! out: the test user in place of `root`, no config file, only the test's
//! key, no agent. Everything else goes out as viy built it.

use std::io;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use viy_exec::BoxFuture;
use viy_exec::Child;
use viy_exec::Command;
use viy_exec::ExecBackend;
use viy_exec::Output;
use viy_exec::Spawn;
use viy_exec::SystemExec;

use crate::scratch::ScratchDir;

pub const SSHD: &str = "/usr/sbin/sshd";

#[derive(Debug)]
pub struct LocalSshd {
    dir: ScratchDir,
    server: std::process::Child,
    pub port: u16,
    exec: Arc<SshdExec>,
}

impl LocalSshd {
    /// Starts the server and waits until it accepts connections.
    pub fn start() -> io::Result<Self> {
        let dir = ScratchDir::new("sshd")?;
        let d = dir.path();
        for key in ["host", "client"] {
            run_quiet(
                std::process::Command::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-C", "viy-test", "-f"])
                    .arg(d.join(key)),
            )?;
        }
        std::fs::copy(d.join("client.pub"), d.join("authorized_keys"))?;
        let port = free_port()?;
        std::fs::write(
            d.join("sshd_config"),
            format!(
                "ListenAddress 127.0.0.1\nPort {port}\nHostKey {}\nAuthorizedKeysFile {}\n\
                 StrictModes no\nUsePAM no\nPidFile none\nPasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\nAllowTcpForwarding yes\n",
                d.join("host").display(),
                d.join("authorized_keys").display(),
            ),
        )?;
        let server = std::process::Command::new(SSHD)
            .args(["-D", "-e", "-f"])
            .arg(d.join("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(d.join("sshd.log"))?)
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("{SSHD}: {e}")))?;
        let exec = Arc::new(SshdExec {
            user: user_name()?,
            key: d.join("client"),
            sent: Mutex::default(),
        });
        let sshd = Self {
            dir,
            server,
            port,
            exec,
        };
        // Ready when this sshd says it listens and the port answers; the
        // port alone could be another test's, picked between free_port and
        // sshd's bind.
        let listening = format!("Server listening on 127.0.0.1 port {port}.");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut sshd = sshd;
        loop {
            if let Some(status) = sshd.server.try_wait()? {
                return Err(io::Error::other(format!(
                    "sshd exited ({status}): {}",
                    sshd.log()
                )));
            }
            if sshd.log().contains(&listening) && TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Ok(sshd);
            }
            if Instant::now() > deadline {
                return Err(io::Error::other(format!(
                    "sshd never listened: {}",
                    sshd.log()
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The backend that reaches this server (module docs).
    pub fn exec(&self) -> Arc<SshdExec> {
        Arc::clone(&self.exec)
    }

    /// The host key as `ssh-ed25519 AAAA...`, without its comment.
    pub fn host_key(&self) -> io::Result<String> {
        let text = std::fs::read_to_string(self.dir.path().join("host.pub"))?;
        Ok(text.split(' ').take(2).collect::<Vec<_>>().join(" "))
    }

    /// The host key's `SHA256:...` fingerprint.
    pub fn host_fingerprint(&self) -> io::Result<String> {
        let out = std::process::Command::new("ssh-keygen")
            .arg("-lf")
            .arg(self.dir.path().join("host.pub"))
            .output()?;
        String::from_utf8_lossy(&out.stdout)
            .split(' ')
            .nth(1)
            .map(str::to_string)
            .ok_or_else(|| io::Error::other("ssh-keygen printed no fingerprint"))
    }

    /// What the server logged, for a failing test's message.
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("sshd.log")).unwrap_or_default()
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for LocalSshd {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

/// [`SystemExec`] with each `ssh` pointed at a [`LocalSshd`]. It records
/// the commands as the code under test gave them.
#[derive(Debug)]
pub struct SshdExec {
    user: String,
    key: PathBuf,
    sent: Mutex<Vec<Command>>,
}

impl SshdExec {
    /// Every command so far, as given.
    pub fn sent(&self) -> Vec<Command> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn rewrite(&self, command: Command) -> Command {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(command.clone());
        if command.program != "ssh" {
            return command;
        }
        let mut out = Command {
            args: Vec::new(),
            ..command.clone()
        };
        out = out.args(["-F", "/dev/null", "-i"]).arg(&self.key).args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "IdentityAgent=none",
        ]);
        for arg in command.args {
            out = match arg.to_str().and_then(|a| a.strip_prefix("root@")) {
                Some(host) => out.arg(format!("{}@{host}", self.user)),
                None => out.arg(arg),
            };
        }
        out
    }
}

impl ExecBackend for SshdExec {
    fn run(&self, command: Command) -> BoxFuture<'_, io::Result<Output>> {
        let command = self.rewrite(command);
        Box::pin(async move { SystemExec.run(command).await })
    }

    fn spawn(&self, command: Command, how: Spawn) -> io::Result<Box<dyn Child>> {
        SystemExec.spawn(self.rewrite(command), how)
    }
}

/// A port nothing listens on just now.
pub fn free_port() -> io::Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// A port on 127.0.0.1 held bound but not listening, so a connection to
/// it is refused for as long as this lives, whatever other tests bind.
#[derive(Debug)]
pub struct ClosedPort {
    _socket: std::os::fd::OwnedFd,
    pub port: u16,
}

pub fn closed_port() -> io::Result<ClosedPort> {
    use std::os::fd::FromRawFd;
    // SAFETY: plain socket calls on a descriptor this function owns; the
    // address structs are zeroed and sized for AF_INET.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = std::os::fd::OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        addr.sin_family = libc::AF_INET as libc::sa_family_t;
        addr.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
        let size = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        if libc::bind(fd, (&raw const addr).cast(), size) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = size;
        if libc::getsockname(fd, (&raw mut addr).cast(), &mut len) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ClosedPort {
            _socket: socket,
            port: u16::from_be(addr.sin_port),
        })
    }
}

fn user_name() -> io::Result<String> {
    let out = std::process::Command::new("id").arg("-un").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn run_quiet(command: &mut std::process::Command) -> io::Result<()> {
    let out = command.stdin(Stdio::null()).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}
