use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// rpctl's fake `tok`: it fails when the PIN reaches it, lists an open and
/// a PIN-gated token, and gives only the open one's value.
const FAKE: &str = r#"#!/bin/sh
[ -n "$TOK_PIN" ] && { echo "pin leaked" >&2; exit 9; }
case "$1" in
list) printf '%s\n' "tavily-dev                       $TAVILY_DEV                     open (no PIN to read)" \
                      "npm-token                        $NPM_TOKEN" ;;
get) [ "$2" = tavily-dev ] && echo tvly-from-vault-01234 || exit 5 ;;
esac
"#;

/// The fake on PATH, and TOK_PIN set in the environment it is given.
fn fake_tok(script: &str) -> (ScratchDir, Tok) {
    let dir = ScratchDir::new("vault").unwrap();
    let exe = dir.path().join("tok");
    fs::write(&exe, script).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    // A sibling test can fork while the script is open for writing, and the
    // child keeps the descriptor until it execs: until then exec fails with
    // ETXTBSY. Retry until it runs, as Codex's test_app_server.rs does.
    for _ in 0..100 {
        let run = std::process::Command::new(&exe)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !run.is_err_and(|e| e.kind() == std::io::ErrorKind::ExecutableFileBusy) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut path = dir.path().as_os_str().to_owned();
    path.push(":/usr/bin:/bin");
    let tok = Tok::with_env([
        (OsString::from("PATH"), path),
        (OsString::from("TOK_PIN"), OsString::from("should-not-pass")),
    ]);
    (dir, tok)
}

#[test]
fn open_token_is_read_without_the_pin() {
    let (_dir, tok) = fake_tok(FAKE);
    assert_eq!(
        tok.open_token("tavily-dev"),
        Ok("tvly-from-vault-01234".to_string())
    );
}

#[test]
fn gated_and_missing_tokens_are_not_requested() {
    let (_dir, tok) = fake_tok(FAKE);
    let why = tok.open_token("npm-token").unwrap_err();
    assert!(why.contains("tok open npm-token"), "{why}");
    assert_eq!(
        tok.open_token("nope"),
        Err("the vault has no token named nope".to_string())
    );
}

/// Not in rpctl's tests: `get` runs for an open token only, which is what
/// keeps DENIED lines out of tok's log, and a name is matched as the first
/// word of a line, not anywhere in it.
#[test]
fn only_an_open_tokens_name_reaches_get() {
    let script = r#"#!/bin/sh
case "$1" in
list) printf '%s\n' "open-one  OPEN_ONE  open (no PIN to read)" "gated  GATED" ;;
get) echo "$2" >> "$(dirname "$0")/got"; echo value ;;
esac
"#;
    let (dir, tok) = fake_tok(script);
    let got = || fs::read_to_string(dir.path().join("got")).unwrap_or_default();
    assert_eq!(
        tok.open_token("gated"),
        Err("gated is PIN-gated in the vault; make it readable without the PIN with: tok open gated --pin=<PIN>".to_string())
    );
    assert_eq!(
        tok.open_token("GATED"),
        Err("the vault has no token named GATED".to_string())
    );
    assert_eq!(got(), "");
    assert_eq!(tok.open_token("open-one"), Ok("value".to_string()));
    assert_eq!(got(), "open-one\n");
}

#[test]
fn tok_missing_failing_or_empty() {
    let tok = Tok::with_env([(OsString::from("PATH"), OsString::from("/nonexistent"))]);
    assert_eq!(tok.open_token("a"), Err("tok is not installed".to_string()));
    let (_dir, tok) = fake_tok("#!/bin/sh\necho 'vault locked' >&2\necho second >&2\nexit 3\n");
    assert_eq!(tok.open_token("a"), Err("tok: vault locked".to_string()));
    let (_dir, tok) = fake_tok("#!/bin/sh\nexit 4\n");
    assert_eq!(tok.open_token("a"), Err("tok: exit 4".to_string()));
    let (_dir, tok) =
        fake_tok("#!/bin/sh\n[ \"$1\" = list ] && echo 'blank   open (no PIN to read)'\nexit 0\n");
    assert_eq!(
        tok.open_token("blank"),
        Err("blank is empty in the vault".to_string())
    );
}

/// Not in rpctl's tests: an empty PATH finds no tok, not one in the
/// working directory, as `shutil.which` (the review of WP 1.4).
#[test]
fn an_empty_path_finds_no_tok() {
    let tok = Tok::with_env([(OsString::from("PATH"), OsString::new())]);
    assert_eq!(tok.open_token("a"), Err("tok is not installed".to_string()));
}

/// Not in rpctl's tests: tok that exits while a process it started keeps
/// its output open still fails at the timeout instead of hanging, as
/// Python's `communicate(timeout=)` does.
#[test]
fn output_held_open_after_exit_times_out() {
    let (_dir, mut tok) = fake_tok("#!/bin/sh\n(sleep 30) &\nexit 0\n");
    tok.timeout = Duration::from_secs(1);
    let started = Instant::now();
    assert_eq!(
        tok.open_token("a"),
        Err("tok failed: timed out after 1 seconds".to_string())
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}
