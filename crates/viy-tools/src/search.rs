//! web_search's backend (rpctl search.py): Tavily, whose results carry a
//! page excerpt of several hundred characters (about 900 on average,
//! rpctl netaccess kb f009), not a one-line snippet.
//!
//! [`SearchClient`] keeps per-chat state so failures cost once, not on
//! every call (rpctl netaccess kb d002):
//!   - a rejected key or exhausted quota (401, 403, 432, 433) turns Tavily
//!     off for the chat, with one clear message ([`SearchClient::take_loud`]);
//!   - a rate limit or outage (429, 5xx, no answer) opens a short circuit
//!     breaker;
//!   - identical queries within ten minutes come from a cache.
//!
//! Queries that would carry a secret off this machine are refused
//! ([`netguard::outbound_secret`]).
//!
//! rpctl falls back to DuckDuckGo through the Python `ddgs` scraper; viy
//! has no fallback (kb d119), so a search Tavily cannot answer is an error
//! that says why.

use std::time::Duration;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;
use serde_json::json;
use tokio::time::Instant;
use viy_config::config::Config;
use viy_config::vault::Tok;
use viy_http::HttpClient;
use viy_http::HttpError;
use viy_http::Method;
use viy_http::Request;
use viy_http::user_agent;

use crate::netguard;
use crate::untrusted;
use crate::webfetch::class_name;

pub const TAVILY_URL: &str = "https://api.tavily.com/search";
/// Who answered and what its results are, as the model is told.
pub const BACKEND: &str = "tavily";
pub const QUALITY: &str = "page excerpts";
/// The `recency` values the tool takes; Tavily's `time_range` has the
/// same names.
pub const RECENCY: [&str; 4] = ["day", "week", "month", "year"];
pub const MAX_QUERY: usize = 400;

const CACHE_TTL: Duration = Duration::from_secs(600);
const CACHE_SIZE: usize = 64;
const DEFAULT_RESULTS: i64 = 5;
const DEFAULT_CHARS: usize = 1200;

/// Why a search gave no results; the model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchError(pub String);

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SearchError {}

fn err(message: impl Into<String>) -> SearchError {
    SearchError(message.into())
}

/// One result, cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub content: String,
    /// Empty when Tavily gave no date.
    pub published: String,
}

/// A search's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Results {
    /// The query as sent, its whitespace runs made single spaces.
    pub query: String,
    pub hits: Vec<Hit>,
    pub cached: bool,
}

/// Tavily's state for this chat.
#[derive(Debug, Default)]
struct Health {
    /// Why it is off for the rest of the chat.
    disabled: Option<String>,
    /// The breaker: skipped until then.
    open_until: Option<Instant>,
    fails: u32,
}

impl Health {
    fn usable(&self) -> bool {
        self.disabled.is_none() && self.open_until.is_none_or(|t| Instant::now() >= t)
    }

    fn ok(&mut self) {
        self.fails = 0;
        self.open_until = None;
    }

    fn failed(&mut self, cooldown: Duration, threshold: u32) {
        self.fails += 1;
        if self.fails >= threshold {
            self.open_until = Some(Instant::now() + cooldown);
        }
    }
}

