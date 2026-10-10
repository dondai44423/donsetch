//! Bridge: Crawler over the real DonShadow fetcher.
//!
//! Maps governor lanes to actual egress: "direct" rides the
//! plain socket, proxy ids ride the shared EgressPool (v4 A2).
//! Dead lanes are skipped at fetch time; outcomes report back
//! into the same health world search and fetch use.
//!
//! Pool use is crawl's own default (`proxy.crawl_rotate`, true):
//! `donsetch proxy crawl off` drops the proxy lanes and every crawl
//! request leaves on the home IP. The fetch lane pick stays out of
//! this path entirely: crawl owns its lane choice (the fetch call
//! passes `pool_pick=false`).

use std::sync::Arc;
use std::time::Instant;

use futures_util::FutureExt;

use crate::detect::walls::Verdict;
use crate::fetch::client::{CacheState, Fetcher};
use crate::ghost::cache::GhostState;
use crate::search::egress::EgressPool;

use super::governor::{Governor, Lane, LaneKind};
use super::{Crawler, FetchedPage, PageFetcher};

/// The crawl governor's lane list: `direct` always; the pool's proxy
/// lanes only while crawl pool use is on (`proxy.crawl_rotate`,
/// default true; `donsetch proxy crawl off` opts out, leaving every
/// crawl request on the home IP).
fn pool_lanes(proxies: &[crate::transport::proxy::Proxy], rotate: bool) -> Vec<Lane> {
    let mut lanes = vec![Lane {
        id: "direct".into(),
        kind: LaneKind::Direct,
    }];
    if rotate {
        for p in proxies {
            lanes.push(Lane {
                id: p.id(),
                kind: LaneKind::Proxy,
            });
        }
    }
    lanes
}

