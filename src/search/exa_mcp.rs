//! Keyless Exa search through its public Streamable HTTP MCP endpoint.
//!
//! Enabled by default when `lang` is unset (`[search] exa_mcp = false` to
//! disable). Sends a single stateless JSON-RPC `tools/call` (`web_search_exa`)
//! `POST` to `https://mcp.exa.ai/mcp` with no key, cookie, account, or session
//! handshake. A 429 rests this lane in the ledger while the other engines answer.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Client as HttpClient, StatusCode};
use serde_json::{Value, json};

use super::{
    engines::Hit,
    intent::Intent,
    tasks::{EngineContext, EngineResult, LaneAnswer},
};

pub(crate) const SOURCE: &str = "exa-mcp";
const ENDPOINT: &str = "https://mcp.exa.ai/mcp";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const MAX_RESULTS: usize = 10;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const DEFAULT_RETRY_SECS: u64 = 60;
const RULE_DENIED_STATUS: &str = "invalid-config: excluded by the operator's local DonSeTch rules";

#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEST_ENDPOINT: String;
}

/// One HTTP client per egress route, built on first use.
static HTTP: LazyLock<Mutex<HashMap<String, HttpClient>>> = LazyLock::new(Default::default);

fn endpoint_url() -> String {
    #[cfg(test)]
    if let Ok(url) = TEST_ENDPOINT.try_with(String::clone) {
        return url;
    }
    ENDPOINT.to_string()
}

fn request_proxy(
    explicit: Option<&crate::egress::proxy::Proxy>,
) -> Result<Option<crate::egress::proxy::Proxy>, String> {
    if let Some(proxy) = explicit {
        return Ok(Some(proxy.clone()));
    }
    crate::egress::proxy::from_env_for(ENDPOINT).map_err(|e| super::tasks::engine_fetch_status(&e))
}

fn http_client(proxy: Option<&crate::egress::proxy::Proxy>) -> Result<HttpClient, String> {
    let route_key = proxy.map(|p| p.id()).unwrap_or_else(|| "direct".into());
    if let Some(existing) = HTTP
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
            return Err("invalid-config: exa-mcp requires an HTTP proxy or a direct route".into());
        }
        builder = builder.proxy(
            reqwest::Proxy::all(proxy.wire_url())
                .map_err(|_| "invalid-config: unsupported proxy configuration".to_string())?,
        );
    }
    let built = builder
        .build()
        .map_err(|e| format!("transport:{}", e.without_url()))?;
    let mut map = HTTP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() >= 16 {
        map.clear();
    }
    map.insert(route_key, built.clone());
    Ok(built)
}

fn objective_for(query: &str, intent: Intent) -> String {
    match intent {
        Intent::News => format!("Find recent news and official announcements about: {query}"),
        Intent::Paper => format!("Find academic papers and technical publications about: {query}"),
        _ => query.to_string(),
    }
}

fn extract_jsonrpc_message(bytes: &[u8], is_sse: bool) -> Result<Value, String> {
    if !is_sse && !bytes.starts_with(b"event:") && !bytes.starts_with(b"data:") {
        return serde_json::from_slice(bytes)
            .map_err(|_| "invalid-response: malformed json".into());
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| "invalid-response: non-utf8 sse".to_string())?;
    let mut data_buf = String::new();
    let mut last_value: Option<Value> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            if !data_buf.is_empty() {
                if let Ok(val) = serde_json::from_str::<Value>(&data_buf) {
                    if val.get("result").is_some() || val.get("error").is_some() {
                        return Ok(val);
                    }
                    last_value = Some(val);
                }
                data_buf.clear();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let chunk = rest.strip_prefix(' ').unwrap_or(rest);
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(chunk);
        }
    }
    if !data_buf.is_empty()
        && let Ok(val) = serde_json::from_str::<Value>(&data_buf)
    {
        return Ok(val);
    }
    last_value.ok_or_else(|| "invalid-response: empty sse payload".into())
}

fn extract_tool_text(msg: Value) -> Result<String, String> {
    if let Some(err) = msg.get("error") {
        let text = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("mcp-error")
            .to_ascii_lowercase();
        if text.contains("rate limit") || text.contains("429") || text.contains("too many") {
            return Err(format!("rate-limited:{DEFAULT_RETRY_SECS}"));
        }
        return Err("blocked:mcp-error".into());
    }
    let Some(result) = msg.get("result") else {
        return Err("invalid-response: missing result".into());
    };
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let joined = result
            .get("content")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
            .to_ascii_lowercase();
        if joined.contains("rate limit") || joined.contains("429") || joined.contains("too many") {
            return Err(format!("rate-limited:{DEFAULT_RETRY_SECS}"));
        }
        return Err("blocked:mcp-error".into());
    }
    let Some(items) = result.get("content").and_then(Value::as_array) else {
        return Err("invalid-response: missing content".into());
    };
    let mut out = String::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("text")
            && let Some(t) = item.get("text").and_then(Value::as_str)
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    Ok(out)
}

