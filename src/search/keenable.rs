//! Keyless Keenable search through its public REST endpoint.
//!
//! Enabled by default when `lang` is unset (`[search] keenable = false` to
//! disable). Sends one `POST` to `https://api.keenable.ai/v1/search/public`
//! with no key, cookie or account, naming the client in `X-Keenable-Title` as
//! the public endpoint requires. A 429 rests this lane in the ledger while the
//! other engines answer.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, RETRY_AFTER};
use reqwest::{Client as HttpClient, StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::{
    engines::Hit,
    tasks::{EngineContext, EngineResult, LaneAnswer},
};

pub(crate) const SOURCE: &str = "keenable";
const ENDPOINT: &str = "https://api.keenable.ai/v1/search/public";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const TITLE_HEADER: &str = "x-keenable-title";
const TITLE: &str = "DonSeTch";
const MAX_RESULTS: usize = 10;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const DEFAULT_RETRY_SECS: u64 = 60;
const LOW_QUOTA_WATERMARK: u64 = 15;
const LOW_QUOTA_COOLDOWN_SECS: u64 = 60;
const RULE_DENIED_STATUS: &str = "invalid-config: excluded by the operator's local DonSeTch rules";

/// Proactive soft cooldown triggered when `X-RateLimit-Remaining` drops below
/// `LOW_QUOTA_WATERMARK`, shifting load to peer engines before hitting a 429.
static LOW_QUOTA_UNTIL: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(Default::default);

/// True while Keenable is proactively cooling down after a low
/// `X-RateLimit-Remaining` header, so the planner benches it before wave
/// selection instead of spending an active slot on an immediate `rested:` return.
pub(super) fn is_cooling() -> bool {
    let mut guard = LOW_QUOTA_UNTIL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match *guard {
        Some(until) if until > Instant::now() => true,
        Some(_) => {
            *guard = None;
            false
        }
        None => false,
    }
}

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
    let route_key = proxy
        .map(|p| p.connection_key())
        .unwrap_or_else(|| "direct".into());
    if let Some(existing) = HTTP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&route_key)
        .cloned()
    {
        return Ok(existing);
    }
    let mut builder = crate::egress::trust::with_trusted_roots(HttpClient::builder())
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(super::search_connect_timeout())
        .pool_max_idle_per_host(2)
        .no_proxy();
    if let Some(proxy) = proxy {
        if proxy.scheme == crate::egress::proxy::ProxyScheme::Socks5 {
            return Err("invalid-config: keenable requires an HTTP proxy or a direct route".into());
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

#[derive(Deserialize)]
struct SearchPayload {
    #[serde(default)]
    results: Vec<RemoteHit>,
}

#[derive(Deserialize)]
struct RemoteHit {
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
}

async fn post_search(
    http: &HttpClient,
    endpoint: &str,
    query: &str,
) -> Result<SearchPayload, String> {
    if !crate::fetch::layer::record_external_request() {
        return Err("budget-skipped".into());
    }
    let mut resp = http
        .post(endpoint)
        .header(ACCEPT, "application/json")
        .header(TITLE_HEADER, TITLE)
        .json(&json!({ "query": query, "max_results": MAX_RESULTS }))
        .send()
        .await
        .map_err(|e| format!("transport:{}", e.without_url()))?;
    let status = resp.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        let secs = super::tasks::parse_retry_after(
            resp.headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            DEFAULT_RETRY_SECS,
        );
        return Err(format!("rate-limited:{secs}"));
    }
    if !status.is_success() {
        return Err(format!("blocked:{}", status.as_u16()));
    }
    if let Some(remaining) = resp
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        && remaining <= LOW_QUOTA_WATERMARK
    {
        *LOW_QUOTA_UNTIL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Instant::now() + Duration::from_secs(LOW_QUOTA_COOLDOWN_SECS));
    }
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
    serde_json::from_slice(&bytes).map_err(|_| "invalid-response: malformed json".into())
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
        if !seen.insert(super::rank::norm_key(raw_url)) {
            continue;
        }
        let title =
            super::parallel_mcp::evidence_text(item.title.as_deref().unwrap_or_default(), 240);
        let title = if title.is_empty() {
            super::rank::host_of(raw_url)
        } else {
            title
        };
        let snippet = item
            .snippet
            .filter(|s| !s.trim().is_empty())
            .or(item.description)
            .unwrap_or_default();
        let published = item
            .published_at
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 32)
            .map(str::to_string);
        let rank = hits.len();
        hits.push(Hit {
            title,
            url: raw_url.to_string(),
            snippet: super::parallel_mcp::evidence_text(&snippet, 1600),
            rank,
            published,
        });
    }
    hits
}

