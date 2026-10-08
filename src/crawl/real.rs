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
pub fn build(fetcher: Arc<Fetcher>, pool: Arc<EgressPool>) -> (Crawler, Arc<Governor>) {
    let proxies = Arc::new(pool.proxies());
    let rotate = crate::config::cfg().proxy.crawl_rotate;
    let governor = Arc::new(Governor::new(pool_lanes(&proxies, rotate)));

    let fetch: PageFetcher = {
        let fetcher = Arc::clone(&fetcher);
        let pool = Arc::clone(&pool);
        let proxies = Arc::clone(&proxies);
        Arc::new(move |url: String, lane: String, referer: Option<String>| {
            let fetcher = Arc::clone(&fetcher);
            let pool = Arc::clone(&pool);
            let proxies = Arc::clone(&proxies);
            // v4 phase 3: the same adapter registry web_fetch uses
            // shapes crawl fetches, so a reddit/npm class URL rides
            // the cheap .json/registry path instead of the HTML app.
            // The candidate URL stays canonical: dedup, history and
            // output rows key on it, the wire just asks for the
            // rewritten endpoint. DONSETCH_NO_ADAPTERS silences this
            // exactly as it does in web_fetch (handled inside
            // adapters::rewrite).
            let fetch_url = url::Url::parse(&url)
                .ok()
                .and_then(|u| crate::adapters::rewrite(&u).map(|(alt, _via)| alt))
                .unwrap_or_else(|| url.clone());
            let host = url::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
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
                match fetcher
                    .fetch_via_jar_opts(
                        &fetch_url,
                        proxy.as_ref(),
                        use_jar,
                        referer.as_deref(),
                        true,
                        false,
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
                                200 | 304 => {
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
                        }
                    }
                    Err(e) => {
                        let msg = format!("{e}");
                        if lane != "direct" {
                            if msg.contains("CONNECT -> 407") {
                                pool.note_fetch_auth_fail(&host, &lane);
                            } else if msg.contains("timeout") || msg.contains("timed out") {
                                pool.note_fetch_timeout(&host, &lane);
                            } else {
                                pool.note_fetch_dead(&host, &lane);
                            }
                        }
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
                            error_hint: Some(format!("network: {e}")),
                        }
                    }
                }
            }
            .boxed()
        })
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
        let (crawler, governor) = build(fetcher, Arc::new(EgressPool::new(vec![proxy.clone()])));
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
        let (crawler, _) = build(fetcher, Arc::new(EgressPool::new(Vec::new())));
        let page = (crawler.fetch)(url, "missing-owned-proxy".into(), None).await;
        let reached_origin = origin.await.unwrap();
        assert!(
            !reached_origin,
            "unknown crawl lane leaked a real direct request"
        );
        assert!(page.route.is_none());
        assert_eq!(page.status, 0);
        assert!(page.error_hint.unwrap().contains("unavailable"));
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
