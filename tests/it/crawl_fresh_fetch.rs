//! Delta-recrawl freshness law (v4 phase 3 live battery, 2026-09-14):
//!
//! A recrawl exists to see what changed. The revalidation cache may
//! serve a fresh-window body WITHOUT dialing the origin; if a crawl
//! fetch rides that window, every delta recrawl compares the previous
//! crawl's own content and reports zero changes forever.
//!
//! Discriminating test: with a populated fresh-window cache entry and
//! a CHANGED origin body,
//!   - skip_cache=false must return the cached OLD body (proving the
//!     cache would have broken the delta), and
//!   - skip_cache=true (the crawl fetch path) must dial the origin
//!     and return the NEW body.

use donsetch::fetch::client::{CacheState, Fetcher};
use donsetch::profile::{BrowserProfile, Platform};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};

/// First GET of /a answers body A with a long fresh window; every
/// later GET answers body B (the "content changed" scenario).
fn serve(listener: TcpListener, hits: std::sync::Arc<AtomicUsize>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let raw = String::from_utf8_lossy(&buf).to_string();
            let token = raw.lines().next().unwrap_or("").to_string();
            let n = hits.fetch_add(1, Ordering::SeqCst);
            let body = if n == 0 && token.contains("/a") {
                "A"
            } else {
                "B"
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: max-age=600\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
}

#[tokio::test]
async fn recrawl_never_serves_the_fresh_window() {
    crate::sandbox();
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };

    let origin = TcpListener::bind("127.0.0.1:0").expect("bind origin");
    let port = origin.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    serve(origin, hits.clone());

    let profile = BrowserProfile::chrome_150(Platform::Linux);
    let fetcher = Fetcher::new(profile).expect("fetcher");
    let url = format!("http://127.0.0.1:{port}/a");

    // 1) Seed fetch: dials the origin, body A, stores a fresh entry.
    let first = fetcher
        .fetch_via_jar_ref(&url, None, false, None)
        .await
        .expect("seed fetch");
    assert_eq!(String::from_utf8_lossy(&first.body).trim(), "A");
    assert!(
        !matches!(first.cache, CacheState::Fresh),
        "first fetch must dial, not read the cache"
    );

    // 2) Plain recrawl fetch (skip_cache=false): the fresh window
    //    serves body A with ZERO requests. This is exactly what broke
    //    the delta leg before the fix: the "new" fetch = the old body.
    let cached = fetcher
        .fetch_via_jar_opts(&url, None, false, None, false)
        .await
        .expect("cached fetch");
    assert!(
        matches!(cached.cache, CacheState::Fresh),
        "fresh window must serve the entry without dialing"
    );
    assert_eq!(
        String::from_utf8_lossy(&cached.body).trim(),
        "A",
        "cache serves the stale body"
    );

    // 3) The crawl fetch path (skip_cache=true): must dial the origin
    //    and see body B even though a fresh entry exists.
    let fresh = fetcher
        .fetch_via_jar_opts(&url, None, false, None, true)
        .await
        .expect("fresh fetch");
    assert_eq!(
        String::from_utf8_lossy(&fresh.body).trim(),
        "B",
        "crawl fetch must see the live body"
    );
    assert!(
        !matches!(fresh.cache, CacheState::Fresh),
        "a skip_cache fetch is never a fresh-window hit"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2, "exactly two origin dials");
}
