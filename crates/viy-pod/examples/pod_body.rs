//! The create-pod body `viy up` would send, written to a file and never
//! sent: viy's config as `viy up` loads it in the working directory, its
//! endpoint key as configured (none is made), the SSH key `viy up` finds,
//! the region and volume given, as `failover::Creation::attempt` sets them.
//! For comparing with rpctl's body for the same inputs (kb f116); the file
//! holds the endpoint key, so it is written 0600.
//!
//!   cargo run -q -p viy-pod --example pod_body -- OUT REGION VOLUME_ID

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use serde_json::json;
use viy_config::Paths;
use viy_config::Project;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, region, volume] = args.as_slice() else {
        return Err("usage: pod_body OUT REGION VOLUME_ID".into());
    };
    let paths = Paths::from_env().map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let project = Project::find(&cwd).map_err(|e| e.to_string())?;
    let cfg = viy_config::config::load(&paths, &project).map_err(|e| e.to_string())?;
    let key = cfg.get("VLLM_API_KEY").unwrap_or_default().to_string();
    if key.is_empty() {
        return Err("VLLM_API_KEY is not set in viy's config".into());
    }
    let pubkey = viy_pod::ssh::find_ssh_pubkey(&paths).map_err(|e| e.to_string())?;
    let mut body = viy_pod::boot::build_pod_body(&cfg, &key, volume, pubkey.as_deref())
        .map_err(|e| e.to_string())?;
    body["dataCenterIds"] = json!([region]);
    body["networkVolumeId"] = json!(volume);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(out)
        .map_err(|e| format!("{out}: {e}"))?;
    f.write_all(body.to_string().as_bytes())
        .map_err(|e| e.to_string())
}
