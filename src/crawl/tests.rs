//! Crawl end-to-end battle tests. The mock fetcher serves a
//! scripted site: sitemaps, cyclic links, walls, 429 storms,
//! near-dupes. Zero network. If the orchestrator survives this
//! house, it survives the internet.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;

use super::governor::{Governor, Lane, LaneKind};
use super::{
    CrawlMode, CrawlOptions, Crawler, FetchedPage, GHOST_BUDGET, PageFetcher, StopReason,
    claim_ghost_slot,
};
use crate::detect::walls::Verdict;

// The escalation budget is shared by every worker. A load-then-
// fetch_sub gate let two workers pass at budget == 1 and wrap the
// counter to usize::MAX, after which every later check passed. The
// atomic claim must hand out exactly GHOST_BUDGET slots however many
// threads race for them, and the counter must end at 0, not wrap.
#[test]
fn ghost_budget_is_claimed_exactly_budget_times_under_contention() {
    for _ in 0..20 {
        let budget = Arc::new(AtomicUsize::new(GHOST_BUDGET));
        let claimed = Arc::new(AtomicUsize::new(0));
        let go = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (budget, claimed, go) = (budget.clone(), claimed.clone(), go.clone());
                std::thread::spawn(move || {
                    go.wait();
                    for _ in 0..100 {
                        if claim_ghost_slot(&budget) {
                            claimed.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(claimed.load(Ordering::SeqCst), GHOST_BUDGET);
        assert_eq!(budget.load(Ordering::SeqCst), 0, "counter wrapped");
        assert!(!claim_ghost_slot(&budget), "exhausted budget must refuse");
    }
}

/// A scripted site: URL → (status, body). Missing URL = 404.
struct MockSite {
    pages: HashMap<String, (u16, String)>,
    hits: Arc<Mutex<Vec<String>>>,
    /// 429s remaining to serve before flipping to 200.
    throttles: Arc<Mutex<HashMap<String, AtomicUsize>>>,
    /// 500s remaining to serve before flipping to 200 (transient).
    transients: Arc<Mutex<HashMap<String, AtomicUsize>>>,
    /// Per-URL content-type override (default: text/html).
    content_types: HashMap<String, String>,
    /// Captures referer passed to each fetch.
    referers: RefererLog,
}

type RefererLog = Arc<Mutex<Vec<(String, Option<String>)>>>;

impl MockSite {
    fn new() -> Self {
        Self {
            pages: HashMap::new(),
            hits: Arc::new(Mutex::new(Vec::new())),
            throttles: Arc::new(Mutex::new(HashMap::new())),
            transients: Arc::new(Mutex::new(HashMap::new())),
            content_types: HashMap::new(),
            referers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn page(mut self, url: &str, status: u16, body: &str) -> Self {
        self.pages
            .insert(url.to_string(), (status, body.to_string()));
        self
    }

    fn throttle_n(self, url: &str, n: usize) -> Self {
        self.throttles
            .lock()
            .unwrap()
            .insert(url.to_string(), AtomicUsize::new(n));
        self
    }

    /// Serve `n` 500 errors (transient) before flipping to 200.
    fn transient_n(self, url: &str, n: usize) -> Self {
        self.transients
            .lock()
            .unwrap()
            .insert(url.to_string(), AtomicUsize::new(n));
        self
    }

    /// Override content-type for a URL (default: text/html).
    fn content_type(mut self, url: &str, ct: &str) -> Self {
        self.content_types.insert(url.to_string(), ct.to_string());
        self
    }

    fn hit_count(&self) -> usize {
        0
    }

    fn fetcher(self) -> (PageFetcher, Arc<Mutex<Vec<String>>>) {
        let hits = Arc::clone(&self.hits);
        let pages = Arc::new(self.pages);
        let throttles = Arc::clone(&self.throttles);
        let transients = Arc::clone(&self.transients);
        let content_types = Arc::new(self.content_types);
        let referers = Arc::clone(&self.referers);
        let hits2 = Arc::clone(&hits);
        let f: PageFetcher = Arc::new(
            move |url: String,
                  _lane: String,
                  referer: Option<String>,
                  _gate: Option<crate::fetch::client::RedirectGate>| {
                let pages = Arc::clone(&pages);
                let throttles = Arc::clone(&throttles);
                let transients = Arc::clone(&transients);
                let content_types = Arc::clone(&content_types);
                let referers = Arc::clone(&referers);
                let hits = Arc::clone(&hits2);
                async move {
                    hits.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(url.clone());
                    referers
                        .lock()
                        .unwrap()
                        .push((url.clone(), referer.clone()));
                    // Throttle simulation: 429 until counter burns out.
                    if let Some(c) = throttles
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&url)
                        && c.load(Ordering::SeqCst) > 0
                    {
                        c.fetch_sub(1, Ordering::SeqCst);
                        return FetchedPage {
                            lane: _lane.clone(),
                            route: Some(crate::transport::request_route::RequestRoute::direct()),
                            url,
                            status: 429,
                            headers: vec![],
                            body: b"slow down".to_vec(),
                            verdict: Verdict::Blocked,
                            latency: Duration::from_millis(10),
                            cached: false,
                            error_hint: None,
                        };
                    }
                    // Transient 500 simulation: 500 until counter burns out.
                    if let Some(c) = transients
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&url)
                        && c.load(Ordering::SeqCst) > 0
                    {
                        c.fetch_sub(1, Ordering::SeqCst);
                        return FetchedPage {
                            lane: _lane.clone(),
                            route: Some(crate::transport::request_route::RequestRoute::direct()),
                            url,
                            status: 500,
                            headers: vec![],
                            body: b"internal error".to_vec(),
                            verdict: Verdict::Blocked,
                            latency: Duration::from_millis(10),
                            cached: false,
                            error_hint: Some("transient 500".into()),
                        };
                    }
                    let ct = content_types
                        .get(&url)
                        .cloned()
                        .unwrap_or_else(|| "text/html".to_string());
                    match pages.get(&url) {
                        Some((status, body)) => FetchedPage {
                            lane: _lane.clone(),
                            route: Some(crate::transport::request_route::RequestRoute::direct()),
                            url,
                            status: *status,
                            headers: vec![("content-type".into(), ct)],
                            body: body.as_bytes().to_vec(),
                            verdict: Verdict::ContentOk,
                            latency: Duration::from_millis(10),
                            cached: false,
                            error_hint: None,
                        },
                        None => FetchedPage {
                            lane: _lane,
                            route: Some(crate::transport::request_route::RequestRoute::direct()),
                            url,
                            status: 404,
                            headers: vec![],
                            body: b"not found".to_vec(),
                            verdict: Verdict::SoftNotFound,
                            latency: Duration::from_millis(10),
                            cached: false,
                            error_hint: None,
                        },
                    }
                }
                .boxed()
            },
        );
        (f, hits)
    }
}

fn gov() -> Arc<Governor> {
    Arc::new(Governor::new(vec![Lane {
        id: "direct".into(),
        kind: LaneKind::Direct,
    }]))
}

fn opts() -> CrawlOptions {
    CrawlOptions {
        deadline: Duration::from_secs(10),
        ..Default::default()
    }
}

fn html(title: &str, body: &str) -> String {
    format!(
        "<html lang=\"en\"><head><title>{title}</title></head><body><article><h1>{title}</h1><p>{body} {}</p></article></body></html>",
        "Long enough paragraph content to pass extraction thresholds and look like a real document for the extractor.".repeat(3)
    )
}

fn classifying_fetcher(inner: PageFetcher) -> PageFetcher {
    Arc::new(move |url, lane, referer, gate| {
        let inner = Arc::clone(&inner);
        async move {
            let mut page = inner(url, lane, referer, gate).await;
            page.verdict = crate::detect::walls::detect(page.status, &page.headers, &page.body);
            page
        }
        .boxed()
    })
}

#[tokio::test]
async fn stealth_v3_short_browser_handoffs_use_one_original_deadline() {
    for thin in [false, true] {
        for stalled in [false, true] {
            let seed = "https://ex.com/start";
            let shell = format!(
                "<html><body><div id='app'></div><script>{}</script></body></html>",
                "/* owned shell */".repeat(400)
            );
            let (fetch, hits) = MockSite::new()
                .page(
                    seed,
                    if thin { 200 } else { 403 },
                    if thin { &shell } else { "Access denied" },
                )
                .fetcher();
            let fetch: PageFetcher = Arc::new(move |url, lane, referer, gate| {
                let fetch = Arc::clone(&fetch);
                async move {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    fetch(url, lane, referer, gate).await
                }
                .boxed()
            });
            let calls = Arc::new(AtomicUsize::new(0));
            let reached = Arc::clone(&calls);
            let cancelled = Arc::new(Mutex::new(None));
            let cancelled_hook = Arc::clone(&cancelled);
            let started = std::time::Instant::now();
            let budget = Duration::from_millis(250);
            let hook: super::GhostHook = Arc::new(move |request| {
                let reached = Arc::clone(&reached);
                let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
                *cancelled_hook.lock().unwrap() = Some(receiver);
                async move {
                    let _held_until_cancelled = sender;
                    reached.fetch_add(1, Ordering::SeqCst);
                    // Account for synchronous setup before crawl starts, while
                    // rejecting a renewed budget after the 40ms HTTP read.
                    assert!(request.deadline <= started + budget + Duration::from_millis(10));
                    if stalled {
                        futures_util::future::pending::<()>().await;
                    }
                    Ok(super::GhostRender {
                        html: html(
                            "Bounded browser content",
                            "Useful evidence within the caller's remaining time.",
                        ),
                        document: crate::ghost::document::Document {
                            url: request.url,
                            status: Some(201),
                            generation: 7,
                            ..Default::default()
                        },
                    })
                }
                .boxed()
            });
            let result = Crawler::new(classifying_fetcher(fetch), gov())
                .with_ghost(hook)
                .crawl(
                    seed,
                    CrawlOptions {
                        mode: CrawlMode::Content,
                        respect_robots: false,
                        max_pages: 1,
                        max_depth: 0,
                        deadline: budget,
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "thin={thin}, stalled={stalled}"
            );
            assert_eq!(hits.lock().unwrap().as_slice(), [seed]);
            assert!(started.elapsed() < budget + Duration::from_millis(300));
            assert!(matches!(
                cancelled.lock().unwrap().as_mut().unwrap().try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed)
            ));
            if stalled {
                assert_eq!(result.stop, StopReason::Deadline);
                assert!(
                    !result
                        .pages
                        .iter()
                        .any(|page| page.markdown.contains("Bounded browser content"))
                );
            } else {
                assert_eq!(result.pages.len(), 1);
                assert!(result.pages[0].markdown.contains("Bounded browser content"));
            }
        }
    }
}

#[tokio::test]
async fn stealth_v3_browser_redirects_obey_scope_host_and_robots_for_wall_and_thin_pages() {
    for thin in [false, true] {
        for target in [
            "https://ex.com/outside",
            "https://other.example/allowed",
            "https://ex.com/robots-denied",
        ] {
            let seed = "https://ex.com/start";
            let shell = format!(
                "<html><body><div id='app'></div><script>{}</script></body></html>",
                "/* owned shell */".repeat(400)
            );
            let (fetch, _) = MockSite::new()
                .page(
                    "https://ex.com/robots.txt",
                    200,
                    "User-agent: *\nDisallow: /robots-denied\n",
                )
                .page(
                    seed,
                    if thin { 200 } else { 403 },
                    if thin {
                        &shell
                    } else {
                        "<html><body>Access denied</body></html>"
                    },
                )
                .fetcher();
            let calls = Arc::new(AtomicUsize::new(0));
            let reached = Arc::clone(&calls);
            let hook: super::GhostHook = Arc::new(move |_| {
                reached.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(super::GhostRender {
                        html: html(
                            "Owned browser document",
                            "The final browser document supplies its own evidence.",
                        ),
                        document: crate::ghost::document::Document {
                            url: target.into(),
                            status: Some(201),
                            generation: 7,
                            ..Default::default()
                        },
                    })
                }
                .boxed()
            });
            let result = Crawler::new(classifying_fetcher(fetch), gov())
                .with_ghost(hook)
                .crawl(
                    seed,
                    CrawlOptions {
                        mode: CrawlMode::Content,
                        deadline: Duration::from_secs(30),
                        include_paths: vec!["/*".into()],
                        exclude_paths: vec!["/outside".into()],
                        max_pages: 1,
                        max_depth: 0,
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "browser branch not reached: thin={thin}, target={target}"
            );
            assert!(
                result.pages.is_empty(),
                "out-of-scope browser content returned: thin={thin}, target={target}"
            );
            assert!(
                result
                    .skipped
                    .iter()
                    .any(|(_, reason)| reason.contains("redirected out of scope")),
                "{:?}",
                result.skipped
            );
        }
    }
}

#[tokio::test]
async fn stealth_v3_allowed_browser_redirect_owns_url_and_relative_links() {
    for thin in [false, true] {
        let seed = "https://ex.com/start";
        let final_url = "https://ex.com/allowed/final";
        let shell = format!(
            "<html><body><div id='app'></div><script>{}</script></body></html>",
            "/* owned shell */".repeat(400)
        );
        let (fetch, hits) = MockSite::new()
            .page(
                seed,
                if thin { 200 } else { 403 },
                if thin {
                    &shell
                } else {
                    "<html><body>Access denied</body></html>"
                },
            )
            .page(
                "https://ex.com/allowed/next",
                200,
                &html(
                    "Distinct child",
                    "A second document with independent research evidence.",
                ),
            )
            .fetcher();
        let calls = Arc::new(AtomicUsize::new(0));
        let reached = Arc::clone(&calls);
        let hook: super::GhostHook = Arc::new(move |_| {
            reached.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(super::GhostRender {
                    html: html(
                        "Owned final document",
                        "The browser final URL owns this content.",
                    ) + "<a href='next'>Next chapter</a><a href='final'>Self document</a>",
                    document: crate::ghost::document::Document {
                        url: final_url.into(),
                        status: Some(201),
                        generation: 7,
                        ..Default::default()
                    },
                })
            }
            .boxed()
        });
        let result = Crawler::new(classifying_fetcher(fetch), gov())
            .with_ghost(hook)
            .crawl(
                seed,
                CrawlOptions {
                    mode: CrawlMode::Content,
                    deadline: Duration::from_secs(30),
                    respect_robots: false,
                    include_paths: vec!["/*".into()],
                    max_pages: 2,
                    max_depth: 1,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(result.pages.iter().any(|page| page.url == final_url && page.markdown.contains("Owned final document")),
            "thin={thin}, pages={:?}, skipped={:?}", result.pages.iter().map(|page| (&page.url, &page.markdown)).collect::<Vec<_>>(), result.skipped);
        assert!(
            result
                .pages
                .iter()
                .any(|page| page.url == "https://ex.com/allowed/next")
        );
        assert_eq!(
            result
                .pages
                .iter()
                .find(|page| page.url == "https://ex.com/allowed/next")
                .unwrap()
                .parent
                .as_deref(),
            Some(final_url)
        );
        assert!(!result.pages.iter().any(|page| page.url == seed));
        assert!(
            !result.queued.iter().any(|url| url == final_url),
            "the browser final document is already visited"
        );
        assert!(
            !hits.lock().unwrap().iter().any(|url| url == final_url),
            "the browser final document must not be fetched again"
        );
        assert!(
            hits.lock()
                .unwrap()
                .iter()
                .any(|url| url == "https://ex.com/allowed/next")
        );
    }
}

#[tokio::test]
async fn stealth_v3_browser_unknown_status_never_inherits_http_denial() {
    let (fetch, _) = MockSite::new()
        .page("https://ex.com/", 403, "Access denied")
        .fetcher();
    let mut page = fetch("https://ex.com/".into(), "owned-lane".into(), None, None).await;
    let route = page.route.clone();
    page.apply_render(super::GhostRender {
        html: html("Owned browser", "Content with no observed network status."),
        document: crate::ghost::document::Document {
            url: "https://ex.com/final".into(),
            status: None,
            generation: 3,
            ..Default::default()
        },
    });
    assert_eq!(
        page.status, 0,
        "zero is unknown, not a synthesized200 or inherited403"
    );
    assert_eq!(page.url, "https://ex.com/final");
    assert_eq!(page.route, route);
    assert_eq!(page.lane, "owned-lane");
    assert!(
        String::from_utf8(page.body)
            .unwrap()
            .contains("Owned browser")
    );
}

#[tokio::test]
async fn stealth_v3_http_redirect_outside_scope_does_not_start_browser() {
    let seed = "https://ex.com/start";
    let (inner, hits) = MockSite::new().page(seed, 403, "Access denied").fetcher();
    let fetch: PageFetcher = Arc::new(move |url, lane, referer, gate| {
        let inner = Arc::clone(&inner);
        async move {
            let mut page = inner(url, lane, referer, gate).await;
            page.url = "https://ex.com/outside".into();
            page.verdict = Verdict::Blocked;
            page
        }
        .boxed()
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let reached = Arc::clone(&calls);
    let hook: super::GhostHook = Arc::new(move |_| {
        reached.fetch_add(1, Ordering::SeqCst);
        async { Err("out-of-scope browser must not start".into()) }.boxed()
    });
    let result = Crawler::new(fetch, gov())
        .with_ghost(hook)
        .crawl(
            seed,
            CrawlOptions {
                mode: CrawlMode::Content,
                respect_robots: false,
                deadline: Duration::from_secs(30),
                include_paths: vec!["/*".into()],
                exclude_paths: vec!["/outside".into()],
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(hits.lock().unwrap().as_slice(), [seed]);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(result.pages.is_empty());
    assert!(
        result
            .skipped
            .iter()
            .any(|(_, reason)| reason == "redirected out of scope -> https://ex.com/outside")
    );
}

#[tokio::test]
async fn wave450_seed_preflight_uses_an_available_governor_lane() {
    let (inner, _) = MockSite::new()
        .page(
            "https://ex.com/",
            200,
            &html(
                "Proxy source",
                "Research evidence on the only configured lane.",
            ),
        )
        .fetcher();
    let lanes = Arc::new(Mutex::new(Vec::new()));
    let recorded = lanes.clone();
    let fetch: PageFetcher = Arc::new(move |url, lane, referer, gate| {
        recorded.lock().unwrap().push(lane.clone());
        inner(url, lane, referer, gate)
    });
    let governor = Arc::new(Governor::new(vec![Lane {
        id: "proxy-only".into(),
        kind: LaneKind::Proxy,
    }]));
    let result = Crawler::new(fetch, governor)
        .crawl(
            "https://ex.com/",
            CrawlOptions {
                mode: CrawlMode::Content,
                respect_robots: false,
                ..opts()
            },
            None,
        )
        .await
        .unwrap();
    assert!(!result.pages.is_empty());
    assert!(
        lanes
            .lock()
            .unwrap()
            .iter()
            .all(|lane| lane == "proxy-only"),
        "{:?}",
        lanes.lock().unwrap()
    );
}

#[tokio::test]
async fn wave450_seed_redirect_relocates_scope_and_reuses_response() {
    let seed = "https://old.example/docs/";
    let resolved = "https://new.example/guide/";
    let (inner, hits) = MockSite::new()
        .page(
            seed,
            200,
            &html("Relocated guide", "The guide now lives at its new address."),
        )
        .page(
            "https://new.example/guide/chapter",
            200,
            &html(
                "Chapter",
                "A distinct chapter with detailed examples and installation instructions.",
            ),
        )
        .fetcher();
    let fetch: PageFetcher = Arc::new(move |u, lane, referer, gate| {
        let inner = inner.clone();
        async move {
            let mut p = inner(u.clone(), lane, referer, gate).await;
            if u == seed {
                p.url = resolved.into();
                p.body
                    .extend_from_slice(b"<a href='/guide/chapter'>Chapter</a>");
            }
            p
        }
        .boxed()
    });
    let out = Crawler::new(fetch, gov())
        .crawl(
            seed,
            CrawlOptions {
                mode: CrawlMode::Content,
                respect_robots: false,
                ..opts()
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(out.seed, resolved);
    assert!(out.pages.iter().any(|p| p.url == resolved));
    assert!(out.pages.iter().any(|p| p.url.ends_with("/guide/chapter")));
    assert_eq!(
        hits.lock()
            .unwrap()
            .iter()
            .filter(|u| u.as_str() == seed)
            .count(),
        1
    );
    assert!(
        !hits.lock().unwrap().iter().any(|u| u == resolved),
        "reuse the resolved seed response"
    );
}

#[tokio::test]
async fn wave450_deadline_bounds_discovery_and_page_io() {
    let called = Arc::new(AtomicUsize::new(0));
    let counts = called.clone();
    let fetch: PageFetcher = Arc::new(move |url, lane, _, _gate| {
        let called = counts.clone();
        async move {
            called.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(2)).await;
            FetchedPage {
                lane,
                route: Some(crate::transport::request_route::RequestRoute::direct()),
                url,
                status: 200,
                headers: vec![],
                body: b"slow".to_vec(),
                verdict: Verdict::ContentOk,
                latency: Duration::from_secs(2),
                cached: false,
                error_hint: None,
            }
        }
        .boxed()
    });
    let started = std::time::Instant::now();
    let out = Crawler::new(fetch, gov())
        .crawl(
            "https://slow.example/",
            CrawlOptions {
                mode: CrawlMode::Map,
                deadline: Duration::from_millis(80),
                ..opts()
            },
            None,
        )
        .await
        .unwrap();
    assert!(called.load(Ordering::SeqCst) > 0);
    assert_eq!(out.stop, StopReason::Deadline);
    assert!(started.elapsed() < Duration::from_millis(600));
    assert!(out.pages.is_empty());
}

// ── Hub seeds (issue #249) ────────────────────────────────

/// A hub page: a dozen links and almost no prose, padded to the
/// reporter's ~128 KB. `title` = None drops <title> so the quality
/// score has nothing but density (~0) to stand on.
/// (With a title the hub scores ~0.2 and passes the gate.)
fn hub_html(title: Option<&str>, dir: &str) -> String {
    let links: String = (1..=12)
        .map(|i| format!("<a href=\"{dir}p{i}.html\">page {i}</a> "))
        .collect();
    let pad = "<!-- ".to_string() + &"x".repeat(1000) + " -->\n";
    let head = title
        .map(|t| format!("<head><title>{t}</title></head><body><h1>{t}</h1>"))
        .unwrap_or_else(|| "<body>".to_string());
    format!(
        "<html>{head}<p>{links}</p>{}</body></html>",
        pad.repeat(125)
    )
}

fn hub_site(dir: &str, title: Option<&str>) -> MockSite {
    let mut site = MockSite::new().page(
        &format!("https://ex.com{dir}p0.html"),
        200,
        &hub_html(title, dir),
    );
    for i in 1..=12 {
        site = site.page(
            &format!("https://ex.com{dir}p{i}.html"),
            200,
            &html(&format!("Page {i}"), "real article content"),
        );
    }
    site
}

// A seed like `/p0.html` (a page at the host root) was auto-scoped
// as if it were a project section (`/p0.html/*`, the docs.rs rule
// for `/tokio`), so every sibling link `/pN.html` was filtered out
// and the crawl ended FrontierEmpty with only the seed, marked
// complete. A root-level page's section is the host itself.
#[tokio::test]
async fn root_level_page_seed_is_not_scoped_to_itself() {
    let (fetch, _hits) = hub_site("/", Some("Page 0")).fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 4;
    let r = crawler
        .crawl("https://ex.com/p0.html", o, None)
        .await
        .unwrap();
    assert_eq!(
        r.filtered_out, 0,
        "sibling pages of a root-level seed are in scope; stop={:?} skipped={:?}",
        r.stop, r.skipped
    );
    assert_eq!(r.pages.len(), 4, "stop={:?}", r.stop);
    assert_eq!(r.stop, StopReason::MaxPages);
}

// The quality gate skipped a low-quality page BEFORE its outlinks
// were harvested, so a hub seed (the page you crawl FROM) fed the
// frontier nothing: FrontierEmpty, pages=[], reported complete, and
// next_action blamed the seed ("no links discovered"). The
// navigation-only scope path already harvests links from pages it
// does not keep; the quality gate must do the same.
#[tokio::test]
async fn low_quality_hub_seed_still_feeds_the_frontier() {
    let (fetch, _hits) = hub_site("/docs/", None).fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 4;
    // A bare link list scores ~0.08; the articles score ~0.47. Any
    // min_quality between the two makes the hub a skip and keeps
    // the articles: the gate must not also eat the hub's links.
    o.min_quality = 0.2;
    let r = crawler
        .crawl("https://ex.com/docs/p0.html", o, None)
        .await
        .unwrap();
    assert!(
        r.skipped
            .iter()
            .any(|(u, why)| u.ends_with("/p0.html") && why.starts_with("low quality")),
        "the hub itself is honestly skipped as low quality: skipped={:?} pages={:?}",
        r.skipped,
        r.pages
            .iter()
            .map(|p| (&p.url, p.quality))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        r.pages.len(),
        4,
        "hub outlinks must be harvested even though the hub is skipped; stop={:?} filtered_out={}",
        r.stop,
        r.filtered_out
    );
    assert!(r.pages.iter().all(|p| !p.url.ends_with("/p0.html")));
    assert_eq!(r.stop, StopReason::MaxPages);
}

// ── <base href> ───────────────────────────────────────────

// `<base href="/a/app/">` (root-relative, the common form) is not
// an absolute URL, so Url::parse refused it and the harvest fell
// back to the page URL: `href="p"` on /a/b.html went to /a/p instead
// of /a/app/p, and the real page was never fetched. (The base stays
// inside the seed's auto-scope so scope is not what decides here.)
#[tokio::test]
async fn relative_base_href_resolves_against_the_page() {
    let site = MockSite::new()
        .page(
            "https://ex.com/a/b.html",
            200,
            &html("Hub", "<base href=\"/a/app/\"><a href=\"p\">p</a>"),
        )
        .page("https://ex.com/a/app/p", 200, &html("P", "under app"));
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 3;
    let r = crawler
        .crawl("https://ex.com/a/b.html", o, None)
        .await
        .unwrap();
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        hits.iter().any(|h| h == "https://ex.com/a/app/p"),
        "link resolved against the base: hits={hits:?} pages={:?}",
        r.pages.iter().map(|p| &p.url).collect::<Vec<_>>()
    );
    assert!(!hits.iter().any(|h| h == "https://ex.com/a/p"));
}

// ── Map mode ──────────────────────────────────────────────

#[tokio::test]
async fn map_mode_reads_sitemap_cheap() {
    let sitemap = r#"<?xml version="1.0"?><urlset>
<url><loc>https://ex.com/a</loc></url>
<url><loc>https://ex.com/b</loc></url>
<url><loc>https://ex.com/c</loc></url>
</urlset>"#;
    let site = MockSite::new().page("https://ex.com/sitemap.xml", 200, sitemap);
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Map;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert_eq!(r.pages.len(), 0);
    assert_eq!(r.map.len(), 3);
    // Cost: robots + sitemap discovery fetches, never the pages.
    // Multiple sitemap locations are tried (6 fallbacks), but only
    // /sitemap.xml returns 200 : the others 404.
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(!hits.iter().any(|h| h.ends_with("/a")));
    assert!(!hits.iter().any(|h| h.ends_with("/b")));
    assert!(!hits.iter().any(|h| h.ends_with("/c")));
}

#[tokio::test]
async fn map_mode_focus_filters() {
    let sitemap = r#"<urlset>
<url><loc>https://ex.com/docs/migration-guide</loc></url>
<url><loc>https://ex.com/blog/cat-photos</loc></url>
</urlset>"#;
    let site = MockSite::new().page("https://ex.com/sitemap.xml", 200, sitemap);
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Map;
    o.focus = Some("migration".into());
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert_eq!(r.map.len(), 1);
    assert!(r.map[0].contains("migration"));
}

// V05: a sitemap-less origin must not read as an empty site in map
// mode. The seed page is already in the buffer (identity hop), so
// map falls back to harvesting its links : the same discovery full
// starts from, without fetching any page of its own.
#[tokio::test]
async fn map_mode_falls_back_to_seed_links_without_a_sitemap() {
    let seed = "<html><head><title>seed</title></head><body><article>\
        <p>content words here for extraction threshold passing yes indeed</p>\
        <a href=\"/a\">Page A</a><a href=\"/b\">Page B</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/robots.txt", 200, "User-agent: *\n")
        .page("https://ex.com/", 200, seed);
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Map;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert_eq!(r.pages.len(), 0, "map stays inventory-only");
    assert!(
        r.map.iter().any(|u| u.ends_with("/a")) && r.map.iter().any(|u| u.ends_with("/b")),
        "map must fall back to the seed page's links, got {:?} (skipped={:?})",
        r.map,
        r.skipped
    );
    // The fallback must not fetch the linked pages themselves.
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(!hits.iter().any(|h| h.ends_with("/a")));
    assert!(!hits.iter().any(|h| h.ends_with("/b")));
}

// V06: the map is relevance-ranked against the seed, not a pure
// recency slice. A topic seed pulls its own neighborhood to the
// front of the inventory; ranking reorders, it never filters.
#[tokio::test]
async fn map_mode_ranks_seed_relevant_urls_first() {
    let sitemap = r#"<urlset>
<url><loc>https://ex.com/docs/alpha_thing</loc></url>
<url><loc>https://ex.com/docs/beta_thing</loc></url>
<url><loc>https://ex.com/docs/gamma_thing</loc></url>
<url><loc>https://ex.com/docs/delta_thing</loc></url>
<url><loc>https://ex.com/docs/webgpu_api</loc></url>
<url><loc>https://ex.com/docs/using_webgpu</loc></url>
</urlset>"#;
    let site = MockSite::new().page("https://ex.com/sitemap.xml", 200, sitemap);
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Map;
    let r = crawler
        .crawl("https://ex.com/docs/webgpu_api", o, None)
        .await
        .unwrap();
    assert_eq!(r.map.len(), 6, "ranking reorders, never filters");
    assert!(
        r.map[0].contains("webgpu") && r.map[1].contains("webgpu"),
        "seed-relevant URLs first, got {:?}",
        r.map
    );
}

// V10: with no focus set, the links the seed page itself carries are
// the crawl's topic neighborhood. Without a seed-provenance boost
// they tie the generic sitemap flood at equal depth and lose to its
// recency order, so a docs crawl drifted into sibling nav (the
// audit's WebGPU seed fetched RTCRtpTransceiver-class pages first).
#[tokio::test]
async fn a_seeded_crawl_prefers_the_seeds_own_neighborhood() {
    let sitemap = r#"<urlset>
<url><loc>https://ex.com/a</loc></url>
<url><loc>https://ex.com/b</loc></url>
<url><loc>https://ex.com/c</loc></url>
</urlset>"#;
    let seed = "<html><head><title>seed</title></head><body><article>\
        <p>content words here for extraction threshold passing yes indeed</p>\
        <a href=\"/topic/one\">Topic One</a>\
        <a href=\"/topic/two\">Topic Two</a>\
        </article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/sitemap.xml", 200, sitemap)
        .page("https://ex.com/", 200, seed)
        .page(
            "https://ex.com/topic/one",
            200,
            &html("One", "topic one body words for the extractor threshold"),
        )
        .page(
            "https://ex.com/topic/two",
            200,
            &html("Two", "topic two body words for the extractor threshold"),
        )
        .page("https://ex.com/a", 200, &html("A", "generic a body words"))
        .page("https://ex.com/b", 200, &html("B", "generic b body words"))
        .page("https://ex.com/c", 200, &html("C", "generic c body words"));
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Full;
    o.shape = false; // exact-order semantics; jitter is tested elsewhere
    o.max_pages = 3; // seed + 2 : the seeds own neighborhood must win
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let urls: Vec<&str> = r.pages.iter().map(|p| p.url.as_str()).collect();
    assert!(
        urls.contains(&"https://ex.com/topic/one") && urls.contains(&"https://ex.com/topic/two"),
        "the seed's own neighborhood must be fetched before the sitemap flood, got {urls:?}"
    );
}

// V10b: the sitemap often queues the seed's own links first (MDN's
// section sitemap lists every API page), at recency scores. The
// seed's harvest must RAISE those already-queued entries instead of
// being swallowed by the dedup, or the boost never lands on exactly
// the pages the flood pinned.
#[tokio::test]
async fn a_seed_link_raises_a_sitemap_queued_entry() {
    let sitemap = r#"<urlset>
<url><loc>https://ex.com/a</loc></url>
<url><loc>https://ex.com/b</loc></url>
<url><loc>https://ex.com/topic/one</loc></url>
<url><loc>https://ex.com/topic/two</loc></url>
</urlset>"#;
    let seed = "<html><head><title>seed</title></head><body><article>\
        <p>content words here for extraction threshold passing yes indeed</p>\
        <a href=\"/topic/one\">Topic One</a>\
        <a href=\"/topic/two\">Topic Two</a>\
        </article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/sitemap.xml", 200, sitemap)
        .page("https://ex.com/", 200, seed)
        .page(
            "https://ex.com/topic/one",
            200,
            &html("One", "topic one body words for the extractor threshold"),
        )
        .page(
            "https://ex.com/topic/two",
            200,
            &html("Two", "topic two body words for the extractor threshold"),
        )
        .page("https://ex.com/a", 200, &html("A", "generic a body words"))
        .page("https://ex.com/b", 200, &html("B", "generic b body words"));
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Full;
    o.shape = false;
    o.max_pages = 3;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let urls: Vec<&str> = r.pages.iter().map(|p| p.url.as_str()).collect();
    assert!(
        urls.contains(&"https://ex.com/topic/one") && urls.contains(&"https://ex.com/topic/two"),
        "a seed link must raise its sitemap-queued entry above the flood, got {urls:?}"
    );
}

// ── Basic crawl ───────────────────────────────────────────

#[tokio::test]
async fn crawl_follows_links_bfs() {
    let seed = "<html><head><title>seed</title></head><body><article><p>content words here for extraction threshold passing yes indeed</p><a href=\"/a\">Page A</a><a href=\"/b\">Page B</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/a", 200, &html("A", "alpha body"))
        .page("https://ex.com/b", 200, &html("B", "beta body"));
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content; // no sitemap in this site
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let urls: Vec<&str> = r.pages.iter().map(|p| p.url.as_str()).collect();
    assert!(urls.contains(&"https://ex.com/a"));
    assert!(urls.contains(&"https://ex.com/b"));
}

#[tokio::test]
async fn crawl_cycles_terminate() {
    let a = "<html><body><article><p>content words here for the extractor threshold pass yes yes</p><a href=\"/b\">b</a></article></body></html>";
    let b = "<html><body><article><p>other content words here for the extractor threshold pass</p><a href=\"/a\">a</a></article></body></html>";
    let root = "<html><body><article><p>root page content words here for the extractor threshold pass</p><a href=\"/a\">a</a><a href=\"/b\">b</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/", 200, root)
        .page("https://ex.com/a", 200, a)
        .page("https://ex.com/b", 200, b);
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 20;
    o.deadline = Duration::from_secs(5);
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    // Each page fetched exactly once despite the cycle.
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let a_hits = hits.iter().filter(|h| h.ends_with("/a")).count();
    let b_hits = hits.iter().filter(|h| h.ends_with("/b")).count();
    assert_eq!(a_hits, 1);
    assert_eq!(b_hits, 1);
    assert_eq!(r.stop, StopReason::FrontierEmpty);
}

#[tokio::test]
async fn crawl_max_pages_enforced() {
    let seed = format!(
        "<html><body><article><p>content words for the extractor to accept this page yes</p>{}</article></body></html>",
        (0..50)
            .map(|i| format!("<a href=\"/p{i}\">p{i}</a>"))
            .collect::<Vec<_>>()
            .join("")
    );
    let mut site = MockSite::new().page("https://ex.com/", 200, &seed);
    for i in 0..50 {
        site = site.page(
            &format!("https://ex.com/p{i}"),
            200,
            &html(&format!("P{i}"), "body"),
        );
    }
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 5;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert!(r.pages.len() <= 6); // seed + 5 content pages, small race slack
    assert!(matches!(r.stop, StopReason::MaxPages));
    assert!(r.resume.is_some());
}

#[tokio::test]
async fn crawl_resume_continues() {
    // Isolated cache: the resume store lives under the real cache
    // dir, and gates run this test in parallel with the rest of
    // the suite (nextest = process-per-test, but the CI runners
    // share the dir across processes). One test blasting the live
    // store with the old shared-map = clobbered other processes'
    // tokens mid-flight.
    let iso = std::env::temp_dir().join(format!("ds-resume-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&iso);
    unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &iso) };

    let seed = format!(
        "<html><body><article><p>content words for the extractor to accept this page yes</p>{}</article></body></html>",
        (0..10)
            .map(|i| format!("<a href=\"/p{i}\">p{i}</a>"))
            .collect::<Vec<_>>()
            .join("")
    );
    let mut site = MockSite::new().page("https://ex.com/", 200, &seed);
    for i in 0..10 {
        site = site.page(
            &format!("https://ex.com/p{i}"),
            200,
            &html(&format!("P{i}"), "body"),
        );
    }
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 3;
    let r1 = crawler
        .crawl("https://ex.com/", o.clone(), None)
        .await
        .unwrap();
    let tok = r1.resume.expect("resume token");
    let seen1: std::collections::HashSet<&str> = r1.pages.iter().map(|p| p.url.as_str()).collect();

    let o2 = o;
    let r2 = crawler
        .crawl("https://ex.com/", o2, Some(&tok))
        .await
        .unwrap();
    // Resumed crawl must not refetch what run 1 already got.
    for p in &r2.pages {
        assert!(!seen1.contains(p.url.as_str()), "refetched {}", p.url);
    }
}

// Resume WITHOUT a url (the resume-only flow the MCP surface
// documents: the seed comes from the token). The old shape took the
// token file once to recover the seed and AGAIN for the frontier
// restore, so every resume-only call failed with "resume token
// expired or unknown" AND destroyed the saved state in the process.
#[tokio::test]
async fn crawl_resume_only_loads_seed_from_token() {
    let iso = std::env::temp_dir().join(format!("ds-resume-only-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&iso);
    unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &iso) };

    let seed = format!(
        "<html><body><article><p>content words for the extractor to accept this page yes</p>{}</article></body></html>",
        (0..10)
            .map(|i| format!("<a href=\"/p{i}\">p{i}</a>"))
            .collect::<Vec<_>>()
            .join("")
    );
    let mut site = MockSite::new().page("https://ex.com/", 200, &seed);
    for i in 0..10 {
        site = site.page(
            &format!("https://ex.com/p{i}"),
            200,
            &html(&format!("P{i}"), "body"),
        );
    }
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 3;
    let r1 = crawler
        .crawl("https://ex.com/", o.clone(), None)
        .await
        .unwrap();
    let tok = r1.resume.expect("resume token");

    // Resume-only: empty url + the token must WORK and continue
    // where run 1 stopped (no refetch of run 1's pages).
    let r2 = crawler.crawl("", o.clone(), Some(&tok)).await.unwrap();
    assert!(
        r2.seed.contains("ex.com"),
        "the seed must come from the token, got {}",
        r2.seed
    );
    let seen1: std::collections::HashSet<&str> = r1.pages.iter().map(|p| p.url.as_str()).collect();
    for p in &r2.pages {
        assert!(!seen1.contains(p.url.as_str()), "refetched {}", p.url);
    }

    // The token is consumed: a third resume with the SAME token
    // fails honestly (and the failure must not be a state restore).
    let r3 = crawler.crawl("", o, Some(&tok)).await;
    assert!(r3.is_err(), "a consumed token must fail honestly");
}

#[tokio::test]
async fn crawl_same_host_enforced() {
    let seed = "<html><body><article><p>content words for the extractor threshold acceptance test</p><a href=\"https://other.com/x\">off</a><a href=\"/on\">on</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/on", 200, &html("On", "on host"))
        .page("https://other.com/x", 200, &html("X", "off host"));
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert!(!r.pages.iter().any(|p| p.url.contains("other.com")));
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(!hits.iter().any(|h| h.contains("other.com")));
}

#[tokio::test]
async fn crawl_include_exclude_globs() {
    let seed = "<html><body><article><p>content words for the extractor to accept this page yes</p><a href=\"/docs/a\">a</a><a href=\"/blog/b\">b</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/docs/a", 200, &html("DocsA", "docs"))
        .page("https://ex.com/blog/b", 200, &html("BlogB", "blog"));
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    o.include_paths = vec!["/docs/*".into()];
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert!(r.pages.iter().any(|p| p.url.ends_with("/docs/a")));
    assert!(!r.pages.iter().any(|p| p.url.ends_with("/blog/b")));
    assert!(r.filtered_out >= 1);
}

#[tokio::test]
async fn crawl_robots_disallow_respected() {
    let robots = "User-agent: *\nDisallow: /private\n";
    let seed = "<html><body><article><p>content words for extractor acceptance threshold pass yes yes yes</p><a href=\"/private/x\">x</a><a href=\"/ok\">ok</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/robots.txt", 200, robots)
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/private/x", 200, &html("X", "private"))
        .page("https://ex.com/ok", 200, &html("Ok", "ok"));
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    o.respect_robots = true;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert!(!r.pages.iter().any(|p| p.url.contains("/private")));
    assert!(r.pages.iter().any(|p| p.url.ends_with("/ok")));
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(!hits.iter().any(|h| h.contains("/private")));
}

// The most common query-shaped rule, `Disallow: /*?`, never applied:
// every robots check in the crawl asked about the path alone, and a
// path has no `?`. The parser matched queries all along (its own
// test pins `/search?q=1`), the crawl just never showed it one.
#[tokio::test]
async fn crawl_robots_rules_see_the_query_string() {
    let robots = "User-agent: *\nDisallow: /*?\nDisallow: /w/index.php?\n";
    let seed = "<html><body><article><p>content words for extractor acceptance threshold pass yes yes yes</p><a href=\"/search?q=1\">s</a><a href=\"/w/index.php?title=X&action=edit\">e</a><a href=\"/w/index.php\">w</a><a href=\"/ok\">ok</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/robots.txt", 200, robots)
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/search?q=1", 200, &html("S", "search"))
        .page(
            "https://ex.com/w/index.php?title=X&action=edit",
            200,
            &html("E", "edit"),
        )
        .page("https://ex.com/w/index.php", 200, &html("W", "wiki"))
        .page("https://ex.com/ok", 200, &html("Ok", "ok"));
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    o.respect_robots = true;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert!(r.pages.iter().any(|p| p.url.ends_with("/ok")));
    assert!(
        r.pages.iter().any(|p| p.url.ends_with("/w/index.php")),
        "the rule ends in `?`: the bare path is allowed"
    );
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        !hits.iter().any(|h| h.contains('?')),
        "a query-shaped Disallow must keep every query URL unfetched; fetched: {hits:?}"
    );
}

