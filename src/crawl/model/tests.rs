//! Model tests: pure inference first, then the bounded inventory and the
//! map through a mock fetcher. No test touches the network.

use super::infer::FilterCounts;
use super::infer::{self, InvUrl, SiteKind};
use super::{MapOutcome, MapRequest};
use crate::crawl::sitemap::Robots;
use crate::crawl::{CrawlMode, CrawlOptions, FetchedPage, PageFetcher, StopReason};
use crate::detect::walls::Verdict;
use futures_util::FutureExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

fn inv(url: &str) -> InvUrl {
    InvUrl {
        url: url.to_string(),
        lastmod: None,
        source: "sitemap".to_string(),
    }
}

fn pages(items: &[(&str, u16, &str)]) -> HashMap<String, (u16, String)> {
    items
        .iter()
        .map(|(u, status, body)| ((*u).to_string(), (*status, (*body).to_string())))
        .collect()
}

fn fetcher(map: HashMap<String, (u16, String)>) -> (PageFetcher, Arc<Mutex<Vec<String>>>) {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&hits);
    let f: PageFetcher = Arc::new(move |url, lane, _referer, _gate| {
        let page = map.get(&url).cloned();
        let recorded = Arc::clone(&recorded);
        async move {
            recorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(url.clone());
            let (status, body) = page.unwrap_or((404, "not found".to_string()));
            FetchedPage {
                lane,
                route: Some(crate::egress::request_route::RequestRoute::direct()),
                url,
                status,
                headers: Vec::new(),
                body: body.into_bytes(),
                verdict: if status == 200 {
                    Verdict::ContentOk
                } else {
                    Verdict::SoftNotFound
                },
                latency: Duration::ZERO,
                cached: false,
                error_hint: None,
                denied: None,
            }
        }
        .boxed()
    });
    (f, hits)
}

fn robots(sitemaps: &[&str]) -> Robots {
    Robots {
        sitemaps: sitemaps.iter().map(|s| (*s).to_string()).collect(),
        ..Robots::default()
    }
}

fn options() -> CrawlOptions {
    CrawlOptions {
        mode: CrawlMode::Map,
        respect_robots: true,
        deadline: Duration::from_secs(30),
        map_cap: 120,
        ..Default::default()
    }
}

async fn run(
    seed: &str,
    map: HashMap<String, (u16, String)>,
    robots: Robots,
    opts: CrawlOptions,
) -> (MapOutcome, Arc<Mutex<Vec<String>>>) {
    let (fetch, hits) = fetcher(map);
    let seed_url = Url::parse(seed).unwrap();
    let outcome = super::map(
        &fetch,
        MapRequest {
            seed: &seed_url,
            requested_seed: seed,
            seed_page: None,
            robots: Some(&robots),
            opts: &opts,
            deadline: Instant::now() + Duration::from_secs(30),
            seed_failure: None,
        },
    )
    .await;
    (outcome, hits)
}

fn urlset(urls: &[&str]) -> String {
    let mut s = String::from("<?xml version=\"1.0\"?><urlset>");
    for u in urls {
        s.push_str(&format!(
            "<url><loc>{u}</loc><lastmod>2026-01-01</lastmod></url>"
        ));
    }
    s.push_str("</urlset>");
    s
}

fn index(children: &[&str]) -> String {
    let mut s = String::from("<?xml version=\"1.0\"?><sitemapindex>");
    for c in children {
        s.push_str(&format!("<sitemap><loc>{c}</loc></sitemap>"));
    }
    s.push_str("</sitemapindex>");
    s
}

#[test]
fn dates_are_slots_and_years_are_sections() {
    let seed = Url::parse("https://blog.example/").unwrap();
    let urls: Vec<InvUrl> = [
        "https://blog.example/2026/Nov/9/a/",
        "https://blog.example/2026/Nov/9/b/",
        "https://blog.example/2025/Dec/31/c/",
        "https://blog.example/2025/Dec/30/d/",
    ]
    .iter()
    .map(|u| inv(u))
    .collect();
    let built = infer::build(&urls, &seed);
    assert_eq!(built.site_kind, SiteKind::Blog);
    assert!(built.sections.iter().any(|s| s.path == "2026" && s.date));
    assert!(built.families.iter().any(|f| f.template == ":v"));
    assert_eq!(built.urls, 4);
}

