//! SSH to the pod and its pinned host key: rpctl's `find_ssh_pubkey`,
//! `host_key_alias`, `ssh_opts`, `ssh_endpoint`, `_tcp_open`,
//! `pinned_fingerprint`, `forget_host_key`, `logged_host_keys`,
//! `scan_host_key`, `_pin_checked_key`, `_preflight_ssh` and `wait_ssh`'s
//! probe. Pinned by the pod/ssh fixture.
//!
//! Host keys live in rp's `known_hosts` (shared until cutover, kb d006),
//! not `~/.ssh/known_hosts`, under an alias per pod name and volume
//! ([`host_key_alias`]): a pod's address changes with every recreate, its
//! key, kept on the network volume, does not. First contact with a key is
//! checked against the fingerprint the pod printed in its boot log, which
//! arrives over RunPod's HTTPS API; with no fingerprint there, it is trust
//! on first use. That defeats someone on the network path, not RunPod,
//! who can read the key off the volume.
//!
//! The commands run through [`ExecBackend`] (kb d037), so tests script them.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use viy_config::Config;
use viy_config::Paths;
use viy_exec::Command;
use viy_exec::ExecBackend;

use crate::api::RunPod;
use crate::notice::Notice;
use crate::notice::Say;
use crate::notice::Tone;
use crate::state;
use crate::state::PodState;

/// What the ssh and tunnel functions work with.
#[derive(Clone, Copy)]
pub struct Ssh<'a> {
    pub paths: &'a Paths,
    pub cfg: &'a Config,
    pub exec: &'a dyn ExecBackend,
}

