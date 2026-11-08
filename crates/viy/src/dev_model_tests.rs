use std::ffi::OsString;

use viy_testkit::scratch::ScratchDir;

use super::*;

fn openai(url: Option<&str>) -> Config {
    let mut cfg = Config::defaults();
    cfg.set("BACKEND", "openai");
    cfg.set("ENDPOINT_API_KEY", "the-key");
    cfg.set("ENDPOINT_MODEL", "the-model");
    if let Some(url) = url {
        cfg.set("ENDPOINT_URL", url);
    }
    cfg
}

fn sh(script: &str) -> Vec<OsString> {
    vec!["sh".into(), "-c".into(), script.into()]
}

#[test]
fn with_model_hands_the_server_to_the_command() {
    let scratch = ScratchDir::new("with-model").unwrap();
    let paths = Paths::from_vars(scratch.path().join("home"), |_| None);
    let mut out = Vec::new();
    let check = "test \"$VLLM_BASE_URL|$VLLM_API_KEY|$SERVED_MODEL_NAME\" = \
                 'http://127.0.0.1:9|the-key|the-model'";
    let cfg = openai(Some("http://127.0.0.1:9/v1/"));
    with_model(&sh(check), &paths, &cfg, &mut out).unwrap();
    assert!(out.is_empty(), "nothing printed: {out:?}");
    assert!(paths.activity_file().exists());

    let failed = with_model(&sh("exit 3"), &paths, &cfg, &mut out).unwrap_err();
    assert_eq!((failed.code, failed.message()), (3, None));
}

#[test]
fn with_model_needs_a_server() {
    let scratch = ScratchDir::new("with-model").unwrap();
    let paths = Paths::from_vars(scratch.path().join("home"), |_| None);
    let failed = with_model(&sh("true"), &paths, &openai(None), &mut Vec::new()).unwrap_err();
    assert_eq!(
        failed.message(),
        Some("ENDPOINT_URL is not set (BACKEND=openai).")
    );
}