/// Search fan-out task for Keenable's keyless public endpoint.
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
        if let Some(until) = *LOW_QUOTA_UNTIL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            && let Some(rem) = until.checked_duration_since(Instant::now())
            && !rem.is_zero()
        {
            return Err(format!("rested:{}", rem.as_secs().max(1)));
        }
        context.pool.pace(SOURCE, &lane).await;
        let proxy = request_proxy(proxy.as_ref())?;
        let http = http_client(proxy.as_ref())?;
        if !context.requests.take(SOURCE) {
            return Err("budget-skipped".into());
        }
        let payload = post_search(&http, &endpoint_url(), &normalized).await?;
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
    use serde_json::Value;
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
    fn keenable_is_enabled_by_default() {
        assert!(crate::config::DonsetchConfig::default().search.keenable);
    }

    #[tokio::test]
    async fn keenable_search_is_keyless_and_keeps_only_page_urls() {
        install_test_config();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/search/public", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut first_line = String::new();
            reader.read_line(&mut first_line).await.unwrap();
            assert!(
                first_line.starts_with("POST /v1/search/public "),
                "{first_line}"
            );
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
            assert_eq!(headers.get(TITLE_HEADER).map(String::as_str), Some(TITLE));
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
            let request: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                request,
                json!({"query": "tokio select macro", "max_results": MAX_RESULTS})
            );
            let payload = json!({
                "query": "tokio select macro",
                "results": [
                    {
                        "url": "https://docs.rs/tokio/latest/tokio/macro.select.html",
                        "title": "\n  select in tokio - Rust \n",
                        "snippet": "Waits on multiple concurrent branches.",
                        "description": "",
                        "published_at": "2026-11-12T00:00:00Z"
                    },
                    { "url": "https://docs.rs/tokio/latest/tokio/macro.select.html", "title": "dup" },
                    { "url": "javascript:alert(1)", "title": "bad", "snippet": "ignored" }
                ]
            })
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
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
        let answer = res.expect("keyless search succeeds");
        assert_eq!(answer.requests, 1);
        assert_eq!(answer.hits.len(), 1);
        assert_eq!(answer.hits[0].title, "select in tokio - Rust");
        assert_eq!(
            answer.hits[0].snippet,
            "Waits on multiple concurrent branches."
        );
        assert_eq!(
            answer.hits[0].published.as_deref(),
            Some("2026-11-12T00:00:00Z")
        );
    }

    /// A TLS-inspecting HTTP proxy on loopback: answers one `CONNECT`, then
    /// terminates the tunnel with `cert` (self-signed for `api.keenable.ai`)
    /// and answers one search. The handle yields whether the client finished
    /// the handshake.
    async fn inspecting_proxy(
        cert: &rcgen::CertifiedKey<rcgen::KeyPair>,
    ) -> (crate::egress::proxy::Proxy, tokio::task::JoinHandle<bool>) {
        let mut tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls_pki_types::CertificateDer::from(
                    cert.cert.der().to_vec(),
                )],
                rustls_pki_types::PrivateKeyDer::Pkcs8(rustls_pki_types::PrivatePkcs8KeyDer::from(
                    cert.signing_key.serialize_der(),
                )),
            )
            .unwrap();
        tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(tls_config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = crate::egress::proxy::Proxy::parse(&format!(
            "http://{}",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            assert!(head.starts_with(b"CONNECT api.keenable.ai:443 "));
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let Ok(mut tls) = acceptor.accept(socket).await else {
                return false;
            };
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(tls.read_u8().await.unwrap());
            }
            let head = String::from_utf8_lossy(&request).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            tls.read_exact(&mut body).await.unwrap();
            let payload = json!({ "results": [{
                "url": "https://docs.rs/tokio/latest/tokio/macro.select.html",
                "title": "select in tokio - Rust",
                "snippet": "Waits on multiple concurrent branches."
            }]})
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            tls.write_all(reply.as_bytes()).await.unwrap();
            let _ = tls.shutdown().await;
            true
        });
        (proxy, server)
    }

    async fn search_through(proxy: crate::egress::proxy::Proxy) -> EngineResult {
        let harness = Harness::new();
        let (_, res) = task(
            "tokio select macro".into(),
            "direct".into(),
            Some(proxy),
            harness.context(),
            Duration::from_secs(5),
        )
        .await;
        res
    }

    // #467: behind a TLS-inspecting proxy the owner's `[tls] cert_file` root
    // must reach this client, as it reaches fetch. Installs the config, so
    // it relies on nextest's process per test.
    #[tokio::test]
    async fn keenable_trusts_the_configured_ca_behind_an_inspecting_proxy() {
        let cert = rcgen::generate_simple_self_signed(vec!["api.keenable.ai".into()]).unwrap();
        let bundle = crate::paths::cache_dir().join("inspecting-proxy-ca.pem");
        std::fs::create_dir_all(bundle.parent().unwrap()).unwrap();
        std::fs::write(&bundle, cert.cert.pem()).unwrap();
        let mut config = crate::config::DonsetchConfig::default();
        config.state.no_disk_state = true;
        config.proxy.from_environment = false;
        config.tls.cert_file = bundle.to_string_lossy().into_owned();
        crate::config::install(config).unwrap();
        let (proxy, server) = inspecting_proxy(&cert).await;
        let answer = search_through(proxy)
            .await
            .expect("the configured root is trusted");
        assert!(server.await.unwrap());
        assert_eq!(answer.hits.len(), 1);
    }

    // #467, the reporter's setup: the root arrives through SSL_CERT_FILE.
    // Sets a process-wide variable, so it relies on nextest's process per test.
    #[tokio::test]
    async fn keenable_trusts_ssl_cert_file_behind_an_inspecting_proxy() {
        install_test_config();
        let cert = rcgen::generate_simple_self_signed(vec!["api.keenable.ai".into()]).unwrap();
        let bundle = crate::paths::cache_dir().join("inspecting-proxy-env-ca.pem");
        std::fs::create_dir_all(bundle.parent().unwrap()).unwrap();
        std::fs::write(&bundle, cert.cert.pem()).unwrap();
        unsafe { std::env::set_var("SSL_CERT_FILE", &bundle) };
        let (proxy, server) = inspecting_proxy(&cert).await;
        let answer = search_through(proxy)
            .await
            .expect("the SSL_CERT_FILE root is trusted");
        assert!(server.await.unwrap());
        assert_eq!(answer.hits.len(), 1);
    }

    // The extra roots add trust, they do not switch verification off: an
    // inspecting proxy whose root nobody configured is still refused.
    #[tokio::test]
    async fn keenable_refuses_an_unconfigured_inspecting_proxy() {
        install_test_config();
        let cert = rcgen::generate_simple_self_signed(vec!["api.keenable.ai".into()]).unwrap();
        let (proxy, server) = inspecting_proxy(&cert).await;
        let (status, _, _) = search_through(proxy).await.unwrap_err();
        assert!(status.starts_with("transport:"), "{status}");
        assert!(
            !server.await.unwrap(),
            "no request crosses an untrusted tunnel"
        );
    }

    #[tokio::test]
    async fn keenable_429_maps_to_rate_limited() {
        install_test_config();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/search/public", listener.local_addr().unwrap());
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
        assert_eq!(res.unwrap_err().0, "rate-limited:120");
        server.abort();
    }

    #[tokio::test]
    async fn keenable_deny_rule_blocks_outbound_requests() {
        let mut config = crate::config::DonsetchConfig::default();
        config.state.no_disk_state = true;
        config.rules.url.insert(
            "api.keenable.ai".into(),
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

    #[tokio::test]
    async fn keenable_accounts_for_requests_and_honors_request_limit() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        install_test_config();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/search/public", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let body =
                r#"{"results":[{"url":"https://example.com/a","title":"A","snippet":"ok"}]}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        });

        let total = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(AtomicUsize::new(0));
        let harness = Harness::new();
        let (_, res) = crate::fetch::layer::REQUEST_COUNT
            .scope(
                Arc::clone(&total),
                crate::fetch::layer::REQUEST_LIMIT.scope(
                    2,
                    crate::fetch::layer::REQUEST_SOURCE_COUNT.scope(
                        Arc::clone(&source),
                        TEST_ENDPOINT.scope(
                            endpoint.clone(),
                            task(
                                "rust async".into(),
                                "direct".into(),
                                None,
                                harness.context(),
                                Duration::from_secs(5),
                            ),
                        ),
                    ),
                ),
            )
            .await;
        server.await.unwrap();
        assert!(res.is_ok());
        assert_eq!(total.load(Ordering::Relaxed), 1);
        assert_eq!(source.load(Ordering::Relaxed), 1);

        // Negative: when REQUEST_LIMIT is already reached, no request goes out.
        let capped = Arc::new(AtomicUsize::new(1));
        let capped_source = Arc::new(AtomicUsize::new(0));
        let (_, blocked) = crate::fetch::layer::REQUEST_COUNT
            .scope(
                Arc::clone(&capped),
                crate::fetch::layer::REQUEST_LIMIT.scope(
                    1,
                    crate::fetch::layer::REQUEST_SOURCE_COUNT.scope(
                        Arc::clone(&capped_source),
                        TEST_ENDPOINT.scope(
                            endpoint,
                            task(
                                "rust async".into(),
                                "direct".into(),
                                None,
                                harness.context(),
                                Duration::from_secs(5),
                            ),
                        ),
                    ),
                ),
            )
            .await;
        assert_eq!(blocked.unwrap_err().0, "budget-skipped");
        assert_eq!(capped_source.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn low_quota_cooling_reports_active_and_clears_when_expired() {
        assert!(!is_cooling());
        *LOW_QUOTA_UNTIL.lock().unwrap() = Some(Instant::now() + Duration::from_secs(30));
        assert!(is_cooling());
        *LOW_QUOTA_UNTIL.lock().unwrap() = Some(Instant::now() - Duration::from_secs(1));
        assert!(!is_cooling());
        assert!(LOW_QUOTA_UNTIL.lock().unwrap().is_none());
    }

    #[test]
    fn http_client_cache_keys_distinguish_scheme_and_credentials_on_same_host_port() {
        let http_px = crate::egress::proxy::Proxy::parse("http://user1:pass1@127.0.0.1:18080")
            .expect("http proxy");
        let socks_px = crate::egress::proxy::Proxy::parse("socks5://user1:pass1@127.0.0.1:18080")
            .expect("socks5 proxy");
        assert_eq!(http_px.id(), socks_px.id());
        assert_ne!(http_px.connection_key(), socks_px.connection_key());

        // Caching the HTTP proxy must not allow a SOCKS5 proxy on the same
        // host:port to bypass SOCKS5 rejection via an id()-only cache hit.
        assert!(http_client(Some(&http_px)).is_ok());
        let err = http_client(Some(&socks_px)).unwrap_err();
        assert!(err.contains("HTTP proxy"), "{err}");
    }
}