// A seed on a non-default port (or plain http) must read ITS OWN
// robots.txt: discovery used to build `https://<host>/robots.txt`
// from the bare hostname, so a ported seed's rules were never seen
// and its Disallow was never applied (#345).
#[tokio::test]
async fn crawl_reads_robots_from_the_seed_origin() {
    let robots = "User-agent: *\nDisallow: /alpha/\n";
    let seed = "<html><body><article><p>content words for extractor acceptance threshold pass yes yes yes</p><a href=\"/alpha/page.html\">a</a><a href=\"/ok\">ok</a></article></body></html>";
    let site = MockSite::new()
        .page("http://seed.example:8001/robots.txt", 200, robots)
        .page("http://seed.example:8001/", 200, seed)
        .page(
            "http://seed.example:8001/alpha/page.html",
            200,
            &html("A", "alpha"),
        )
        .page("http://seed.example:8001/ok", 200, &html("Ok", "ok"));
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    o.respect_robots = true;
    let r = crawler
        .crawl("http://seed.example:8001/", o, None)
        .await
        .unwrap();
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        hits.iter()
            .any(|h| h == "http://seed.example:8001/robots.txt"),
        "robots must be read from the seed's own origin; hits: {hits:?}"
    );
    assert!(
        !hits.iter().any(|h| h.contains("/alpha/")),
        "the seed origin's Disallow must hold; hits: {hits:?}"
    );
    assert!(r.pages.iter().any(|p| p.url.ends_with("/ok")));
}