/// The query, result count, recency and site a search was asked with.
type CacheKey = (String, usize, Option<&'static str>, Option<String>);

/// web_search for one chat.
#[derive(Debug)]
pub struct SearchClient {
    api_key: String,
    vault_token: String,
    depth: String,
    /// SEARCH_MAX_RESULTS, `None` when it is not a number.
    max_results: Option<i64>,
    max_chars: usize,
    secrets: Vec<String>,
    tok: Tok,
    /// The open vault token's value, once looked up ("" when there is none).
    vault_key: Option<String>,
    /// Why the vault gave no key.
    key_problem: Option<String>,
    tavily: Health,
    cache: IndexMap<CacheKey, (Instant, Results)>,
    told_vault: bool,
    told_tavily: bool,
}

impl SearchClient {
    /// `secrets` are the values a query must not carry out (rp's
    /// `literal_secrets`); `tok` reads the open vault token named by
    /// TAVILY_VAULT_TOKEN when TAVILY_API_KEY is empty.
    pub fn new(cfg: &Config, secrets: Vec<String>, tok: Tok) -> Self {
        let get = |key: &str| cfg.get(key).unwrap_or_default().trim().to_string();
        let depth = cfg
            .get("TAVILY_SEARCH_DEPTH")
            .unwrap_or("basic")
            .to_string();
        Self {
            api_key: get("TAVILY_API_KEY"),
            vault_token: get("TAVILY_VAULT_TOKEN"),
            depth,
            max_results: get("SEARCH_MAX_RESULTS").parse().ok(),
            max_chars: get("SEARCH_MAX_CHARS_PER_RESULT")
                .parse()
                .unwrap_or(DEFAULT_CHARS),
            secrets,
            tok,
            vault_key: None,
            key_problem: None,
            tavily: Health::default(),
            cache: IndexMap::new(),
            told_vault: false,
            told_tavily: false,
        }
    }

    /// Reads the open vault token TAVILY_VAULT_TOKEN names, when
    /// TAVILY_API_KEY is empty and it was not read yet: once per chat, off
    /// the runtime's thread, as `tok` takes about a second.
    pub async fn load_key(&mut self) {
        if !self.api_key.is_empty() || self.vault_key.is_some() {
            return;
        }
        let found = if self.vault_token.is_empty() {
            Err(None)
        } else {
            let (tok, name) = (self.tok.clone(), self.vault_token.clone());
            tokio::task::spawn_blocking(move || tok.open_token(&name))
                .await
                .unwrap_or_else(|e| Err(format!("tok failed: {e}")))
                .map_err(Some)
        };
        match found {
            Ok(value) => {
                // Outbound URLs and queries are checked against it like any key.
                if !self.secrets.contains(&value) {
                    self.secrets.push(value.clone());
                }
                self.vault_key = Some(value);
            }
            Err(why) => {
                self.key_problem = why;
                self.vault_key = Some(String::new());
            }
        }
    }

    /// TAVILY_API_KEY, else the vault's key once [`Self::load_key`] read
    /// it, else "".
    pub fn key(&self) -> &str {
        if self.api_key.is_empty() {
            self.vault_key.as_deref().unwrap_or_default()
        } else {
            &self.api_key
        }
    }

    /// The values outbound URLs and queries must not carry, the vault's
    /// key among them once it was read.
    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }

    /// One line for `/search`, once [`Self::load_key`] ran.
    pub fn status(&self) -> String {
        if self.key().is_empty() {
            match &self.key_problem {
                Some(why) => format!("no Tavily key ({why})"),
                None => "no Tavily key".to_string(),
            }
        } else if let Some(why) = &self.tavily.disabled {
            format!("Tavily off: {why}")
        } else if !self.tavily.usable() {
            let left = self
                .tavily
                .open_until
                .map_or(0, |t| t.saturating_duration_since(Instant::now()).as_secs());
            format!("Tavily paused for {left} s")
        } else {
            "Tavily".to_string()
        }
    }

    /// Messages the user should see once per chat: no key from the vault,
    /// a dead key, a used-up plan.
    pub fn take_loud(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.told_vault && self.key_problem.is_some() && self.key().is_empty() {
            self.told_vault = true;
            let why = self.key_problem.as_deref().unwrap_or_default();
            out.push(format!(
                "web search: no Tavily key ({why}); searches fail in this chat."
            ));
        }
        if let Some(why) = self.tavily.disabled.as_ref().filter(|_| !self.told_tavily) {
            self.told_tavily = true;
            out.push(format!(
                "web search: {why}; no web search for the rest of this chat. Check TAVILY_API_KEY."
            ));
        }
        out
    }

    /// Searches the web for `query`. `k` is the tool's `max_results` as the
    /// model wrote it; `recency` and `site` are dropped when they are not
    /// one of [`RECENCY`] or a domain.
    pub async fn search(
        &mut self,
        http: &dyn HttpClient,
        query: &str,
        k: Option<&Value>,
        recency: Option<&str>,
        site: Option<&str>,
    ) -> Result<Results, SearchError> {
        let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
        if query.is_empty() {
            return Err(err("no query given"));
        }
        if query.chars().count() > MAX_QUERY {
            return Err(err(format!(
                "the query is longer than {MAX_QUERY} characters"
            )));
        }
        // The vault's key first: a chat's first query is checked against it
        // too (rpctl 5533dd2).
        self.load_key().await;
        if let Some(why) = netguard::outbound_secret(&query, &self.secrets) {
            return Err(err(format!(
                "refused: the query would leave this machine and {why}"
            )));
        }
        let k = result_count(k, self.max_results);
        let recency = recency.and_then(|r| RECENCY.into_iter().find(|&known| known == r));
        let site = site.and_then(clean_site);
        let ck = (query.to_lowercase(), k, recency, site.clone());
        if let Some((at, hit)) = self.cache.get(&ck)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(Results {
                cached: true,
                ..hit.clone()
            });
        }

        let key = self.key().to_string();
        let why = if key.is_empty() {
            match &self.key_problem {
                Some(why) => format!("no Tavily key: {why}"),
                None => "no TAVILY_API_KEY set".to_string(),
            }
        } else if let Some(why) = &self.tavily.disabled {
            format!("Tavily is off for this chat ({why})")
        } else if !self.tavily.usable() {
            "Tavily is paused after repeated failures".to_string()
        } else {
            match self
                .ask_tavily(http, &key, &query, k, recency, site.as_deref())
                .await
            {
                Ok(raw) => {
                    self.tavily.ok();
                    let hits = normalise(&raw, self.max_chars, k);
                    if !hits.is_empty() {
                        let res = Results {
                            query,
                            hits,
                            cached: false,
                        };
                        self.remember(ck, res.clone());
                        return Ok(res);
                    }
                    "no results".to_string()
                }
                Err(e) => e.0,
            }
        };
        Err(err(format!("no backend returned results (tavily: {why})")))
    }

    fn remember(&mut self, ck: CacheKey, res: Results) {
        self.cache.insert(ck, (Instant::now(), res));
        while self.cache.len() > CACHE_SIZE {
            self.cache.shift_remove_index(0);
        }
    }

    /// Tavily's results as `{title, url, content, published}` objects.
    async fn ask_tavily(
        &mut self,
        http: &dyn HttpClient,
        key: &str,
        query: &str,
        k: usize,
        recency: Option<&str>,
        site: Option<&str>,
    ) -> Result<Vec<Value>, SearchError> {
        let mut body = json!({
            "query": query,
            "max_results": k,
            "search_depth": self.depth,
            "include_answer": false,
            "include_raw_content": false,
        });
        if let Some(recency) = recency {
            body["time_range"] = json!(recency);
        }
        if let Some(site) = site {
            body["include_domains"] = json!([site]);
        }
        let request = Request::new(Method::POST, TAVILY_URL)
            .header("User-Agent", user_agent::RP_REQUESTS)
            .header("Accept", "*/*")
            .header("Authorization", format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&body).unwrap_or_default())
            .timeouts(Duration::from_secs(5), Duration::from_secs(20));
        let answer = match viy_http::send(http, request).await {
            Ok(response) => {
                let status = response.status;
                response.bytes().await.map(|bytes| (status, bytes))
            }
            Err(e) => Err(e),
        };
        let (status, bytes) = answer.map_err(|e: HttpError| {
            self.tavily.failed(Duration::from_secs(120), 2);
            err(format!("Tavily unreachable ({})", class_name(&e)))
        })?;
        if let Some(why) = disable_reason(status) {
            let why = format!("{why} (HTTP {status})");
            self.tavily.disabled = Some(why.clone());
            return Err(err(why));
        }
        if matches!(status, 429 | 500 | 502 | 503 | 504) {
            if status == 429 {
                self.tavily.failed(Duration::from_secs(60), 1);
            } else {
                self.tavily.failed(Duration::from_secs(120), 2);
            }
            return Err(err(format!("Tavily HTTP {status}")));
        }
        if status != 200 {
            let text: String = String::from_utf8_lossy(&bytes).chars().take(120).collect();
            return Err(err(format!("Tavily HTTP {status}: {text}")));
        }
        let data: Value =
            serde_json::from_slice(&bytes).map_err(|_| err("Tavily returned no JSON"))?;
        let hits = data.get("results").and_then(Value::as_array);
        Ok(hits
            .into_iter()
            .flatten()
            .filter(|h| h.is_object())
            .map(|h| {
                json!({
                    "title": h["title"],
                    "url": h["url"],
                    "content": h["content"],
                    "published": h["published_date"],
                })
            })
            .collect())
    }
}