async fn post_search(
    http: &HttpClient,
    endpoint: &str,
    query: &str,
    objective: &str,
) -> Result<String, String> {
    if !crate::fetch::layer::record_external_request() {
        return Err("budget-skipped".into());
    }
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "web_search_exa",
            "arguments": {
                "query": query,
                "objective": objective,
                "numResults": MAX_RESULTS,
            }
        }
    });
    let mut resp = http
        .post(endpoint)
        .header(ACCEPT, "application/json, text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("transport:{}", e.without_url()))?;
    let status = resp.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        let secs = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|s| s.clamp(30, 900))
            .unwrap_or(DEFAULT_RETRY_SECS);
        return Err(format!("rate-limited:{secs}"));
    }
    if !status.is_success() {
        return Err(format!("blocked:{}", status.as_u16()));
    }
    let is_sse = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"));
    let mut bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("transport:{}", e.without_url()))?
    {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err("blocked:oversize-serp".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let msg = extract_jsonrpc_message(&bytes, is_sse)?;
    extract_tool_text(msg)
}

#[derive(Default)]
struct RawRecord {
    title: String,
    url: String,
    published: Option<String>,
    highlights: String,
}

fn push_record(rec: RawRecord, seen: &mut HashSet<String>, hits: &mut Vec<Hit>) {
    let raw_url = rec.url.trim();
    if raw_url.is_empty()
        || raw_url.len() > 4096
        || raw_url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return;
    }
    let Ok(parsed) = url::Url::parse(raw_url) else {
        return;
    };
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.has_host()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return;
    }
    if !seen.insert(super::rank::norm_key(raw_url)) {
        return;
    }
    let title = super::parallel_mcp::evidence_text(&rec.title, 240);
    let title = if title.is_empty() {
        super::rank::host_of(raw_url)
    } else {
        title
    };
    let rank = hits.len();
    hits.push(Hit {
        title,
        url: raw_url.to_string(),
        snippet: super::parallel_mcp::evidence_text(&rec.highlights, 1600),
        rank,
        published: rec.published,
    });
}

fn parse_exa_text(text: &str) -> Vec<Hit> {
    let mut seen = HashSet::new();
    let mut hits = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut cur: Option<RawRecord> = None;
    let mut in_highlights = false;
    for (idx, &raw) in lines.iter().enumerate() {
        let line = raw.trim_end_matches('\r');
        // A new Exa record opens with `Title: ...` immediately followed by
        // `URL: ...`, so highlight prose mentioning `Title:` never splits a hit.
        let starts_record = line.starts_with("Title:")
            && lines
                .get(idx + 1)
                .is_some_and(|next| next.trim_end_matches('\r').starts_with("URL:"));
        if starts_record {
            if let Some(prev) = cur.take() {
                push_record(prev, &mut seen, &mut hits);
                if hits.len() >= 20 {
                    break;
                }
            }
            cur = Some(RawRecord {
                title: line["Title:".len()..].trim().to_string(),
                ..Default::default()
            });
            in_highlights = false;
            continue;
        }
        let Some(rec) = cur.as_mut() else {
            continue;
        };
        if !in_highlights {
            if let Some(rest) = line.strip_prefix("URL:") {
                rec.url = rest.trim().to_string();
                continue;
            }
            if let Some(rest) = line.strip_prefix("Published:") {
                let pub_val = rest.trim();
                if !pub_val.is_empty()
                    && !pub_val.eq_ignore_ascii_case("n/a")
                    && !pub_val.eq_ignore_ascii_case("none")
                    && pub_val.len() <= 32
                {
                    rec.published = Some(pub_val.to_string());
                }
                continue;
            }
            if line.starts_with("Author:") {
                continue;
            }
            if let Some(rest) = line
                .strip_prefix("Highlights:")
                .or_else(|| line.strip_prefix("Text:"))
            {
                in_highlights = true;
                let trimmed = rest.trim();
                if !trimmed.is_empty() {
                    rec.highlights.push_str(trimmed);
                }
                continue;
            }
        }
        if in_highlights && rec.highlights.len() < 4096 {
            if !rec.highlights.is_empty() {
                rec.highlights.push('\n');
            }
            rec.highlights.push_str(line);
        }
    }
    if let Some(prev) = cur
        && hits.len() < 20
    {
        push_record(prev, &mut seen, &mut hits);
    }
    hits
}

