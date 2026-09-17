//! rpctl's BackendTests (the registry, the openai backend, the host
//! override), and `viy status` with the openai backend against a scripted
//! endpoint.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_http::HttpClient;
use viy_testkit::exec::FakeExec;
use viy_testkit::http::Reply;
use viy_testkit::http::RouteHttp;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::time::FakeTime;

use super::*;
use crate::boot::build_vllm_args;
use crate::notice::Report;

fn cfg_with(pairs: &[(&str, &str)]) -> Config {
    let mut cfg = Config::defaults();
    for (k, v) in pairs {
        cfg.set(k, v);
    }
    cfg
}

#[test]
fn registry() {
    for name in ["runpod", "openai", "local"] {
        let cfg = cfg_with(&[("BACKEND", name)]);
        assert_eq!(Backend::resolve(&cfg).unwrap().name(), name);
    }
    assert_eq!(
        Backend::resolve(&cfg_with(&[("BACKEND", "nope")])),
        Err("unknown BACKEND 'nope'; supported: local, openai, runpod".into())
    );
}

#[test]
fn openai_backend() {
    let b = Backend::Openai;
    let mut cfg = cfg_with(&[
        ("ENDPOINT_URL", "https://api.example.com/v1/"),
        ("ENDPOINT_API_KEY", "ek"),
        ("ENDPOINT_MODEL", "vendor/model"),
    ]);
    assert_eq!(
        endpoint_url(&cfg).as_deref(),
        Some("https://api.example.com")
    );
    assert_eq!(b.api_key(&cfg), "ek");
    assert_eq!(b.model_name(&cfg), "vendor/model");
    cfg.set("ENDPOINT_MODEL", "");
    assert_eq!(b.model_name(&cfg), cfg.get("SERVED_MODEL_NAME").unwrap());
    cfg.set("ENDPOINT_URL", "");
    assert_eq!(endpoint_url(&cfg), None);
    assert_eq!(
        not_supported(b, "down").to_string(),
        "`viy down` is not available for the 'openai' backend."
    );
    assert!(!b.manages_server() && !b.is_vllm() && b.exit_hint().is_none());
    assert_eq!(
        Backend::Runpod.api_key(&cfg_with(&[("VLLM_API_KEY", "vk")])),
        "vk"
    );
}

#[test]
fn plain_http_is_flagged_unless_it_is_this_machine() {
    for (url, loopback) in [
        ("http://127.0.0.1:8000", true),
        ("http://127.5.0.1/v1", true),
        ("http://LOCALHOST:8000", true),
        ("http://[::1]:8000/v1", true),
        ("http://user:pw@localhost/v1", true),
        ("http://10.0.0.5:8000", false),
        ("http://localhost.example.com", false),
        ("http://[::2]:8000", false),
    ] {
        assert_eq!(is_loopback(url), loopback, "{url}");
    }
}

#[test]
fn vllm_args_host_override() {
    let cfg = cfg_with(&[("TUNNEL", "0")]);
    let host = |args: Vec<String>| {
        let i = args.iter().position(|a| a == "--host").unwrap();
        args[i + 1].clone()
    };
    assert_eq!(host(build_vllm_args(&cfg, "k", None)), "0.0.0.0");
    assert_eq!(
        host(build_vllm_args(&cfg, "k", Some("127.0.0.1"))),
        "127.0.0.1"
    );
}

#[test]
fn hints_name_the_key_to_set() {
    let paths = Paths::under("/h");
    assert_eq!(
        Backend::Openai.missing_key_hint(&paths),
        "ENDPOINT_API_KEY is not set. Add it to /h/.config/viy/config (or the active model \
         profile); for an endpoint without auth, any placeholder value works."
    );
    assert_eq!(
        Backend::Local.missing_key_hint(&paths),
        "VLLM_API_KEY is missing. Run viy up first."
    );
    assert_eq!(
        Backend::Runpod.exit_hint(),
        Some("Pod is still running. viy stop to pause billing, viy down to terminate.")
    );
}

// ------------------------------------------------------------ openai status

