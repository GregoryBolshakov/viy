use std::ffi::OsString;
use std::path::Path;

use pretty_assertions::assert_eq;
use serde_json::Value;
use viy_testkit::fixtures;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::Paths;

/// Builds a case's tree under `root`.
fn build(root: &Path, case: &Value) {
    for dir in case["dirs"].as_array().unwrap() {
        std::fs::create_dir_all(root.join(dir.as_str().unwrap())).unwrap();
    }
    for file in case["files"].as_array().unwrap() {
        std::fs::write(root.join(file.as_str().unwrap()), "").unwrap();
    }
    for (link, target) in case["symlinks"].as_object().unwrap() {
        std::os::unix::fs::symlink(target.as_str().unwrap(), root.join(link)).unwrap();
    }
}

/// The case's XDG variables: absent ones point into `root`, `null` is unset.
fn xdg(root: &Path, case: &Value, name: &str) -> Option<OsString> {
    match case["xdg"].get(name) {
        Some(Value::Null) => None,
        Some(value) => Some(value.as_str().unwrap().into()),
        None => {
            let sub = match name {
                "XDG_CONFIG_HOME" => "config",
                "XDG_STATE_HOME" => "state",
                "XDG_DATA_HOME" => "data",
                _ => return None,
            };
            Some(root.join(sub).into_os_string())
        }
    }
}

#[test]
fn the_root_and_global_paths_match_rpctl() {
    // rpctl's paths with its names for viy's: `rpctl` under the XDG roots
    // becomes `viy`, `.rpctl/` in the project `.viy/`. The pod state stays
    // at rp's paths until cutover (kb d006).
    let fixture = fixtures::load("config/project").unwrap();
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let scratch = ScratchDir::new("project").unwrap();
        let root = scratch.path();
        build(root, case);
        let project = Project::find(&root.join(case["cwd"].as_str().unwrap())).unwrap();
        let paths = Paths::from_vars(root.join("home"), |var| xdg(root, case, var));

        let shown = |path: &Path| path.to_str().unwrap().replace(root.to_str().unwrap(), "$T");
        let rpctl = |key: &str| case["paths"][key].as_str().unwrap().to_string();
        let as_viy = |key: &str| {
            rpctl(key)
                .replace("/rpctl", "/viy")
                .replace("/.rpctl/", "/.viy/")
        };
        let got = [
            ("PROJECT_DIR", shown(project.root()), rpctl("PROJECT_DIR")),
            (
                "HISTORY_PATH",
                shown(&project.history_file()),
                as_viy("HISTORY_PATH"),
            ),
            (
                "USER_CONFIG_DIR",
                shown(&paths.config_dir()),
                as_viy("USER_CONFIG_DIR"),
            ),
            (
                "USER_CONFIG_DIR",
                shown(&paths.rpctl_config_dir()),
                rpctl("USER_CONFIG_DIR"),
            ),
            (
                "CONFIG_PATH",
                shown(&paths.config_file()),
                as_viy("CONFIG_PATH"),
            ),
            (
                "PROFILES_DIR",
                shown(&paths.profiles_dir()),
                as_viy("PROFILES_DIR"),
            ),
            (
                "USER_DATA_DIR",
                shown(&paths.data_dir()),
                as_viy("USER_DATA_DIR"),
            ),
            (
                "USER_DATA_DIR",
                shown(&paths.rpctl_data_dir()),
                rpctl("USER_DATA_DIR"),
            ),
            (
                "SESSIONS_DIR",
                shown(&paths.sessions_dir()),
                as_viy("SESSIONS_DIR"),
            ),
            (
                "USER_STATE_DIR",
                shown(&paths.state_dir()),
                as_viy("USER_STATE_DIR"),
            ),
            (
                "USER_STATE_DIR",
                shown(&paths.pod_state_dir()),
                rpctl("USER_STATE_DIR"),
            ),
            (
                "STATE_PATH",
                shown(&paths.pod_state_file()),
                rpctl("STATE_PATH"),
            ),
        ];
        for (key, viy, expected) in got {
            assert_eq!(viy, expected, "{name}: {key}");
        }
        assert_eq!(
            Value::Bool(project.has_marker()),
            case["paths"]["has_project"],
            "{name}: has_project"
        );
    }
}

#[test]
fn either_marker_makes_the_project_and_the_nearest_wins() {
    let scratch = ScratchDir::new("project").unwrap();
    let root = scratch.path();
    for dir in [
        "viy/.viy",
        "both/.viy",
        "both/.rpctl",
        "outer/.viy",
        "outer/inner/.rpctl",
    ] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    for dir in ["viy/src", "both/src", "outer/inner/src"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let find = |cwd: &str| Project::find(&root.join(cwd)).unwrap();
    assert_eq!(find("viy/src").root(), root.join("viy"));
    assert_eq!(find("both/src").root(), root.join("both"));
    assert_eq!(find("outer/inner/src").root(), root.join("outer/inner"));
    assert!(find("outer/inner/src").has_marker());
}

#[test]
fn viy_writes_under_viy_even_in_an_rpctl_project() {
    let scratch = ScratchDir::new("project").unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join(".rpctl")).unwrap();
    let project = Project::find(root).unwrap();
    assert_eq!(project.root(), root);
    assert!(project.has_marker());
    assert_eq!(project.dir(), root.join(".viy"));
    assert_eq!(project.rpctl_dir(), root.join(".rpctl"));
    assert_eq!(project.history_file(), root.join(".viy/history"));
    assert_eq!(
        project.config_files(),
        [root.join(".rpctl/config"), root.join(".viy/config")]
    );
}

