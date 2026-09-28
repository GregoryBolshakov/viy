use super::*;

fn run_capture(args: &[&str]) -> (i32, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run(
        args.iter().map(OsString::from),
        &mut out,
        &mut err,
        Terminal::default(),
    );
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Not in rp, which has no --version.
#[test]
fn version_long_and_short() {
    for flag in ["--version", "-V"] {
        let (code, out, err) = run_capture(&[flag]);
        assert_eq!(code, 0);
        assert_eq!(out, format!("viy {}\n", env!("CARGO_PKG_VERSION")));
        assert_eq!(err, "");
    }
}

#[test]
fn usage_errors_exit_2_on_stderr() {
    for args in [&["bogus"][..], &["model"], &["down", "now"]] {
        let (code, out, err) = run_capture(args);
        assert_eq!(code, 2, "{args:?}");
        assert_eq!(out, "", "{args:?}");
        assert!(err.contains("Usage: viy"), "{args:?}: {err}");
    }
}

#[test]
fn help_exits_0_on_stdout_with_rps_descriptions() {
    let (code, out, err) = run_capture(&["--help"]);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.contains("viy - run Qwen3.8-27B-abliterated"), "{out}");
    assert!(
        out.contains("terminate the pod (weights stay on the volume)"),
        "{out}"
    );
    let (code, out, _) = run_capture(&["shutdown", "-h"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("viy shutdown 23:30        at the next 23:30\n"),
        "{out}"
    );
}

/// `viy` alone opens the chat (kb d067), as `viy chat` does.
#[test]
fn viy_alone_is_the_chat() {
    let cli = cli::parse(Vec::<OsString>::new()).unwrap();
    assert!(matches!(cli.command, Command::Chat(_)));
    let opening = chat_opening(&cli.command);
    assert!(opening.session.is_none() && opening.url.is_none());
}

/// A chat named that is not there ends viy before the chat opens, in
/// Codex's words (kb d083).
#[test]
fn resume_and_fork_of_a_chat_not_there_say_so() {
    for action in ["resume", "fork"] {
        let (code, out, err) = run_capture(&[action, "20990101-000000-0000"]);
        assert_eq!((code, out.as_str()), (1, ""), "{action}");
        assert_eq!(
            err,
            format!(
                "No saved session found with ID 20990101-000000-0000. Run `viy {action}` \
                 without an ID to choose from existing sessions.\n"
            )
        );
    }
}

#[test]
fn commands_still_to_come_say_so() {
    let (code, out, err) = run_capture(&["trace"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert_eq!(err, "viy trace: not ported yet (ROADMAP phase 7)\n");
}

/// Only what neither schedules nor runs anything: these go to this
/// machine's processes.
#[test]
fn shutdown_shows_and_refuses() {
    let (code, out, err) = run_capture(&["shutdown"]);
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(
        out,
        "no shutdown is scheduled\nschedule one with: viy shutdown 90m   (or 1h30m, 23:30)\n"
    );
    let (code, out, _) = run_capture(&["shutdown", "--status"]);
    assert_eq!((code, out.as_str()), (0, "no shutdown is scheduled\n"));
    let (code, _, err) = run_capture(&["shutdown", "soon"]);
    assert_eq!(
        (code, err.as_str()),
        (1, "not a duration: 'soon' (try 90m, 1h30m or 23:30)\n")
    );
    let (code, _, err) = run_capture(&["shutdown", "--run-at", "later"]);
    assert_eq!((code, err.as_str()), (1, "--run-at: not a time: later\n"));
}
