//! The system prompt with its instruction files against rpctl's own, for
//! the synthetic trees of `tests/fixtures/prompt/system.json`: every case
//! is laid out again in a scratch directory, at viy's config directory in
//! place of rp's, and the prompt, the workspace, the files loaded and what
//! `/instructions` prints must be rp's.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_config::Paths;
use viy_config::Project;
use viy_testkit::fixtures;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::prompt::SystemPrompt;
use crate::prompt::workspace;

fn texts(case: &Value, key: &str) -> Vec<(String, String)> {
    case[key]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect()
}

/// `\xNN` escapes to bytes, as the generator writes raw files.
fn raw_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = text.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'\\' && tail.first() == Some(&b'x') {
            let hex = std::str::from_utf8(&tail[1..3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            rest = &tail[3..];
        } else {
            out.push(b);
            rest = tail;
        }
    }
    out
}

/// What viy makes of one case: the fixture's output fields.
fn replay(case: &Value) -> Value {
    let scratch = ScratchDir::new("prompt").unwrap();
    let t = std::fs::canonicalize(scratch.path()).unwrap();
    let at = |rel: &str| t.join(rel.replace("{GLOBAL}", "config/viy"));
    for dir in ["home", "config", "state", "data", "cache", "project"] {
        std::fs::create_dir_all(t.join(dir)).unwrap();
    }
    if case["marker"] == true {
        std::fs::create_dir(t.join("project/.rpctl")).unwrap();
    }
    let cwd = t.join(case["cwd"].as_str().unwrap());
    std::fs::create_dir_all(&cwd).unwrap();
    let write = |rel: &str, bytes: &[u8]| {
        let path = at(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    for (rel, text) in texts(case, "files") {
        write(&rel, text.as_bytes());
    }
    for (rel, text) in texts(case, "raw_files") {
        write(&rel, &raw_bytes(&text));
    }
    for rel in case["executable"].as_array().unwrap() {
        let path = at(rel.as_str().unwrap());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for (link, target) in texts(case, "symlinks") {
        std::os::unix::fs::symlink(target, at(&link)).unwrap();
    }

    let mut cfg = Config::defaults();
    for (key, value) in [
        ("VLLM_API_KEY", "k"),
        ("TRACE_ENABLED", "0"),
        ("CONSULT_ENABLED", "0"),
    ] {
        cfg.set(key, value);
    }
    for (key, value) in texts(case, "config") {
        cfg.set(&key, &value.replace("$T", t.to_str().unwrap()));
    }
    let on = |key: &str| cfg.get(key) == Some("1");
    let paths = Paths::from_vars(t.join("home"), |var| match var {
        "XDG_CONFIG_HOME" => Some(t.join("config").into()),
        "XDG_STATE_HOME" => Some(t.join("state").into()),
        "XDG_DATA_HOME" => Some(t.join("data").into()),
        _ => None,
    });
    let project = Project::find(&cwd).unwrap();
    let ws = workspace(&cfg, project.root(), &paths.home);
    let loaded = if on("INSTRUCTIONS_ENABLED") {
        load(&cfg, &paths.config_dir(), &ws)
    } else {
        Vec::new()
    };
    let block = render(&loaded);
    let tools_on = on("TOOLS_ENABLED");
    let claude = cfg
        .get("CONSULT_CLAUDE_BIN")
        .filter(|b| !b.is_empty())
        .unwrap_or("claude");
    let consult = tools_on
        && on("CONSULT_ENABLED")
        && viy_config::vault::find_executable(Some("/usr/bin:/bin".as_ref()), claude).is_some();

    let system = SystemPrompt {
        user: case["user_system"].as_str(),
        tools_on,
        search_on: on("SEARCH_ENABLED"),
        guide_on: on("SYSTEM_GUIDE"),
        explore_trust: on("EXPLORE_ENABLED") && on("SYSTEM_GUIDE_TRUST_EXPLORE"),
        consult,
        toolbox_index: None,
        workspace: &ws,
        today: "{TODAY}",
        instructions: block.as_deref(),
    }
    .render();
    // viy's own config directory is where rp names its own: the one
    // difference in what the model reads (kb d071).
    let shown = |text: &str| {
        text.replace(t.to_str().unwrap(), "$T")
            .replace("$T/config/viy/", "$T/config/rpctl/")
    };
    let loaded_rows: Vec<Value> = loaded
        .iter()
        .map(|l| {
            json!([
                shown(&l.path.display().to_string()),
                l.text.chars().count(),
                l.truncated
            ])
        })
        .collect();
    json!({
        "system": system.as_deref().map(shown),
        "workspace": shown(&ws.display().to_string()),
        "loaded": loaded_rows,
        "report": report(on("INSTRUCTIONS_ENABLED"), &names(&cfg), &loaded)
            .iter()
            .map(|l| shown(l))
            .collect::<Vec<_>>(),
    })
}

#[test]
fn every_case_equals_rpctl() {
    let fixture = fixtures::load("prompt/system").unwrap();
    assert!(fixture.cases.len() >= 25);
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let want = json!({
            "system": case["system"],
            "workspace": case["workspace"],
            "loaded": case["loaded"],
            "report": case["report"],
        });
        assert_eq!(replay(case), want, "{name}");
    }
}

#[test]
fn a_link_to_a_file_already_found_counts_once() {
    let scratch = ScratchDir::new("instructions").unwrap();
    let ws = std::fs::canonicalize(scratch.path()).unwrap();
    std::fs::write(ws.join("AGENTS.md"), "rules").unwrap();
    std::os::unix::fs::symlink("AGENTS.md", ws.join("CLAUDE.md")).unwrap();
    let names = names(&Config::defaults());
    assert_eq!(
        find(&names, Path::new("/nonexistent"), &ws),
        [ws.join("AGENTS.md")]
    );
}
