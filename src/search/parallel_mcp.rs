//! Anonymous Parallel search through its Streamable HTTP MCP endpoint.
//!
//! Works without an account or API key against `https://mcp.parallel.ai/mcp`.
//! Sessions are cached per route (in memory and in `search-parallel-mcp.json`,
//! mirroring `cse.rs`) so warm searches cost a single HTTP POST (~1.1 s) and
//! a rate-limit or transport failure rests this lane cleanly in the ledger
//! while the remaining search engines answer.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Client as HttpClient, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    engines::Hit,
    intent::Intent,
    tasks::{EngineContext, EngineResult, LaneAnswer},
};

pub(crate) const SOURCE: &str = "parallel-mcp";
const ENDPOINT: &str = "https://mcp.parallel.ai/mcp";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const VERSION: &str = "2025-06-18";
const PROTOCOL_VERSION: &str = "mcp-protocol-version";
const SESSION_ID: &str = "mcp-session-id";
const ACCEPT_TYPES: &str = "application/json, text/event-stream";
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const TOOL: &str = "web_search";
const SESSION_TTL: u64 = 1500; // 25 minutes
const DEFAULT_RETRY_SECS: u64 = 60;
const RULE_DENIED_STATUS: &str = "invalid-config: excluded by the operator's local DonSeTch rules";

#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEST_ENDPOINT: String;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedSession {
    id: String,
    protocol_version: String,
    written: u64,
}

impl CachedSession {
    fn fresh(&self, time: u64) -> bool {
        !self.id.is_empty()
            && !self.protocol_version.is_empty()
            && time
                .checked_sub(self.written)
                .is_some_and(|age| age < SESSION_TTL)
    }
}

#[derive(Default)]
pub(super) struct Client {
    sessions: Mutex<HashMap<String, CachedSession>>,
    http: Mutex<HashMap<String, HttpClient>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn cache_path() -> std::path::PathBuf {
    crate::paths::cache_dir().join("search-parallel-mcp.json")
}

fn session_key(proxy: Option<&crate::egress::proxy::Proxy>) -> String {
    format!(
        "{}|{}|{}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        proxy.map(|p| p.id()).unwrap_or_else(|| "direct".into())
    )
}

impl Client {
    pub(super) fn request_cost(&self, proxy: Option<&crate::egress::proxy::Proxy>) -> usize {
        let Ok(proxy) = request_proxy(proxy) else {
            return 2;
        };
        match self.get(&session_key(proxy.as_ref())) {
            Some(session) if session.fresh(now().saturating_add(10)) => 1,
            _ => 2,
        }
    }

    pub fn load() -> Self {
        let client = Self::default();
        if crate::config::cfg().state.no_disk_state {
            return client;
        }
        let path = cache_path();
        if std::fs::metadata(&path).is_ok_and(|m| m.len() < 128 << 10)
            && let Ok(bytes) = std::fs::read(path)
            && let Ok(mut sessions) =
                serde_json::from_slice::<HashMap<String, CachedSession>>(&bytes)
        {
            sessions.retain(|_, session| session.fresh(now()));
            if sessions.len() <= 64 {
                *client
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = sessions;
            }
        }
        client
    }

    fn get(&self, key: &str) -> Option<CachedSession> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .filter(|s| s.fresh(now()))
            .cloned()
    }

