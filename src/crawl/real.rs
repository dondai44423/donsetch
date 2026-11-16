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
fn pool_lanes(proxies: &[crate::egress::proxy::Proxy], rotate: bool) -> Vec<Lane> {
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
        Arc::new(
            move |url: String,
                  lane: String,
                  referer: Option<String>,
                  redirect_gate: Option<crate::fetch::client::RedirectGate>| {
                let fetcher = Arc::clone(&fetcher);
                let pool = Arc::clone(&pool);
                let proxies = Arc::clone(&proxies);
                // the same adapter registry web_fetch uses
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
                    match fetcher
                        .fetch_via_jar_opts(
                            &fetch_url,
                            proxy.as_ref(),
                            use_jar,
                            referer.as_deref(),
                            true,
                            false,
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
                                None => match &e {
                                    crate::error::FetchError::Dns(_) => format!("network.dns: {e}"),
                                    crate::error::FetchError::DnsTimeout(_) => {
                                        format!("network.dns_timeout: {e}")
                                    }
                                    _ => format!("network: {e}"),
                                },
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
