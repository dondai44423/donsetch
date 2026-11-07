//! The fan-out task runners: one future per engine/vertical/lane,
//! all returning `(engine-id, EngineResult)` so the merge loop can
//! do uniform bookkeeping (trust, quarantine, pool health, reports)
//! over every lane shape.

use std::time::Instant;

use super::ENGINE_TIMEOUT;
use super::egress::EgressPool;
use super::engines;
use super::verticals;
use crate::detect::walls::Verdict;
use crate::error::FetchError;
use crate::fetch::client::Fetcher;

/// Engine fan-out outcome.
///
/// Ok: (hits, latency_ms, egress_id, was_engine, instant).
/// `was_engine` is true for web engines (pool bookkeeping applies)
/// and false for verticals. `instant` is a byte-derived SERP
/// answer, if any; it is never merged into the organic hit list.
/// Err: (status, egress_id, was_engine).
pub(super) type EngineResult = Result<
    (
        Vec<engines::Hit>,
        u64,
        String,
        bool,
        Option<super::instant::InstantAnswer>,
    ),
    (String, String, bool),
>;

pub(super) type TaskFut<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = (String, EngineResult)> + Send + 'a>>;

#[derive(Clone, Copy)]
pub(super) struct EngineContext<'a> {
    pub fetcher: &'a Fetcher,
    pub pool: &'a EgressPool,
    pub google: &'a engines::google_wml::ProfileSelector,
}

pub(super) async fn engine_task(
    engine: String,
    query: String,
    egress_id: String,
    proxy: Option<crate::egress::proxy::Proxy>,
    context: EngineContext<'_>,
) -> (String, EngineResult) {
    engine_task_with_budget(
        engine,
        query,
        egress_id,
        proxy,
        context,
        ENGINE_TIMEOUT,
        None,
    )
    .await
}

/// One admission and HTTP attempt under a single deadline, including pacing.
pub(super) async fn engine_task_with_budget(
    engine: String,
    query: String,
    egress_id: String,
    proxy: Option<crate::egress::proxy::Proxy>,
    context: EngineContext<'_>,
    budget: std::time::Duration,
    previous: Option<(&str, &str)>,
) -> (String, EngineResult) {
    let EngineContext {
        fetcher,
        pool,
        google,
    } = context;
    let mut label = engine.clone();
    let deadline = tokio::time::Instant::now() + budget;
    let started = Instant::now();
    if tokio::time::timeout_at(deadline, pool.pace(&engine, &egress_id))
        .await
        .is_err()
    {
        return (label, Err(("pacing-timeout".into(), egress_id, true)));
    }
    let lease = if engine == "google" {
        match google.select(&egress_id, previous) {
            Ok(lease) => {
                label = lease.label();
                Some(lease)
            }
            Err(status) => return (label, Err((status.into(), egress_id, true))),
        }
    } else {
        None
    };
    let google_ua = lease.as_ref().map(|lease| lease.user_agent());
    let Some(url) = engines::serp_url(&engine, &query) else {
        return (label, Err(("no-url".into(), egress_id, true)));
    };
    // The search lane hands over a picked egress lane, and with no pool
    // configured (the default) that is None, which used to mean a DIRECT
    // dial even with HTTP_PROXY/HTTPS_PROXY set: search engines were
    // unreachable on any host whose only route out is a forward proxy.
    // Same convention as every other request path.
    let env_proxy = if proxy.is_none() {
        match crate::egress::proxy::from_env_for(&url) {
            Ok(proxy) => proxy,
            Err(error) => {
                return (
                    label,
                    Err((format!("invalid-config: {error}"), egress_id, true)),
                );
            }
        }
    } else {
        None
    };
    let lane = proxy.as_ref().or(env_proxy.as_ref());
    let out = match tokio::time::timeout_at(deadline, async {
        if let Some(ua) = google_ua {
            fetcher.fetch_once_via_user_agent(&url, lane, ua).await
        } else {
            fetcher.fetch_once_via(&url, &[], lane, false, None).await
        }
    })
    .await
    {
        Err(_) => return (label, Err(("timeout".into(), egress_id, true))),
        Ok(Err(e)) => {
            let status = engine_fetch_status(&e);
            return (label, Err((status.into(), egress_id, true)));
        }
        Ok(Ok(o)) => o,
    };
    let ms = started.elapsed().as_millis() as u64;
    pool.observe_rtt(&egress_id, started.elapsed());
    // The deadline covers the FETCH only; the decode + DOM parse
    // below run synchronously after it. A hostile (or
    // misconfigured-proxy) 64 MiB 200 would then stall the whole
    // fan-out in html5ever for seconds past the deadline. SERP
    // pages are small: cap the parse input and treat oversize as a
    // blocked response.
    const SERP_BODY_CAP: usize = 3 << 20;
    if out.body.len() > SERP_BODY_CAP {
        return (
            label,
            Err(("blocked:oversize-serp".into(), egress_id, true)),
        );
    }
    let html = crate::extract::charset::decode(
        &out.body,
        out.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or(""),
    );
    if engine == "google"
        && let Some(status) = engines::google_wml::response_error(out.status, &out.headers, &html)
    {
        if let Some(lease) = &lease {
            google.finish(lease, status);
        }
        return (label, Err((status.into(), egress_id, true)));
    }
    if out.status == 429 || !matches!(out.verdict, Verdict::ContentOk) {
        return (
            label,
            Err((format!("blocked:{}", out.status), egress_id, true)),
        );
    }
    let (mut hits, instant) = engines::parse_with_instant(&engine, &html);
    let scoped = super::query::compile(&query).site;
    if let Some(site) = &scoped {
        let (site, path) = super::query::site_scope(site);
        let site = site.trim_start_matches("www.");
        hits.retain(|hit| {
            url::Url::parse(&hit.url).ok().is_some_and(|u| {
                u.host_str()
                    .is_some_and(|host| host == site || host.ends_with(&format!(".{site}")))
                    && super::query::site_path_matches(&u, path)
            })
        });
    }
    if hits.len() < if scoped.is_some() { 1 } else { 3 } {
        // Honest "no results" is NOT an engine failure :
        // don't burn trust/lanes for a dry query.
        let lower = html.to_lowercase();
        let dry = lower.contains("no results")
            || lower.contains("did not match any")
            || lower.contains("no good results")
            || lower.contains("nothing found");
        let status = if dry || scoped.is_some() {
            "no-results"
        } else {
            "empty-parse"
        };
        return (label, Err((status.into(), egress_id, true)));
    }
    if let Some(lease) = &lease {
        google.finish(lease, "ok");
    }
    (label, Ok((hits, ms, egress_id, true, instant)))
}

