use std::os::unix::fs::PermissionsExt;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// A scratch HOME with rp's config dir laid out from `files` (paths under
/// `~/.config/rpctl/`).
fn home(files: &[(&str, &str)]) -> (ScratchDir, Paths) {
    let scratch = ScratchDir::new("migrate").unwrap();
    let paths = Paths::under(scratch.path());
    for (rel, text) in files {
        let path = paths.rpctl_config_dir().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
    }
    (scratch, paths)
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

const RP: &[(&str, &str)] = &[
    ("config", "RUNPOD_API_KEY=rp-key\nMODEL_PROFILE=beta\n"),
    ("config.bak.1787584181", "OLD=1\n"),
    ("profiles/beta.env", "MODEL_REPO=org/beta\n"),
    ("profiles/alpha.env", "MODEL_REPO=org/alpha\n"),
    ("profiles/notes.txt", "not a profile\n"),
];

#[test]
fn copies_config_and_profiles_owner_only_and_leaves_rps() {
    let (_home, paths) = home(RP);
    let report = migrate(&paths).unwrap();
    let rp = paths.rpctl_config_dir();
    assert_eq!(
        report.copied,
        [
            (rp.join("config"), paths.config_file()),
            (
                rp.join("profiles/alpha.env"),
                paths.profiles_dir().join("alpha.env")
            ),
            (
                rp.join("profiles/beta.env"),
                paths.profiles_dir().join("beta.env")
            ),
        ]
    );
    assert_eq!(report.kept, []);
    for (src, dst) in &report.copied {
        assert_eq!(read(dst), read(src));
        let mode = fs::metadata(dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", dst.display());
    }
    // rp's own files stay as they were, and nothing else was copied.
    for (rel, text) in RP {
        assert_eq!(read(&rp.join(rel)), *text);
        let mode = fs::metadata(rp.join(rel)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o664);
    }
    let mut viy: Vec<_> = fs::read_dir(paths.config_dir())
        .unwrap()
        .chain(fs::read_dir(paths.profiles_dir()).unwrap())
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    viy.sort();
    assert_eq!(viy, ["alpha.env", "beta.env", "config", "profiles"]);
}

#[test]
fn what_viy_has_is_kept_and_a_second_run_copies_nothing() {
    let (_home, paths) = home(RP);
    fs::create_dir_all(paths.profiles_dir()).unwrap();
    fs::write(
        paths.profiles_dir().join("beta.env"),
        "MODEL_REPO=viy-edit\n",
    )
    .unwrap();
    let first = migrate(&paths).unwrap();
    assert_eq!(first.copied.len(), 2);
    assert_eq!(
        first.kept,
        [(
            paths.rpctl_config_dir().join("profiles/beta.env"),
            paths.profiles_dir().join("beta.env")
        )]
    );
    assert_eq!(
        read(&paths.profiles_dir().join("beta.env")),
        "MODEL_REPO=viy-edit\n"
    );
    fs::write(paths.config_file(), "RUNPOD_API_KEY=viy-key\n").unwrap();
    let second = migrate(&paths).unwrap();
    assert_eq!(second.copied, []);
    assert_eq!(second.kept.len(), 3);
    assert_eq!(read(&paths.config_file()), "RUNPOD_API_KEY=viy-key\n");
}

#[test]
fn nothing_to_copy_without_rps_config() {
    let (_home, paths) = home(&[]);
    assert_eq!(migrate(&paths).unwrap(), Report::default());
    assert!(!paths.config_dir().exists());
}

/// A dangling symlink where viy's file goes counts as there: the copy
/// neither follows it nor replaces it.
#[test]
fn a_symlink_in_the_way_is_neither_followed_nor_replaced() {
    let (home_dir, paths) = home(&[("config", "RUNPOD_API_KEY=rp-key\n")]);
    fs::create_dir_all(paths.config_dir()).unwrap();
    let elsewhere = home_dir.path().join("elsewhere");
    std::os::unix::fs::symlink(&elsewhere, paths.config_file()).unwrap();
    let report = migrate(&paths).unwrap();
    assert_eq!(report.copied, []);
    assert_eq!(report.kept.len(), 1);
    assert!(!elsewhere.exists());
    assert!(
        fs::symlink_metadata(paths.config_file())
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn no_temporary_file_is_left_behind() {
    let (_home, paths) = home(RP);
    migrate(&paths).unwrap();
    for dir in [paths.config_dir(), paths.profiles_dir()] {
        for entry in fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            assert!(!name.contains(".migrate."), "{name}");
        }
    }
}
