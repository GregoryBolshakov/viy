//! RunPod's APIs: REST (`rest.runpod.io/v1`) for pods and network volumes,
//! GraphQL for the GPU stock, and the v2 platform log stream, readable
//! before ssh is. rpctl's `api.py`; what is sent and what comes back are
//! pinned by the pod/api fixture.
//!
//! The safety invariant (rpctl docs/architecture.md 4): never confuse "API
//! unreachable" with "pod gone". Every failure is a [`RunPodError`]:
//!
//! | What happened | `status_code` | `transient()` | How callers read it |
//! |---|---|---|---|
//! | No answer: refused, reset, DNS, timeout | `None` | yes | unknown: keep what we had |
//! | HTTP 429, 500, 502, 503, 504 | the status | yes | unknown: keep what we had |
//! | HTTP 404 | 404 | no | gone; [`RunPod::get_pod`] returns `Ok(None)` |
//! | Any other HTTP status from 400 | the status | no | refused (key, quota, request) |
//! | GraphQL answer with `errors` or without `data` | the status, often 200 | by the status, as above | refused |
//! | Log stream answers other than 200 | the status | by the status, as above | no platform log |
//!
//! The text is rpctl's, `"<METHOD> <path> -> HTTP <status>\n<detail>"`
//! (the detail is the JSON body pretty-printed, or the first 2000
//! characters of the text), except where `requests` named an exception
//! class: viy names its own error kind (flags.md F22), as in
//! `"GET /pods -> connection failed: ..."`.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use tokio::time::Instant;
use viy_http::HttpClient;
use viy_http::HttpError;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;

pub const BASE: &str = "https://rest.runpod.io/v1";
pub const GRAPHQL: &str = "https://api.runpod.io/graphql";
const LOGS: &str = "https://api.runpod.io/v2/pods";
/// RunPod's v2 REST API: the one whose volume creation takes a storage tier.
const V2: &str = "https://api.runpod.io/v2";

/// How long [`RunPod::pod_logs`] reads when its caller has no budget of its
/// own: the stream never closes.
pub const LOG_BUDGET: Duration = Duration::from_secs(8);
const LOG_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A quiet gap this long ends the log stream.
const LOG_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// The deadline of a budget too long to add to a clock reading.
const FAR_FUTURE: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

/// How much of a non-JSON error body the message keeps.
const DETAIL_CHARS: usize = 2000;

/// A failed RunPod call; see the module docs for the kinds.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct RunPodError {
    message: String,
    status_code: Option<u16>,
}

impl RunPodError {
    pub fn new(message: impl Into<String>, status_code: Option<u16>) -> Self {
        Self {
            message: message.into(),
            status_code,
        }
    }

    /// The HTTP status when RunPod answered, `None` when it did not.
    pub fn status_code(&self) -> Option<u16> {
        self.status_code
    }

    /// The answer may differ if asked again: no answer, or 429 or a 5xx
    /// from 500 to 504.
    pub fn transient(&self) -> bool {
        matches!(self.status_code, None | Some(429 | 500 | 502 | 503 | 504))
    }
}

/// One line of the platform log. Entries without a string `line` are
/// skipped; a `ts` or `source` that is not a string counts as absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogEntry {
    pub ts: Option<String>,
    pub source: Option<String>,
    pub line: String,
}

impl LogEntry {
    /// When the line was written. System lines carry whole seconds
    /// ("...:52Z"), container lines nanoseconds ("...:52.771976788Z"):
    /// compared as text, "52Z" would come after "52.7Z".
    pub fn time(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let ts = self.ts.as_deref()?;
        chrono::DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|t| t.to_utc())
    }

    /// The order rpctl sorts by (`api.log_time(entry) or entry["ts"]`): the
    /// time to the nanosecond as text, else the timestamp as written.
    fn sort_key(&self) -> String {
        match self.time() {
            Some(time) => time.format("%Y-%m-%dT%H:%M:%S%.9f").to_string(),
            None => self.ts.clone().unwrap_or_default(),
        }
    }
}

/// The stock RunPod reports for one GPU in one data center: on a host whose
/// driver supports at least the minimum CUDA (`fit`), and on one whose
/// driver is exactly one of the older versions asked about (`old`). `None`
/// when none is free, or when no older versions were asked about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CudaStock {
    pub fit: Option<String>,
    pub old: Option<String>,
}

/// The client. Cheap to clone; every clone shares the HTTP connections.
#[derive(Clone)]
pub struct RunPod {
    http: Arc<dyn HttpClient>,
    api_key: String,
    user_agent: &'static str,
}

/// The key stays out.
impl fmt::Debug for RunPod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunPod").finish_non_exhaustive()
    }
}