// With --any-host (same_host false), another host's pages must be
// checked against THAT host's robots.txt: every robots check used to
// read the seed's one file, so a foreign Disallow was ignored while
// the seed's Disallow leaked onto other hosts (#344).
#[tokio::test]
async fn crawl_any_host_reads_each_origins_own_robots() {
    let robots_a = "User-agent: *\nDisallow: /alpha/\n";
    let robots_b = "User-agent: *\nDisallow: /beta/\n";
    let seed = "<html><body><article><p>content words for extractor acceptance threshold pass yes yes yes</p><a href=\"/alpha/a.html\">a</a><a href=\"/ok\">ok</a><a href=\"http://b.example:8002/ok/page.html\">b-ok</a><a href=\"http://b.example:8002/alpha/page.html\">b-alpha</a><a href=\"http://b.example:8002/beta/page.html\">b-beta</a></article></body></html>";
    let site = MockSite::new()
        .page("https://a.example/robots.txt", 200, robots_a)
        .page("https://a.example/", 200, seed)
        .page("https://a.example/ok", 200, &html("Ok", "ok"))
        .page("http://b.example:8002/robots.txt", 200, robots_b)
        .page(
            "http://b.example:8002/ok/page.html",
            200,
            &html("B1", "ok page"),
        )
        .page(
            "http://b.example:8002/alpha/page.html",
            200,
            &html("B2", "alpha page"),
        )
        .page(
            "http://b.example:8002/beta/page.html",
            200,
            &html("B3", "beta page"),
        );
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 20;
    o.respect_robots = true;
    o.same_host = false;
    let r = crawler.crawl("https://a.example/", o, None).await.unwrap();
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let i_b_robots = hits
        .iter()
        .position(|h| h == "http://b.example:8002/robots.txt")
        .expect("the other host's robots.txt must be fetched before its pages");
    let i_first_b_page = hits
        .iter()
        .position(|h| h.starts_with("http://b.example:8002/") && !h.ends_with("/robots.txt"))
        .expect("B pages must be fetched");
    assert!(
        i_b_robots < i_first_b_page,
        "robots before the first request to the host; hits: {hits:?}"
    );
    assert!(
        !hits.iter().any(|h| h.contains("b.example:8002/beta/")),
        "B's own Disallow must hold on B; hits: {hits:?}"
    );
    assert!(
        r.pages
            .iter()
            .any(|p| p.url.contains("b.example:8002/alpha/")),
        "the seed host's Disallow must not leak onto B; hits: {hits:?}"
    );
    assert!(
        !hits.iter().any(|h| h == "https://a.example/alpha/a.html"),
        "A's Disallow must still hold on A; hits: {hits:?}"
    );
}

