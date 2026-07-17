use super::*;

fn run_capture(args: &[&str]) -> (i32, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run(args.iter().map(OsString::from), &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

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
fn anything_else_is_a_usage_error() {
    for args in [&[][..], &["chat"], &["--version", "extra"]] {
        let (code, out, err) = run_capture(args);
        assert_eq!(code, EXIT_USAGE);
        assert_eq!(out, "");
        assert!(err.starts_with("usage: viy"), "{err:?}");
    }
}