impl RunPod {
    pub fn new(api_key: impl Into<String>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            http,
            api_key: api_key.into(),
            user_agent: user_agent::PROVIDER_API,
        }
    }

    /// As [`RunPod::new`], with CLIENT_TAG off sending rp's User-Agent.
    pub fn configured(
        api_key: impl Into<String>,
        http: Arc<dyn HttpClient>,
        cfg: &viy_config::Config,
    ) -> Self {
        let mut this = Self::new(api_key, http);
        if !viy_config::config::client_tag(cfg) {
            this.user_agent = user_agent::RP_REQUESTS;
        }
        this
    }

    /// Every call's headers, rp's `requests.Session` ones in its order,
    /// without Accept-Encoding and Connection (kb d046), the User-Agent
    /// the provider marker unless CLIENT_TAG is off (kb d137); `requests`'
    /// default timeouts, [`viy_http::DEFAULT_TIMEOUT`].
    fn request(&self, method: Method, url: String) -> Request {
        Request::new(method, url)
            .header("User-Agent", self.user_agent)
            .header("Accept", "*/*")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
    }

    /// One REST call: the JSON it answers, `None` for an empty or non-JSON
    /// body (or JSON `null`).
    async fn rest(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Option<Value>, RunPodError> {
        self.rest_at(BASE, method, path, body).await
    }

    /// [`RunPod::rest`] against `base`.
    async fn rest_at(
        &self,
        base: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Option<Value>, RunPodError> {
        let mut request = self.request(method.clone(), format!("{base}{path}"));
        if let Some(body) = body {
            request = request.body(body.to_string());
        }
        let request = with_content_length(request);
        let no_answer =
            |error: HttpError| RunPodError::new(format!("{method} {path} -> {error}"), None);
        let (status, bytes) = self.fetch(request).await.map_err(no_answer)?;
        if status >= 400 {
            let detail = match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_default(),
                Err(_) => text(&bytes),
            };
            return Err(RunPodError::new(
                format!("{method} {path} -> HTTP {status}\n{detail}"),
                Some(status),
            ));
        }
        Ok(serde_json::from_slice(&bytes)
            .ok()
            .filter(|value: &Value| !value.is_null()))
    }

    async fn fetch(&self, request: Request) -> Result<(u16, Vec<u8>), HttpError> {
        let response = viy_http::send(&*self.http, request).await?;
        let status = response.status;
        Ok((status, response.bytes().await?))
    }

    /// One GraphQL query: its `data`, an empty map when that is `null`.
    async fn graphql(&self, query: &str) -> Result<Map<String, Value>, RunPodError> {
        let request = with_content_length(
            self.request(Method::POST, GRAPHQL.to_string())
                .body(json!({ "query": query }).to_string()),
        );
        let (status, bytes) = self
            .fetch(request)
            .await
            .map_err(|error| RunPodError::new(format!("GraphQL -> {error}"), None))?;
        let mut body = match serde_json::from_slice(&bytes) {
            Ok(Value::Object(body)) => body,
            _ => Map::new(),
        };
        let errors = body.get("errors").filter(|errors| truthy(errors));
        if status >= 400 || errors.is_some() || !body.contains_key("data") {
            let detail = errors.map_or_else(|| text(&bytes), Value::to_string);
            return Err(RunPodError::new(
                format!("GraphQL -> HTTP {status}\n{detail}"),
                Some(status),
            ));
        }
        Ok(match body.remove("data") {
            Some(Value::Object(data)) => data,
            _ => Map::new(),
        })
    }

    // ---- stock ----------------------------------------------------------

    /// Every data center with the GPU stock RunPod reports right now. Only
    /// the GraphQL API has it, and `available` flickers: sample it.
    pub async fn data_centers(&self) -> Result<Vec<Value>, RunPodError> {
        let mut data = self
            .graphql(
                "query { dataCenters { id location listed storageSupport \
                 gpuAvailability { gpuTypeId stockStatus available } } }",
            )
            .await?;
        Ok(rows(data.remove("dataCenters")))
    }

    /// The stock of each GPU type in each data center ([`CudaStock`]),
    /// keyed `(data center, GPU type)` in the order asked. One GraphQL
    /// request: `gpuAvailability` above has no CUDA filter, and REST's
    /// `allowedCudaVersions` stops at 13.0 and matches exactly.
    pub async fn cuda_stock(
        &self,
        data_centers: &[String],
        gpu_types: &[String],
        min_cuda: &str,
        older: &[String],
        secure: bool,
    ) -> Result<IndexMap<(String, String), CudaStock>, RunPodError> {
        let price = |gpu: &str, dc: &str, cond: &str| {
            format!(
                "gpuTypes(input: {{id: {}}}) {{ lowestPrice(input: {{gpuCount: 1, \
                 secureCloud: {secure}, dataCenterId: {}, {cond}}}) {{ stockStatus }} }}",
                quote(gpu),
                quote(dc),
            )
        };
        let fit = format!("minCudaVersion: {}", quote(min_cuda));
        let old = format!(
            "allowedCudaVersions: [{}]",
            older
                .iter()
                .map(|v| quote(v))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut keys = Vec::new();
        let mut parts = Vec::new();
        for (i, dc) in data_centers.iter().enumerate() {
            for (j, gpu) in gpu_types.iter().enumerate() {
                let alias = format!("s{i}_{j}");
                parts.push(format!("{alias}f: {}", price(gpu, dc, &fit)));
                if !older.is_empty() {
                    parts.push(format!("{alias}o: {}", price(gpu, dc, &old)));
                }
                keys.push((dc, gpu, alias));
            }
        }
        let data = self
            .graphql(&format!("query {{ {} }}", parts.join(" ")))
            .await?;
        let status = |alias: String| {
            data.get(&alias)
                .and_then(Value::as_array)
                .and_then(|rows| rows.first())
                .and_then(|row| row.get("lowestPrice"))
                .and_then(|price| price.get("stockStatus"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Ok(keys
            .into_iter()
            .map(|(dc, gpu, alias)| {
                let stock = CudaStock {
                    fit: status(format!("{alias}f")),
                    old: if older.is_empty() {
                        None
                    } else {
                        status(format!("{alias}o"))
                    },
                };
                ((dc.clone(), gpu.clone()), stock)
            })
            .collect())
    }

    // ---- network volumes ------------------------------------------------

    pub async fn list_volumes(&self) -> Result<Vec<Value>, RunPodError> {
        Ok(rows(self.rest(Method::GET, "/networkvolumes", None).await?))
    }

    pub async fn get_volume(&self, volume_id: &str) -> Result<Option<Value>, RunPodError> {
        self.rest(Method::GET, &format!("/networkvolumes/{volume_id}"), None)
            .await
    }

    /// A new volume. With no `tier` RunPod takes the data center's default
    /// storage tier; `STANDARD` or `HIGH_PERFORMANCE` asks for that one,
    /// through the v2 API (v1 refuses the field), whose answer gets v1's
    /// `dataCenterId`.
    pub async fn create_volume(
        &self,
        name: &str,
        size_gb: u64,
        data_center_id: &str,
        tier: Option<&str>,
    ) -> Result<Option<Value>, RunPodError> {
        let Some(tier) = tier else {
            let body = json!({ "name": name, "size": size_gb, "dataCenterId": data_center_id });
            return self.rest(Method::POST, "/networkvolumes", Some(body)).await;
        };
        let body =
            json!({ "name": name, "size": size_gb, "dataCenter": data_center_id, "type": tier });
        let mut volume = self
            .rest_at(V2, Method::POST, "/network-volumes", Some(body))
            .await?;
        if let Some(fields) = volume.as_mut().and_then(Value::as_object_mut)
            && !fields.contains_key("dataCenterId")
            && let Some(region) = fields.get("dataCenter").cloned()
        {
            fields.insert("dataCenterId".to_string(), region);
        }
        Ok(volume)
    }

    pub async fn delete_volume(&self, volume_id: &str) -> Result<(), RunPodError> {
        self.rest(
            Method::DELETE,
            &format!("/networkvolumes/{volume_id}"),
            None,
        )
        .await
        .map(drop)
    }

    // ---- pods -----------------------------------------------------------

    pub async fn list_pods(&self) -> Result<Vec<Value>, RunPodError> {
        Ok(rows(self.rest(Method::GET, "/pods", None).await?))
    }

    /// The pod, or `None` when RunPod says 404 (or answers with no body).
    pub async fn get_pod(&self, pod_id: &str) -> Result<Option<Value>, RunPodError> {
        match self
            .rest(Method::GET, &format!("/pods/{pod_id}"), None)
            .await
        {
            Err(error) if error.status_code == Some(404) => Ok(None),
            result => result,
        }
    }

    pub async fn create_pod(&self, body: Value) -> Result<Option<Value>, RunPodError> {
        self.rest(Method::POST, "/pods", Some(body)).await
    }

    pub async fn stop_pod(&self, pod_id: &str) -> Result<(), RunPodError> {
        self.rest(Method::POST, &format!("/pods/{pod_id}/stop"), None)
            .await
            .map(drop)
    }

    pub async fn start_pod(&self, pod_id: &str) -> Result<(), RunPodError> {
        self.rest(Method::POST, &format!("/pods/{pod_id}/start"), None)
            .await
            .map(drop)
    }

    pub async fn delete_pod(&self, pod_id: &str) -> Result<(), RunPodError> {
        self.rest(Method::DELETE, &format!("/pods/{pod_id}"), None)
            .await
            .map(drop)
    }

    /// The last `tail` lines of the pod's platform log, oldest first: a
    /// bounded read of the v2 SSE stream, which never closes. Only log
    /// payloads count, not keepalives. It stops after a quiet gap of 2 s,
    /// and fails if no entry had come by then; and it stops at the end of
    /// `budget`, once anything at all arrives after it (rpctl looks at the
    /// budget when a line comes), or at once if entries are in hand.
    pub async fn pod_logs(
        &self,
        pod_id: &str,
        tail: u32,
        budget: Duration,
    ) -> Result<Vec<LogEntry>, RunPodError> {
        let start = Instant::now();
        let deadline = start.checked_add(budget).unwrap_or(start + FAR_FUTURE);
        let request = self
            .request(Method::GET, format!("{LOGS}/{pod_id}/logs?tail={tail}"))
            .timeouts(LOG_CONNECT_TIMEOUT, LOG_READ_TIMEOUT);
        let unavailable = |error: HttpError| {
            RunPodError::new(format!("Platform logs unavailable ({error})"), None)
        };
        let mut response = viy_http::send(&*self.http, request)
            .await
            .map_err(unavailable)?;
        if response.status != 200 {
            let status = response.status;
            return Err(RunPodError::new(
                format!("Platform logs -> HTTP {status}"),
                Some(status),
            ));
        }
        let mut entries = Vec::new();
        let mut pending = Vec::new();
        'read: loop {
            let mut next = std::pin::pin!(response.chunk());
            let read = match tokio::time::timeout_at(deadline, &mut next).await {
                Ok(read) => read,
                // Nothing in hand: rpctl still waits for the next line or
                // the quiet gap.
                Err(_budget_spent) if entries.is_empty() => match next.await {
                    Err(error) => return Err(unavailable(error)),
                    Ok(_) => break,
                },
                Err(_budget_spent) => break,
            };
            let chunk = match read {
                Ok(Some(chunk)) => chunk,
                Ok(None) => {
                    // The last line may have no line break.
                    if Instant::now() < deadline {
                        entries.extend(log_entry(&pending));
                    }
                    break;
                }
                Err(error) if entries.is_empty() => return Err(unavailable(error)),
                Err(_) => break,
            };
            // Lines end at \n, \r or \r\n, as `bytes.splitlines` has them; the
            // empty line inside a \r\n is no `data:` line.
            let mut line_start = 0;
            let mut scan = pending.len();
            pending.extend(chunk);
            while let Some(offset) = pending[scan..]
                .iter()
                .position(|&b| b == b'\n' || b == b'\r')
            {
                let end = scan + offset;
                if Instant::now() >= deadline {
                    break 'read;
                }
                entries.extend(log_entry(&pending[line_start..end]));
                line_start = end + 1;
                scan = line_start;
            }
            pending.drain(..line_start);
        }
        entries.sort_by_cached_key(LogEntry::sort_key);
        Ok(entries)
    }
}

/// RunPod fronts every exposed http port at this hostname.
pub fn proxy_url(pod_id: &str, port: impl fmt::Display) -> String {
    format!("https://{pod_id}-{port}.proxy.runpod.net")
}

/// A list RunPod answers, bare or as `{"data": [...]}`; anything else is
/// no rows.
fn rows(value: Option<Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(rows)) => rows,
        Some(Value::Object(mut wrapper)) => match wrapper.remove("data") {
            Some(Value::Array(rows)) => rows,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// One SSE line as a log entry, if it is a `data:` line holding one.
fn log_entry(line: &[u8]) -> Option<LogEntry> {
    let data = line.strip_prefix(b"data:")?;
    let Ok(Value::Object(entry)) = serde_json::from_slice(data) else {
        return None;
    };
    let field = |key| entry.get(key).and_then(Value::as_str).map(str::to_string);
    Some(LogEntry {
        line: field("line")?,
        ts: field("ts"),
        source: field("source"),
    })
}

/// rp's `requests` declares a length on every request with a body and a
/// zero one on a bodiless request other than GET and HEAD; hyper leaves the
/// header off when there is no body.
fn with_content_length(request: Request) -> Request {
    let length = match &request.body {
        Some(body) => body.len(),
        None if request.method == Method::GET || request.method == Method::HEAD => {
            return request;
        }
        None => 0,
    };
    request.header("Content-Length", length.to_string())
}

/// A body as text, for an error message.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(DETAIL_CHARS)
        .collect()
}

/// A string as a GraphQL (and JSON) string literal.
fn quote(s: &str) -> String {
    Value::from(s).to_string()
}

/// A JSON value as Python's `str()` shows it in rpctl's messages and
/// listings: `None`, `True`, strings bare.
pub(crate) fn shown(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(other) => other.to_string(),
    }
}

/// Python's truth of a JSON value, as rp tests one: not null, false, 0,
/// or empty. GraphQL's `errors`, and pod.json's `tunnel_only` and
/// `tunnel_pid`.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