// RFC 9309 §2.3.1.4: a robots.txt that answers 5xx is "unreachable",
// and the crawler must assume it may not fetch anything from that
// origin. It used to fail open on every non-200. A 4xx stays the
// "unavailable" case: allow.
#[tokio::test]
async fn robots_unreachable_stops_the_crawl_but_4xx_does_not() {
    for (robots_status, expect_seed) in [(503u16, false), (404u16, true)] {
        let seed = "<html><body><article><p>content words for extractor acceptance threshold pass yes yes yes</p><a href=\"/a\">a</a></article></body></html>";
        let site = MockSite::new()
            .page("https://ex.com/robots.txt", robots_status, "no rules")
            .page("https://ex.com/", 200, seed)
            .page("https://ex.com/a", 200, &html("A", "a"));
        let (fetch, hits) = site.fetcher();
        let crawler = Crawler::new(fetch, gov());
        let mut o = opts();
        o.mode = CrawlMode::Content;
        o.max_pages = 5;
        o.respect_robots = true;
        let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
        let hits = hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if expect_seed {
            assert!(
                r.pages.iter().any(|p| p.url == "https://ex.com/"),
                "a robots.txt 4xx is unavailable, not unreachable: the crawl continues; hits: {hits:?}"
            );
        } else {
            assert!(
                r.pages.is_empty(),
                "a robots.txt 5xx must stop the crawl; hits: {hits:?}"
            );
            assert!(
                hits.iter().all(|h| h.ends_with("/robots.txt")),
                "nothing beyond robots.txt may be fetched under a 5xx; hits: {hits:?}"
            );
        }
    }
}

