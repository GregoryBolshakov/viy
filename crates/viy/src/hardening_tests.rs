use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;
use std::process::Stdio;

use pretty_assertions::assert_eq;

use super::*;

/// rpctl's test_tok_planted_in_the_working_directory_is_not_run, its
/// PATHs; that the planted tok is then not found is
/// viy-config's vault_tests.rs.
#[test]
fn only_absolute_path_entries_are_kept() {
    for (path, kept) in [
        (":/nonexistent", "/nonexistent"),
        ("/nonexistent::", "/nonexistent"),
        (".:/nonexistent", "/nonexistent"),
        ("", DEFAULT_PATH),
        ("bin:./x:~/bin", DEFAULT_PATH),
        (
            "/usr/local/bin:/usr/bin:/bin",
            "/usr/local/bin:/usr/bin:/bin",
        ),
        ("/a:a/b:/c/:", "/a:/c/"),
    ] {
        assert_eq!(absolute_path_entries(OsStr::new(path)), kept, "{path:?}");
    }
    let odd = OsStr::from_bytes(b"/opt/\xff:rel");
    assert_eq!(absolute_path_entries(odd), OsStr::from_bytes(b"/opt/\xff"));
}

/// Codex's two tests of the same function.
#[test]
fn env_keys_with_prefix_handles_non_utf8_entries() {
    let non_utf8_key1 = OsStr::from_bytes(b"R\xD6DBURK").to_os_string();
    let non_utf8_key2 = OsString::from_vec(vec![b'L', b'D', b'_', 0xF0]);
    let non_utf8_value = OsString::from_vec(vec![0xF0, 0x9F, 0x92, 0xA9]);
    let keys = env_keys_with_prefix(
        vec![
            (non_utf8_key1, non_utf8_value.clone()),
            (non_utf8_key2.clone(), non_utf8_value),
        ],
        b"LD_",
    );
    assert_eq!(keys, vec![non_utf8_key2]);
}

#[test]
fn env_keys_with_prefix_filters_only_matching_keys() {
    let vars = vec![
        (OsString::from("PATH"), OsString::from("/usr/bin")),
        (OsString::from("LD_TEST"), OsString::from("1")),
        (OsString::from("DYLD_FOO"), OsString::from("bar")),
    ];
    assert_eq!(
        env_keys_with_prefix(vars, b"LD_"),
        vec![OsString::from("LD_TEST")]
    );
}

const CHILD: &str = "VIY_HARDENING_TEST_CHILD";

/// The child half of `a_hardened_process_is_not_readable_or_dumpable`,
/// which starts this test binary with CHILD set; otherwise it does nothing.
/// It waits for a line, hardens itself, reports, and waits for EOF.
#[test]
fn hardened_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let mut stdin = std::io::stdin().lock();
    let mut line = String::new();
    println!("child: ready");
    stdin.read_line(&mut line).unwrap();
    pre_main();
    // SAFETY: no arguments besides the option.
    let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
    let core = std::fs::read_to_string("/proc/self/limits")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("Max core file size"))
        .unwrap()
        .split_whitespace()
        .skip(4)
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let ld: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .filter(|k| k.starts_with("LD_"))
        .collect();
    let path = std::env::var("PATH").unwrap();
    println!("child: dumpable={dumpable} core={core} ld={ld:?} path={path}");
    std::io::stdout().flush().unwrap();
    line.clear();
    stdin.read_line(&mut line).unwrap();
}

#[test]
fn a_hardened_process_is_not_readable_or_dumpable() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "hardening::tests::hardened_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env("LD_BIND_NOW", "1")
        .env("LD_VIY_TEST", "planted")
        .env("PATH", ":/usr/bin:.:bin:/bin:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map(Result::unwrap)
        // libtest prints the test's name first, on the same line.
        .filter_map(|l| l.split_once("child: ").map(|(_, said)| said.to_string()));
    let environ = format!("/proc/{}/environ", child.id());

    assert_eq!(lines.next().unwrap(), "ready");
    // Before: another process of the same user reads its environment.
    let before = std::fs::read(&environ).unwrap();
    assert!(
        before
            .split(|&b| b == 0)
            .any(|var| var == b"LD_VIY_TEST=planted")
    );

    writeln!(stdin, "go").unwrap();
    assert_eq!(
        lines.next().unwrap(),
        "dumpable=0 core=0 0 ld=[] path=/usr/bin:/bin"
    );
    // After: refused, as a ptrace attach is.
    let after = std::fs::read(&environ).unwrap_err();
    assert_eq!(after.kind(), std::io::ErrorKind::PermissionDenied);

    drop(stdin);
    assert!(child.wait().unwrap().success());
}