pub(super) async fn vertical_task(
    vertical: String,
    query: String,
    fetcher: &Fetcher,
    proxy: Option<crate::egress::proxy::Proxy>,
) -> (String, EngineResult) {
    let started = Instant::now();
    match tokio::time::timeout(
        ENGINE_TIMEOUT,
        verticals::run(fetcher, &vertical, &query, proxy.as_ref()),
    )
    .await
    {
        Err(_) => (vertical, Err(("timeout".into(), "direct".into(), false))),
        Ok(Err(e)) => (
            vertical,
            Err((vertical_failure_status(&e), "direct".into(), false)),
        ),
        Ok(Ok(hits)) => vertical_success(vertical, hits, started.elapsed().as_millis() as u64),
    }
}

/// Pure outcome mapping for a vertical fetch (#164 S2).
///
/// An empty vertical is a "no results" outcome, not a healthy engine:
/// counting it as Ok would inflate ok_engines and relax the
/// retry/cache gates, labeling degraded searches healthy exactly when
/// retrieval is worst. "no-results" is excluded from retries, trust
/// penalties, and egress fault reporting (is_engine_fault), so this
/// stays an honest, penalty-free report.
fn vertical_success(vertical: String, hits: Vec<engines::Hit>, ms: u64) -> (String, EngineResult) {
    if hits.is_empty() {
        return (vertical, Err(("no-results".into(), "direct".into(), false)));
    }
    (vertical, Ok((hits, ms, "direct".into(), false, None)))
}

/// The engine-report status for a SERP or vertical request a local rule
/// refused; in the `invalid-config:` family, so not an engine fault.
const RULE_DENIED_STATUS: &str = "invalid-config: excluded by the operator's local DonSeTch rules";

/// The engine-report status for a failed vertical fetch: a rule refusal
/// gets [`RULE_DENIED_STATUS`], anything else its error text.
fn vertical_failure_status(e: &FetchError) -> String {
    match e {
        FetchError::Denied { .. } => RULE_DENIED_STATUS.into(),
        _ => format!("{e}"),
    }
}

