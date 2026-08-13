use std::os::unix::fs::PermissionsExt;

use pretty_assertions::assert_eq;
use viy_testkit::config_tree::ConfigTree;
use viy_testkit::config_tree::case_env;
use viy_testkit::config_tree::case_files;
use viy_testkit::fixtures;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::Layers;
use crate::Project;

fn paths(tree: &ConfigTree) -> Paths {
    let config_home = tree.config_home();
    Paths::from_vars(tree.home(), move |name| {
        (name == "XDG_CONFIG_HOME").then(|| config_home.clone().into_os_string())
    })
}

fn load(tree: &ConfigTree, case: &serde_json::Value) -> Config {
    let env = case_env(case);
    let project = Project::find(&tree.project()).unwrap();
    Layers::new(&paths(tree), &project)
        .load(|key| env.get(key).cloned())
        .unwrap()
}

#[test]
fn persist_writes_where_rpctl_writes() {
    let fixture = fixtures::load("config/persist").unwrap();
    assert_eq!(fixture.cases.len(), 18);
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        let tree = ConfigTree::new(case, ".viy").unwrap();
        let mut cfg = load(&tree, case);
        let key = case["key"].as_str().unwrap();
        persist(
            &mut cfg,
            &paths(&tree),
            key,
            case["value"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(cfg.get(key), case["cfg_value"].as_str(), "{name}");
        assert_eq!(tree.files().unwrap(), case_files(case), "{name}");
    }
}

#[test]
fn a_write_leaves_the_file_owner_only_and_no_temporary_behind() {
    let scratch = ScratchDir::new("profiles").unwrap();
    let paths = Paths::under(scratch.path());
    let file = paths.config_file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "POD_NAME=p\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o664)).unwrap();
    set_key(&paths, "VLLM_API_KEY", "sk-test").unwrap();
    let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let names: Vec<_> = fs::read_dir(paths.config_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["config"]);
}

#[test]
fn a_symlinked_file_is_written_through_and_stays_a_link() {
    let scratch = ScratchDir::new("profiles").unwrap();
    let paths = Paths::under(scratch.path());
    let real = scratch.path().join("dotfiles/viy-config");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, "POD_NAME=p\n").unwrap();
    fs::create_dir_all(paths.config_dir()).unwrap();
    std::os::unix::fs::symlink("../../dotfiles/viy-config", paths.config_file()).unwrap();
    set_key(&paths, "MODEL_PROFILE", "alpha").unwrap();
    assert!(
        fs::symlink_metadata(paths.config_file())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(&real).unwrap(),
        "POD_NAME=p\nMODEL_PROFILE=alpha\n"
    );
}

#[test]
fn a_symlink_loop_is_an_error_not_a_hang() {
    let scratch = ScratchDir::new("profiles").unwrap();
    let paths = Paths::under(scratch.path());
    fs::create_dir_all(paths.config_dir()).unwrap();
    let a = paths.config_dir().join("a");
    std::os::unix::fs::symlink("config", &a).unwrap();
    std::os::unix::fs::symlink("a", paths.config_file()).unwrap();
    let err = set_key(&paths, "MODEL_PROFILE", "alpha").unwrap_err();
    assert!(err.to_string().contains("config"), "{err}");
}

#[test]
fn list_profiles_takes_env_files_only_sorted() {
    let scratch = ScratchDir::new("profiles").unwrap();
    let paths = Paths::under(scratch.path());
    assert_eq!(list_profiles(&paths).unwrap(), Vec::<String>::new());
    let dir = paths.profiles_dir();
    fs::create_dir_all(&dir).unwrap();
    for name in ["b.env", "a.b.env", "README", "c.env.bak", "Z.env"] {
        fs::write(dir.join(name), "").unwrap();
    }
    assert_eq!(list_profiles(&paths).unwrap(), ["Z", "a.b", "b"]);
}

