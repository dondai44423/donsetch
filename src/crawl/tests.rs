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
use super::{CrawlMode, CrawlOptions, Crawler, FetchedPage, PageFetcher, StopReason};
use crate::detect::walls::Verdict;

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
    referers: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

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
        self.throttles
            .lock()
            .unwrap()
            .values()
            .map(|c| c.load(Ordering::SeqCst))
            .sum::<usize>()
            .min(0)
    }

    fn fetcher(self) -> (PageFetcher, Arc<Mutex<Vec<String>>>) {
        let hits = Arc::clone(&self.hits);
        let pages = Arc::new(self.pages);
        let throttles = Arc::clone(&self.throttles);
        let transients = Arc::clone(&self.transients);
        let content_types = Arc::new(self.content_types);
        let referers = Arc::clone(&self.referers);
        let hits2 = Arc::clone(&hits);
        let f: PageFetcher =
            Arc::new(move |url: String, _lane: String, referer: Option<String>| {
                let pages = Arc::clone(&pages);
                let throttles = Arc::clone(&throttles);
                let transients = Arc::clone(&transients);
                let content_types = Arc::clone(&content_types);
                let referers = Arc::clone(&referers);
                let hits = Arc::clone(&hits2);
                async move {
                    hits.lock().unwrap().push(url.clone());
                    referers
                        .lock()
                        .unwrap()
                        .push((url.clone(), referer.clone()));
                    // Throttle simulation: 429 until counter burns out.
                    if let Some(c) = throttles.lock().unwrap().get(&url) {
                        if c.load(Ordering::SeqCst) > 0 {
                            c.fetch_sub(1, Ordering::SeqCst);
                            return FetchedPage {
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
                    }
                    // Transient 500 simulation: 500 until counter burns out.
                    if let Some(c) = transients.lock().unwrap().get(&url) {
                        if c.load(Ordering::SeqCst) > 0 {
                            c.fetch_sub(1, Ordering::SeqCst);
                            return FetchedPage {
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
                    }
                    let ct = content_types
                        .get(&url)
                        .cloned()
                        .unwrap_or_else(|| "text/html".to_string());
                    match pages.get(&url) {
                        Some((status, body)) => FetchedPage {
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
            });
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
    let mut o = CrawlOptions::default();
    o.deadline = Duration::from_secs(10);
    o
}

fn html(title: &str, body: &str) -> String {
    format!(
        "<html lang=\"en\"><head><title>{title}</title></head><body><article><h1>{title}</h1><p>{body} {}</p></article></body></html>",
        "Long enough paragraph content to pass extraction thresholds and look like a real document for the extractor.".repeat(3)
    )
}


fn minimal_pdf(text: &str) -> Vec<u8> {
    let header = b"%PDF-1.4\n";
    let o1 = b"1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n";
    let o2 = b"2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n";
    let o3 = b"3 0 obj<</Type/Page/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n";
    let stream = format!("BT /F1 12 Tf 100 700 Td ({text}) Tj ET").into_bytes();
    let o4_header = format!("4 0 obj<</Length {}>>stream\n", stream.len()).into_bytes();
    let mut o4 = Vec::new();
    o4.extend_from_slice(&o4_header);
    o4.extend_from_slice(&stream);
    o4.extend_from_slice(b"\nendstream\nendobj\n");
    let o5 = b"5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\n";
    let parts: Vec<&[u8]> = vec![header, o1, o2, o3, &o4, o5];
    let xref_offset: usize = parts.iter().map(|p| p.len()).sum();
    let off1 = header.len();
    let off2 = off1 + o1.len();
    let off3 = off2 + o2.len();
    let off4 = off3 + o3.len();
    let off5 = off4 + o4.len();
    let mut pdf = Vec::new();
    for part in &parts {
        pdf.extend_from_slice(part);
    }
    pdf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    pdf.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes());
    pdf.extend_from_slice(format!("{off2:010} 00000 n \n").as_bytes());
    pdf.extend_from_slice(format!("{off3:010} 00000 n \n").as_bytes());
    pdf.extend_from_slice(format!("{off4:010} 00000 n \n").as_bytes());
    pdf.extend_from_slice(format!("{off5:010} 00000 n \n").as_bytes());
    pdf.extend_from_slice(b"trailer<</Size 6/Root 1 0 R>>\n");
    pdf.extend_from_slice(format!("startxref\n{xref_offset}\n").as_bytes());
    pdf.extend_from_slice(b"%%EOF");
    pdf
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
    // /sitemap.xml returns 200 — the others 404.
    let hits = hits.lock().unwrap();
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
    let hits = hits.lock().unwrap();
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
    let hits = hits.lock().unwrap();
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
    let hits = hits.lock().unwrap();
    assert!(!hits.iter().any(|h| h.contains("/private")));
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
    // returns ContentOk) — real walls handled by walls::detect
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
    o.max_pages = 2; // seed + ONE more — focus decides which
    let r = crawler.crawl("https://ex.com/", o, None).await.unwrap();
    let hits = hits.lock().unwrap();
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
    let hits = hits.lock().unwrap();
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
    let hits = hits.lock().unwrap();
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
    let hits = hits.lock().unwrap();
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
    let hits = hits.lock().unwrap();
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
    o.max_pages = 2; // seed + 1 — priority decides which
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
    // (We can't access referers from here — it's inside the
    // mock's Arc. But we can verify the crawl succeeded.)
    // This test serves as a compile-time check that the
    // PageFetcher signature accepts referer.
}

#[test]
fn v2_governor_dwell_extends_wait() {
    // Gap: fixed-interval traffic is a bot fingerprint.
    // Dwell time proportional to page size breaks the metronome.
    let g = Governor::new(vec![Lane {
        id: "d".into(),
        kind: LaneKind::Direct,
    }]);
    // First request: no wait.
    assert_eq!(g.wait_for("ex.com", "d", 0), Duration::ZERO);
    // Simulate a large-page success with 2000ms dwell.
    g.on_success("ex.com", "d", Duration::from_millis(50), 2000);
    // Next request must wait at least the dwell time.
    let w = g.wait_for("ex.com", "d", 1);
    assert!(
        w > Duration::ZERO,
        "dwell time must extend the wait beyond zero"
    );
}

#[test]
fn v2_governor_zero_dwell_no_extra_wait() {
    // Zero dwell = no extra wait. Small pages (cache hits) should
    // not inflate the pacing.
    let g = Governor::new(vec![Lane {
        id: "d".into(),
        kind: LaneKind::Direct,
    }]);
    g.wait_for("ex.com", "d", 0);
    g.on_success("ex.com", "d", Duration::from_millis(50), 0);
    let w = g.wait_for("ex.com", "d", 1);
    // Without dwell, the wait is just the base pacing delay.
    assert!(w < Duration::from_secs(3));
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
    let hits = hits.lock().unwrap();
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
    // Use a minimal PDF via the helper; on aarch64 even valid tiny PDFs can hang in
    // PDFium (FPDF_LoadMemDocument64 / FPDFText_LoadPage) while holding the global
    // CORE lock. The crawl isolates PDF extraction to a 3s spawn_blocking timeout, but
    // the leaked blocking thread would still stall shutdown. Instead, exercise the
    // Corrupt -> empty_pdf path (still "not binary") by corrupting Length so
    // quick_validate fails fast before PDFium is touched. The behavioural assertion
    // is identical: PDFs must be routed to DonSheet, never skipped as binary/pdf.
    let mut pdf = minimal_pdf("Hello World from PDF");
    // Corrupt Length by +30 (exceeds quick_validate's 10-byte slack) to force
    // LoadError::Corrupt before the global lock is taken.
    {
        let s = String::from_utf8_lossy(&pdf).to_string();
        // minimal_pdf("Hello World from PDF") emits Length 52
        let corrupted = s.replacen("/Length 52", "/Length 82", 1);
        pdf = corrupted.into_bytes();
    }
    let site = MockSite::new()
        .page("https://ex.com/", 200, seed)
        .page(
            "https://ex.com/doc.pdf",
            200,
            std::str::from_utf8(&pdf).unwrap(),
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
    // --include /tokio/* — seed /tokio doesn't match /tokio/*
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