/// `viy status` with the openai backend against `endpoint`: what it said,
/// and the requests it made.
async fn status_with(
    cfg: &Config,
    endpoint: impl Fn(&viy_http::Request) -> Reply + Send + Sync + 'static,
) -> (
    Result<(), CommandError>,
    Vec<String>,
    Vec<viy_http::Request>,
) {
    let dir = ScratchDir::new("backend").unwrap();
    let paths = Paths::under(dir.path());
    let api = Arc::new(RouteHttp::new(endpoint));
    let http: Arc<dyn HttpClient> = api.clone();
    let exec = FakeExec::default();
    let time = FakeTime::at(chrono::Utc::now());
    let env = Env {
        paths: &paths,
        exec: &exec,
        http: &http,
        time: &time,
        exe: Path::new("/opt/viy/bin/viy"),
    };
    let said = Mutex::new(Vec::new());
    let report = |r: Report| {
        if let Report::Notice(n) = r {
            said.lock().unwrap().push(n.text);
        }
    };
    let result = openai_status(&env, cfg, &report).await;
    let said = said.into_inner().unwrap();
    (result, said, api.sent())
}

fn endpoint_cfg() -> Config {
    cfg_with(&[
        ("BACKEND", "openai"),
        ("ENDPOINT_URL", "https://api.example.com/v1"),
        ("ENDPOINT_API_KEY", "ek"),
        ("ENDPOINT_MODEL", "vendor/model"),
    ])
}

#[tokio::test]
async fn status_lists_the_endpoint_and_whether_it_has_the_model() {
    let models = |ids: &[&str]| {
        let data: Vec<_> = ids.iter().map(|id| json!({"id": id})).collect();
        json!({"data": data}).to_string()
    };
    let listed = models(&["other", "vendor/model"]);
    let (result, said, sent) =
        status_with(&endpoint_cfg(), move |_| Reply::answer(200, listed.clone())).await;
    result.unwrap();
    assert_eq!(
        said,
        [
            "  backend:   openai (hosted endpoint, not managed by viy)",
            "  endpoint:  https://api.example.com",
            "  model:     vendor/model",
            "  reachable: yes; model listed",
        ]
    );
    assert_eq!(sent[0].url, "https://api.example.com/v1/models");
    assert_eq!(sent[0].header_value("Authorization"), Some("Bearer ek"));

    let unlisted = models(&["other"]);
    let (_, said, _) = status_with(&endpoint_cfg(), move |_| {
        Reply::answer(200, unlisted.clone())
    })
    .await;
    assert_eq!(said[3], "  reachable: yes; model NOT in /v1/models");
    let (_, said, _) = status_with(&endpoint_cfg(), |_| Reply::answer(200, "not json")).await;
    assert_eq!(said[3], "  reachable: yes");
    let (_, said, _) = status_with(&endpoint_cfg(), |_| Reply::answer(401, "{}")).await;
    assert_eq!(said[3], "  reachable: HTTP 401");
}

#[tokio::test]
async fn status_without_a_url_says_what_to_set() {
    let cfg = cfg_with(&[("BACKEND", "openai")]);
    let (result, said, sent) = status_with(&cfg, |r| panic!("unexpected {}", r.url)).await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "ENDPOINT_URL is not set (BACKEND=openai)."
    );
    assert!(said.is_empty() && sent.is_empty());
}

#[tokio::test]
async fn preflight_and_base_url_for_the_endpoint() {
    let dir = ScratchDir::new("backend").unwrap();
    let paths = Paths::under(dir.path());
    let http: Arc<dyn HttpClient> = Arc::new(RouteHttp::new(|r| panic!("unexpected {}", r.url)));
    let exec = FakeExec::default();
    let time = FakeTime::at(chrono::Utc::now());
    let env = Env {
        paths: &paths,
        exec: &exec,
        http: &http,
        time: &time,
        exe: Path::new("/opt/viy/bin/viy"),
    };
    let mut cfg = endpoint_cfg();
    let b = Backend::Openai;
    assert!(b.preflight(&env, &cfg).await.unwrap().is_empty());
    assert_eq!(
        b.base_url(&env, &cfg, true, &|_| {})
            .await
            .unwrap()
            .as_deref(),
        Some("https://api.example.com")
    );
    cfg.set("ENDPOINT_URL", "http://10.0.0.5:8000");
    assert_eq!(
        b.preflight(&env, &cfg).await.unwrap(),
        [
            "ENDPOINT_URL is plain http: prompts, replies and the API key cross the network \
          unencrypted (http://10.0.0.5:8000)."
        ]
    );
    cfg.set("ENDPOINT_URL", "http://127.0.0.1:8000");
    assert!(b.preflight(&env, &cfg).await.unwrap().is_empty());
}