#[test]
fn has_marker_reads_the_disk_each_time() {
    let scratch = ScratchDir::new("project").unwrap();
    let project = Project::find(scratch.path()).unwrap();
    assert!(!project.has_marker());
    std::fs::create_dir(project.dir()).unwrap();
    assert!(project.has_marker());
}

#[test]
fn a_missing_start_is_an_error() {
    let scratch = ScratchDir::new("project").unwrap();
    assert!(Project::find(&scratch.path().join("gone")).is_err());
}

/// Not in rpctl, which writes through it: a dangling `.gitignore` symlink
/// in the marker is left alone, not followed.
#[test]
fn init_does_not_write_through_a_gitignore_symlink() {
    let scratch = ScratchDir::new("init").unwrap();
    let marker = scratch.path().join(MARKER);
    std::fs::create_dir(&marker).unwrap();
    let elsewhere = scratch.path().join("elsewhere");
    std::os::unix::fs::symlink(&elsewhere, marker.join(".gitignore")).unwrap();
    let (project, created) = Project::init(scratch.path()).unwrap();
    assert!(!created);
    assert!(project.dir().is_dir());
    assert!(!elsewhere.exists());
}

/// A chat in a folder nobody marked marks it as `viy init` does, the
/// `.gitignore` included, and the root stays the folder (kb d118).
#[test]
fn a_chat_marks_a_folder_nobody_marked() {
    let scratch = ScratchDir::new("auto").unwrap();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    let home = root.join("home");
    let folder = root.join("work/app");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    assert_eq!(Project::find(&folder).unwrap().saved_history(), None);

    let project = Project::find_or_init(&folder, &home).unwrap();
    assert_eq!(project.root(), folder);
    assert_eq!(
        std::fs::read_to_string(project.dir().join(".gitignore")).unwrap(),
        "history\n"
    );
    assert_eq!(project.saved_history(), Some(project.history_file()));
    assert!(!root.join("work").join(MARKER).exists());
}

/// Inside a project, a subfolder gets no marker of its own.
#[test]
fn a_chat_in_a_marked_project_marks_nothing_new() {
    let scratch = ScratchDir::new("auto").unwrap();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    std::fs::create_dir_all(root.join("app").join(MARKER)).unwrap();
    std::fs::create_dir_all(root.join("app/src")).unwrap();
    let project = Project::find_or_init(&root.join("app/src"), &root.join("home")).unwrap();
    assert_eq!(project.root(), root.join("app"));
    assert!(!root.join("app/src").join(MARKER).exists());
    // An existing marker is not rewritten: no `.gitignore` appears in it.
    assert!(!project.dir().join(".gitignore").exists());
}

/// In rp's project the chat adds viy's marker with its `.gitignore` at
/// the root, before any history is written there, and leaves `.rpctl/`
/// alone.
#[test]
fn a_chat_in_an_rp_project_adds_viys_marker_beside_it() {
    let scratch = ScratchDir::new("auto").unwrap();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    std::fs::create_dir_all(root.join("app").join(RPCTL_MARKER)).unwrap();
    std::fs::create_dir_all(root.join("app/src")).unwrap();
    let project = Project::find_or_init(&root.join("app/src"), &root.join("home")).unwrap();
    assert_eq!(project.root(), root.join("app"));
    assert!(project.dir().join(".gitignore").is_file());
    assert_eq!(std::fs::read_dir(project.rpctl_dir()).unwrap().count(), 0);
}

/// Home and the folders above it are never marked, and a chat there keeps
/// its input history in memory only.
#[test]
fn a_chat_never_marks_home_or_above() {
    let scratch = ScratchDir::new("auto").unwrap();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    let home = root.join("home/me");
    std::fs::create_dir_all(&home).unwrap();
    for start in [home.clone(), root.join("home"), root.clone()] {
        let project = Project::find_or_init(&start, &home).unwrap();
        assert_eq!(project.root(), start);
        assert!(!project.dir().exists(), "{}", start.display());
        assert_eq!(project.saved_history(), None);
    }
    // Home named through a symlink is still home.
    let link = root.join("home-link");
    std::os::unix::fs::symlink(&home, &link).unwrap();
    assert!(!Project::find_or_init(&home, &link).unwrap().dir().exists());
}

/// A folder the chat cannot write to opens unmarked.
#[test]
fn a_read_only_folder_opens_unmarked() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = ScratchDir::new("auto").unwrap();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    let folder = root.join("ro");
    std::fs::create_dir(&folder).unwrap();
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
    let project = Project::find_or_init(&folder, &root.join("home"));
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
    let project = project.unwrap();
    assert_eq!(project.root(), folder);
    assert!(!project.dir().exists());
    assert_eq!(project.saved_history(), None);
}