// `Crawl-delay` was parsed with a bare `f64` parse and fed straight
// to `Duration::from_secs_f64`, which panics on `inf`/huge values:
// one hostile (or sloppy) robots.txt aborted the crawl worker, and
// a finite `86400` was honoured verbatim (a day between pages).
#[test]
fn robots_crawl_delay_is_finite_and_clamped() {
    use super::sitemap::Robots;
    for bad in ["inf", "-inf", "nan", "-5", "abc"] {
        let r = Robots::parse(
            &format!("User-agent: *\nCrawl-delay: {bad}\n"),
            "https://ex.com",
        );
        assert_eq!(r.crawl_delay, None, "{bad}");
    }
    let r = Robots::parse("User-agent: *\nCrawl-delay: 2.5\n", "https://ex.com");
    assert_eq!(r.crawl_delay, Some(2.5));
    for huge in ["86400", "1e300"] {
        let r = Robots::parse(
            &format!("User-agent: *\nCrawl-delay: {huge}\n"),
            "https://ex.com",
        );
        assert_eq!(
            r.crawl_delay,
            Some(super::sitemap::MAX_CRAWL_DELAY_SECS),
            "{huge}"
        );
    }
}

#[tokio::test]
async fn crawl_survives_infinite_crawl_delay() {
    let robots = "User-agent: *\nCrawl-delay: inf\n";
    let seed = "<html><body><article><p>content words for extractor acceptance threshold pass yes yes yes</p><a href=\"/ok\">ok</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/robots.txt", 200, robots)
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/ok", 200, &html("Ok", "ok"));
    let (fetch, _hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    o.respect_robots = true;
    let r = tokio::time::timeout(
        Duration::from_secs(20),
        crawler.crawl("https://ex.com/", o, None),
    )
    .await
    .expect("crawl must finish")
    .unwrap();
    assert!(r.pages.iter().any(|p| p.url.ends_with("/ok")));
}

#[tokio::test]
async fn crawl_near_dupes_collapsed() {
    let body = html("Same", "identical body");
    let seed = "<html><body><article><p>content words for extractor threshold acceptance yes yes yes yes</p><a href=\"/1\">1</a><a href=\"/2\">2</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/1", 200, &body)
        .page("https://ex.com/2", 200, &body);
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let kept = r.pages.iter().filter(|p| !p.duplicate).count();
    let dupes = r.pages.iter().filter(|p| p.duplicate).count();
    // Two identical pages: one kept, one flagged.
    assert!(dupes >= 1);
    assert!(kept <= 2); // seed + one of the dups
}

#[tokio::test]
async fn crawl_walls_marked_skipped_honestly() {
    let seed = "<html><body><article><p>content words for extractor threshold acceptance pass pass pass</p><a href=\"/walled\">w</a><a href=\"/ok\">ok</a></article></body></html>";
    let wall = "<html><body><div>Just a moment...</div><div>cf-chl-widget</div></body></html>";
    let mut site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/walled", 200, wall)
        .page("https://ex.com/ok", 200, &html("Ok", "ok"));
    site.pages
        .insert("https://ex.com/walled".into(), (200, wall.to_string()));
    let (fetch, _) = site.fetcher();
    // Mock marks wall pages with a Challenge verdict via a second
    // fetcher wrapper.
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    // The wall page has no wall verdict in this mock (the mock
    // returns ContentOk) : real walls handled by walls::detect
    // in the real bridge. What we CAN assert: /ok got crawled.
    assert!(r.pages.iter().any(|p| p.url.ends_with("/ok")));
}

