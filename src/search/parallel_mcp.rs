//! Anonymous Parallel search through its Streamable HTTP MCP endpoint.
//!
//! Each search owns a short-lived MCP session. The HTTP pool is shared;
//! session headers and request IDs stay local to one exchange.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, pin_mut, stream};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sse_stream::{Error as SseError, SseByteStream};

use super::{EngineReport, SearchOutcome, intent::Intent, rank::Merged};

pub(crate) const SOURCE: &str = "parallel-mcp";
const ENDPOINT: &str = "https://mcp.parallel.ai/mcp";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const VERSION: &str = "2025-06-18";
const PROTOCOL_VERSION: &str = "mcp-protocol-version";
const SESSION_ID: &str = "mcp-session-id";
const ACCEPT_TYPES: &str = "application/json, text/event-stream";
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const TOOL: &str = "web_search";

#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEST_ENDPOINT: String;
}

#[derive(Debug)]
pub(crate) enum SearchError {
    Input(&'static str),
    Denied,
    Protocol(String),
}

impl SearchError {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Input(_) | Self::Denied => "permanent",
            Self::Protocol(_) => "transient",
        }
    }
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input(message) => f.write_str(message),
            Self::Denied => f.write_str("endpoint blocked by DonSeTch's URL policy"),
            Self::Protocol(message) => f.write_str(message),
        }
    }
}

type Result<T> = std::result::Result<T, SearchError>;

fn protocol(message: impl Into<String>) -> SearchError {
    SearchError::Protocol(message.into())
}

fn transport(error: reqwest::Error) -> SearchError {
    protocol(format!("request failed ({})", error.without_url()))
}

fn http_client() -> Result<&'static (Client, &'static str)> {
    static CLIENT: OnceLock<Result<(Client, &'static str)>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let mut builder = Client::builder()
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(4))
                .no_proxy();
            let proxy = crate::egress::proxy::from_env_for(ENDPOINT)
                .map_err(|_| protocol("invalid proxy configuration"))?;
            let egress = if let Some(proxy) = proxy {
                if proxy.scheme == crate::egress::proxy::ProxyScheme::Socks5 {
                    return Err(protocol(
                        "Parallel MCP requires an HTTP proxy or a direct route",
                    ));
                }
                builder = builder.proxy(
                    reqwest::Proxy::all(proxy.wire_url())
                        .map_err(|_| protocol("unsupported proxy configuration"))?,
                );
                "proxy"
            } else {
                "direct"
            };
            Ok((builder.build().map_err(transport)?, egress))
        })
        .as_ref()
        .map_err(|error| protocol(error.to_string()))
}

/// Run one search against the explicitly selected source. Tool content is
/// converted to evidence rows; remote diagnostics never become instructions.
pub(crate) async fn search(
    query: &str,
    max: usize,
    intent: Option<Intent>,
) -> Result<SearchOutcome> {
    let query = super::query::normalize(query);
    let query = query.as_str();
    if query.chars().count() > 200 {
        return Err(SearchError::Input("shorten the query to 200 characters"));
    }
    crate::fetch::guards::validate_url_basic(ENDPOINT).map_err(|_| SearchError::Denied)?;
    let intent = intent.unwrap_or_else(|| super::intent::detect(query));
    let objective = match intent {
        Intent::News => format!("Find recent news about: {query}"),
        Intent::Paper => format!("Find academic papers about: {query}"),
        _ => query.to_string(),
    };
    let started = Instant::now();
    #[cfg(test)]
    let endpoint = TEST_ENDPOINT.try_with(String::clone).ok();
    #[cfg(test)]
    let endpoint = endpoint.as_deref().unwrap_or(ENDPOINT);
    #[cfg(not(test))]
    let endpoint = ENDPOINT;

    let (client, egress) = http_client()?;
    let mut session = Session {
        client,
        endpoint,
        id: None,
        protocol_version: VERSION.to_string(),
        next_request: 1,
    };
    let response =
        tokio::time::timeout(Duration::from_secs(12), session.search(&objective, query)).await;
    session.close().await;
    let payload = response.map_err(|_| protocol("request timed out"))??;
    payload.outcome(query, max, intent, started.elapsed(), egress)
}