    fn put(&self, key: &str, session: Option<CachedSession>) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.retain(|_, s| s.fresh(now()));
        if let Some(session) = session {
            if sessions.len() >= 64 {
                sessions.clear();
            }
            sessions.insert(key.to_string(), session);
        } else {
            sessions.remove(key);
        }
        if crate::config::cfg().state.no_disk_state {
            return;
        }
        if let Ok(bytes) = serde_json::to_vec(&*sessions) {
            let _ = crate::config::write_private(&cache_path(), &bytes);
        }
    }

    fn http_client(
        &self,
        proxy: Option<&crate::egress::proxy::Proxy>,
    ) -> Result<HttpClient, String> {
        let route_key = proxy.map(|p| p.id()).unwrap_or_else(|| "direct".into());
        if let Some(existing) = self
            .http
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&route_key)
            .cloned()
        {
            return Ok(existing);
        }
        let mut builder = HttpClient::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .pool_max_idle_per_host(2)
            .no_proxy();
        if let Some(proxy) = proxy {
            if proxy.scheme == crate::egress::proxy::ProxyScheme::Socks5 {
                return Err(
                    "invalid-config: parallel-mcp requires an HTTP proxy or a direct route".into(),
                );
            }
            builder = builder.proxy(
                reqwest::Proxy::all(proxy.wire_url())
                    .map_err(|_| "invalid-config: unsupported proxy configuration".to_string())?,
            );
        }
        let built = builder
            .build()
            .map_err(|e| format!("transport:{}", e.without_url()))?;
        let mut map = self
            .http
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.len() >= 16 {
            map.clear();
        }
        map.insert(route_key, built.clone());
        Ok(built)
    }
}

fn request_proxy(
    explicit: Option<&crate::egress::proxy::Proxy>,
) -> Result<Option<crate::egress::proxy::Proxy>, String> {
    if let Some(proxy) = explicit {
        return Ok(Some(proxy.clone()));
    }
    crate::egress::proxy::from_env_for(ENDPOINT).map_err(|e| super::tasks::engine_fetch_status(&e))
}

fn endpoint_url() -> String {
    #[cfg(test)]
    if let Ok(url) = TEST_ENDPOINT.try_with(String::clone) {
        return url;
    }
    ENDPOINT.to_string()
}

/// Build the `search_queries` string conforming to Parallel's `web_search`
/// schema: concise keywords, bare-domain `site:` (no URL path), no double quotes.
pub(crate) fn format_search_query(raw: &str) -> String {
    let mut parts = Vec::new();
    for token in raw.split_whitespace() {
        let unquoted = token.replace('"', "");
        if unquoted.is_empty() {
            continue;
        }
        if let Some(rest) = unquoted
            .strip_prefix("site:")
            .or_else(|| unquoted.strip_prefix("SITE:"))
        {
            let domain = rest
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or("")
                .trim_end_matches('.');
            if !domain.is_empty() {
                parts.push(format!("site:{domain}"));
            }
            continue;
        }
        if let Some(rest) = unquoted
            .strip_prefix("intitle:")
            .or_else(|| unquoted.strip_prefix("INTITLE:"))
        {
            if !rest.is_empty() {
                parts.push(rest.to_string());
            }
            continue;
        }
        if let Some(rest) = unquoted
            .strip_prefix("filetype:")
            .or_else(|| unquoted.strip_prefix("FILETYPE:"))
        {
            if !rest.is_empty() {
                parts.push(rest.to_string());
            }
            continue;
        }
        parts.push(unquoted);
    }
    let joined = parts.join(" ");
    if joined.is_empty() {
        raw.trim().to_string()
    } else {
        joined
    }
}

fn objective_for(query: &str, intent: Intent) -> String {
    match intent {
        Intent::News => format!("Find recent news about: {query}"),
        Intent::Paper => format!("Find academic papers about: {query}"),
        _ => query.to_string(),
    }
}

fn retry_after_secs(response: &Response) -> u64 {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|s| s.clamp(30, 900))
        .unwrap_or(DEFAULT_RETRY_SECS)
}

#[derive(Debug)]
enum WireError {
    RateLimited(u64),
    SessionExpired,
    Other(String),
}

impl WireError {
    fn into_status(self) -> String {
        match self {
            Self::RateLimited(secs) => format!("rate-limited:{secs}"),
            Self::SessionExpired => "session-expired".into(),
            Self::Other(s) => s,
        }
    }
}