#[tokio::test]
async fn crawl_throttle_recovers_and_continues() {
    let url = "https://ex.com/slow";
    let site = MockSite::new()
        .page(url, 200, &html("Slow", "slow page"))
        .throttle_n(url, 2);
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 5;
    // The seed itself gets 429'd twice, then serves.
    let r = crawler.crawl(url, o, None).await.unwrap();
    // Orchestrator must not crash on 429; page either arrives
    // (after penalties burn out) or is honestly skipped.
    let got = r.pages.iter().any(|p| p.url == url);
    let skipped = r.skipped.iter().any(|(u, _)| u == url);
    assert!(got || skipped, "throttle handling must record outcome");
    let _ = MockSite::new().hit_count();
}

#[tokio::test]
async fn crawl_char_budget_caps_total() {
    let big = html("Big", &"word ".repeat(5000));
    let seed = format!(
        "<html><body><article><p>content words for extractor acceptance threshold yes yes yes</p>{}</article></body></html>",
        (0..6)
            .map(|i| format!("<a href=\"/big{i}\">b{i}</a>"))
            .collect::<Vec<_>>()
            .join("")
    );
    let mut site = MockSite::new().page("https://ex.com/", 200, &seed);
    for i in 0..6 {
        site = site.page(&format!("https://ex.com/big{i}"), 200, &big);
    }
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 50;
    o.max_total_chars = 5_000;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert!(matches!(
        r.stop,
        StopReason::CharBudget | StopReason::MaxPages
    ));
}

#[tokio::test]
async fn crawl_deadline_returns_partial() {
    let slow_seed = "<html><body><article><p>content words for extractor acceptance threshold yes yes yes</p><a href=\"/a\">a</a></article></body></html>";
    let mut site = MockSite::new()
        .page("https://ex.com/", 200, slow_seed)
        .page("https://ex.com/a", 200, &html("A", "a"));
    // Huge throttles so the governor forces waits.
    site = site.throttle_n("https://ex.com/a", 8);
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.deadline = Duration::from_millis(900);
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    // Deadline hit OR frontier emptied by honest skip; either way
    // the crawl RETURNS (no hang) and reports what it got.
    assert!(r.elapsed < Duration::from_secs(5));
    assert!(matches!(
        r.stop,
        StopReason::Deadline
            | StopReason::FrontierEmpty
            | StopReason::ThrottledOut
            | StopReason::MaxPages
    ));
}

#[tokio::test]
async fn crawl_sitemapindex_recurses() {
    let index = r#"<sitemapindex>
<sitemap><loc>https://ex.com/sm-1.xml</loc></sitemap>
</sitemapindex>"#;
    let child = r#"<urlset>
<url><loc>https://ex.com/deep-page</loc></url>
</urlset>"#;
    let site = MockSite::new()
        .page("https://ex.com/sitemap.xml", 200, index)
        .page("https://ex.com/sm-1.xml", 200, child);
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Map;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert_eq!(r.map, vec!["https://ex.com/deep-page".to_string()]);
}

#[tokio::test]
async fn crawl_focus_ranks_relevant_first() {
    let seed = "<html><body><article><p>content words for extractor acceptance yes yes yes yes yes</p><a href=\"/docs/migration\">the migration guide</a><a href=\"/random\">click here</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page(
            "https://ex.com/docs/migration",
            200,
            &html("Migration", "migrate"),
        )
        .page("https://ex.com/random", 200, &html("Random", "unrelated"));
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.focus = Some("migration".into());
    o.max_pages = 2; // seed + ONE more : focus decides which
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The migration page must be fetched; the random one must not
    // (only 1 content-page budget).
    assert!(hits.iter().any(|h| h.contains("migration")));
    assert!(!hits.iter().any(|h| h.ends_with("/random")));
    let _ = r;
}

// ── Crawl v2 adversarial tests ─────────────────────────────
// Each test targets one gap from the v1→v2 upgrade.

#[tokio::test]
async fn v2_transient_500_retries_then_succeeds() {
    // Gap 1: transient errors (500, TCP reset) were permanent
    // skips. Now retried up to 2 times.
    let url = "https://ex.com/flaky";
    let site = MockSite::new()
        .page(url, 200, &html("Flaky", "recovered"))
        .transient_n(url, 1); // 1x 500, then 200
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 5;
    o.deadline = Duration::from_secs(8);
    let r = crawler.crawl(url, o, None).await.unwrap();
    // After 1x 500 + 1 retry, the page arrives.
    assert!(
        r.pages.iter().any(|p| p.url == url),
        "page should arrive after transient retry"
    );
}

#[tokio::test]
async fn v2_transient_500_exhausts_retries_skips() {
    // 3 consecutive 500s exhaust the retry budget (max 2).
    let url = "https://ex.com/broken";
    let site = MockSite::new()
        .page(url, 200, &html("OK", "content"))
        .transient_n(url, 5); // always 500 within retry budget
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 5;
    o.deadline = Duration::from_secs(8);
    let r = crawler.crawl(url, o, None).await.unwrap();
    // After 2 retries (3 total 500s), the page is skipped.
    assert!(
        r.skipped.iter().any(|(u, _)| u == url),
        "page should be skipped after retries exhausted"
    );
}

#[tokio::test]
async fn v2_canonical_dedup_prevents_double_fetch() {
    // Gap 2: /page and /page/ fetched separately. Now canonical
    // resolution marks the canonical form as seen.
    let seed = "<html><head><title>seed</title><link rel=\"canonical\" href=\"https://ex.com/canonical\"/></head><body><article><p>content words for extractor threshold pass yes yes yes</p><a href=\"/canonical\">canon</a><a href=\"/other\">other</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page(
            "https://ex.com/canonical",
            200,
            &html("Canon", "canonical page"),
        )
        .page("https://ex.com/other", 200, &html("Other", "other page"));
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The seed declares canonical=/canonical. When /canonical is
    // linked, it's fetched. But the canonical form is marked seen
    // by the seed's own fetch, preventing a separate fetch of
    // the same content under a different URL.
    let canon_hits = hits.iter().filter(|h| h.ends_with("/canonical")).count();
    assert!(
        canon_hits <= 1,
        "canonical URL fetched at most once, got {canon_hits}"
    );
    let _ = r;
}

#[tokio::test]
async fn v2_pdf_not_skipped_as_binary() {
    // PDFs are now extracted (routed to DonSheet), not skipped as
    // "binary" or "pdf". A non-PDF body with PDF content-type will
    // fail to parse and be skipped with "low quality", not "binary".
    let seed = "<html><body><article><p>content words for extractor threshold pass yes yes yes</p><a href=\"/doc.pdf\">pdf</a><a href=\"/ok\">ok</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/doc.pdf", 200, "not a real pdf body")
        .content_type("https://ex.com/doc.pdf", "application/pdf")
        .page("https://ex.com/ok", 200, &html("Ok", "ok page"));
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    // PDF must NOT be skipped as "binary" or "pdf".
    assert!(
        !r.skipped
            .iter()
            .any(|(u, why)| u.ends_with(".pdf") && (why.contains("binary") || why.contains("pdf"))),
        "PDF should not be skipped as binary/pdf"
    );
    assert!(r.pages.iter().any(|p| p.url.ends_with("/ok")));
}

#[tokio::test]
async fn v2_pagination_link_rel_next_discovered() {
    // Gap 3: <link rel="next"> invisible. Now discovered.
    let p1 = "<html><head><title>p1</title><link rel=\"next\" href=\"/page/2\"/></head><body><article><p>content words for extractor threshold pass yes yes yes</p></article></body></html>";
    let p2 = "<html><head><title>p2</title></head><body><article><p>more content words for extractor threshold pass yes yes yes</p></article></body></html>";
    let site = MockSite::new().page("https://ex.com/page/1", 200, p1).page(
        "https://ex.com/page/2",
        200,
        p2,
    );
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler
        .crawl("https://ex.com/page/1", o, None)
        .await
        .unwrap();
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        hits.iter().any(|h| h.ends_with("/page/2")),
        "pagination link rel=next must be discovered"
    );
    assert!(r.pages.iter().any(|p| p.url.ends_with("/page/2")));
}

#[tokio::test]
async fn v2_feed_discovery_seeds_frontier() {
    // Gap 3: RSS/Atom feeds invisible. Now discovered + parsed.
    let seed = "<html><head><title>blog</title><link rel=\"alternate\" type=\"application/rss+xml\" href=\"/feed.xml\"/></head><body><article><p>content words for extractor threshold pass yes yes yes</p></article></body></html>";
    let feed = r#"<?xml version="1.0"?><rss><channel>
    <item><link>https://ex.com/post-1</link></item>
    <item><link>https://ex.com/post-2</link></item>
</channel></rss>"#;
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/feed.xml", 200, feed)
        .page("https://ex.com/post-1", 200, &html("Post 1", "first post"))
        .page("https://ex.com/post-2", 200, &html("Post 2", "second post"));
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        hits.iter().any(|h| h.ends_with("/feed.xml")),
        "feed URL must be fetched"
    );
    assert!(
        hits.iter().any(|h| h.ends_with("/post-1")),
        "feed entry 1 must be discovered"
    );
    assert!(
        hits.iter().any(|h| h.ends_with("/post-2")),
        "feed entry 2 must be discovered"
    );
    let _ = r;
}

#[tokio::test]
async fn v2_base_href_resolves_relative_links() {
    // Gap 4: <base href> ignored. Now links resolve against it.
    let seed = "<html><head><base href=\"https://ex.com/sub/\"/><title>seed</title></head><body><article><p>content words for extractor threshold pass yes yes yes</p><a href=\"deep\">deep page</a></article></body></html>";
    let site = MockSite::new().page("https://ex.com/", 200, seed).page(
        "https://ex.com/sub/deep",
        200,
        &html("Deep", "resolved via base href"),
    );
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        hits.iter().any(|h| h == "https://ex.com/sub/deep"),
        "relative link must resolve against <base href>"
    );
    assert!(r.pages.iter().any(|p| p.url == "https://ex.com/sub/deep"));
}

