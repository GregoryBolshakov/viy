use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use pretty_assertions::assert_eq;
use viy_testkit::fixtures;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// rp's names as viy's: the marker and the config directory.
fn as_viy(text: &str) -> String {
    text.replace(".rpctl", ".viy")
        .replace("$T/config/rpctl/", "$T/config/viy/")
}

fn paths(root: &Path) -> Paths {
    let config_home = root.join("config");
    Paths::from_vars(root.join("home"), move |name| {
        (name == "XDG_CONFIG_HOME").then(|| config_home.clone().into_os_string())
    })
}

/// Every path under `dir`, directories with a trailing `/`, and the files'
/// text, as the fixture lists them.
fn tree(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.insert(format!("{rel}/"), String::new());
                stack.push(path);
            } else {
                out.insert(rel, fs::read_to_string(&path).unwrap());
            }
        }
    }
    out
}

#[test]
fn viy_init_does_what_rp_init_does() {
    let fixture = fixtures::load("config/init").unwrap();
    assert_eq!(fixture.cases.len(), 7);
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let scratch = ScratchDir::new("init").unwrap();
        let root = scratch.path();
        let paths = paths(root);
        if let Some(text) = case["global_config"].as_str() {
            fs::create_dir_all(paths.config_dir()).unwrap();
            fs::write(paths.config_file(), text).unwrap();
        }
        let project = root.join("project");
        fs::create_dir(&project).unwrap();
        for (rel, text) in case["project_files"].as_object().unwrap() {
            let path = project.join(as_viy(rel));
            if rel.ends_with('/') {
                fs::create_dir_all(path).unwrap();
            } else {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, text.as_str().unwrap()).unwrap();
            }
        }
        let mut out = Vec::new();
        let result = cmd_init(
            &paths,
            &project.join(case["cwd"].as_str().unwrap()),
            &mut out,
        );
        let expected_files: BTreeMap<String, String> = case["project_files_after"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (as_viy(k), v.as_str().unwrap().to_string()))
            .collect();
        assert_eq!(tree(&project), expected_files, "{name}");
        if case["exit"] == 0 {
            assert_eq!(result, Ok(()), "{name}");
            let expected =
                as_viy(case["stdout"].as_str().unwrap()).replace("$T", &root.to_string_lossy());
            assert_eq!(String::from_utf8(out).unwrap(), expected, "{name}");
        } else {
            // rp fails with a Python traceback; viy with the path and the
            // OS error, nothing written.
            let err = result.unwrap_err();
            assert!(err.contains("Not a directory"), "{name}: {err}");
            assert_eq!(out, b"", "{name}");
        }
    }
}

/// Not in rp: a folder rp marked gets viy's own marker beside it, and
/// rp's is left as it was (kb d039).
#[test]
fn an_rp_project_gets_viys_marker_and_rps_is_untouched() {
    let scratch = ScratchDir::new("init").unwrap();
    let paths = paths(scratch.path());
    let project = scratch.path().join("project");
    fs::create_dir_all(project.join(".rpctl")).unwrap();
    fs::write(project.join(".rpctl").join("config"), "CHAT_MODE=normal\n").unwrap();
    let mut out = Vec::new();
    cmd_init(&paths, &project, &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.starts_with("initialised project at "), "{out}");
    let files = tree(&project);
    assert_eq!(files[".rpctl/config"], "CHAT_MODE=normal\n");
    assert_eq!(
        files.keys().filter(|k| k.starts_with(".rpctl")).count(),
        2,
        "{files:?}"
    );
    assert!(files.contains_key(".viy/transcripts/"));
}

/// Not in rp: without viy's config but with rp's, the hint is migrate.
#[test]
fn the_hint_names_migrate_when_rp_has_a_config() {
    let scratch = ScratchDir::new("init").unwrap();
    let paths = paths(scratch.path());
    fs::create_dir_all(paths.rpctl_config_dir()).unwrap();
    fs::write(paths.rpctl_config_dir().join("config"), "POD_NAME=p\n").unwrap();
    let project = scratch.path().join("project");
    fs::create_dir(&project).unwrap();
    let mut out = Vec::new();
    cmd_init(&paths, &project, &mut out).unwrap();
    let last = String::from_utf8(out)
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .to_string();
    assert_eq!(
        last,
        format!(
            "  next: viy migrate copies rp's config and profiles to {}",
            paths.config_dir().display()
        )
    );
}