fn apply_headers(
    mut req: RequestBuilder,
    protocol_version: &str,
    session_id: Option<&str>,
) -> Result<RequestBuilder, WireError> {
    req = req
        .header(ACCEPT, ACCEPT_TYPES)
        .header(PROTOCOL_VERSION, protocol_version);
    if let Some(id) = session_id {
        let value = HeaderValue::from_str(id).map_err(|_| WireError::SessionExpired)?;
        req = req.header(SESSION_ID, value);
    }
    Ok(req)
}

async fn post_json(
    client: &HttpClient,
    endpoint: &str,
    protocol_version: &str,
    session_id: Option<&str>,
    body: Value,
) -> Result<Response, WireError> {
    let req = apply_headers(client.post(endpoint), protocol_version, session_id)?;
    if !crate::fetch::layer::record_external_request() {
        return Err(WireError::Other("budget-skipped".into()));
    }
    let resp = req
        .json(&body)
        .send()
        .await
        .map_err(|e| WireError::Other(format!("transport:{}", e.without_url())))?;
    let status = resp.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(WireError::RateLimited(retry_after_secs(&resp)));
    }
    if session_id.is_some() && matches!(status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND) {
        return Err(WireError::SessionExpired);
    }
    if !status.is_success() {
        return Err(WireError::Other(format!("blocked:{}", status.as_u16())));
    }
    Ok(resp)
}

async fn initialize_session(
    client: &HttpClient,
    endpoint: &str,
) -> Result<CachedSession, WireError> {
    let resp = post_json(
        client,
        endpoint,
        VERSION,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": env!("CARGO_PKG_NAME"),
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
    )
    .await?;
    let session_id = resp
        .headers()
        .get(SESSION_ID)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_default();
    let initialized = read_mcp_response(resp, 1).await?;
    let negotiated = initialized["protocolVersion"]
        .as_str()
        .filter(|v| matches!(*v, VERSION | "2025-03-26"))
        .ok_or_else(|| WireError::Other("invalid-response: unsupported MCP version".into()))?
        .to_string();
    let sid_opt = (!session_id.is_empty()).then_some(session_id.as_str());
    let ack = post_json(
        client,
        endpoint,
        &negotiated,
        sid_opt,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await?;
    if !matches!(
        ack.status(),
        StatusCode::ACCEPTED | StatusCode::NO_CONTENT | StatusCode::OK
    ) {
        return Err(WireError::Other(
            "invalid-response: init not acknowledged".into(),
        ));
    }
    Ok(CachedSession {
        id: session_id,
        protocol_version: negotiated,
        written: now(),
    })
}

async fn call_web_search(
    client: &HttpClient,
    endpoint: &str,
    session: &CachedSession,
    objective: &str,
    search_query: &str,
) -> Result<SearchPayload, WireError> {
    let sid_opt = (!session.id.is_empty()).then_some(session.id.as_str());
    let resp = post_json(
        client,
        endpoint,
        &session.protocol_version,
        sid_opt,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": TOOL,
                "arguments": {
                    "objective": objective,
                    "search_queries": [search_query]
                }
            }
        }),
    )
    .await?;
    let reply = read_mcp_response(resp, 2).await?;
    parse_search_reply(reply)
}