/// The engine-report status for a failed SERP fetch.
fn engine_fetch_status(e: &FetchError) -> &'static str {
    match e {
        FetchError::Timeout | FetchError::DnsTimeout(_) => "timeout",
        FetchError::Http(m) if m.contains("CONNECT -> 407") => "auth-fail",
        FetchError::Http(m) if m.contains("CONNECT") => "dead-proxy",
        // A local rule refused the engine's host. That is the
        // operator's configuration, not the engine misbehaving, so it
        // takes the "invalid-config:" status family, which
        // search/mod.rs already keeps out of quarantine, trust erosion
        // and vertical retries; "net" would persist a bad score for an
        // engine that outlives the rule.
        FetchError::Denied { .. } => RULE_DENIED_STATUS,
        _ => "net",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rule_denial_is_not_a_network_failure() {
        let denied = FetchError::Denied {
            rule: "duckduckgo.com".into(),
            message: "use another engine".into(),
            kind: "walled",
            reason: None,
            url: "https://html.duckduckgo.com/html/".into(),
        };
        let status = engine_fetch_status(&denied);
        assert!(status.starts_with("invalid-config:"), "{status}");
        assert!(!super::super::is_engine_fault(status));
        assert!(!super::super::retry_engine_failure(status));
    }

    #[test]
    fn transport_failures_keep_their_statuses() {
        assert_eq!(engine_fetch_status(&FetchError::Timeout), "timeout");
        assert_eq!(
            engine_fetch_status(&FetchError::Http("CONNECT -> 407".into())),
            "auth-fail"
        );
        assert_eq!(
            engine_fetch_status(&FetchError::Http("CONNECT refused".into())),
            "dead-proxy"
        );
        assert_eq!(engine_fetch_status(&FetchError::Dns("x".into())), "net");
    }

    #[test]
    fn a_vertical_refused_by_a_rule_is_not_an_engine_fault() {
        let denied = FetchError::Denied {
            rule: "github.com".into(),
            message: "use the local mirror".into(),
            kind: "permanent",
            reason: None,
            url: "https://api.github.com/search/repositories".into(),
        };
        let status = vertical_failure_status(&denied);
        assert_eq!(status, RULE_DENIED_STATUS);
        assert!(!super::super::is_engine_fault(&status));
        assert!(!super::super::retry_engine_failure(&status));
        // Negative: any other failure keeps its text and stays a fault.
        let status = vertical_failure_status(&FetchError::Http("connection reset".into()));
        assert_eq!(status, "http: connection reset");
        assert!(super::super::is_engine_fault(&status));
    }

    #[tokio::test]
    async fn engine_budget_includes_pacing_without_starting_network() {
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let pool = EgressPool::new(Vec::new());
        let google = engines::google_wml::ProfileSelector::from_env();
        let context = EngineContext {
            fetcher: &fetcher,
            pool: &pool,
            google: &google,
        };
        pool.pace("bing", "direct").await;
        let (_, outcome) = engine_task_with_budget(
            "bing".into(),
            "unused".into(),
            "direct".into(),
            None,
            context,
            std::time::Duration::from_millis(1),
            None,
        )
        .await;
        let (status, _, _) = outcome.unwrap_err();
        assert_eq!(status, "pacing-timeout");
        assert!(!super::super::is_engine_fault(&status));
    }

    /// Explicit live test of the actual fan-out task, not a second HTTP client.
    /// Never runs in the ordinary offline test suite.
    #[tokio::test]
    #[ignore = "makes three paced, direct requests to Google; no browser or paid API"]
    async fn google_wml_live() {
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let pool = EgressPool::new(Vec::new());
        let google = engines::google_wml::ProfileSelector::from_env();
        let context = EngineContext {
            fetcher: &fetcher,
            pool: &pool,
            google: &google,
        };
        for query in [
            "rust programming language",
            "PostgreSQL documentation",
            "musei di Roma",
        ] {
            let (engine, outcome) = engine_task(
                "google".into(),
                query.into(),
                "direct".into(),
                None,
                context,
            )
            .await;
            let (hits, ms, egress, was_engine, _instant) =
                outcome.expect("Google HTTP lane must return usable results");
            assert!(engine.starts_with("google@"));
            assert_eq!(egress, "direct");
            assert!(was_engine);
            assert!(hits.len() >= 3);
            assert!(
                hits.iter()
                    .all(|h| !h.title.is_empty() && url::Url::parse(&h.url).is_ok())
            );
            assert!(hits.iter().any(|h| !h.snippet.is_empty()));
            crate::diag!(
                "{query:?}: {} hits, {ms} ms, first={}",
                hits.len(),
                hits[0].url
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    fn hit(title: &str, url: &str) -> engines::Hit {
        engines::Hit {
            title: title.into(),
            url: url.into(),
            snippet: String::new(),
            rank: 1,
            published: None,
        }
    }

    #[test]
    fn vertical_empty_hits_is_no_results_not_success() {
        // #164: an empty vertical used to count as a healthy engine,
        // inflating ok_engines and relaxing retry/cache gates exactly
        // when retrieval was worst. It must report no-results instead.
        let (name, result) = vertical_success("github".into(), Vec::new(), 12);
        assert_eq!(name, "github");
        let (status, egress, was_engine) = result.expect_err("empty vertical must not be Ok");
        assert_eq!(status, "no-results");
        assert_eq!(egress, "direct");
        assert!(!was_engine, "vertical outcomes are not engine faults");
    }

    #[test]
    fn vertical_no_results_is_neither_retried_nor_a_fault() {
        let (_, result) = vertical_success("wikipedia".into(), Vec::new(), 5);
        let (status, _, _) = result.expect_err("empty vertical must be Err");
        assert!(
            !crate::search::is_engine_fault(&status),
            "no-results must stay out of trust/quarantine penalties"
        );
    }

    #[test]
    fn vertical_with_hits_is_success() {
        let (name, result) = vertical_success("github".into(), vec![hit("A", "https://a.com")], 42);
        assert_eq!(name, "github");
        let (hits, ms, egress, was_engine, instant) = result.expect("hits => success");
        assert_eq!(hits.len(), 1);
        assert_eq!(ms, 42);
        assert_eq!(egress, "direct");
        assert!(!was_engine);
        assert!(instant.is_none());
    }

    /// Serves `body` for one HTTPS request to `host` through a CONNECT
    /// proxy on loopback, with a certificate the fetcher trusts through
    /// SSL_CERT_FILE. Installs the config and sets SSL_CERT_FILE, both
    /// process-wide: callers depend on nextest's process per test.
    async fn owned_serp(
        host: &str,
        body: String,
    ) -> (crate::egress::proxy::Proxy, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut config = crate::config::DonsetchConfig::default();
        config.fetch.allow_private_egress = true;
        config.proxy.from_environment = false;
        crate::config::install(config).unwrap();
        let cert = rcgen::generate_simple_self_signed(vec![host.to_string()]).unwrap();
        let bundle = crate::paths::cache_dir().join("owned-serp-ca.pem");
        std::fs::write(&bundle, cert.cert.pem()).unwrap();
        unsafe { std::env::set_var("SSL_CERT_FILE", &bundle) };
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut tls = acceptor.accept(socket).await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(tls.read_u8().await.unwrap());
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            tls.write_all(response.as_bytes()).await.unwrap();
            let _ = tls.shutdown().await;
            String::from_utf8_lossy(&request).into_owned()
        });
        (proxy, server)
    }

    fn bing_serp(hits: &[(&str, &str)]) -> String {
        let items: String = hits
            .iter()
            .map(|(url, title)| {
                format!(
                    "<li class=\"b_algo\"><h2><a href=\"{url}\">{title}</a></h2>\
                     <div class=\"b_caption\"><p>{title}: a page about the subject of the query, with details.</p></div></li>"
                )
            })
            .collect();
        format!(
            "<!DOCTYPE html><html><head><title>owned query - Search</title></head><body>\
             <ol id=\"b_results\">{items}</ol></body></html>"
        )
    }

    /// Runs the real Bing HTTP lane for `query` against an owned SERP.
    async fn run_bing(query: &str, body: String) -> (String, EngineResult) {
        let (proxy, server) = owned_serp("www.bing.com", body).await;
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let pool = EgressPool::new(vec![proxy.clone()]);
        let google = engines::google_wml::ProfileSelector::from_env();
        let context = EngineContext {
            fetcher: &fetcher,
            pool: &pool,
            google: &google,
        };
        let out = engine_task_with_budget(
            "bing".into(),
            query.into(),
            proxy.id(),
            Some(proxy),
            context,
            std::time::Duration::from_secs(6),
            None,
        )
        .await;
        let request = tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .expect("the owned SERP was requested")
            .unwrap();
        assert!(request.starts_with("GET /search?q="), "{request}");
        out
    }

    #[tokio::test]
    async fn http_lane_keeps_hits_for_a_path_scoped_site_operator() {
        let body = bing_serp(&[
            ("https://github.com/tokio-rs/tokio", "tokio-rs/tokio"),
            ("https://github.com/tokio-rs/axum", "tokio-rs/axum"),
            ("https://github.com/tokio-rs/mio", "tokio-rs/mio"),
        ]);
        let (_, result) = run_bing("runtime site:github.com/tokio-rs", body).await;
        match result {
            Ok((hits, ..)) => assert_eq!(hits.len(), 3),
            Err((status, ..)) => panic!(
                "Bing returned 3 github.com/tokio-rs hits; the lane reported {status:?} with none kept"
            ),
        }
    }

    /// Negative: a path scope still drops on-host hits outside the path
    /// and off-host hits under the same path.
    #[tokio::test]
    async fn http_lane_drops_hits_outside_a_path_scoped_site_operator() {
        let body = bing_serp(&[
            ("https://github.com/rust-lang/rust", "rust-lang/rust"),
            ("https://github.com/tokio-rs/tokio", "tokio-rs/tokio"),
            ("https://gitlab.com/tokio-rs/tokio", "tokio-rs/tokio mirror"),
        ]);
        let (_, result) = run_bing("runtime site:github.com/tokio-rs", body).await;
        let (hits, ..) = result.expect("the one on-path hit is kept");
        let kept: Vec<&str> = hits.iter().map(|hit| hit.url.as_str()).collect();
        assert_eq!(kept, vec!["https://github.com/tokio-rs/tokio"]);
    }
}
