use std::collections::HashMap;
use std::path::Path;

use pretty_assertions::assert_eq;
use serde_json::Value;
use viy_testkit::fixtures;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// A scratch tree with viy's global files under `config/viy` and a project
/// under `project`.
struct Tree {
    scratch: ScratchDir,
}

impl Tree {
    fn new() -> Self {
        let scratch = ScratchDir::new("config").unwrap();
        std::fs::create_dir_all(scratch.path().join("project")).unwrap();
        Self { scratch }
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.scratch.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn paths(&self) -> Paths {
        let root = self.scratch.path();
        Paths::from_vars(root.join("home"), |name| match name {
            "XDG_CONFIG_HOME" => Some(root.join("config").into_os_string()),
            _ => None,
        })
    }

    fn load_with(&self, env: &[(&str, &str)]) -> Config {
        let project = Project::find(&self.scratch.path().join("project")).unwrap();
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Layers::new(&self.paths(), &project)
            .load(|key| env.get(key).cloned())
            .unwrap()
    }

    fn load(&self) -> Config {
        self.load_with(&[])
    }
}

fn as_map(cfg: &Config) -> Vec<(String, String)> {
    cfg.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn replay(case: &Value, marker: &str) -> Config {
    let tree = Tree::new();
    let text = |key: &str| case[key].as_str();
    if let Some(global) = text("global_config") {
        tree.write("config/viy/config", global);
    }
    for (name, profile) in case["profiles"].as_object().unwrap() {
        tree.write(
            &format!("config/viy/profiles/{name}.env"),
            profile.as_str().unwrap(),
        );
    }
    let project = tree.scratch.path().join("project");
    if !text("project_marker").unwrap().is_empty() {
        std::fs::create_dir(project.join(marker)).unwrap();
    }
    if let Some(config) = text("project_config") {
        tree.write(&format!("project/{marker}/config"), config);
    }
    let cwd = project.join(text("cwd").unwrap());
    std::fs::create_dir_all(&cwd).unwrap();
    let env: HashMap<String, String> = case["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    Layers::new(&tree.paths(), &Project::find(&cwd).unwrap())
        .load(|key| env.get(key).cloned())
        .unwrap()
}

#[test]
fn the_layers_merge_as_rpctl_merges_them() {
    // Each case with the project's file under rp's marker and under viy's.
    let fixture = fixtures::load("config/layering").unwrap();
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let mut expected: Vec<(String, String)> = case["config"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        if name == "global_file_syntax" {
            // Python's strip() removes U+001F, Rust's trim() keeps it
            // (kb d032).
            for (key, value) in &mut expected {
                if key == "ENDPOINT_MODEL" {
                    *value = "unit-sep\u{1f}".to_string();
                }
            }
        }
        for marker in [".rpctl", ".viy"] {
            assert_eq!(
                as_map(&replay(case, marker)),
                expected,
                "{name} under {marker}"
            );
        }
    }
}

#[test]
fn defaults_are_rpctls_110_keys() {
    assert_eq!(DEFAULTS.len(), 110);
    assert_eq!(Config::defaults().iter().count(), 110);
}

#[test]
fn consult_guarded_is_what_the_guard_restores() {
    let mut keys: Vec<&str> = CONSULT_PINNED.to_vec();
    keys.extend(CONSULT_STRICTER.iter().map(|(k, _)| *k));
    assert_eq!(keys, CONSULT_GUARDED);
}

// rpctl tests/test_consult.py ConfigGuardTests
#[test]
fn project_config_cannot_loosen_consult() {
    let tree = Tree::new();
    tree.write("config/viy/config", "");
    tree.write(
        "project/.viy/config",
        "CONSULT_CLAUDE_BIN=/tmp/evil\nCONSULT_APPROVAL=auto\n\
         CONSULT_PERMISSION_MODE=plan\nCONSULT_ROOT=/tmp/x\nCONSULT_MODEL=sonnet\n",
    );
    let cfg = tree.load();
    assert_eq!(cfg.get("CONSULT_CLAUDE_BIN"), Some("claude"));
    assert_eq!(cfg.get("CONSULT_APPROVAL"), Some("ask"));
    assert_eq!(
        cfg.get("CONSULT_PERMISSION_MODE"),
        Some("bypassPermissions")
    );
    assert_eq!(cfg.get("CONSULT_ROOT"), Some(""));
    assert_eq!(cfg.get("CONSULT_MODEL"), Some("sonnet")); // not a guard
    tree.write(
        "project/.viy/config",
        "CONSULT_MODE=advise\nCONSULT_ENABLED=0\n",
    );
    let cfg = tree.load();
    assert_eq!(
        (cfg.get("CONSULT_MODE"), cfg.get("CONSULT_ENABLED")),
        (Some("advise"), Some("0"))
    );
}

// rpctl tests/test_features.py ConfigGuardTests
#[test]
fn project_config_cannot_start_auto_yes() {
    let tree = Tree::new();
    tree.write("config/viy/config", "");
    tree.write("project/.viy/config", "CHAT_MODE=auto-yes\n");
    assert_eq!(tree.load().get("CHAT_MODE"), Some("normal"));
    // your own config may; the project may then only go back to normal
    tree.write("config/viy/config", "CHAT_MODE=auto-yes\n");
    assert_eq!(tree.load().get("CHAT_MODE"), Some("auto-yes"));
    tree.write("project/.viy/config", "CHAT_MODE=normal\n");
    assert_eq!(tree.load().get("CHAT_MODE"), Some("normal"));
}

// rpctl tests/test_features.py ConfigGuardTests
#[test]
fn profile_blank_volume_id_clears_the_global_one() {
    // viy model add writes NETWORK_VOLUME_ID= so the new model gets its own
    // volume; if the blank were skipped it would boot on the old volume.
    let tree = Tree::new();
    tree.write(
        "config/viy/config",
        "MODEL_PROFILE=other\nNETWORK_VOLUME_ID=old\nPOD_NAME=a\nTAVILY_API_KEY=k\n",
    );
    tree.write(
        "config/viy/profiles/other.env",
        "NETWORK_VOLUME_ID=\nPOD_NAME=b\nTAVILY_API_KEY=\n",
    );
    let cfg = tree.load();
    assert_eq!(cfg.get("NETWORK_VOLUME_ID"), Some(""));
    assert_eq!(cfg.get("POD_NAME"), Some("b"));
    assert_eq!(cfg.get("TAVILY_API_KEY"), Some("k")); // not a profile key: blank skipped
}

// rpctl tests/test_egress.py ChatEgressTests
#[test]
fn project_config_cannot_widen_the_network() {
    let tree = Tree::new();
    tree.write("config/viy/config", "SANDBOX_NET_ALLOW=mirror.corp.test\n");
    tree.write(
        "project/.viy/config",
        "SANDBOX_NET=host\nSANDBOX_NET_ALLOW=evil.test\n",
    );
    let cfg = tree.load();
    assert_eq!(cfg.get("SANDBOX_NET"), Some("proxy"));
    assert_eq!(cfg.get("SANDBOX_NET_ALLOW"), Some("mirror.corp.test"));
    tree.write("config/viy/config", "SANDBOX_NET=host\n");
    tree.write("project/.viy/config", "SANDBOX_NET=proxy\n");
    assert_eq!(tree.load().get("SANDBOX_NET"), Some("proxy")); // tighter is fine
}

#[test]
fn a_tightening_in_rps_project_file_holds_under_viys() {
    // kb d039: each project file is guarded on its own.
    let tree = Tree::new();
    tree.write(
        "config/viy/config",
        "CHAT_MODE=auto-yes\nCONSULT_APPROVAL=auto\nSANDBOX_NET=host\nPOD_NAME=global\n",
    );
    tree.write(
        "project/.rpctl/config",
        "CHAT_MODE=normal\nCONSULT_APPROVAL=ask\nSANDBOX_NET=proxy\nPOD_NAME=rp\nTOP_K=10\n",
    );
    tree.write(
        "project/.viy/config",
        "CHAT_MODE=auto-yes\nCONSULT_APPROVAL=auto\nSANDBOX_NET=host\nPOD_NAME=viy\n",
    );
    let cfg = tree.load();
    assert_eq!(cfg.get("CHAT_MODE"), Some("normal"));
    assert_eq!(cfg.get("CONSULT_APPROVAL"), Some("ask"));
    assert_eq!(cfg.get("SANDBOX_NET"), Some("proxy"));
    assert_eq!(cfg.get("POD_NAME"), Some("viy"));
    assert_eq!(cfg.get("TOP_K"), Some("10"));
}

#[test]
fn the_environment_wins_for_known_keys_when_not_empty() {
    let tree = Tree::new();
    tree.write("config/viy/config", "POD_NAME=global\nEXTRA=file\n");
    let cfg = tree.load_with(&[
        ("POD_NAME", "env"),
        ("MAX_TOKENS", ""),
        ("EXTRA", "env"),
        ("UNKNOWN", "x"),
    ]);
    assert_eq!(cfg.get("POD_NAME"), Some("env"));
    assert_eq!(cfg.get("MAX_TOKENS"), Some("16384"));
    assert_eq!(cfg.get("EXTRA"), Some("env"));
    assert_eq!(cfg.get("UNKNOWN"), None);
}

#[test]
fn an_unreadable_file_is_an_error_naming_it() {
    let tree = Tree::new();
    tree.write("config/viy/config", "");
    let global = tree.paths().config_file();
    std::fs::write(&global, b"POD_NAME=\xff\n").unwrap();
    let project = Project::find(&tree.scratch.path().join("project")).unwrap();
    let err = Layers::new(&tree.paths(), &project)
        .load(|_| None)
        .unwrap_err();
    assert!(
        err.to_string().contains(&global.display().to_string()),
        "{err}"
    );
}

#[test]
fn parse_skips_comments_and_keeps_the_first_position() {
    let tree = Tree::new();
    tree.write("f", "# c\nB=1\n\nno equals\nA=\"q\"\nB=2\n=empty key\n");
    let parsed = parse_env_file(&tree.scratch.path().join("f")).unwrap();
    let got: Vec<(&str, &str)> = parsed
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(got, [("B", "2"), ("A", "q"), ("", "empty key")]);
    assert!(
        parse_env_file(Path::new("/nonexistent/viy-config"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn sandbox_net_host_is_trimmed_and_ascii_case_blind() {
    for value in ["host", " HoSt ", "HOST"] {
        assert!(sandbox_net_is_host(Some(value)), "{value:?}");
    }
    for value in [
        None,
        Some(""),
        Some("proxy"),
        Some("hosts"),
        Some("host\u{1f}"),
    ] {
        assert!(!sandbox_net_is_host(value), "{value:?}");
    }
}

#[test]
fn tunnel_and_gpu_getters_follow_rpctl() {
    let mut cfg = Config::defaults();
    assert!(tunnel_enabled(&cfg));
    for off in ["", " 0 ", "false", "no"] {
        cfg.set("TUNNEL", off);
        assert!(!tunnel_enabled(&cfg), "{off:?}");
    }
    cfg.set("TUNNEL", "No");
    assert!(tunnel_enabled(&cfg)); // rpctl compares case-sensitively
    assert_eq!(tunnel_local_port(&cfg), "8000");
    cfg.set("TUNNEL_LOCAL_PORT", " 18000 ");
    assert_eq!(tunnel_local_port(&cfg), "18000");
    cfg.set("GPU_TYPE_IDS", " a , ,b,");
    assert_eq!(gpu_type_list(&cfg), ["a", "b"]);
}

#[test]
fn a_missing_api_key_says_where_to_put_it() {
    let paths = Paths::under("/h");
    let mut cfg = Config::defaults();
    assert_eq!(
        require_api_key(&cfg, &paths).unwrap_err(),
        "RUNPOD_API_KEY is not set.\n  Add it to /h/.config/viy/config\n  \
         Get one at https://console.runpod.io/user/settings -> API Keys"
    );
    cfg.set("RUNPOD_API_KEY", "k");
    assert_eq!(require_api_key(&cfg, &paths), Ok("k"));
}