/// Why Tavily is off for the rest of the chat after `status`.
fn disable_reason(status: u16) -> Option<&'static str> {
    match status {
        401 => Some("Tavily rejected the API key"),
        403 => Some("Tavily refused the API key"),
        432 => Some("the Tavily plan's credits are used up"),
        433 => Some("the Tavily pay-as-you-go limit is reached"),
        _ => None,
    }
}

/// A tool argument read as rp's `int(x or default)` reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntArg {
    /// Absent or empty (null, 0, "", false, [] or {}): the default holds.
    Missing,
    /// Present but not a whole number.
    Bad,
    Int(i64),
}

/// `v` as [`IntArg`]: an integer, a number cut toward zero, a string of
/// digits, true as 1.
pub fn int_arg(v: Option<&Value>) -> IntArg {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => IntArg::Missing,
        Some(Value::String(s)) if s.is_empty() => IntArg::Missing,
        Some(Value::Array(a)) if a.is_empty() => IntArg::Missing,
        Some(Value::Object(o)) if o.is_empty() => IntArg::Missing,
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => IntArg::Missing,
        Some(Value::Bool(true)) => IntArg::Int(1),
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
            .map_or(IntArg::Bad, IntArg::Int),
        Some(Value::String(s)) => s.trim().parse().map_or(IntArg::Bad, IntArg::Int),
        Some(_) => IntArg::Bad,
    }
}