#[test]
fn a_docs_hub_lands_on_its_product_sections() {
    let seed = Url::parse("https://docs.example/en/").unwrap();
    let mut urls = Vec::new();
    for section in [
        "html",
        "css",
        "javascript",
        "api",
        "http",
        "svg",
        "webassembly",
        "glossary",
    ] {
        for i in 0..4 {
            urls.push(inv(&format!(
                "https://docs.example/en/docs/{section}/page-{i}"
            )));
        }
    }
    let built = infer::build(&urls, &seed);
    assert!(built.sections.iter().any(|s| s.path == "en/docs/html"));
    assert_eq!(built.sections.len(), 8);
}

#[test]
fn the_same_inventory_hashes_the_same() {
    let seed = Url::parse("https://example.com/").unwrap();
    let urls = vec![
        inv("https://example.com/docs/a"),
        inv("https://example.com/docs/b"),
    ];
    let a = infer::build(&urls, &seed);
    let b = infer::build(&urls, &seed);
    assert_eq!(a.inventory_hash, b.inventory_hash);
    assert_eq!(a.families.len(), b.families.len());
}

#[tokio::test]
async fn a_sitemap_is_the_inventory_and_the_seed_is_not_fetched() {
    let sitemap = "https://example.com/sitemap.xml";
    let body = urlset(&[
        "https://example.com/docs/a",
        "https://example.com/docs/b",
        "https://example.com/blog/one",
    ]);
    let map = pages(&[
        ("https://example.com/llms.txt", 404, "not found"),
        (sitemap, 200, &body),
        (
            "https://example.com/",
            200,
            "<html>should not be read</html>",
        ),
    ]);
    let (out, hits) = run("https://example.com/", map, robots(&[sitemap]), options()).await;
    assert_eq!(out.summary.urls, 3, "{:?}", out.summary);
    assert!(out.summary.complete);
    assert_eq!(out.summary.requests, 2, "{:?}", hits.lock().unwrap());
    assert_eq!(out.inventory.source, "sitemap");
    assert_eq!(out.map.len(), 3);
    assert!(out.summary.dossier_saved);
    let hits = hits.lock().unwrap().clone();
    assert_eq!(
        hits,
        vec![
            "https://example.com/llms.txt".to_string(),
            sitemap.to_string()
        ]
    );
}

#[tokio::test]
async fn a_page_list_is_the_inventory() {
    let llms = "# Docs\n\n* [Page List API](https://example.com/api/pagelist): all pages\n";
    let list = "/en/docs/a\n/en/docs/b\n";
    let map = pages(&[
        ("https://example.com/llms.txt", 200, llms),
        ("https://example.com/api/pagelist", 200, list),
        (
            "https://example.com/",
            200,
            "<html>should not be read</html>",
        ),
    ]);
    let (out, hits) = run("https://example.com/", map, Robots::default(), options()).await;
    assert_eq!(out.summary.urls, 2, "{:?}", out.summary);
    assert!(out.summary.complete);
    assert_eq!(out.inventory.source, "pagelist");
    assert_eq!(out.summary.requests, 2, "{:?}", hits.lock().unwrap());
    assert!(
        !hits
            .lock()
            .unwrap()
            .contains(&"https://example.com/".to_string())
    );
}

#[tokio::test]
async fn a_sitemap_index_reads_the_seed_locale_child_only() {
    let idx = "https://example.com/sitemap.xml";
    let en = "https://example.com/sitemaps/en-us/sitemap.xml";
    let de = "https://example.com/sitemaps/de/sitemap.xml";
    let map = pages(&[
        ("https://example.com/en-US/llms.txt", 404, "not found"),
        (idx, 200, &index(&[en, de])),
        (
            en,
            200,
            &urlset(&[
                "https://example.com/en-US/docs/a",
                "https://example.com/en-US/docs/b",
            ]),
        ),
        (de, 200, &urlset(&["https://example.com/de/docs/a"])),
    ]);
    let (out, hits) = run("https://example.com/en-US/", map, robots(&[idx]), options()).await;
    assert_eq!(out.summary.urls, 2, "{:?}", out.summary);
    assert!(out.summary.complete);
    assert!(!hits.lock().unwrap().contains(&de.to_string()));
    assert_eq!(out.summary.requests, 3);
}

