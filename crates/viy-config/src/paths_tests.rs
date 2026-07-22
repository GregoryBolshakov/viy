use std::path::Path;

use pretty_assertions::assert_eq;

use super::*;

#[test]
fn defaults_sit_under_home() {
    let p = Paths::under("/h");
    assert_eq!(p.home, Path::new("/h"));
    assert_eq!(p.config_home, Path::new("/h/.config"));
    assert_eq!(p.data_home, Path::new("/h/.local/share"));
    assert_eq!(p.state_home, Path::new("/h/.local/state"));
}

#[test]
fn a_set_variable_wins_and_an_empty_one_is_unset() {
    let p = Paths::from_vars(PathBuf::from("/h"), |name| match name {
        "XDG_STATE_HOME" => Some("/s".into()),
        "XDG_DATA_HOME" => Some("".into()),
        _ => None,
    });
    assert_eq!(p.state_home, Path::new("/s"));
    assert_eq!(p.data_home, Path::new("/h/.local/share"));
    assert_eq!(p.config_home, Path::new("/h/.config"));
}

#[test]
fn under_cargo_the_environment_is_the_test_home() {
    // .cargo/config.toml points HOME and XDG_* into target/test-home for
    // every process cargo starts; viy-testkit's guard test checks the rest.
    let p = Paths::from_env().unwrap();
    for dir in [&p.home, &p.config_home, &p.data_home, &p.state_home] {
        assert!(
            dir.starts_with(&p.home) && p.home.ends_with("target/test-home"),
            "{dir:?}"
        );
    }
}