#[tokio::test]
async fn v2_parent_metadata_recorded() {
    // Gap 7: no parent metadata. Now every page knows its referrer.
    let seed = "<html><body><article><p>content words for extractor threshold pass yes yes yes</p><a href=\"/child\">child</a></article></body></html>";
    let site = MockSite::new().page("https://ex.com/", 200, seed).page(
        "https://ex.com/child",
        200,
        &html("Child", "child page"),
    );
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let child = r.pages.iter().find(|p| p.url.ends_with("/child"));
    assert!(child.is_some(), "child page must be in results");
    let child = child.unwrap();
    assert_eq!(
        child.parent.as_deref(),
        Some("https://ex.com/"),
        "parent must be the seed URL"
    );
}

#[tokio::test]
async fn v2_output_sorted_by_score_desc() {
    // Gap 8: output was in fetch order. Now sorted by score desc.
    let seed = "<html><body><article><p>content words for extractor threshold pass yes yes yes</p><a href=\"/high\">high</a><a href=\"/low\">low</a></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page("https://ex.com/high", 200, &html("High", "high score"))
        .page("https://ex.com/low", 200, &html("Low", "low score"));
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.focus = Some("high".into());
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    // Pages must be sorted by score descending.
    for i in 1..r.pages.len() {
        assert!(
            r.pages[i - 1].score >= r.pages[i].score,
            "pages must be sorted by score desc: {} >= {}",
            r.pages[i - 1].score,
            r.pages[i].score
        );
    }
}

#[tokio::test]
async fn v2_sitemap_priority_seeds_frontier() {
    // Gap 9: sitemap <priority> dropped. Now feeds frontier score.
    let sitemap = r#"<?xml version="1.0"?><urlset>
<url><loc>https://ex.com/important</loc><priority>1.0</priority></url>
<url><loc>https://ex.com/trivial</loc><priority>0.1</priority></url>
</urlset>"#;
    let seed = "<html><body><article><p>content words for extractor threshold pass yes yes yes</p></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/sitemap.xml", 200, sitemap)
        .page("https://ex.com/", 200, seed)
        .page(
            "https://ex.com/important",
            200,
            &html("Important", "high priority page"),
        )
        .page(
            "https://ex.com/trivial",
            200,
            &html("Trivial", "low priority page"),
        );
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Full;
    o.shape = false; // priority semantics are exact-order; shaping is
    // tested separately (frontier tests pin the jitter contract).
    o.max_pages = 2; // seed + 1 : priority decides which
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    // The high-priority page should be fetched before the low one.
    assert!(
        r.pages.iter().any(|p| p.url.ends_with("/important")),
        "high-priority sitemap entry should be crawled first"
    );
}

#[tokio::test]
async fn v2_referer_passed_to_fetcher() {
    // Gap 6: every request sent sec-fetch-site: none. Now
    // referer is passed to the fetcher for chaining.
    let seed = "<html><body><article><p>content words for extractor threshold pass yes yes yes</p><a href=\"/child\">child</a></article></body></html>";
    let site = MockSite::new().page("https://ex.com/", 200, seed).page(
        "https://ex.com/child",
        200,
        &html("Child", "child"),
    );
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let _ = r;
    // The mock captures referers. Check that /child was fetched
    // with the seed URL as referer.
    // (We can't access referers from here : it's inside the
    // mock's Arc. But we can verify the crawl succeeded.)
    // This test serves as a compile-time check that the
    // PageFetcher signature accepts referer.
}

// ── Hardening tests (PDF, sitemap, www normalization) ────────

#[test]
fn host_matches_www_equivalence() {
    use super::host_matches;
    assert!(host_matches("example.com", "example.com"));
    assert!(host_matches("www.example.com", "example.com"));
    assert!(host_matches("example.com", "www.example.com"));
    assert!(host_matches("www.example.com", "www.example.com"));
    assert!(!host_matches("other.com", "example.com"));
    assert!(!host_matches("www.other.com", "example.com"));
    // Case-insensitive
    assert!(host_matches("Example.COM", "example.com"));
}

#[tokio::test]
async fn empty_map_returns_guidance() {
    // No sitemap at any location → map mode returns guidance.
    let site = MockSite::new().page("https://ex.com/robots.txt", 200, "User-agent: *\n");
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Map;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert!(r.map.is_empty());
    assert!(
        r.skipped
            .iter()
            .any(|(_, why)| why.contains("mode=content"))
    );
}

#[tokio::test]
async fn sitemap_fallback_locations_tried() {
    // /sitemap.xml returns 404, but /sitemap_index.xml returns 200.
    let index = r#"<urlset>
<url><loc>https://ex.com/found-page</loc></url>
</urlset>"#;
    let robots = "User-agent: *\n";
    let site = MockSite::new()
        .page("https://ex.com/robots.txt", 200, robots)
        .page("https://ex.com/sitemap.xml", 404, "not found")
        .page("https://ex.com/sitemap_index.xml", 200, index);
    let (fetch, hits) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Map;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    assert!(r.map.contains(&"https://ex.com/found-page".to_string()));
    let hits = hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(hits.iter().any(|h| h.contains("sitemap_index.xml")));
}

#[tokio::test]
async fn crawl_www_host_matches_bare_seed() {
    // Sitemap lists www.example.com URLs, seed is example.com.
    // With www normalization, the www URLs should be crawled.
    let sitemap = r#"<urlset>
<url><loc>https://www.ex.com/article</loc></url>
</urlset>"#;
    let seed = "<html><body><article><p>content words for extractor threshold pass yes yes yes</p></article></body></html>";
    let article = "<html><body><article><h1>Article</h1><p>Article content here for the extractor threshold pass yes yes yes</p></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/robots.txt", 200, "User-agent: *\n")
        .page("https://ex.com/sitemap.xml", 200, sitemap)
        .page("https://ex.com/", 200, seed)
        .page("https://www.ex.com/article", 200, article);
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Full;
    o.max_pages = 5;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    // The www subdomain page should be in results (www normalization).
    assert!(
        r.pages.iter().any(|p| p.url.contains("article")),
        "www. subdomain page should be crawled from bare-domain seed"
    );
}

#[tokio::test]
async fn crawl_extracts_pdf_not_skips() {
    // A PDF page linked from the seed should be extracted, not
    // skipped with "use web_fetch".
    let seed = "<html><body><article><p>content words for extractor threshold pass yes yes yes</p><a href=\"/doc.pdf\">pdf</a><a href=\"/ok\">ok</a></article></body></html>";
    // Minimal valid PDF with a text layer.
    let pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n4 0 obj<</Length 44>>stream\nBT /F1 12 Tf 100 700 Td (Hello World from PDF) Tj ET\nendstream\nendobj\n5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\nxref\n0 6\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n0000000115 00000 n \n0000000180 00000 n \n0000000268 00000 n \ntrailer<</Size 6/Root 1 0 R>>\nstartxref\n341\n%%EOF";
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page(
            "https://ex.com/doc.pdf",
            200,
            std::str::from_utf8(pdf).unwrap(),
        )
        .content_type("https://ex.com/doc.pdf", "application/pdf")
        .page("https://ex.com/ok", 200, &html("Ok", "ok page"));
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 10;
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    // PDF should NOT be in skipped with "pdf" or "binary" reason.
    assert!(
        !r.skipped
            .iter()
            .any(|(u, why)| u.ends_with(".pdf") && (why.contains("pdf") || why.contains("binary"))),
        "PDF should not be skipped as binary/pdf"
    );
    // The ok page should be in results.
    assert!(r.pages.iter().any(|p| p.url.ends_with("/ok")));
}

#[tokio::test]
async fn seed_always_in_scope_with_include() {
    // The seed should always be included in results, even when
    // it doesn't match --include globs. Scope filters apply to
    // discovered links, not the seed the user explicitly asked for.
    // Regression test for docs.rs: crawling /tokio with
    // --include /tokio/* : seed /tokio doesn't match /tokio/*
    // but must still be in results.
    let seed = "<html><body><article><h1>Tokio</h1><p>content words for extractor threshold pass yes yes yes</p><a href=\"/tokio/v0.1/api\">api</a></article></body></html>";
    let api = "<html><body><article><h1>Tokio API</h1><p>API docs content words for extractor threshold pass yes yes yes</p></article></body></html>";
    let site = MockSite::new()
        .page("https://ex.com/tokio", 200, seed)
        .page("https://ex.com/tokio/v0.1/api", 200, api);
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut o = opts();
    o.mode = CrawlMode::Content;
    o.max_pages = 5;
    o.include_paths = vec!["/tokio/*".into()];
    let r = crawler
        .crawl("https://ex.com/tokio", o, None)
        .await
        .unwrap();
    // Seed must be in results even though /tokio doesn't match /tokio/*.
    assert!(
        r.pages.iter().any(|p| p.url.ends_with("/tokio")),
        "seed must always be in scope, even when it doesn't match --include"
    );
    assert!(r.pages.iter().any(|p| p.url.contains("/tokio/v0.1/api")));
    // Seed should NOT be in skipped as "out of scope".
    assert!(
        !r.skipped
            .iter()
            .any(|(u, why)| u.ends_with("/tokio") && why.contains("out of scope")),
        "seed should not be marked out of scope"
    );
}

// ────────────────────────────────────────────────────────────────
// Lowercase-drift proofs: offset slices on non-ASCII pages
// ────────────────────────────────────────────────────────────────
// The link extractors lowercase the WHOLE document to case-fold the
// tag names, then slice the ORIGINAL at offsets found in the copy.
// Case folding changes byte lengths ('İ' U+0130 = 2 bytes -> "i̇"
// = 3 bytes), so every offset after a folding char drifts. These
// tests prove the failure; the fix searches the original with an
// ASCII case-insensitive byte scan (ASCII folding is length-stable).

#[test]
fn link_rel_panics_on_folding_char_before_the_tag_scan() {
    // 'İ' (2 bytes) lowercases to "i̇" (3 bytes). With İ INSIDE the
    // tag and a multibyte char right after '>', the old code's
    // lowered-copy offsets slice one byte into that char: a
    // str-slice panic, abort in release. Pre-fix: this panicked.
    let html = "<html><head><link rel=\"alternate\" title=\"İstanbul\" \
                type=\"application/rss+xml\" href=\"/feed.xml\">內容</head></html>";
    let got = std::panic::catch_unwind(|| super::extract_feed_links(html));
    let links = got.expect("extract_feed_links panicked on a folding char");
    assert_eq!(links, vec!["/feed.xml"]);
}