/// Read one JSON-RPC response from either `application/json` or
/// `text/event-stream`, enforcing [`MAX_RESPONSE_BYTES`] while streaming.
async fn read_mcp_response(mut response: Response, request_id: u64) -> Result<Value, WireError> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split(';').next())
        .unwrap_or_default()
        .trim();
    let is_sse = content_type.eq_ignore_ascii_case("text/event-stream");
    if !is_sse && !content_type.eq_ignore_ascii_case("application/json") {
        return Err(WireError::Other(
            "invalid-response: unexpected content type".into(),
        ));
    }

    if !is_sse {
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| WireError::Other(format!("transport:{}", e.without_url())))?
        {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(WireError::Other("blocked:oversize-serp".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = rpc_json(&bytes)?;
        return rpc_result(body, request_id)?
            .ok_or_else(|| WireError::Other("invalid-response: missing matching id".into()));
    }

    // Streamable HTTP SSE parser (no extra crate): accumulate lines up to the
    // 256 KiB cap, dispatching each event on a blank line.
    let mut total_bytes = 0usize;
    let mut pending = Vec::<u8>::new();
    let mut data_lines = Vec::<String>::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| WireError::Other(format!("transport:{}", e.without_url())))?
    {
        total_bytes += chunk.len();
        if total_bytes > MAX_RESPONSE_BYTES {
            return Err(WireError::Other("blocked:oversize-serp".into()));
        }
        pending.extend_from_slice(&chunk);
        while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
            let mut line_bytes = pending.drain(..=nl).collect::<Vec<u8>>();
            if line_bytes.ends_with(b"\n") {
                line_bytes.pop();
            }
            if line_bytes.ends_with(b"\r") {
                line_bytes.pop();
            }
            let line = std::str::from_utf8(&line_bytes)
                .map_err(|_| WireError::Other("invalid-response: non-utf8 sse".into()))?;
            if let Some(found) = process_sse_line(line, &mut data_lines, request_id)? {
                return Ok(found);
            }
        }
    }
    if !pending.is_empty() {
        if pending.ends_with(b"\r") {
            pending.pop();
        }
        let line = std::str::from_utf8(&pending)
            .map_err(|_| WireError::Other("invalid-response: non-utf8 sse".into()))?;
        if let Some(found) = process_sse_line(line, &mut data_lines, request_id)? {
            return Ok(found);
        }
    }
    // Flush trailing event if stream closed right after `data:` without a blank line.
    if let Some(found) = flush_sse_event(&mut data_lines, request_id)? {
        return Ok(found);
    }
    Err(WireError::Other(
        "invalid-response: missing matching sse event".into(),
    ))
}

fn process_sse_line(
    line: &str,
    data_lines: &mut Vec<String>,
    request_id: u64,
) -> Result<Option<Value>, WireError> {
    if line.is_empty() {
        return flush_sse_event(data_lines, request_id);
    }
    if line.starts_with(':') {
        return Ok(None);
    }
    let (field, value) = match line.split_once(':') {
        Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
        None => (line, ""),
    };
    if field == "data" {
        data_lines.push(value.to_string());
    }
    Ok(None)
}

fn flush_sse_event(
    data_lines: &mut Vec<String>,
    request_id: u64,
) -> Result<Option<Value>, WireError> {
    if data_lines.is_empty() {
        return Ok(None);
    }
    let joined = data_lines.join("\n");
    data_lines.clear();
    let body = rpc_json(joined.as_bytes())?;
    rpc_result(body, request_id)
}

fn rpc_json(bytes: &[u8]) -> Result<Value, WireError> {
    serde_json::from_slice(bytes)
        .map_err(|_| WireError::Other("invalid-response: malformed json".into()))
}

fn rpc_result(body: Value, request_id: u64) -> Result<Option<Value>, WireError> {
    let matching_id = body["id"].as_u64() == Some(request_id);
    let error = body.get("error");
    let uncorrelated_error = body.get("id").is_some_and(Value::is_null) && error.is_some();
    if !matching_id && !uncorrelated_error {
        return Ok(None);
    }
    if body["jsonrpc"] != "2.0" {
        return Err(WireError::Other("invalid-response: bad jsonrpc".into()));
    }
    if let Some(error) = error {
        let code = error["code"]
            .as_i64()
            .ok_or_else(|| WireError::Other("invalid-response: bad rpc error".into()))?;
        return Err(WireError::Other(format!("rpc-error:{code}")));
    }
    body.get("result")
        .cloned()
        .map(Some)
        .ok_or_else(|| WireError::Other("invalid-response: missing result".into()))
}

#[derive(Deserialize)]
struct SearchPayload {
    results: Vec<RemoteHit>,
}