/// Build the real crawl stack. `fetcher` is shared state (same
/// jar/pool/cache as everything else in the process); `pool` is
/// the process-wide egress fabric (health + dead benches shared
/// with search and fetch).
pub fn build(
    fetcher: Arc<Fetcher>,
    pool: Arc<EgressPool>,
    state: Option<Arc<tokio::sync::Mutex<GhostState>>>,
) -> (Crawler, Arc<Governor>) {
    let proxies = Arc::new(pool.proxies());
    let rotate = crate::config::cfg().proxy.crawl_rotate;
    let governor = Arc::new(Governor::new(pool_lanes(&proxies, rotate)));

    let fetch: PageFetcher = {
        let fetcher = Arc::clone(&fetcher);
        let pool = Arc::clone(&pool);
        let proxies = Arc::clone(&proxies);
        Arc::new(
            move |url: String,
                  lane: String,
                  referer: Option<String>,
                  redirect_gate: Option<crate::fetch::client::RedirectGate>| {
                let fetcher = Arc::clone(&fetcher);
                let pool = Arc::clone(&pool);
                let proxies = Arc::clone(&proxies);
                let state = state.clone();
                // v4 phase 3: the same adapter registry web_fetch uses
                // shapes crawl fetches, so a reddit/npm class URL rides
                // the cheap .json/registry path instead of the HTML app.
                // The candidate URL stays canonical: dedup, history and
                // output rows key on it, the wire just asks for the
                // rewritten endpoint. DONSETCH_NO_ADAPTERS silences this
                // exactly as it does in web_fetch (handled inside
                // adapters::rewrite).
                let parsed = url::Url::parse(&url).ok();
                let fetch_url = parsed
                    .as_ref()
                    .and_then(|u| crate::adapters::rewrite(u).map(|(alt, _via)| alt))
                    .unwrap_or_else(|| url.clone());
                let host = parsed
                    .as_ref()
                    .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
                    .unwrap_or_default();
                let path = parsed
                    .as_ref()
                    .map(|u| u.path().to_string())
                    .unwrap_or_default();
                async move {
                    let started = Instant::now();
                    let proxy = if lane == "direct" {
                        None
                    } else {
                        proxies.iter().find(|p| p.id() == lane).cloned()
                    };
                    // A missing or benched assignment must not become a direct read.
                    if lane != "direct" && (proxy.is_none() || pool.is_dead(&lane)) {
                        let reason = if proxy.is_none() {
                            "unavailable"
                        } else {
                            "benched"
                        };
                        return FetchedPage {
                            lane: lane.clone(),
                            route: None,
                            url,
                            status: 0,
                            headers: Vec::new(),
                            body: Vec::new(),
                            verdict: Verdict::Blocked,
                            latency: started.elapsed(),
                            cached: false,
                            error_hint: Some(format!("egress: lane {lane} is {reason}")),
                            denied: None,
                        };
                    }
                    // Proxy lanes: shared jar OUT : one cookie carrying
                    // lane B's identity would link the two egress IPs.
                    let use_jar = proxy.is_none();
                    // v4 F2: cross-process politeness floor. The in-process
                    // governor already spaced this request; a second
                    // donsetch process on the same host still needs a
                    // shared last-stamp. Best-effort, kill-switchable.
                    crate::crawl::host_pace::wait_and_stamp(&host).await;
                    // A recrawl exists to see what changed : serve the
                    // live response, never the revalidation cache (the
                    // cached body made every delta recrawl compare the
                    // previous crawl's own content and report zero
                    // changes forever).
                    // v4 E2 coherence: a host with a persona presents
                    // the persona's language on crawl pages exactly as
                    // web_fetch and the ghost do; a host without one
                    // keeps the TLD/script heuristic.
                    let persona_al = match &state {
                        Some(state) => {
                            let st = state.lock().await;
                            st.personas
                                .get(&host)
                                .filter(|p| p.quarantine_reason.is_none())
                                .map(|p| {
                                    crate::profile::accept_language_with_persona(
                                        &host, &path, &p.locale,
                                    )
                                })
                        }
                        None => None,
                    };
                    match fetcher
                        .fetch_via_jar_language(
                            &fetch_url,
                            proxy.as_ref(),
                            use_jar,
                            referer.as_deref(),
                            true,
                            false,
                            persona_al.as_deref(),
                            redirect_gate,
                        )
                        .await
                    {
                        Ok(out) => {
                            // Fresh-window cache hit made ZERO requests:
                            // exclude from governor pacing. Revalidated
                            // hits made a (304) request, keep them.
                            let cached = matches!(out.cache, CacheState::Fresh);
                            if !cached {
                                match out.status {
                                    200..=299 | 304 => {
                                        if !host.is_empty() {
                                            pool.report_ok(&host, &lane);
                                        }
                                        pool.observe_rtt(&lane, started.elapsed());
                                    }
                                    429 | 503 if !host.is_empty() && lane != "direct" => {
                                        pool.note_fetch_rate_limited(&host, &lane);
                                    }
                                    _ => {}
                                }
                            }
                            FetchedPage {
                                lane: lane.clone(),
                                route: Some(out.route),
                                url: out.url,
                                status: out.status,
                                headers: out.headers,
                                body: out.body,
                                verdict: out.verdict,
                                latency: started.elapsed(),
                                cached,
                                error_hint: None,
                                denied: None,
                            }
                        }
                        Err(e) => {
                            // The typed classifier is the single source of
                            // attribution: origin DNS and certificate
                            // failures, policy refusals and local errors
                            // leave lane health alone; only genuine
                            // lane-level failures bench it.
                            if lane != "direct" {
                                crate::fetch::client::note_lane_outcome(&pool, &host, &lane, &e);
                            }
                            // A rule refusal is a policy decision, not a
                            // network failure: carry the typed payload so
                            // the worker can treat the page as final and
                            // name the rule. Status stays 0, so robots and
                            // sitemap readers see a refused fetch as before.
                            let denied = crate::rules::Denial::from_error(&e);
                            let error_hint = match &denied {
                                Some(d) => super::policy_skip_reason(d),
                                None => format!("network: {e}"),
                            };
                            // A refused redirect hop lands the page on the
                            // hop's target, as a followed redirect would:
                            // a failed seed reports it as `landing_url`.
                            let url = match e {
                                crate::error::FetchError::Denied { url: refused, .. } => refused,
                                _ => url,
                            };
                            FetchedPage {
                                lane: lane.clone(),
                                route: None,
                                url,
                                status: 0,
                                headers: Vec::new(),
                                body: Vec::new(),
                                verdict: Verdict::Blocked,
                                latency: started.elapsed(),
                                cached: false,
                                error_hint: Some(error_hint),
                                denied,
                            }
                        }
                    }
                }
                .boxed()
            },
        )
    };

    (Crawler::new(fetch, Arc::clone(&governor)), governor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::proxy::Proxy;

    #[tokio::test]
    async fn stealth_v3_buffered_seed_feedback_charges_the_actual_lane() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/owned-prime", origin.local_addr().unwrap());
        let proxy =
            Proxy::parse(&format!("http://{}", proxy_listener.local_addr().unwrap())).unwrap();
        let origin = tokio::spawn(async move {
            let (mut socket, _) = origin.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(socket.read_u8().await.unwrap());
            }
            socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found").await.unwrap();
            String::from_utf8(head).unwrap()
        });
        let fetcher =
            Arc::new(Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap());
        let (crawler, governor) = build(
            fetcher,
            Arc::new(EgressPool::new(vec![proxy.clone()])),
            None,
        );
        let result = crawler
            .crawl(
                &url,
                super::super::CrawlOptions {
                    mode: super::super::CrawlMode::Content,
                    respect_robots: false,
                    max_pages: 1,
                    max_depth: 0,
                    deadline: std::time::Duration::from_secs(2),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let request = origin.await.unwrap();
        assert!(request.starts_with("GET /owned-prime HTTP/1.1\r\n"));
        assert!(result.pages.is_empty());
        assert_eq!(
            governor.best_lane("127.0.0.1").unwrap().id,
            proxy.id(),
            "the actual DIRECT404 must delay direct, leaving the unused proxy ready"
        );
    }

    // Installs a ruleset into the process config: depends on nextest's
    // process-per-test.
    #[tokio::test]
    async fn a_seed_redirected_into_a_denied_host_lands_on_the_refused_hop() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = true;
        config.rules.url.insert(
            "banned.example".into(),
            crate::rules::UrlRule {
                action: crate::rules::RuleAction::Deny,
                message: Some("ask the human operator to download the file".into()),
                ..crate::rules::UrlRule::default()
            },
        );
        crate::config::install(config).unwrap();
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/paper", origin.local_addr().unwrap());
        let origin = tokio::spawn(async move {
            let (mut socket, _) = origin.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(socket.read_u8().await.unwrap());
            }
            socket.write_all(b"HTTP/1.1 301 Moved Permanently\r\nLocation: http://www.banned.example/publication/1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let fetcher =
            Arc::new(Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap());
        let (crawler, _governor) = build(fetcher, Arc::new(EgressPool::new(Vec::new())), None);
        let result = crawler
            .crawl(
                &url,
                super::super::CrawlOptions {
                    mode: super::super::CrawlMode::Content,
                    respect_robots: false,
                    max_pages: 1,
                    max_depth: 0,
                    deadline: std::time::Duration::from_secs(2),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        origin.await.unwrap();
        let failure = result.seed_failure.expect("the seed failed");
        assert_eq!(
            failure.denial.as_ref().map(|d| d.rule.as_str()),
            Some("banned.example")
        );
        assert_eq!(failure.requested, url);
        assert_eq!(
            failure.landing.as_deref(),
            Some("http://www.banned.example/publication/1"),
            "the landing is the refused hop, not the requested seed"
        );
    }

    #[tokio::test]
    async fn stealth_v3_missing_crawl_lane_never_falls_back_to_direct() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/owned", listener.local_addr().unwrap());
        let origin = tokio::spawn(async move {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                    .await
            else {
                return false;
            };
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(socket.read_u8().await.unwrap());
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nowned",
                )
                .await
                .unwrap();
            true
        });
        let fetcher =
            Arc::new(Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap());
        let (crawler, _) = build(fetcher, Arc::new(EgressPool::new(Vec::new())), None);
        let page = (crawler.fetch)(url, "missing-owned-proxy".into(), None, None).await;
        let reached_origin = origin.await.unwrap();
        assert!(
            !reached_origin,
            "unknown crawl lane leaked a real direct request"
        );
        assert!(page.route.is_none());
        assert_eq!(page.status, 0);
        assert!(page.error_hint.unwrap().contains("unavailable"));
    }

    // Installs a ruleset into the process config, so it depends on
    // nextest's process-per-test (a second install is an error).
    #[tokio::test]
    async fn a_rule_refusal_reaches_the_worker_typed_not_as_a_network_error() {
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = true;
        config.rules.url.insert(
            "127.0.0.1".to_string(),
            crate::rules::UrlRule {
                action: crate::rules::RuleAction::Deny,
                message: Some("ask the operator for this one".into()),
                reason: Some("local_only".into()),
                ..Default::default()
            },
        );
        crate::config::install(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/denied", listener.local_addr().unwrap());
        let origin = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_millis(300), listener.accept())
                .await
                .is_ok()
        });
        let fetcher =
            Arc::new(Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap());
        let (crawler, _) = build(fetcher, Arc::new(EgressPool::new(Vec::new())), None);
        let page = (crawler.fetch)(url, "direct".into(), None, None).await;
        assert!(!origin.await.unwrap(), "a denied URL must not be dialed");
        assert_eq!(page.status, 0, "robots and sitemap readers keep status 0");
        let denial = page.denied.expect("the refusal arrives typed");
        assert_eq!(denial.rule, "127.0.0.1");
        assert_eq!(denial.reason.as_deref(), Some("local_only"));
        assert_eq!(
            page.error_hint.as_deref(),
            Some("policy.denied.local_only: blocked by a local DonSeTch rule `127.0.0.1`"),
            "never labelled a network failure"
        );
    }

    #[test]
    fn crawl_leaves_the_pool_with_the_opt_out() {
        let proxies = vec![
            Proxy::parse("socks5://127.0.0.1:1080").unwrap(),
            Proxy::parse("http://127.0.0.1:3128").unwrap(),
        ];
        let on = pool_lanes(&proxies, true);
        assert_eq!(on.len(), 3, "direct plus both proxy lanes");
        assert_eq!(on[0].kind, LaneKind::Direct);
        assert!(on[1..].iter().all(|l| l.kind == LaneKind::Proxy));

        let off = pool_lanes(&proxies, false);
        assert_eq!(off.len(), 1, "the opt-out leaves only the direct lane");
        assert_eq!(off[0].id, "direct");
    }
}