struct Session<'a> {
    client: &'a Client,
    endpoint: &'a str,
    id: Option<HeaderValue>,
    protocol_version: String,
    next_request: u64,
}

impl Session<'_> {
    fn headers(&self, mut request: RequestBuilder) -> RequestBuilder {
        request = request
            .header(ACCEPT, ACCEPT_TYPES)
            .header(PROTOCOL_VERSION, &self.protocol_version);
        if let Some(id) = &self.id {
            request = request.header(SESSION_ID, id);
        }
        request
    }

    async fn post(&self, body: Value) -> Result<Response> {
        let response = self
            .headers(self.client.post(self.endpoint))
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(protocol(format!("HTTP {}", status.as_u16())));
        }
        Ok(response)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let request = self.next_request;
        self.next_request += 1;
        let response = self
            .post(json!({"jsonrpc": "2.0", "id": request, "method": method, "params": params}))
            .await?;
        if method == "initialize" {
            self.id = response.headers().get(SESSION_ID).cloned();
        }
        read_response(response, request).await
    }

    async fn search(&mut self, objective: &str, query: &str) -> Result<SearchPayload> {
        let initialized = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        let version = initialized["protocolVersion"]
            .as_str()
            .filter(|v| matches!(*v, VERSION | "2025-03-26"))
            .ok_or_else(|| protocol("unsupported negotiated MCP version"))?;
        self.protocol_version = version.to_string();
        let acknowledged = self
            .post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        if !matches!(
            acknowledged.status(),
            StatusCode::ACCEPTED | StatusCode::NO_CONTENT
        ) {
            return Err(protocol("initialization notification was not acknowledged"));
        }
        self.discover_search().await?;
        let reply = self
            .request(
                "tools/call",
                json!({
                    "name": TOOL,
                    "arguments": {"objective": objective, "search_queries": [query]}
                }),
            )
            .await?;
        parse_search_reply(reply)
    }

    async fn discover_search(&mut self) -> Result<()> {
        let mut params = json!({});
        // Bound discovery even if a server keeps returning a cursor.
        for _ in 0..8 {
            let page = self.request("tools/list", params).await?;
            let tools = page["tools"]
                .as_array()
                .ok_or_else(|| protocol("invalid MCP tool list"))?;
            if tools.iter().any(|tool| tool["name"] == TOOL) {
                return Ok(());
            }
            let Some(cursor) = page["nextCursor"].as_str() else {
                return Err(protocol("web_search is unavailable"));
            };
            params = json!({"cursor": cursor});
        }
        Err(protocol("MCP tool list exceeded the page limit"))
    }

    async fn close(&mut self) {
        let budget = super::stage_budget(Duration::from_millis(250), Duration::from_millis(50));
        if !budget.is_zero()
            && let Some(request) = self.take_close_request()
        {
            // Session DELETE is optional on the server; its failure cannot
            // change either the search result or the error that preceded it.
            let _ = request.timeout(budget).send().await;
        }
    }

    fn take_close_request(&mut self) -> Option<RequestBuilder> {
        self.id.as_ref()?;
        let request = self.headers(self.client.delete(self.endpoint));
        self.id = None;
        Some(request)
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        // If the host cancels the search, its future is dropped before it can
        // await close(). Cleanup may continue briefly without the query.
        if let Some(request) = self.take_close_request()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = request.timeout(Duration::from_millis(250)).send().await;
            });
        }
    }
}

async fn read_response(response: Response, request: u64) -> Result<Value> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split(';').next())
        .unwrap_or_default()
        .trim();
    let events = content_type.eq_ignore_ascii_case("text/event-stream");
    if !events && !content_type.eq_ignore_ascii_case("application/json") {
        return Err(protocol("unexpected MCP response content type"));
    }
    // Apply the same byte budget to streamed and non-streamed responses,
    // before either decoder buffers the data.
    let chunks = stream::try_unfold((response, 0), |(mut response, total)| async move {
        let Some(chunk) = response.chunk().await.map_err(transport_io)? else {
            return Ok(None);
        };
        let total = total + chunk.len();
        if total > MAX_RESPONSE_BYTES {
            return Err(std::io::Error::other("MCP response exceeded 256 KiB"));
        }
        Ok(Some((chunk, (response, total))))
    });
    if events {
        let events = SseByteStream::new(chunks);
        pin_mut!(events);
        while let Some(event) = events.next().await {
            let event = event.map_err(|error| match error {
                SseError::Body(error) => protocol(error.to_string()),
                _ => protocol("invalid MCP event stream"),
            })?;
            if let Some(data) = event.data {
                let body = rpc_json(data.as_bytes())?;
                if let Some(result) = rpc_result(body, request)? {
                    return Ok(result);
                }
            }
        }
    } else {
        pin_mut!(chunks);
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            bytes.extend_from_slice(&chunk.map_err(|error| protocol(error.to_string()))?);
        }
        if let Some(result) = rpc_result(rpc_json(&bytes)?, request)? {
            return Ok(result);
        }
    }
    Err(protocol("missing matching MCP response"))
}

