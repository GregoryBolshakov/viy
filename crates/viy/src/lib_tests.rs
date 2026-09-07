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
    for args in [&[][..], &["bogus"], &["model"], &["down", "now"]] {
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

#[test]
fn commands_still_to_come_say_so() {
    for (args, phase) in [
        (&["volume", "list"][..], 2),
        (&["chat", "-c"], 3),
        (&["trace"], 7),
    ] {
        let (code, out, err) = run_capture(args);
        assert_eq!(code, 1, "{args:?}");
        assert_eq!(out, "");
        assert_eq!(
            err,
            format!("viy {}: not ported yet (ROADMAP phase {phase})\n", args[0])
        );
    }
}