#[derive(Deserialize)]
struct RemoteHit {
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    publish_date: Option<String>,
    #[serde(default)]
    excerpts: Vec<String>,
}

fn parse_search_reply(mut reply: Value) -> Result<SearchPayload, WireError> {
    match reply.get("isError") {
        Some(Value::Bool(true)) => {
            return Err(WireError::Other("tool-error".into()));
        }
        Some(Value::Bool(false)) | None => {}
        Some(_) => {
            return Err(WireError::Other("invalid-response: bad isError".into()));
        }
    }
    if let Some(payload) = reply.get_mut("structuredContent").map(Value::take)
        && !payload.is_null()
    {
        return serde_json::from_value(payload)
            .map_err(|_| WireError::Other("invalid-response: bad structuredContent".into()));
    }
    let text = reply["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|block| block["type"] == "text"))
        .and_then(|block| block["text"].as_str())
        .ok_or_else(|| WireError::Other("invalid-response: missing content".into()))?;
    serde_json::from_str(text)
        .map_err(|_| WireError::Other("invalid-response: bad text payload".into()))
}

pub(super) fn evidence_text(text: &str, max_chars: usize) -> String {
    text.split_whitespace()
        .flat_map(|word| word.chars().chain(std::iter::once(' ')))
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn payload_to_hits(payload: SearchPayload) -> Vec<Hit> {
    let mut seen = HashSet::new();
    let mut hits = Vec::new();
    for item in payload.results.into_iter().take(20) {
        let raw_url = item.url.trim();
        if raw_url.is_empty()
            || raw_url.len() > 4096
            || raw_url.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            continue;
        }
        let Ok(parsed) = url::Url::parse(raw_url) else {
            continue;
        };
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.has_host()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            continue;
        }
        let clean_url =
            super::contract::decode_redirect(raw_url).unwrap_or_else(|| raw_url.to_string());
        if !seen.insert(super::rank::norm_key(&clean_url)) {
            continue;
        }
        let title = evidence_text(item.title.as_deref().unwrap_or_default(), 240);
        let title = if title.is_empty() {
            super::rank::host_of(&clean_url)
        } else {
            title
        };
        let snippet = evidence_text(&item.excerpts.join(" "), 1600);
        let published = item
            .publish_date
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 32)
            .map(str::to_string);
        let rank = hits.len();
        hits.push(Hit {
            title,
            url: clean_url,
            snippet,
            rank,
            published,
        });
    }
    hits
}