fn transport_io(error: reqwest::Error) -> std::io::Error {
    std::io::Error::other(transport(error).to_string())
}

fn rpc_json(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| protocol("invalid MCP JSON response"))
}

fn rpc_result(body: Value, request: u64) -> Result<Option<Value>> {
    let matching_id = body["id"].as_u64() == Some(request);
    let error = body.get("error");
    let uncorrelated_error = body.get("id").is_some_and(Value::is_null) && error.is_some();
    if !matching_id && !uncorrelated_error {
        return Ok(None);
    }
    if body["jsonrpc"] != "2.0" {
        return Err(protocol("invalid MCP response version"));
    }
    if let Some(error) = error {
        let code = error["code"]
            .as_i64()
            .ok_or_else(|| protocol("invalid MCP RPC error"))?;
        return Err(protocol(format!("MCP RPC error {code}")));
    }
    body.get("result")
        .cloned()
        .map(Some)
        .ok_or_else(|| protocol("missing MCP result"))
}

#[derive(Deserialize)]
struct SearchPayload {
    results: Vec<RemoteHit>,
    #[serde(default)]
    warnings: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct RemoteHit {
    url: String,
    title: Option<String>,
    excerpts: Vec<String>,
}

fn parse_search_reply(mut reply: Value) -> Result<SearchPayload> {
    match reply.get("isError") {
        Some(Value::Bool(true)) => return Err(protocol("web_search reported a tool error")),
        Some(Value::Bool(false)) | None => {}
        Some(_) => return Err(protocol("invalid MCP tool status")),
    }
    if let Some(payload) = reply.get_mut("structuredContent").map(Value::take) {
        return serde_json::from_value(payload).map_err(|_| protocol("invalid web_search payload"));
    }
    let text = reply["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|block| block["type"] == "text"))
        .and_then(|block| block["text"].as_str())
        .ok_or_else(|| protocol("missing web_search payload"))?;
    serde_json::from_str(text).map_err(|_| protocol("invalid web_search payload"))
}

impl SearchPayload {
    fn outcome(
        self,
        query: &str,
        max: usize,
        intent: Intent,
        elapsed: Duration,
        egress: &str,
    ) -> Result<SearchOutcome> {
        let empty = self.results.is_empty();
        let mut seen = HashSet::new();
        let mut results = self
            .results
            .into_iter()
            .filter_map(|hit| {
                let url = url::Url::parse(&hit.url).ok()?;
                if !matches!(url.scheme(), "http" | "https")
                    || !url.has_host()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || hit.url.len() > 4096
                    || hit.url.chars().any(|c| c.is_whitespace() || c.is_control())
                    || !seen.insert(super::rank::norm_key(&hit.url))
                {
                    return None;
                }
                let title = evidence_text(hit.title.as_deref().unwrap_or_default(), 240);
                Some(Merged {
                    title: if title.is_empty() {
                        super::rank::host_of(&hit.url)
                    } else {
                        title
                    },
                    url: hit.url,
                    snippet: evidence_text(&hit.excerpts.join(" "), 4000),
                    score: 0.0,
                    sources: Vec::new(),
                    published: None,
                    families: 0,
                    from_page: false,
                    tier: 3,
                })
            })
            .collect::<Vec<_>>();
        if !empty && results.is_empty() {
            return Err(protocol("web_search returned no usable URLs"));
        }
        super::site_filter(query, &mut results);
        super::query::intitle_filter(query, &mut results);
        super::query::filetype_filter(query, &mut results);
        results.truncate(max.clamp(1, 12));
        for (index, result) in results.iter_mut().enumerate() {
            result.sources.push((SOURCE.into(), index + 1));
        }
        let status = if results.is_empty() {
            "no-results"
        } else if self.warnings.is_some_and(|warnings| !warnings.is_empty()) {
            "thin"
        } else {
            "ok"
        };
        let report = vec![EngineReport {
            engine: SOURCE.into(),
            cache_age_ms: None,
            profile: None,
            status: status.into(),
            hits: results.len(),
            ms: elapsed.as_millis() as u64,
            egress: egress.into(),
            requests: 0,
        }];
        let quality = super::quality::assess(query, &results, &report, &Default::default());
        Ok(SearchOutcome {
            results,
            intent,
            report,
            cached: false,
            quality,
            cache_age: None,
            elapsed,
            provider: Some(SOURCE.into()),
            reranked: false,
            instant: None,
            requests: None,
            stage_ms: Vec::new(),
        })
    }
}

fn evidence_text(text: &str, max: usize) -> String {
    text.split_whitespace()
        .flat_map(|word| word.chars().chain(std::iter::once(' ')))
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::server::{Daemon, call_tool};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    #[derive(Clone, Copy)]
    enum ToolReply {
        Stream,
        Empty,
        RateLimited,
        RpcError,
        ToolError,
        Malformed,
        Oversized,
        Redirect,
    }

    async fn fixture(replies: &[ToolReply]) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let replies = replies.to_vec();
        let server = tokio::spawn(async move {
            for (index, reply) in replies.into_iter().enumerate() {
                let session = format!("fixture-session-{index}");
                for method in [
                    "initialize",
                    "notifications/initialized",
                    "tools/list",
                    "tools/list",
                    "tools/call",
                    "DELETE",
                ] {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut socket = BufReader::new(socket);
                    let mut request = String::new();
                    socket.read_line(&mut request).await.unwrap();
                    let verb = if method == "DELETE" { "DELETE" } else { "POST" };
                    assert_eq!(request.trim_end(), format!("{verb} /mcp HTTP/1.1"));
                    let mut headers = std::collections::HashMap::new();
                    loop {
                        let mut line = String::new();
                        socket.read_line(&mut line).await.unwrap();
                        if line.trim().is_empty() {
                            break;
                        }
                        let (name, value) = line.split_once(':').unwrap();
                        headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
                    }
                    assert_eq!(headers["user-agent"], USER_AGENT);
                    assert_eq!(headers["accept"], ACCEPT_TYPES);
                    assert!(!headers.contains_key("authorization"));
                    assert!(!headers.contains_key("x-api-key"));
                    assert!(!headers.contains_key("cookie"));
                    assert_eq!(
                        headers.get(SESSION_ID),
                        (method != "initialize").then_some(&session)
                    );
                    let len = headers
                        .get("content-length")
                        .map_or(0, |v| v.parse().unwrap());
                    let mut body = vec![0; len];
                    socket.read_exact(&mut body).await.unwrap();
                    let request: Value = if verb == "POST" {
                        let request: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(request["method"], method);
                        request
                    } else {
                        Value::Null
                    };
                    let id = &request["id"];
                    let rpc =
                        |result| json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
                    let mut extra = String::new();
                    let mut content_type = "application/json";
                    let (status, text) = match method {
                        "initialize" => {
                            assert_eq!(headers[PROTOCOL_VERSION], VERSION);
                            extra = format!("{SESSION_ID}: {session}\r\n");
                            ("200 OK", rpc(json!({"protocolVersion":"2025-03-26"})))
                        }
                        "notifications/initialized" => {
                            assert!(id.is_null());
                            ("202 Accepted", String::new())
                        }
                        "tools/list" if request["params"]["cursor"].is_null() => {
                            ("200 OK", rpc(json!({"tools": [], "nextCursor": "page2"})))
                        }
                        "tools/list" => {
                            assert_eq!(request["params"]["cursor"], "page2");
                            ("200 OK", rpc(json!({"tools": [{"name": TOOL}]})))
                        }
                        "tools/call" => {
                            assert_eq!(request["params"]["name"], TOOL);
                            assert_eq!(
                                request["params"]["arguments"],
                                json!({
                                    "objective": "Find academic papers about: site:example.org Rust paper",
                                    "search_queries": ["site:example.org Rust paper"]
                                })
                            );
                            match reply {
                                ToolReply::Stream => {
                                    content_type = "text/event-stream";
                                    let results = json!({
                                        "results": [
                                            {"url": "https://badexample.org/a", "title": "Outside site", "excerpts": ["Filtered."]},
                                            {"url": "https://example.org/a?ref=original#section", "title": "Rust\npaper", "excerpts": ["Evidence from this Rust paper."]},
                                            {"url": "javascript:doSomething()", "title": "Invalid URL", "excerpts": ["Ignored."]},
                                            {"url": "https://example.org/b", "title": "Rank two", "excerpts": ["Limited by max_results."]}
                                        ]
                                    });
                                    let result = json!({"isError": false, "structuredContent": results, "content": [{"type":"text","text":"IGNORED_TEXT"}]});
                                    // A notification and an unrelated response arrive before the
                                    // correct response, which spans two lines in one SSE event.
                                    (
                                        "200 OK",
                                        format!(
                                            ": comment\r\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\r\n\r\ndata: {{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{{}}}}\r\n\r\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\r\ndata: \"result\":{result}}}\r\n\r\n"
                                        ),
                                    )
                                }
                                ToolReply::Empty => (
                                    "200 OK",
                                    rpc(json!({"content": [{"type": "text", "text": "{\"results\":[]}"}]})),
                                ),
                                ToolReply::RateLimited => ("429 Too Many Requests", "UNTRUSTED".into()),
                                ToolReply::RpcError => (
                                    "200 OK",
                                    json!({"jsonrpc": "2.0", "id":null, "error":{"code":-32000,"message":"UNTRUSTED"}}).to_string(),
                                ),
                                ToolReply::ToolError => (
                                    "200 OK",
                                    rpc(json!({"isError": true, "content": [{"type":"text","text":"UNTRUSTED"}]})),
                                ),
                                ToolReply::Malformed => ("200 OK", rpc(json!({"structuredContent": {}}))),
                                ToolReply::Oversized => ("200 OK", "x".repeat(MAX_RESPONSE_BYTES + 1)),
                                ToolReply::Redirect => {
                                    extra = "Location: /other-endpoint\r\n".to_string();
                                    ("307 Temporary Redirect", String::new())
                                }
                            }
                        }
                        // Failed cleanup must leave the search answer intact.
                        "DELETE" => ("405 Method Not Allowed", String::new()),
                        _ => unreachable!(),
                    };
                    if method != "initialize" {
                        assert_eq!(headers[PROTOCOL_VERSION], "2025-03-26");
                    }
                    let socket = socket.get_mut();
                    socket.write_all(format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
                        text.len()
                    ).as_bytes()).await.unwrap();
                    // A client enforcing the limit may stop reading the oversized body.
                    let written = socket.write_all(text.as_bytes()).await;
                    if !matches!(reply, ToolReply::Oversized) {
                        written.unwrap();
                    }
                }
            }
        });
        (endpoint, server)
    }