/// The first public key in `~/.ssh` the pod is created with, ed25519
/// first.
pub fn find_ssh_pubkey(paths: &Paths) -> io::Result<Option<String>> {
    for name in ["id_ed25519.pub", "id_rsa.pub", "id_ecdsa.pub"] {
        match std::fs::read_to_string(paths.home.join(".ssh").join(name)) {
            Ok(text) => return Ok(Some(text.trim().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// The name a pod's host key is pinned under: `rpctl-<POD_NAME>`, the same
/// for every pod on the primary volume, since the key lives on the volume.
/// A pod on another volume (the backup region's) is another identity and
/// gets the volume id appended. rp's spelling: the two tools share the
/// pins.
pub fn host_key_alias(state: &PodState, cfg: &Config) -> String {
    let name = cfg.get("POD_NAME").unwrap_or("");
    match (state.volume_id(), cfg.get("NETWORK_VOLUME_ID")) {
        (Some(volume), Some(primary)) if !primary.is_empty() && volume != primary => {
            format!("rpctl-{name}-{volume}")
        }
        _ => format!("rpctl-{name}"),
    }
}

/// `(ip, port)` for direct TCP SSH into `pod`, once RunPod has mapped 22.
pub fn ssh_endpoint(pod: &Value) -> Option<(String, u16)> {
    let ip = pod.get("publicIp")?.as_str().filter(|ip| !ip.is_empty())?;
    let port = match pod.get("portMappings")?.get("22")? {
        Value::Number(n) => n.as_u64().and_then(|p| u16::try_from(p).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|&p| p != 0)?;
    Some((ip.to_string(), port))
}

/// Whether `host:port` accepts a TCP connection within `timeout`.
pub async fn port_open(host: &str, port: u16, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

/// Matches the line the pod's boot script prints for its sshd key.
static LOGGED_HOST_KEY: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"sshd host key: \d+ (SHA256:[A-Za-z0-9+/=]+) .*\(ED25519\)"));

static FINGERPRINT: LazyLock<Regex> = LazyLock::new(|| compile_regex(r"(SHA256:[A-Za-z0-9+/=]+)"));

fn compile_regex(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(err) => panic!("invalid regex pattern `{pattern}`: {err}"),
    }
}

/// The fingerprints the pod printed for its sshd key, from RunPod's log
/// API: a channel apart from the SSH connection.
pub async fn logged_host_keys(
    rp: &RunPod,
    pod_id: &str,
) -> Result<BTreeSet<String>, crate::api::RunPodError> {
    let entries = rp.pod_logs(pod_id, 5000, Duration::from_secs(10)).await?;
    Ok(entries
        .iter()
        .filter_map(|e| LOGGED_HOST_KEY.captures(&e.line))
        .map(|m| m[1].to_string())
        .collect())
}

/// How a first contact went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {
    /// The key matched the boot log and is pinned.
    Pinned,
    /// It did not; nothing is pinned.
    Mismatch,
    /// Nothing to compare with: trust on first use applies.
    Unchecked,
}

impl Ssh<'_> {
    /// The alias for the tracked pod ([`host_key_alias`]).
    pub fn alias(&self) -> io::Result<String> {
        Ok(host_key_alias(&state::load(self.paths)?, self.cfg))
    }

    /// The host-key options every ssh gets: rp's `known_hosts`, the alias,
    /// `StrictHostKeyChecking=strict` (`accept-new` pins on first use), and
    /// keepalives that end a dead connection within 45 s.
    pub fn opts(&self, strict: &str) -> io::Result<Vec<OsString>> {
        self.opts_under(&self.alias()?, strict)
    }

    fn opts_under(&self, alias: &str, strict: &str) -> io::Result<Vec<OsString>> {
        let known_hosts = self.paths.known_hosts_file();
        if let Some(dir) = known_hosts.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OsString::from("UserKnownHostsFile=");
        file.push(&known_hosts);
        Ok([
            file,
            "GlobalKnownHostsFile=/dev/null".into(),
            format!("HostKeyAlias={alias}").into(),
            format!("StrictHostKeyChecking={strict}").into(),
            "ServerAliveInterval=15".into(),
            "ServerAliveCountMax=3".into(),
        ]
        .into_iter()
        .flat_map(|o| [OsString::from("-o"), o])
        .collect())
    }

    /// The host key pinned for this pod, as `ssh-keygen -l` prints it.
    pub async fn pinned_fingerprint(&self) -> io::Result<Option<String>> {
        self.fingerprint_under(&self.alias()?).await
    }

    async fn fingerprint_under(&self, alias: &str) -> io::Result<Option<String>> {
        let known_hosts = self.paths.known_hosts_file();
        if !known_hosts.exists() {
            return Ok(None);
        }
        let out = self
            .exec
            .run(
                Command::new("ssh-keygen")
                    .args(["-l", "-F"])
                    .arg(alias)
                    .arg("-f")
                    .arg(known_hosts),
            )
            .await?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|line| line.contains("SHA256:"))
            .map(|line| line.trim().to_string()))
    }

    /// Drops the pinned key, so the next connect trusts whatever answers:
    /// once per genuinely new identity (a fresh volume, a pod from before
    /// the key was persisted), never to silence a mismatch nobody can
    /// explain.
    pub async fn forget_host_key(&self, say: Say<'_>) -> io::Result<()> {
        let alias = self.alias()?;
        if self.fingerprint_under(&alias).await?.is_none() {
            say(Notice::new(
                Tone::Dim,
                format!("No host key pinned for '{alias}'."),
            ));
            return Ok(());
        }
        self.exec
            .run(
                Command::new("ssh-keygen")
                    .arg("-R")
                    .arg(&alias)
                    .arg("-f")
                    .arg(self.paths.known_hosts_file()),
            )
            .await?;
        say(Notice::new(
            Tone::Warn,
            format!(
                "Forgot the pinned host key for '{alias}'. The next `viy tunnel` trusts whatever answers."
            ),
        ));
        Ok(())
    }

    /// The fingerprint and the `ssh-ed25519 AAAA...` key the endpoint
    /// presents; `None` when it presents none in 20 s.
    pub async fn scan_host_key(&self, ip: &str, port: u16) -> io::Result<Option<(String, String)>> {
        let scan = Command::new("ssh-keyscan")
            .args(["-T", "10", "-t", "ed25519", "-p"])
            .arg(port.to_string())
            .arg(ip)
            .timeout(Duration::from_secs(20));
        let Ok(out) = self.exec.run(scan).await else {
            return Ok(None);
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let Some(key) = stdout
            .lines()
            .find(|line| line.contains(" ssh-ed25519 "))
            .and_then(|line| line.trim_start().split_once(char::is_whitespace))
            .map(|(_, key)| key.trim().to_string())
        else {
            return Ok(None);
        };
        let out = self
            .exec
            .run(
                Command::new("ssh-keygen")
                    .arg("-lf")
                    .arg("-")
                    .stdin(format!("x {key}\n")),
            )
            .await?;
        Ok(FINGERPRINT
            .captures(&String::from_utf8_lossy(&out.stdout))
            .map(|m| (m[1].to_string(), key)))
    }

    /// First contact with a volume's host key: pinned only if it matches
    /// the fingerprint in the pod's boot log. About 3 s, once per volume.
    pub async fn pin_checked_key(
        &self,
        rp: &RunPod,
        ip: &str,
        port: u16,
        say: Say<'_>,
    ) -> io::Result<Checked> {
        let state = state::load(self.paths)?;
        self.pin_checked(rp, &state, &host_key_alias(&state, self.cfg), ip, port, say)
            .await
    }

    async fn pin_checked(
        &self,
        rp: &RunPod,
        state: &PodState,
        alias: &str,
        ip: &str,
        port: u16,
        say: Say<'_>,
    ) -> io::Result<Checked> {
        let logged = match state.pod_id() {
            Some(pod_id) => logged_host_keys(rp, pod_id).await.unwrap_or_default(),
            None => BTreeSet::new(),
        };
        if logged.is_empty() {
            return Ok(Checked::Unchecked);
        }
        let Some((seen, key)) = self.scan_host_key(ip, port).await? else {
            return Ok(Checked::Unchecked);
        };
        if !logged.contains(&seen) {
            let logged: Vec<&str> = logged.iter().map(String::as_str).collect();
            say(Notice::new(
                Tone::Error,
                format!(
                    "SSH to {ip}:{port} presented host key {seen}, but the pod's boot log \
                     (read over RunPod's HTTPS API) says {}.\n\
                     Treat it as a machine-in-the-middle: nothing was pinned and no tunnel started.",
                    logged.join(", ")
                ),
            ));
            return Ok(Checked::Mismatch);
        }
        let known_hosts = self.paths.known_hosts_file();
        if let Some(dir) = known_hosts.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&known_hosts)?;
        file.write_all(format!("{alias} {key}\n").as_bytes())?;
        Ok(Checked::Pinned)
    }

    /// Checks the login and the host key where the user sees the result:
    /// a background ssh that dies on a changed key leaves nothing to read.
    /// True when the pod lets us in.
    pub async fn preflight(
        &self,
        rp: &RunPod,
        ip: &str,
        port: u16,
        say: Say<'_>,
    ) -> io::Result<bool> {
        // One reading of pod.json, so the key is checked and pinned under
        // one alias even if rp saves the state meanwhile.
        let state = state::load(self.paths)?;
        let alias = host_key_alias(&state, self.cfg);
        let first_contact = self.fingerprint_under(&alias).await?.is_none();
        let checked = if first_contact {
            Some(
                self.pin_checked(rp, &state, &alias, ip, port, &mut *say)
                    .await?,
            )
        } else {
            None
        };
        if checked == Some(Checked::Mismatch) {
            return Ok(false);
        }
        let out = self
            .exec
            .run(
                Command::new("ssh")
                    .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15"])
                    .args(self.opts_under(&alias, "accept-new")?)
                    .arg("-p")
                    .arg(port.to_string())
                    .arg(format!("root@{ip}"))
                    .arg("true"),
            )
            .await?;
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        if !out.status.success() {
            say(Notice::new(
                Tone::Error,
                format!("SSH to {ip}:{port} failed; no tunnel started."),
            ));
            if !err.is_empty() {
                say(Notice::new(Tone::Dim, err));
            }
            if err.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
                || err.to_lowercase().contains("host key")
            {
                say(Notice::new(
                    Tone::Warn,
                    format!(
                        "\nThe pod presented a different host key than the one pinned for '{alias}'.\n\
                         Expected if the network volume was recreated, or if the pod predates the persisted key.\n\
                         Otherwise treat it as a machine-in-the-middle and do not clear it.\n  \
                         viy tunnel --forget-key   (only when you can explain the change)"
                    ),
                ));
            } else if err.contains("Permission denied") {
                say(Notice::new(
                    Tone::Warn,
                    "The pod has your public key only if it existed when the pod was created. \
                     Recreate it:  viy down && viy up",
                ));
            }
            return Ok(false);
        }
        if first_contact {
            let fingerprint = self.fingerprint_under(&alias).await?;
            say(if checked == Some(Checked::Pinned) {
                Notice::new(
                    Tone::Ok,
                    "Pinned this pod's SSH host key; it matches the fingerprint in the pod's boot log:",
                )
            } else {
                Notice::new(
                    Tone::Warn,
                    "Pinned this pod's SSH host key on first use, unchecked: the pod's boot log had \
                     no fingerprint to compare with.",
                )
            });
            if let Some(fingerprint) = fingerprint {
                say(Notice::new(Tone::Dim, format!("  {fingerprint}")));
            }
            say(Notice::new(
                Tone::Dim,
                "The key lives on the network volume, so every later pod must present the same \
                 one or the connection is refused.",
            ));
        }
        Ok(true)
    }
}