#[tokio::test]
async fn seed_links_are_the_fallback() {
    let seed = "https://links.example/";
    let html = r#"<html><body>
        <a href="/docs/a">A</a>
        <a href="/docs/b">B</a>
        <a href="/about">About</a>
        </body></html>"#;
    let map = pages(&[
        ("https://links.example/llms.txt", 404, "not found"),
        ("https://links.example/sitemap.xml", 404, "not found"),
        ("https://links.example/sitemap_index.xml", 404, "not found"),
        (seed, 200, html),
    ]);
    let (out, _hits) = run(seed, map, Robots::default(), options()).await;
    assert_eq!(out.summary.urls, 3, "{:?}", out.summary);
    assert!(!out.summary.complete);
    assert_eq!(out.inventory.source, "seed-links");
    assert!(out.seed_failure.is_none());
    assert_eq!(out.summary.requests, 4);
}

#[tokio::test]
async fn a_dead_seed_with_no_map_is_a_failure() {
    let seed = "https://dead.example/";
    let map = pages(&[
        ("https://dead.example/llms.txt", 404, "not found"),
        ("https://dead.example/sitemap.xml", 404, "not found"),
        ("https://dead.example/sitemap_index.xml", 404, "not found"),
        (seed, 404, "not found"),
    ]);
    let (out, _hits) = run(seed, map, Robots::default(), options()).await;
    assert!(out.map.is_empty());
    assert_eq!(out.stop, StopReason::FrontierEmpty);
    let failure = out.seed_failure.expect("a dead seed is a failure");
    assert_eq!(failure.status, 404);
}

#[tokio::test]
async fn locale_variants_collapse_to_the_seed_locale() {
    let sitemap = "https://example.com/sitemap.xml";
    let body = urlset(&[
        "https://example.com/en-US/docs/a",
        "https://example.com/de/docs/a",
        "https://example.com/docs/a",
    ]);
    let map = pages(&[
        ("https://example.com/llms.txt", 404, "not found"),
        (sitemap, 200, &body),
    ]);
    let mut opts = options();
    opts.include_paths = vec!["/*".to_string()];
    let (out, _hits) = run("https://example.com/en-US/", map, robots(&[sitemap]), opts).await;
    assert_eq!(out.summary.urls, 1, "{:?}", out.summary);
    assert_eq!(out.map, vec!["https://example.com/en-US/docs/a"]);
}

#[tokio::test]
async fn a_deadline_stops_before_a_read() {
    let (fetch, hits) = fetcher(HashMap::new());
    let seed = Url::parse("https://slow.example/").unwrap();
    let opts = options();
    let out = super::map(
        &fetch,
        MapRequest {
            seed: &seed,
            requested_seed: "https://slow.example/",
            seed_page: None,
            robots: Some(&Robots::default()),
            opts: &opts,
            deadline: Instant::now(),
            seed_failure: None,
        },
    )
    .await;
    assert_eq!(out.stop, StopReason::Deadline);
    assert!(out.map.is_empty());
    assert!(hits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_dossier_remembers_the_previous_map() {
    let sitemap = "https://dossier.example/sitemap.xml";
    let body = urlset(&["https://dossier.example/docs/a"]);
    let map = pages(&[
        ("https://dossier.example/llms.txt", 404, "not found"),
        (sitemap, 200, &body),
    ]);
    let (first, _) = run(
        "https://dossier.example/",
        map.clone(),
        robots(&[sitemap]),
        options(),
    )
    .await;
    assert!(first.summary.previous.is_none());
    assert!(first.summary.dossier_saved);
    let (second, _) = run(
        "https://dossier.example/",
        map,
        robots(&[sitemap]),
        options(),
    )
    .await;
    let previous = second.summary.previous.expect("second map sees the first");
    assert!(!previous.changed);
    assert!(super::dossier::load("dossier.example").is_some());
}

#[test]
fn filter_counts_add_up() {
    let f = FilterCounts {
        robots: 1,
        rules: 2,
        scope: 3,
        locale: 4,
        assets: 5,
        other: 6,
    };
    assert_eq!(f.total(), 21);
}