/// First-class search fan-out task for Parallel's anonymous MCP endpoint.
pub(super) async fn task(
    query: String,
    lane: String,
    proxy: Option<crate::egress::proxy::Proxy>,
    context: EngineContext<'_>,
    budget: Duration,
) -> (String, EngineResult) {
    let start = Instant::now();
    let work = async {
        let normalized = super::query::normalize(&query);
        let normalized = normalized.as_str();
        if normalized.chars().count() > 200 {
            return Err("not-asked".into());
        }
        if crate::fetch::guards::validate_url_basic(ENDPOINT).is_err() {
            return Err(RULE_DENIED_STATUS.into());
        }
        context.pool.pace(SOURCE, &lane).await;
        let proxy = request_proxy(proxy.as_ref())?;
        let http = context.parallel_mcp.http_client(proxy.as_ref())?;
        let key = session_key(proxy.as_ref());
        let endpoint = endpoint_url();

        let intent = context
            .facts
            .map(|f| super::intent::detect(f.raw()))
            .unwrap_or_else(|| super::intent::detect(normalized));
        let objective = objective_for(normalized, intent);
        let search_query = format_search_query(normalized);

        let mut requests = 0usize;
        let mut used_cached = false;
        let mut session = match context.parallel_mcp.get(&key) {
            Some(cached) => {
                used_cached = true;
                cached
            }
            None => {
                if !context.requests.take(SOURCE) {
                    return Err("budget-skipped".into());
                }
                requests += 1;
                let fresh = match initialize_session(&http, &endpoint).await {
                    Ok(s) => s,
                    Err(err) => {
                        context.requests.release(SOURCE);
                        return Err(err.into_status());
                    }
                };
                context.parallel_mcp.put(&key, Some(fresh.clone()));
                fresh
            }
        };

        if !context.requests.take(SOURCE) {
            return Err("budget-skipped".into());
        }
        requests += 1;

        let payload =
            match call_web_search(&http, &endpoint, &session, &objective, &search_query).await {
                Ok(payload) => payload,
                Err(WireError::SessionExpired) if used_cached => {
                    context.parallel_mcp.put(&key, None);
                    if !context.requests.take(SOURCE) {
                        return Err("budget-skipped".into());
                    }
                    requests += 1;
                    session = initialize_session(&http, &endpoint)
                        .await
                        .map_err(WireError::into_status)?;
                    context.parallel_mcp.put(&key, Some(session.clone()));
                    call_web_search(&http, &endpoint, &session, &objective, &search_query)
                        .await
                        .map_err(WireError::into_status)?
                }
                Err(err) => {
                    if matches!(err, WireError::SessionExpired) {
                        context.parallel_mcp.put(&key, None);
                    }
                    return Err(err.into_status());
                }
            };

        context.pool.observe_rtt(&lane, start.elapsed());
        let hits = payload_to_hits(payload);
        let facts = super::query::QueryFacts::new(&query);
        let judged = super::quality::judge(context.facts.unwrap_or(&facts), hits);
        Ok(LaneAnswer {
            hits: judged.kept,
            items: judged.items,
            ms: start.elapsed().as_millis() as u64,
            egress: lane.clone(),
            engine_lane: true,
            instant: None,
            class: judged.class,
            requests,
            cache_age: None,
        })
    };

    let result = match tokio::time::timeout(budget, work).await {
        Ok(Ok(answer)) => Ok(answer),
        Ok(Err(status)) => Err((status, lane, true)),
        Err(_) => Err(("timeout".into(), lane, true)),
    };
    (SOURCE.into(), result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    #[test]
    fn format_search_query_cleans_quotes_and_site_paths() {
        assert_eq!(
            format_search_query(r#"site:docs.rs/tokio/latest "select macro" filetype:html"#),
            "site:docs.rs select macro html"
        );
        assert_eq!(
            format_search_query("intitle:cancellation rust tokio"),
            "cancellation rust tokio"
        );
    }

    #[test]
    fn sse_parser_handles_comments_unrelated_ids_and_multiline_data() {
        let mut lines = Vec::new();
        assert!(
            process_sse_line(": heartbeat", &mut lines, 2)
                .unwrap()
                .is_none()
        );
        assert!(
            process_sse_line(
                r#"data: {"jsonrpc":"2.0","method":"notifications/progress"}"#,
                &mut lines,
                2
            )
            .unwrap()
            .is_none()
        );
        assert!(process_sse_line("", &mut lines, 2).unwrap().is_none());
        assert!(
            process_sse_line(
                r#"data: {"jsonrpc":"2.0","id":999,"result":{}}"#,
                &mut lines,
                2
            )
            .unwrap()
            .is_none()
        );
        assert!(process_sse_line("", &mut lines, 2).unwrap().is_none());
        assert!(
            process_sse_line(r#"data: {"jsonrpc":"2.0","id":2,"#, &mut lines, 2)
                .unwrap()
                .is_none()
        );
        assert!(
            process_sse_line(r#"data: "result":{"ok":true}}"#, &mut lines, 2)
                .unwrap()
                .is_none()
        );
        let found = process_sse_line("", &mut lines, 2)
            .unwrap()
            .expect("matching id");
        assert_eq!(found["ok"], true);
    }

    #[tokio::test]
    async fn parallel_mcp_reuses_session_and_extracts_publish_dates() {
        let mut config = crate::config::DonsetchConfig::default();
        config.state.no_disk_state = true;
        config.proxy.from_environment = false;
        crate::config::install(config).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let init_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let init_c = std::sync::Arc::clone(&init_count);
        let call_c = std::sync::Arc::clone(&call_count);

        let server = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let mut reader = BufReader::new(socket);
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).await.unwrap_or(0) == 0 {
                    continue;
                }
                let mut headers = HashMap::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line.trim().is_empty()
                    {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                assert_eq!(
                    headers.get("user-agent").map(String::as_str),
                    Some(USER_AGENT)
                );
                assert!(!headers.contains_key("authorization"));
                assert!(!headers.contains_key("x-api-key"));
                assert!(!headers.contains_key("cookie"));

                let len: usize = headers
                    .get("content-length")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).await.unwrap();
                let rpc: Value = serde_json::from_slice(&body).unwrap();
                let method = rpc["method"].as_str().unwrap_or("");
                let id = rpc["id"].clone();

                let (status, ct, extra, payload) = match method {
                    "initialize" => {
                        let n = init_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        (
                            "200 OK",
                            "application/json",
                            format!("{SESSION_ID}: sess-{n}\r\n"),
                            json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": VERSION}})
                                .to_string(),
                        )
                    }
                    "notifications/initialized" => (
                        "202 Accepted",
                        "application/json",
                        String::new(),
                        String::new(),
                    ),
                    "tools/call" => {
                        let n = call_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        if n == 3 && headers.get(SESSION_ID).map(String::as_str) == Some("sess-1") {
                            // Simulate server expiring the cached session on the 3rd search.
                            (
                                "404 Not Found",
                                "application/json",
                                String::new(),
                                String::new(),
                            )
                        } else {
                            let res = json!({
                                "isError": false,
                                "structuredContent": {
                                    "results": [
                                        {
                                            "url": "https://docs.rs/tokio/latest/tokio/macro.select.html",
                                            "title": "\n  select in tokio - Rust \n",
                                            "publish_date": "2026-11-12",
                                            "excerpts": ["The select! macro waits on multiple async branches."]
                                        },
                                        {
                                            "url": "javascript:alert(1)",
                                            "title": "bad",
                                            "excerpts": ["ignored"]
                                        }
                                    ]
                                }
                            });
                            let sse = format!(
                                ": ok\r\nevent: message\r\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{res}}}\r\n\r\n"
                            );
                            ("200 OK", "text/event-stream", String::new(), sse)
                        }
                    }
                    other => panic!("unexpected MCP method: {other}"),
                };

                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{payload}",
                    payload.len()
                );
                let _ = reader.get_mut().write_all(reply.as_bytes()).await;
            }
        });

        let mcp = Client::default();
        let fetcher =
            crate::fetch::client::Fetcher::new(crate::profile::BrowserProfile::host_default())
                .unwrap();
        let pool = super::super::egress::EgressPool::new(Vec::new());
        let google = super::super::engines::google_wml::ProfileSelector::from_env();
        let cse = super::super::cse::Client::default();
        let allowance = super::super::tasks::RequestAllowance::new(20);
        let facts = super::super::query::QueryFacts::new("tokio select macro");
        let ctx = EngineContext {
            fetcher: &fetcher,
            pool: &pool,
            google: &google,
            cse: &cse,
            parallel_mcp: &mcp,
            lang: None,
            requests: &allowance,
            facts: Some(&facts),
        };

        TEST_ENDPOINT
            .scope(endpoint, async {
                // 1st search: cold init (2 requests).
                assert_eq!(mcp.request_cost(None), 2);
                let (name, res1) = task(
                    "tokio select macro".into(),
                    "direct".into(),
                    None,
                    ctx.clone(),
                    Duration::from_secs(5),
                )
                .await;
                assert_eq!(name, SOURCE);
                let ans1 = res1.expect("first search succeeds");
                assert_eq!(ans1.hits.len(), 1);
                assert_eq!(ans1.hits[0].title, "select in tokio - Rust");
                assert_eq!(ans1.hits[0].published.as_deref(), Some("2026-11-12"));
                assert_eq!(ans1.requests, 2);

                // 2nd search: warm session reuse (1 request, no re-init).
                assert_eq!(mcp.request_cost(None), 1);
                let (_, res2) = task(
                    "tokio select macro".into(),
                    "direct".into(),
                    None,
                    ctx.clone(),
                    Duration::from_secs(5),
                )
                .await;
                let ans2 = res2.expect("second search reuses session");
                assert_eq!(ans2.requests, 1);
                assert_eq!(init_count.load(std::sync::atomic::Ordering::SeqCst), 1);

                // 3rd search: expired session (404) triggers transparent re-init.
                let (_, res3) = task(
                    "tokio select macro".into(),
                    "direct".into(),
                    None,
                    ctx.clone(),
                    Duration::from_secs(5),
                )
                .await;
                let ans3 = res3.expect("expired session transparently re-inits");
                assert_eq!(ans3.hits.len(), 1);
                assert_eq!(init_count.load(std::sync::atomic::Ordering::SeqCst), 2);
            })
            .await;

        server.abort();
    }

    #[tokio::test]
    async fn parallel_mcp_429_maps_to_rate_limited() {
        let mut config = crate::config::DonsetchConfig::default();
        config.state.no_disk_state = true;
        config.proxy.from_environment = false;
        crate::config::install(config).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        });

        let mcp = Client::default();
        let fetcher =
            crate::fetch::client::Fetcher::new(crate::profile::BrowserProfile::host_default())
                .unwrap();
        let pool = super::super::egress::EgressPool::new(Vec::new());
        let google = super::super::engines::google_wml::ProfileSelector::from_env();
        let cse = super::super::cse::Client::default();
        let allowance = super::super::tasks::RequestAllowance::new(10);
        let ctx = EngineContext {
            fetcher: &fetcher,
            pool: &pool,
            google: &google,
            cse: &cse,
            parallel_mcp: &mcp,
            lang: None,
            requests: &allowance,
            facts: None,
        };

        TEST_ENDPOINT
            .scope(endpoint, async {
                let (_, res) = task(
                    "rust async".into(),
                    "direct".into(),
                    None,
                    ctx,
                    Duration::from_secs(5),
                )
                .await;
                let err = res.unwrap_err();
                assert_eq!(err.0, "rate-limited:120");
            })
            .await;
        server.abort();
    }

    #[tokio::test]
    async fn parallel_mcp_deny_rule_blocks_outbound_requests() {
        let mut denied_cfg = crate::config::DonsetchConfig::default();
        denied_cfg.state.no_disk_state = true;
        denied_cfg.rules.url.insert(
            "mcp.parallel.ai".into(),
            crate::rules::UrlRule {
                action: crate::rules::RuleAction::Deny,
                message: Some("blocked".into()),
                ..Default::default()
            },
        );
        crate::config::install(denied_cfg).unwrap();

        let mcp = Client::default();
        let fetcher =
            crate::fetch::client::Fetcher::new(crate::profile::BrowserProfile::host_default())
                .unwrap();
        let pool = super::super::egress::EgressPool::new(Vec::new());
        let google = super::super::engines::google_wml::ProfileSelector::from_env();
        let cse = super::super::cse::Client::default();
        let allowance = super::super::tasks::RequestAllowance::new(10);
        let ctx = EngineContext {
            fetcher: &fetcher,
            pool: &pool,
            google: &google,
            cse: &cse,
            parallel_mcp: &mcp,
            lang: None,
            requests: &allowance,
            facts: None,
        };
        let (_, res) = task(
            "rust async".into(),
            "direct".into(),
            None,
            ctx,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(res.unwrap_err().0, RULE_DENIED_STATUS);
    }
}
