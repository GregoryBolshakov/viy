//! Read-only calls against the live RunPod API through `viy_pod::api` and
//! the real reqwest client: what RunPod answers viy's requests (TLS, the
//! User-Agent, the response shapes), with nothing on any pod changed. Its
//! twin `tools/live_api.py` makes the same calls through rpctl's api.py;
//! `tools/live_api.sh` runs both and compares them.
//!
//!     tok run --pin=<PIN> -- cargo run -p viy-pod --example live_api
//!
//! Reads the key from `RUNPOD_READ_TOKEN` (the vault's read-only key, so a
//! mistake here cannot stop or delete anything) and prints one JSON line
//! per call: never a key, a pod's env, or a log line's text, which may hold
//! a request's prompt.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;
use viy_http::ReqwestHttp;
use viy_pod::api::RunPod;
use viy_pod::api::RunPodError;

/// No pod has this id.
const MISSING_POD: &str = "zzzzzzzzzzzzzz";
const GPU: &str = "NVIDIA A100-SXM4-80GB";

fn say(call: &str, started: Instant, outcome: Value) {
    let ms = started.elapsed().as_millis();
    println!("{}", json!({"call": call, "ms": ms, "outcome": outcome}));
}

fn failed(error: &RunPodError) -> Value {
    json!({
        "error": error.to_string().lines().next(),
        "status_code": error.status_code(),
        "transient": error.transient(),
    })
}

/// The fields of a pod or volume that say which one it is, nothing else.
fn identity(row: &Value) -> Value {
    json!({
        "id": row.get("id"),
        "name": row.get("name"),
        "desiredStatus": row.get("desiredStatus"),
        "dataCenterId": row.get("dataCenterId"),
        "keys": row.as_object().map(serde_json::Map::len),
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let Some(key) = std::env::var_os("RUNPOD_READ_TOKEN") else {
        eprintln!("RUNPOD_READ_TOKEN is not set: run under `tok run`");
        std::process::exit(2);
    };
    let http = match ReqwestHttp::new() {
        Ok(http) => http,
        Err(error) => {
            eprintln!("no HTTP client: {error}");
            std::process::exit(1);
        }
    };
    let rp = RunPod::new(key.to_string_lossy(), Arc::new(http));

    let t = Instant::now();
    let pods = rp.list_pods().await;
    let running = pods.as_ref().ok().and_then(|pods| {
        pods.iter()
            .find(|p| p["desiredStatus"] == "RUNNING")
            .and_then(|p| p["id"].as_str())
            .map(str::to_string)
    });
    say(
        "list_pods",
        t,
        match &pods {
            Ok(pods) => json!(pods.iter().map(identity).collect::<Vec<_>>()),
            Err(e) => failed(e),
        },
    );

    let t = Instant::now();
    let volumes = rp.list_volumes().await;
    say(
        "list_volumes",
        t,
        match &volumes {
            Ok(volumes) => json!(volumes.iter().map(identity).collect::<Vec<_>>()),
            Err(e) => failed(e),
        },
    );

    if let Some(pod_id) = &running {
        let t = Instant::now();
        let pod = rp.get_pod(pod_id).await;
        say(
            "get_pod",
            t,
            match &pod {
                Ok(pod) => json!(pod.as_ref().map(identity)),
                Err(e) => failed(e),
            },
        );
    }

    let t = Instant::now();
    let missing = rp.get_pod(MISSING_POD).await;
    say(
        "get_pod_missing",
        t,
        match &missing {
            Ok(pod) => json!({"found": pod.is_some()}),
            Err(e) => failed(e),
        },
    );

    let t = Instant::now();
    let centers = rp.data_centers().await;
    let first_center = centers.as_ref().ok().and_then(|dcs| {
        dcs.iter()
            .filter_map(|dc| dc["id"].as_str())
            .min()
            .map(str::to_string)
    });
    say(
        "data_centers",
        t,
        match &centers {
            Ok(dcs) => json!({
                "count": dcs.len(),
                "with_gpu_availability": dcs.iter().filter(|dc| dc["gpuAvailability"].is_array()).count(),
                "keys": dcs.first().and_then(Value::as_object).map(|o| o.keys().cloned().collect::<Vec<_>>()),
            }),
            Err(e) => failed(e),
        },
    );

    if let Some(center) = first_center {
        let t = Instant::now();
        let stock = rp
            .cuda_stock(
                std::slice::from_ref(&center),
                &[GPU.to_string()],
                "12.8",
                &["12.4".to_string()],
                true,
            )
            .await;
        say(
            "cuda_stock",
            t,
            match &stock {
                Ok(stock) => json!(
                    stock
                        .iter()
                        .map(|((dc, gpu), s)| json!([dc, gpu, s.fit.is_some(), s.old.is_some()]))
                        .collect::<Vec<_>>()
                ),
                Err(e) => failed(e),
            },
        );
    }

    if let Some(pod_id) = &running {
        let t = Instant::now();
        let logs = rp.pod_logs(pod_id, 20, Duration::from_secs(8)).await;
        say(
            "pod_logs",
            t,
            match &logs {
                Ok(entries) => {
                    let mut sources: Vec<&str> =
                        entries.iter().filter_map(|e| e.source.as_deref()).collect();
                    sources.sort_unstable();
                    sources.dedup();
                    json!({
                        "entries": entries.len(),
                        "with_ts": entries.iter().filter(|e| e.ts.is_some()).count(),
                        "sources": sources,
                    })
                }
                Err(e) => failed(e),
            },
        );
    }
}
