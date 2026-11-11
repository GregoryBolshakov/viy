use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;

use pretty_assertions::assert_eq;
use serde_json::Value;
use viy_config::DEFAULTS;
use viy_testkit::fixtures;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// How the `pod/boot` fixture writes the embedded startup.py and the body's
/// copy of the script (tools/fixtures.py).
const STARTUP_MARKER: &str = "<startup.py>";
const BOOT_SCRIPT_MARKER: &str = "<boot_script>";

fn with<'a>(overrides: impl IntoIterator<Item = (&'a str, &'a str)>) -> Config {
    let mut cfg = Config::defaults();
    for (key, value) in overrides {
        cfg.set(key, value);
    }
    cfg
}

/// DEFAULTS and a case's `cfg` object.
fn config(overrides: &Value) -> Config {
    let overrides = overrides.as_object().unwrap();
    with(
        overrides
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str().unwrap())),
    )
}

/// The script with its one copy of startup.py written as the fixture does.
fn elided(script: &str) -> String {
    assert_eq!(script.matches(STARTUP_PY).count(), 1);
    assert!(!script.contains(STARTUP_MARKER) && !script.contains(BOOT_SCRIPT_MARKER));
    script.replacen(STARTUP_PY, STARTUP_MARKER, 1)
}

#[test]
fn startup_py_is_rpctls_at_the_pin() {
    let fixture = fixtures::load("pod/boot").unwrap();
    let rp = fixture.case("startup.py").unwrap()["text"]
        .as_str()
        .unwrap();
    assert!(
        STARTUP_PY == rp,
        "assets/startup.py differs from rpctl's at the pin"
    );
}

#[test]
fn each_config_builds_what_rpctls_boot_py_does() {
    let fixture = fixtures::load("pod/boot").unwrap();
    let mut seen = 0;
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        if name == "startup.py" {
            continue;
        }
        seen += 1;
        let cfg = config(&case["cfg"]);
        let key = case["key"].as_str().unwrap();
        let args = build_vllm_args(&cfg, key, case["host"].as_str());
        assert_eq!(json!(args), case["vllm_args"], "{name}: vLLM arguments");
        if case["host"].is_string() {
            continue;
        }
        let script = build_boot_script(&cfg, key);
        assert_eq!(
            elided(&script).as_str(),
            case["boot_script"].as_str().unwrap(),
            "{name}: boot script"
        );
        let body = build_pod_body(
            &cfg,
            key,
            case["volume_id"].as_str().unwrap(),
            case["pubkey"].as_str(),
        );
        match (body, case.get("pod_body")) {
            (Ok(mut body), Some(rp)) => {
                assert_eq!(body["dockerStartCmd"], json!([script]), "{name}");
                body["dockerStartCmd"] = json!([BOOT_SCRIPT_MARKER]);
                // Ordered: the body's keys in rpctl's order (flags.md F02).
                assert_eq!(body.to_string(), rp.to_string(), "{name}: pod body");
            }
            (Err(err), None) => {
                assert!(case["pod_body_error"].is_string(), "{name}");
                assert_eq!(err.key, case["bad_key"], "{name}: refused for");
            }
            (body, _) => panic!(
                "{name}: viy {body:?}, rp {}",
                case.get("pod_body_error").unwrap_or(&Value::Null)
            ),
        }
    }
    assert_eq!(seen, 37);
}

#[test]
fn a_bad_number_names_its_key() {
    let err = build_pod_body(&with([("GPU_COUNT", "two")]), "k", "v", None).unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"GPU_COUNT must be a whole number, not "two""#
    );
    let err = build_pod_body(&with([("IDLE_TTL_MIN", "inf")]), "k", "v", None).unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"IDLE_TTL_MIN must be a number, not "inf""#
    );
}