/// `wait_ssh`'s probe: the pod's SSH endpoint, asked of RunPod on every
/// probe, since a starting or restarted pod can change its mapping.
pub struct SshWatch<'a> {
    rp: &'a RunPod,
    pod_id: String,
    endpoint: Option<(String, u16)>,
}

impl<'a> SshWatch<'a> {
    pub fn new(rp: &'a RunPod, pod_id: impl Into<String>) -> Self {
        Self {
            rp,
            pod_id: pod_id.into(),
            endpoint: None,
        }
    }

    /// The endpoint once it accepts a connection, or what is awaited. A
    /// failed call keeps the last endpoint RunPod gave.
    pub async fn probe(&mut self) -> Result<(String, u16), String> {
        if let Ok(pod) = self.rp.get_pod(&self.pod_id).await {
            self.endpoint = pod.as_ref().and_then(ssh_endpoint);
        }
        match &self.endpoint {
            Some((ip, port)) => {
                if port_open(ip, *port, Duration::from_secs(5)).await {
                    Ok((ip.clone(), *port))
                } else {
                    Err(format!("{ip}:{port} not accepting"))
                }
            }
            None => Err("no port mapping yet".to_string()),
        }
    }
}

#[cfg(test)]
#[path = "ssh_tests.rs"]
mod tests;
