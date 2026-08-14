use std::fs;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// `viy migrate` twice on a scratch HOME: what it says each time.
#[test]
fn says_what_it_copied_kept_and_left() {
    let scratch = ScratchDir::new("migrate").unwrap();
    let paths = Paths::under(scratch.path());
    let mut out = Vec::new();
    cmd_migrate(&paths, &mut out).unwrap();
    let rp = paths.rpctl_config_dir();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("nothing to migrate; no rp config in {}\n", rp.display())
    );

    fs::create_dir_all(rp.join("profiles")).unwrap();
    fs::write(rp.join("config"), "POD_NAME=p\n").unwrap();
    fs::write(rp.join("profiles/a.env"), "MODEL_REPO=org/a\n").unwrap();
    fs::create_dir_all(paths.profiles_dir()).unwrap();
    fs::write(paths.profiles_dir().join("a.env"), "MODEL_REPO=mine\n").unwrap();
    let mut out = Vec::new();
    cmd_migrate(&paths, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!(
            "copied {}\n    -> {}\nkept {} (already there; {} not copied)\n\
             Originals left in place: rp keeps using them until cutover.\n",
            rp.join("config").display(),
            paths.config_file().display(),
            paths.profiles_dir().join("a.env").display(),
            rp.join("profiles/a.env").display(),
        )
    );

    let mut out = Vec::new();
    cmd_migrate(&paths, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!(
            "nothing to migrate; viy already has rp's config and profiles in {}\n",
            paths.config_dir().display()
        )
    );
}
