use std::process::Command;

use pretty_assertions::assert_eq;

use super::*;

#[test]
fn tests_run_with_a_redirected_home() {
    assert_isolated();
}

#[test]
fn child_processes_inherit_the_redirected_home() {
    let out = Command::new("sh")
        .args(["-c", r#"printf '%s\n' "$HOME" "$XDG_STATE_HOME""#])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let fake = test_home();
    assert_eq!(
        lines,
        [
            fake.to_str().unwrap(),
            fake.join(".local/state").to_str().unwrap()
        ]
    );
}

#[test]
fn the_real_home_is_found_without_home() {
    // The guard compares against it, so it must not come from HOME.
    let real = real_home().expect("this user is in /etc/passwd");
    assert_ne!(Some(real.as_os_str()), std::env::var_os("HOME").as_deref());
    assert!(!real.starts_with(test_home()));
}