#[test]
fn quoting_is_shlex_quotes() {
    for (word, quoted) in [
        ("", "''"),
        ("a-Z_0.9/x@y%z+w=v:u,t", "a-Z_0.9/x@y%z+w=v:u,t"),
        ("a b", "'a b'"),
        ("it's", r#"'it'"'"'s'"#),
        ("é", "'é'"),
        ("~", "'~'"),
    ] {
        assert_eq!(quote(word), quoted, "{word:?}");
    }
}

// ------------------------------------------------ rpctl tests/test_boot_budget.py

const HOW_TO_APPROVE: &str = "\nThe pod's boot path changed. Boot-time budget is a hard 180s (kb d004).\n\
     1. Measure a real boot with the change and read the breakdown.\n\
     2. It must not be slower than before, and the total must stay within budget.\n\
     3. Only then update the approved value, here and in rpctl's tests/test_boot_budget.py.\n";

/// Normalised foreground skeleton of the boot script: comments dropped, the
/// backgrounded sshd subshell and the embedded startup.py elided. Each line
/// runs before vLLM starts (or is explicitly backgrounded with &).
/// Approved 2026-06-26 after measured boots: 92s total (vLLM launched
/// through startup.py to skip the 16.5s multimodal warmup; MM_WARMUP=1
/// restores it). rpctl's value, unchanged.
const APPROVED_FOREGROUND: &str = r#"set -uo pipefail
export HF_HOME=/workspace/hf
export VLLM_CACHE_ROOT=/root/.cache/vllm
export TORCHINDUCTOR_CACHE_DIR=/root/.cache/torch/inductor
export TRITON_CACHE_DIR=/root/.triton/cache
export CUDA_CACHE_PATH=/root/.nv/ComputeCache
export HF_XET_HIGH_PERFORMANCE=1
export RPCTL_CACHE_DIR=/workspace/rpctl-cache
export RPCTL_MODEL_REPO=<repo>
export RPCTL_MODEL_CACHE_MODE=auto
export RPCTL_STARTUP_CACHE=1
export RPCTL_PREWARM=1
export RPCTL_MM_WARMUP=0
export RPCTL_PORT=8000
mkdir -p /workspace/hf
echo "=== boot $(date -u) ===" | tee -a /workspace/vllm.log
echo "RPCTL_PHASE container started; preparing SSH"
<sshd subshell, backgrounded>
cat > /run/rpctl-startup.py <<'RPCTL_STARTUP_PY'
<startup.py>
RPCTL_STARTUP_PY
if [ "${RPCTL_PREWARM:-1}" = 1 ]; then
python3 /run/rpctl-startup.py prewarm &
fi
if python3 /run/rpctl-startup.py prepare; then
source /run/rpctl-runtime.env
else
echo "RPCTL_PHASE cache preparation failed; continuing with normal startup"
fi
python3 /run/rpctl-startup.py watch &
echo "RPCTL_PHASE starting vLLM" | tee -a /workspace/vllm.log
python3 /run/rpctl-startup.py vllm serve <args> 2>&1 | tee -a /workspace/vllm.log"#;

/// Every vLLM flag the boot may pass, with all optional features switched
/// on. A new flag can change startup time (profiling, compilation, warmup).
const APPROVED_VLLM_FLAGS: [&str; 17] = [
    "--host",
    "--port",
    "--served-model-name",
    "--api-key",
    "--max-model-len",
    "--gpu-memory-utilization",
    "--max-num-seqs",
    "--tensor-parallel-size",
    "--reasoning-parser",
    "--limit-mm-per-prompt",
    "--enable-auto-tool-choice",
    "--tool-call-parser",
    "--kv-cache-dtype",
    "--speculative-config",
    "--trust-remote-code",
    "--safetensors-load-strategy",
    // Approved 2026-06-26 after measured boots: 172s with a one-time
    // torch.compile rebuild (72s), then 103s with the cache hit
    // (torch.compile 4s), against 97-117s without it. rpctl kb
    // f052-rp-up-prefix-cache-boot.
    "--enable-prefix-caching",
];

/// rpctl's `foreground_skeleton`, line for line.
fn foreground_skeleton(cfg: &Config) -> String {
    let script = build_boot_script(cfg, "KEY");
    let repo = at(cfg, "MODEL_REPO");
    let sshd = "<sshd subshell, backgrounded>";
    let mut lines: Vec<String> = Vec::new();
    let (mut skip_heredoc, mut depth) = (false, false);
    for raw in script.lines() {
        let line = raw.trim();
        if skip_heredoc {
            if line == "RPCTL_STARTUP_PY" {
                skip_heredoc = false;
                lines.extend(["<startup.py>".to_string(), line.to_string()]);
            }
            continue;
        }
        if depth {
            if line.starts_with(") ") && line.ends_with('&') {
                depth = false;
                lines.push(sshd.to_string());
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with(r#"if [ -n "${PUBLIC_KEY"#) {
            depth = true;
            continue;
        }
        if line == "fi" && lines.last().is_some_and(|l| l == sshd) {
            continue;
        }
        if line == "(" {
            continue;
        }
        if line.starts_with("cat > /run/rpctl-startup.py") {
            skip_heredoc = true;
        }
        // Flags are checked by vllm_flags_are_approved.
        // rpctl's line.index raises without " 2>&1"; so does this.
        let line = match line.find("vllm serve ") {
            Some(start) => {
                let end = line.find(" 2>&1").unwrap();
                format!("{}vllm serve <args>{}", &line[..start], &line[end..])
            }
            None => line.to_string(),
        };
        lines.push(line.replace(repo, "<repo>"));
    }
    lines.join("\n")
}

#[test]
fn foreground_boot_script_is_unchanged() {
    assert_eq!(
        foreground_skeleton(&Config::defaults()),
        APPROVED_FOREGROUND,
        "{HOW_TO_APPROVE}APPROVED_FOREGROUND"
    );
}

#[test]
fn vllm_flags_are_approved() {
    let cfg = with([
        ("KV_CACHE_DTYPE", "fp8"),
        ("MTP_SPECULATIVE_TOKENS", "3"),
        ("TRUST_REMOTE_CODE", "1"),
    ]);
    let flags: BTreeSet<String> = build_vllm_args(&cfg, "KEY", None)
        .into_iter()
        .filter(|a| a.starts_with("--"))
        .collect();
    let unapproved: Vec<&String> = flags
        .iter()
        .filter(|f| !APPROVED_VLLM_FLAGS.contains(&f.as_str()))
        .collect();
    assert!(
        unapproved.is_empty(),
        "{unapproved:?}{HOW_TO_APPROVE}APPROVED_VLLM_FLAGS"
    );
}

#[test]
fn no_installs_or_downloads_in_the_foreground() {
    let skeleton = foreground_skeleton(&Config::defaults());
    for banned in [
        "apt-get",
        "pip ",
        "curl ",
        "wget ",
        "sleep ",
        "huggingface-cli",
        "hf download",
        "git clone",
    ] {
        assert!(!skeleton.contains(banned), "{banned}");
    }
}

#[test]
fn default_image_is_stock_docker_hub() {
    // A custom image a host has not cached cost 12m45s on 2026-06-25.
    assert!(at(&Config::defaults(), "VLLM_IMAGE").starts_with("vllm/vllm-openai:"));
}

#[test]
fn prewarm_and_watch_are_backgrounded() {
    let skeleton = foreground_skeleton(&Config::defaults());
    assert!(skeleton.contains("python3 /run/rpctl-startup.py prewarm &"));
    assert!(skeleton.contains("python3 /run/rpctl-startup.py watch &"));
}

// ---------------------------------------------------- rpctl tests/test_startup.py

#[test]
fn boot_script_syntax() {
    let mut bash = Command::new("bash")
        .arg("-n")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let script = build_boot_script(&Config::defaults(), "test-key");
    std::io::Write::write_all(&mut bash.stdin.take().unwrap(), script.as_bytes()).unwrap();
    assert!(bash.wait().unwrap().success());
}

/// The fake sshd of rpctl's test: it accepts the key only with mode 0600
/// in a 0700 directory and with the public half the pod was given.
const FAKE_SSHD: &str = r#"#!/usr/bin/env python3
import os, sys, subprocess
from pathlib import Path
key=Path(sys.argv[sys.argv.index('-h')+1])
assert key.stat().st_mode & 0o777 == 0o600
assert key.parent.stat().st_mode & 0o777 == 0o700
actual=subprocess.check_output(['ssh-keygen','-y','-f',str(key)],text=True).split()[:2]
assert actual == os.environ['PUBLIC_KEY'].split()[:2]
print('key accepted',flush=True)
if '-D' in sys.argv: Path(os.environ['TEST_READY']).touch()
"#;

/// Runs the script's sshd part to its end, at most 10 s as rpctl's test
/// does; its stderr. Output goes to a file, so no pipe can fill up while
/// it runs, and the script gets its own process group, so a timeout kills
/// the backgrounded subshell and tee with it.
fn run_sshd_part(script: &str, public_key: &str, ready: &Path) -> String {
    let stderr = ready.with_file_name("stderr");
    let mut child = Command::new("bash")
        .args(["-c", &format!("{script}\nwait")])
        .env("PUBLIC_KEY", public_key)
        .env("TEST_READY", ready)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .process_group(0)
        .spawn()
        .unwrap();
    let mut done = false;
    for _ in 0..100 {
        done = child.try_wait().unwrap().is_some();
        if done {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !done {
        let group = format!("-{}", child.id());
        let _ = Command::new("kill").args(["-KILL", "--", &group]).status();
        let _ = child.wait();
    }
    let text = std::fs::read_to_string(&stderr).unwrap();
    assert!(done, "the sshd part did not finish in 10 s: {text}");
    text
}

#[test]
fn world_writable_volume_key_keeps_identity_on_local_disk() {
    let dir = ScratchDir::new("boot-sshd").unwrap();
    let root = dir.path();
    for name in ["workspace/.sshd", "etc/ssh", "bin"] {
        std::fs::create_dir_all(root.join(name)).unwrap();
    }
    let seed = root.join("workspace/.sshd/ssh_host_ed25519_key");
    let made = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&seed)
        .status()
        .unwrap();
    assert!(made.success());
    let original = std::fs::read(&seed).unwrap();
    std::fs::set_permissions(&seed, std::fs::Permissions::from_mode(0o666)).unwrap();
    std::fs::write(
        root.join("etc/ssh/sshd_config"),
        "#PermitRootLogin yes\n#PasswordAuthentication yes\n",
    )
    .unwrap();
    let daemon = root.join("bin/sshd");
    std::fs::write(&daemon, FAKE_SSHD).unwrap();
    std::fs::set_permissions(&daemon, std::fs::Permissions::from_mode(0o700)).unwrap();

    let script = build_boot_script(&Config::defaults(), "test-key");
    let start = script.find(r#"if [ -n "${PUBLIC_KEY"#).unwrap();
    let end = script.find("# Restore local").unwrap();
    let mut script = script[start..end].to_string();
    for path in [
        "/workspace",
        "/root/.ssh",
        "/var/run/sshd",
        "/run/rpctl-sshd",
        "/etc/ssh",
    ] {
        let moved = root.join(path.trim_start_matches('/'));
        script = script.replace(path, moved.to_str().unwrap());
    }
    let script = script.replace("/usr/sbin/sshd", daemon.to_str().unwrap());
    let public_key = std::fs::read_to_string(seed.with_extension("pub")).unwrap();
    let ready = root.join("ready");

    let stderr = run_sshd_part(&script, &public_key, &ready);
    assert!(ready.exists(), "{stderr}");
    assert_eq!(std::fs::read(&seed).unwrap(), original);
    let mode = std::fs::metadata(&seed).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o666);
    let log = std::fs::read_to_string(root.join("workspace/sshd.log")).unwrap();
    assert!(log.contains("key accepted"));

    std::fs::remove_file(&ready).unwrap();
    std::fs::write(&daemon, "#!/bin/sh\necho invalid-config >&2\nexit 23\n").unwrap();
    let stderr = run_sshd_part(&script, &public_key, &ready);
    assert!(!ready.exists());
    assert!(
        stderr.contains("RPCTL_ERROR sshd setup or daemon failed (exit 23)"),
        "{stderr}"
    );
}

#[test]
fn cache_archive_survives_image_and_config_changes() {
    // vLLM keys compiled variants itself; an image or argument change must
    // not throw away every other variant on the volume. The archive-name
    // half is in startup_tests.py.
    for extra in [
        &[][..],
        &[("VLLM_IMAGE", "other/image:tag")],
        &[("SAFETENSORS_LOAD_STRATEGY", "eager")],
    ] {
        let script = build_boot_script(&with(extra.iter().copied()), "k");
        assert!(!script.contains("RPCTL_CACHE_KEY") && !script.contains("RPCTL_IMAGE"));
    }
}

#[test]
fn default_image_is_docker_hub() {
    let image = DEFAULTS.iter().find(|(k, _)| *k == "VLLM_IMAGE").unwrap().1;
    assert_eq!(image, "vllm/vllm-openai:v0.27.1");
}

/// rpctl's tests of startup.py alone, run on the embedded copy (F17: the
/// foreground source hash among them).
#[test]
fn startup_py_passes_its_tests() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/startup_tests.py");
    let out = Command::new("python3")
        .args(["-I", "-B"])
        .arg(&tests)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Ran 24 tests"), "{stderr}");
}

// ------------------------------------------------------- rpctl tests/test_idle.py

#[test]
fn pod_env_carries_the_limits_and_the_watchdog_key() {
    let cfg = with([("RUNPOD_WATCHDOG_API_KEY", "rpa_w")]);
    let env = build_pod_body(&cfg, "k", "v", None).unwrap()["env"].take();
    assert_eq!(env["RPCTL_WATCHDOG_KEY"], "rpa_w");
    assert_eq!(env["RPCTL_IDLE_TTL_S"], "3600");
    assert_eq!(env["RPCTL_MAX_POD_S"], "28800");
    let cfg = with([("IDLE_TTL_MIN", "0")]);
    let off = build_pod_body(&cfg, "k", "v", None).unwrap()["env"].take();
    assert_eq!(off["RPCTL_IDLE_TTL_S"], "0");
    assert!(off.get("RPCTL_WATCHDOG_KEY").is_none());
}

#[test]
fn hosts_are_limited_to_drivers_the_image_supports() {
    let body = build_pod_body(&Config::defaults(), "k", "v", None).unwrap();
    assert_eq!(body["allowedCudaVersions"], json!(["13.0"]));
    let body = build_pod_body(&with([("CUDA_VERSIONS", "")]), "k", "v", None).unwrap();
    assert!(body.get("allowedCudaVersions").is_none());
}

// rpctl's test_features.py ContextTests.test_prefix_caching_flag_follows_config.
#[test]
fn prefix_caching_flag_follows_config() {
    let on = with([("VLLM_PORT", "8000"), ("SERVED_MODEL_NAME", "m")]);
    assert!(build_vllm_args(&on, "k", None).contains(&"--enable-prefix-caching".to_string()));
    let off = with([
        ("VLLM_PORT", "8000"),
        ("SERVED_MODEL_NAME", "m"),
        ("PREFIX_CACHING", "0"),
    ]);
    assert!(!build_vllm_args(&off, "k", None).contains(&"--enable-prefix-caching".to_string()));
}