/// Search fan-out task for Exa's keyless public MCP endpoint.
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
        if normalized.chars().count() > 400 {
            return Err("not-asked".into());
        }
        if crate::fetch::guards::validate_url_basic(ENDPOINT).is_err() {
            return Err(RULE_DENIED_STATUS.into());
        }
        context.pool.pace(SOURCE, &lane).await;
        let proxy = request_proxy(proxy.as_ref())?;
        let http = http_client(proxy.as_ref())?;
        if !context.requests.take(SOURCE) {
            return Err("budget-skipped".into());
        }
        let intent = context
            .facts
            .map(|f| super::intent::detect(f.raw()))
            .unwrap_or_else(|| super::intent::detect(&normalized));
        let objective = objective_for(&normalized, intent);
        let text = post_search(&http, &endpoint_url(), &normalized, &objective).await?;
        context.pool.observe_rtt(&lane, start.elapsed());
        let hits = parse_exa_text(&text);
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
            requests: 1,
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

    struct Harness {
        fetcher: crate::fetch::client::Fetcher,
        pool: super::super::egress::EgressPool,
        google: super::super::engines::google_wml::ProfileSelector,
        cse: super::super::cse::Client,
        parallel_mcp: super::super::parallel_mcp::Client,
        allowance: super::super::tasks::RequestAllowance,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                fetcher: crate::fetch::client::Fetcher::new(
                    crate::profile::BrowserProfile::host_default(),
                )
                .unwrap(),
                pool: super::super::egress::EgressPool::new(Vec::new()),
                google: super::super::engines::google_wml::ProfileSelector::from_env(),
                cse: super::super::cse::Client::default(),
                parallel_mcp: super::super::parallel_mcp::Client::default(),
                allowance: super::super::tasks::RequestAllowance::new(10),
            }
        }

        fn context(&self) -> EngineContext<'_> {
            EngineContext {
                fetcher: &self.fetcher,
                pool: &self.pool,
                google: &self.google,
                cse: &self.cse,
                parallel_mcp: &self.parallel_mcp,
                lang: None,
                requests: &self.allowance,
                facts: None,
            }
        }
    }

    fn install_test_config() {
        let mut config = crate::config::DonsetchConfig::default();
        config.state.no_disk_state = true;
        config.proxy.from_environment = false;
        crate::config::install(config).unwrap();
    }

    #[test]
    fn exa_mcp_is_enabled_by_default() {
        assert!(crate::config::DonsetchConfig::default().search.exa_mcp);
    }

    #[tokio::test]
    async fn exa_mcp_search_is_stateless_keyless_and_parses_sse_records() {
        install_test_config();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut first_line = String::new();
            reader.read_line(&mut first_line).await.unwrap();
            assert!(first_line.starts_with("POST /mcp "), "{first_line}");
            let mut headers = HashMap::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line.trim().is_empty() {
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
            let req: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(req["method"], "tools/call");
            assert_eq!(req["params"]["name"], "web_search_exa");
            assert_eq!(req["params"]["arguments"]["query"], "tokio select macro");

            let text = "\
Title: select in tokio - Rust
URL: https://docs.rs/tokio/latest/tokio/macro.select.html
Published: 2026-11-12T00:00:00.000Z
Author: Tokio Team
Highlights:
Waits on multiple concurrent branches.
Title: not a split because no URL follows

Title: duplicate
URL: https://docs.rs/tokio/latest/tokio/macro.select.html
Published: N/A
Highlights:
ignored

Title: bad scheme
URL: javascript:alert(1)
Published: N/A
Highlights:
ignored
";
            let rpc = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "content": [{ "type": "text", "text": text }] }
            });
            let sse = format!("event: message\ndata: {rpc}\n\n");
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
            reader.get_mut().write_all(reply.as_bytes()).await.unwrap();
        });

        let harness = Harness::new();
        let (name, res) = TEST_ENDPOINT
            .scope(
                endpoint,
                task(
                    "tokio select macro".into(),
                    "direct".into(),
                    None,
                    harness.context(),
                    Duration::from_secs(5),
                ),
            )
            .await;
        server.await.unwrap();
        assert_eq!(name, SOURCE);
        let answer = res.expect("keyless Exa MCP search succeeds");
        assert_eq!(answer.requests, 1);
        assert_eq!(answer.hits.len(), 1);
        assert_eq!(answer.hits[0].title, "select in tokio - Rust");
        assert!(
            answer.hits[0]
                .snippet
                .contains("Waits on multiple concurrent branches.")
                && answer.hits[0].snippet.contains("Title: not a split"),
            "{}",
            answer.hits[0].snippet
        );
        assert_eq!(
            answer.hits[0].published.as_deref(),
            Some("2026-11-12T00:00:00.000Z")
        );
    }

    #[tokio::test]
    async fn exa_mcp_429_maps_to_rate_limited() {
        install_test_config();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 90\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        });
        let harness = Harness::new();
        let (_, res) = TEST_ENDPOINT
            .scope(
                endpoint,
                task(
                    "rust async".into(),
                    "direct".into(),
                    None,
                    harness.context(),
                    Duration::from_secs(5),
                ),
            )
            .await;
        assert_eq!(res.unwrap_err().0, "rate-limited:90");
        server.abort();
    }

    #[tokio::test]
    async fn exa_mcp_deny_rule_blocks_outbound_requests() {
        let mut config = crate::config::DonsetchConfig::default();
        config.state.no_disk_state = true;
        config.rules.url.insert(
            "mcp.exa.ai".into(),
            crate::rules::UrlRule {
                action: crate::rules::RuleAction::Deny,
                message: Some("blocked".into()),
                ..Default::default()
            },
        );
        crate::config::install(config).unwrap();
        let harness = Harness::new();
        let (_, res) = task(
            "rust async".into(),
            "direct".into(),
            None,
            harness.context(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(res.unwrap_err().0, RULE_DENIED_STATUS);
    }
}