/// How many results to ask for, 1 to 10: the model's `max_results` when it
/// gave one, else SEARCH_MAX_RESULTS; 5 when the one used is not a number.
pub fn result_count(k: Option<&Value>, configured: Option<i64>) -> usize {
    let k = match int_arg(k) {
        IntArg::Missing => configured.unwrap_or(DEFAULT_RESULTS),
        IntArg::Bad => DEFAULT_RESULTS,
        IntArg::Int(n) => n,
    };
    k.clamp(1, 10) as usize
}

/// The domain in `site` (a bare host or a URL), lower-case, or `None`.
pub fn clean_site(site: &str) -> Option<String> {
    let site = site.trim().to_lowercase();
    let site = site
        .strip_prefix("https://")
        .or_else(|| site.strip_prefix("http://"))
        .unwrap_or(&site);
    let host = site.split('/').next().unwrap_or_default();
    let ok = (3..=253).contains(&host.len())
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
    ok.then(|| host.to_string())
}

/// A JSON value as text: a string as it is, nothing for null, anything
/// else as JSON.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// An http(s) URL without its fragment, else `None`.
fn norm_url(u: &str) -> Option<String> {
    let u = u.trim();
    if !(u.starts_with("http://") || u.starts_with("https://"))
        || u.chars().count() > netguard::MAX_URL
    {
        return None;
    }
    Some(u.split('#').next().unwrap_or_default().to_string())
}

static WWW: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"^https?://(www\.)?").unwrap_or_else(|e| panic!("search.rs WWW: {e}"))
});

/// Two URLs that differ only in scheme, `www.`, a trailing slash or case
/// are the same result.
fn dedupe_key(u: &str) -> String {
    WWW.replace(u, "").trim_end_matches('/').to_lowercase()
}

/// The first `k` results with an http(s) URL, one per page, their text
/// cleaned: the content as one line cut to `max_chars` characters, the
/// title to 200, the date to 40.
pub fn normalise(hits: &[Value], max_chars: usize, k: usize) -> Vec<Hit> {
    let mut out: Vec<Hit> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for h in hits {
        let Some(url) = h.get("url").and_then(Value::as_str).and_then(norm_url) else {
            continue;
        };
        if !seen.insert(dedupe_key(&url)) {
            continue;
        }
        let field = |name: &str| h.get(name).map(text_of).unwrap_or_default();
        let content = untrusted::clean(&field("content"))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        out.push(Hit {
            title: untrusted::terminal_safe(&field("title"), Some(200)),
            url,
            content: content.chars().take(max_chars).collect(),
            published: untrusted::terminal_safe(&field("published"), Some(40)),
        });
        if out.len() >= k {
            break;
        }
    }
    out
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