#[test]
fn write_profile_writes_the_mapping_in_order() {
    let scratch = ScratchDir::new("profiles").unwrap();
    let paths = Paths::under(scratch.path());
    let path = write_profile(
        &paths,
        "new",
        [("POD_NAME", "p"), ("NETWORK_VOLUME_ID", "")],
    )
    .unwrap();
    assert_eq!(path, paths.profiles_dir().join("new.env"));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "POD_NAME=p\nNETWORK_VOLUME_ID=\n"
    );
    let read = read_profile(&paths, "new").unwrap();
    assert_eq!(
        read.keys().collect::<Vec<_>>(),
        ["POD_NAME", "NETWORK_VOLUME_ID"]
    );
}

#[test]
fn write_kv_sets_what_load_reads() {
    let scratch = ScratchDir::new("profiles").unwrap();
    let path = scratch.path().join("config");
    for (before, after) in [
        ("MODEL_PROFILE=a\nMODEL_PROFILE=b\n", "MODEL_PROFILE=new\n"),
        (
            "#MODEL_PROFILE=old\nMODEL_PROFILE=b\n",
            "#MODEL_PROFILE=old\nMODEL_PROFILE=new\n",
        ),
        (
            "MODEL_PROFILE = b\nPOD_NAME=p\n",
            "MODEL_PROFILE=new\nPOD_NAME=p\n",
        ),
        // with nothing setting the key, a commented line still comes back
        (
            "POD_NAME=p\n#MODEL_PROFILE=old\n",
            "POD_NAME=p\nMODEL_PROFILE=new\n",
        ),
    ] {
        fs::write(&path, before).unwrap();
        write_kv(&path, "MODEL_PROFILE", "new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), after, "{before:?}");
        assert_eq!(parse_env_file(&path).unwrap()["MODEL_PROFILE"], "new");
    }
}

#[test]
fn persist_writes_to_the_profile_load_layered() {
    let scratch = ScratchDir::new("profiles").unwrap();
    let paths = Paths::under(scratch.path());
    fs::create_dir_all(paths.profiles_dir()).unwrap();
    fs::write(paths.config_file(), "MODEL_PROFILE=a\n").unwrap();
    for name in ["a", "b"] {
        fs::write(profile_path(&paths, name), format!("POD_NAME=pod-{name}\n")).unwrap();
    }
    let project = scratch.path().join("project-config");
    fs::write(&project, "MODEL_PROFILE=b\n").unwrap();
    let layers = Layers {
        global: paths.config_file(),
        profiles_dir: paths.profiles_dir(),
        project: vec![project],
    };
    let env = |profile: Option<&'static str>| {
        move |key: &str| {
            profile
                .filter(|_| key == "MODEL_PROFILE")
                .map(str::to_string)
        }
    };
    // a project does not pick the profile
    let mut cfg = layers.load(env(None)).unwrap();
    assert_eq!(cfg.get("MODEL_PROFILE"), Some("a"));
    assert_eq!(cfg.get("POD_NAME"), Some("pod-a"));
    persist(&mut cfg, &paths, "NETWORK_VOLUME_ID", "vol-a").unwrap();
    let cfg = layers.load(env(None)).unwrap();
    assert_eq!(cfg.get("NETWORK_VOLUME_ID"), Some("vol-a"));
    // the environment does
    let mut cfg = layers.load(env(Some("b"))).unwrap();
    assert_eq!(cfg.get("MODEL_PROFILE"), Some("b"));
    assert_eq!(cfg.get("POD_NAME"), Some("pod-b"));
    persist(&mut cfg, &paths, "NETWORK_VOLUME_ID", "vol-b").unwrap();
    let cfg = layers.load(env(Some("b"))).unwrap();
    assert_eq!(cfg.get("NETWORK_VOLUME_ID"), Some("vol-b"));
    assert_eq!(
        fs::read_to_string(profile_path(&paths, "a")).unwrap(),
        "POD_NAME=pod-a\nNETWORK_VOLUME_ID=vol-a\n"
    );
}