    #[tokio::test]
    async fn parallel_mcp_wire_and_results() {
        let mut config = fixture_config();
        config.search.parallel_mcp = true;
        crate::config::install(config).unwrap();
        let mut daemon = Daemon::new().await.unwrap();
        daemon.search_handles = false;
        let daemon = Arc::new(daemon);
        let (endpoint, server) =
            fixture(&[ToolReply::Stream, ToolReply::Stream, ToolReply::Empty]).await;

        let params = json!({
            "name": TOOL,
            "arguments": {"query": "site:example.org Rust paper", "intent": "paper", "max_results": 1}
        });
        TEST_ENDPOINT
            .scope(endpoint, async {
                for _ in 0..2 {
                    let result = call_tool(&daemon, &params).await.unwrap();
                    let state = &result["structuredContent"];
                    assert_eq!(state["provider"], SOURCE);
                    assert_eq!(state["results"].as_array().unwrap().len(), 1);
                    assert_eq!(
                        state["results"][0]["url"],
                        "https://example.org/a?ref=original#section"
                    );
                    let text = result["content"][0]["text"].as_str().unwrap();
                    assert!(
                        text.contains("Rust paper")
                            && text.contains("Evidence from this Rust paper.")
                    );
                    assert!(!text.contains("IGNORED_TEXT"));
                }
                let empty = call_tool(&daemon, &params).await.unwrap();
                assert_eq!(empty["structuredContent"]["ok"], true);
                assert_eq!(empty["structuredContent"]["results"], json!([]));
            })
            .await;
        server.await.unwrap();
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn parallel_mcp_failure_layers() {
        crate::config::install(fixture_config()).unwrap();
        let cases = [
            (ToolReply::RateLimited, "HTTP 429"),
            (ToolReply::RpcError, "MCP RPC error -32000"),
            (ToolReply::ToolError, "web_search reported a tool error"),
            (ToolReply::Malformed, "invalid web_search payload"),
            (ToolReply::Oversized, "MCP response exceeded 256 KiB"),
            (ToolReply::Redirect, "HTTP 307"),
        ];
        let (endpoint, server) = fixture(&cases.map(|(reply, _)| reply)).await;
        TEST_ENDPOINT
            .scope(endpoint, async {
                for (_, expected) in cases {
                    let error = search("site:example.org Rust paper", 1, Some(Intent::Paper))
                        .await
                        .unwrap_err();
                    assert_eq!(error.to_string(), expected);
                    assert_eq!(error.kind(), "transient");
                }
                let error = search(&"ä".repeat(201), 1, None).await.unwrap_err();
                assert_eq!(error.kind(), "permanent");
            })
            .await;
        server.await.unwrap();
    }

    fn fixture_config() -> crate::config::DonsetchConfig {
        let mut config = crate::config::DonsetchConfig::default();
        config.browser.chromium_path = crate::paths::cache_dir()
            .join("no-browser")
            .display()
            .to_string();
        config.proxy.from_environment = false;
        config
    }

    #[tokio::test]
    async fn parallel_mcp_preserves_default_search_with_existing_config() {
        let config = fixture_config();
        assert!(!config.search.parallel_mcp);
        local_search_uses_its_cached_result(config, None).await;
    }

    #[tokio::test]
    async fn parallel_mcp_preserves_explicit_language_and_url_policy() {
        let mut config = fixture_config();
        config.search.parallel_mcp = true;
        config.rules = serde_json::from_value(json!({
            "url": {"mcp.parallel.ai": {"action": "deny", "message": "Blocked in fixture"}}
        }))
        .unwrap();
        local_search_uses_its_cached_result(config, Some("en")).await;
        assert!(matches!(
            search("rust toolchain", 1, None).await,
            Err(SearchError::Denied)
        ));
    }

    async fn local_search_uses_its_cached_result(
        config: crate::config::DonsetchConfig,
        lang: Option<&str>,
    ) {
        crate::config::install(config).unwrap();
        let query = "rust toolchain documentation";
        let url = "https://example.org/local-search";
        let outcome = SearchPayload {
            results: vec![RemoteHit {
                url: url.into(),
                title: Some("Rust toolchain documentation".into()),
                excerpts: vec!["Instructions for installing and configuring the Rust toolchain on your system.".into()],
            }],
            warnings: None,
        }.outcome(query, 1, Intent::Web, Duration::ZERO, "direct").unwrap();
        super::super::persist::save_cache_disk(
            &[(
                format!("{query}|{}|lang=en", Intent::Web.code()),
                (
                    Instant::now(),
                    outcome.results,
                    1,
                    vec![],
                    super::super::quality::Assessment {
                        confidence: super::super::quality::Confidence::High,
                        ..Default::default()
                    },
                ),
            )]
            .into(),
        );

        let daemon = Arc::new(Daemon::new().await.unwrap());
        let mut args =
            json!({"query": query, "intent": "web", "max_results": 1, "deadline_ms": 500});
        if let Some(lang) = lang {
            args["lang"] = json!(lang);
        }
        let result = TEST_ENDPOINT
            .scope("http://127.0.0.1:1/mcp".into(), async {
                call_tool(&daemon, &json!({"name": TOOL, "arguments": args}))
                    .await
                    .unwrap()
            })
            .await;
        let result = &result["structuredContent"];
        assert_eq!(result["provider"], "keyless");
        assert_eq!(result["results"][0]["url"], url);
        daemon.shutdown().await;
    }
}