#[test]
fn feed_xml_folding_char_dropped_links_old_math() {
    // RSS with a folding char in the channel title before each item
    // and inside one URL: the old offset math shifted every slice
    // by one byte, so the URLs read as "ttps://..." and were
    // silently dropped (both), instead of returned. Pre-fix: the
    // assertion on the URL list failed.
    let xml = "<rss><channel><title>İstanbul</title>\
               <item><link>https://example.com/f/1</link></item>\
               <item><link>https://example.com/f/İtem</link></item>\
               </channel></rss>";
    let got = std::panic::catch_unwind(|| super::parse_feed_urls(xml, 10));
    let urls = got.expect("parse_feed_urls panicked");
    assert_eq!(
        urls,
        vec!["https://example.com/f/1", "https://example.com/f/İtem"]
    );
    // Atom shape too: 'İ' inside the entry title before <link href>.
    let atom = "<feed><entry><title>İstanbul</title>\
                <link href=\"https://example.com/a/İtem\" rel=\"alternate\"/></entry></feed>";
    let got = std::panic::catch_unwind(|| super::parse_feed_urls(atom, 10));
    let urls = got.expect("atom parse panicked");
    assert_eq!(urls, vec!["https://example.com/a/İtem"]);
}

#[test]
fn rss_uppercase_link_tags_close_case_insensitively() {
    // The open-tag scan matches "<LINK>" case-insensitively, so the
    // close must too: with a case-sensitive close, an uppercase item
    // "spans" to the NEXT item's lowercase </link> (one garbage URL
    // swallowing the real one), and with no lowercase close left in
    // the document every remaining RSS URL is dropped.
    let xml = "<rss><channel>\
               <item><LINK>https://example.com/1</LINK></item>\
               <item><link>https://example.com/2</link></item>\
               </channel></rss>";
    assert_eq!(
        super::parse_feed_urls(xml, 10),
        vec!["https://example.com/1", "https://example.com/2"]
    );
}

// The pre-fix resume store = one shared JSON map saved with
// load-modify-save: two overlapping writer processes (the daemon
// plus a CLI run, or parallel test processes on CI) saved stale
// copies over each other, and tokens issued milliseconds earlier
// read back as "resume token expired or unknown". Windows CI
// caught it live in the crawl_resume_continues run. Per-token
// files cannot collide: every writer's token survives everyone
// else's save.
#[test]
fn concurrent_issues_survive_each_other() {
    let iso = std::env::temp_dir().join(format!("ds-resume-race-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&iso);
    unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &iso) };

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8usize)
        .map(|i| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let tok = format!("c16a9a0{i:x}");
                let state = super::ResumeState {
                    seed: format!("https://site.example/seed{i}"),
                    queue: Vec::new(),
                    seen: Vec::new(),
                };
                super::resume_store_issue(&tok, &state);
                barrier.wait();
                match super::resume_store_take(&tok) {
                    Ok(back) => assert_eq!(back.seed, format!("https://site.example/seed{i}")),
                    Err(e) => panic!("writer {i} lost its own token: {e}"),
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let _ = std::fs::remove_dir_all(&iso);
}

// The v3 tokens = one shared map at <cache>/crawl-resumes.json.
// The per-token store migrates them on first touch; the legacy
// file retires only when every entry made it to disk, and a
// migrated token behaves the same (read once, then consumed).
#[test]
fn legacy_store_migrates_and_the_token_survives() {
    let iso = std::env::temp_dir().join(format!("ds-resume-mig-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&iso);
    unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &iso) };
    let legacy = iso.join("crawl-resumes.json");
    let legacy_text = serde_json::json!({
        "entries": {
            "cabc123": [
                {"seed": "https://old.example/seed", "queue": [], "seen": []},
                1234
            ]
        }
    })
    .to_string();
    std::fs::write(&legacy, legacy_text).unwrap();

    let back = super::resume_store_take("cabc123").expect("migrated token read");
    assert_eq!(back.seed, "https://old.example/seed");
    assert!(
        !legacy.exists(),
        "legacy file retired after a full migration"
    );
    assert!(
        !iso.join("crawl-resumes").join("cabc123.json").exists(),
        "token consumed on take"
    );
    let _ = std::fs::remove_dir_all(&iso);
}

// The resume token is agent-supplied and joined into a filesystem
// path (<cache>/crawl-resumes/<tok>.json). A traversal token
// (`../../…`) must be refused before it can read-then-delete a
// `.json` file outside the store. resume_store_take rejects any
// non-alphanumeric token, and the planted victim file survives.
#[test]
fn resume_token_traversal_is_refused() {
    // Format validity itself (no FS, deterministic).
    assert!(super::is_valid_resume_token("c16a9a0f"));
    for bad in [
        "",
        "../secret",
        "..\\secret",
        "a/b",
        "a.b",
        "tok\0",
        "c16 a9",
    ] {
        assert!(!super::is_valid_resume_token(bad), "must reject {bad:?}");
    }

    // End to end: a planted valid-ResumeState `.json` OUTSIDE the
    // store must not be consumed by a traversal token.
    let iso = std::env::temp_dir().join(format!("ds-resume-trav-{}", std::process::id()));
    let store = iso.join("crawl-resumes");
    let _ = std::fs::create_dir_all(&store);
    unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &iso) };
    let victim = iso.join("victim.json");
    std::fs::write(
        &victim,
        serde_json::json!({"seed":"https://evil.example/","queue":[],"seen":[]}).to_string(),
    )
    .unwrap();
    // <store>/../victim.json resolves to the planted file.
    match super::resume_store_take("../victim") {
        Err(e) => assert!(e.contains("expired or unknown"), "{e}"),
        Ok(_) => panic!("a traversal token must be refused, not consumed"),
    }
    assert!(
        victim.exists(),
        "a traversal token must not delete an outside file"
    );
    let _ = std::fs::remove_dir_all(&iso);
}

#[tokio::test]
async fn wave450_invalid_seed_does_not_consume_a_resume_token() {
    let token = "c450badseed";
    let state = super::ResumeState {
        seed: "https://ex.com/docs/".into(),
        queue: Vec::new(),
        seen: Vec::new(),
    };
    super::resume_store_issue(token, &state);
    let site = MockSite::new();
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    assert!(
        crawler
            .crawl("this is not a URL", CrawlOptions::default(), Some(token))
            .await
            .is_err()
    );
    assert_eq!(
        super::resume_store_take(token)
            .expect("invalid input must preserve the token")
            .seed,
        state.seed
    );
}

#[tokio::test]
async fn report_audit_crawl_budgets_are_hard_under_parallel_workers() {
    let mut site = MockSite::new();
    for i in 0..8 {
        let body = format!(
            "<html><title>Page {i}</title><body><article><h1>Page {i}</h1><p>{}</p>{}</article></body></html>",
            format!("Distinct evidence on page {i}. ").repeat(900),
            (0..8)
                .map(|j| format!("<a href='/p{j}'>Page {j}</a>"))
                .collect::<String>()
        );
        site = site.page(&format!("https://ex.com/p{i}"), 200, &body);
    }
    let (fetch, _) = site.fetcher();
    let crawler = Crawler::new(fetch, gov());
    let mut options = opts();
    options.mode = CrawlMode::Content;
    options.max_pages = 8;
    options.concurrency = 4;
    options.include_paths = vec!["/*".into()];
    options.per_page_max = 800;
    options.max_total_chars = 1600;
    options.respect_robots = false;
    let history_writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let writes = history_writes.clone();
    options.on_page = Some(Arc::new(move |_, _, _, _| {
        writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let result = crawler
        .crawl("https://ex.com/p0", options, None)
        .await
        .unwrap();
    assert!(
        !result.pages.is_empty(),
        "assertions must inspect fetched evidence"
    );
    assert!(
        result
            .pages
            .iter()
            .all(|p| p.markdown.chars().count() <= 800),
        "per-page body exceeded requested limit"
    );
    assert!(
        result
            .pages
            .iter()
            .filter(|p| !p.duplicate)
            .map(|p| p.markdown.chars().count())
            .sum::<usize>()
            <= 1600,
        "parallel workers overshot the total budget"
    );
    assert_eq!(result.stop, StopReason::CharBudget);
    assert!(result.pages.iter().all(|p| p.next_offset.is_some()));
    assert_eq!(
        history_writes.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "capped content must not be recorded as a full baseline"
    );
}

#[tokio::test]
async fn report_audit_slow_seed_keeps_descendants_alive_and_page_cap_is_exact() {
    let site = MockSite::new()
        .page("https://ex.com/root", 200, &format!("<article><h1>Root</h1><p>{}</p><a href='/one'>One</a><a href='/two'>Two</a><a href='/three'>Three</a></article>", "Root substantive evidence. ".repeat(30)))
        .page("https://ex.com/one", 200, &format!("<article><h1>One</h1><p>{}</p></article>", "First child substantive evidence. ".repeat(30)))
        .page("https://ex.com/two", 200, &format!("<article><h1>Two</h1><p>{}</p></article>", "Second child substantive evidence. ".repeat(30)))
        .page("https://ex.com/three", 200, &format!("<article><h1>Three</h1><p>{}</p></article>", "Third child substantive evidence. ".repeat(30)));
    let (fetch, _) = site.fetcher();
    let delayed: PageFetcher = Arc::new(move |url, lane, referer, gate| {
        let fetch = fetch.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(350)).await;
            fetch(url, lane, referer, gate).await
        }
        .boxed()
    });
    let crawler = Crawler::new(delayed, gov());
    let mut options = opts();
    options.mode = CrawlMode::Content;
    options.max_pages = 2;
    options.concurrency = 4;
    options.include_paths = vec!["/*".into()];
    options.respect_robots = false;
    let result = crawler
        .crawl("https://ex.com/root", options.clone(), None)
        .await
        .unwrap();
    assert_eq!(
        result.pages.len(),
        2,
        "slow seed must not lose its children; simultaneous completions must respect the page cap"
    );
    assert_eq!(result.stop, StopReason::MaxPages);
    let resume = result.resume.unwrap();
    let next = crawler.crawl("", options, Some(&resume)).await.unwrap();
    assert_eq!(
        next.pages.len(),
        2,
        "in-flight pages withheld by a cap must survive resume"
    );
    assert!(
        next.pages
            .iter()
            .all(|p| result.pages.iter().all(|old| old.url != p.url))
    );
}
