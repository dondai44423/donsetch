//! Subresource shadow-fetching (v4 phase 1.3): page-load realism.
//!
//! The deepest pure-HTTP stealth tell is ABSENCE: real browsers
//! fetch the HTML, then burst 5-15 subresources (CSS, scripts,
//! fonts, first images, favicon); every scraper fetches one
//! document and vanishes. Server access logs see the difference
//! instantly, no fingerprinting required. Shadow-fetching closes
//! it: after a successful HTML navigation on a stealth-relevant
//! host, the page's subresources are fetched in the background
//! with full realism: per-class header sets (phase 1.2), the page
//! as Referer, the shared cookie jar, the shared connection pool
//! (h2 multiplexes the burst exactly like a browser), in document
//! order, browser-style concurrency.
//!
//! Policy (route memory): only hosts with a wall signal get
//! shadowed (defended = worth the bytes); open hosts skip it
//! (speed law). Budgets are hard: asset count, total bytes, wall
//! time, failure tolerance. Assets land in the revalidation cache
//! for free (shared CSS/fonts are then cached for later pages,
//! browser-true). Failures are silent: this is best-effort
//! realism, never a reason to fail the agent's fetch.
//!
//! Kill switch: DONSETCH_NO_SHADOW_FETCH. Force for testing:
//! DONSETCH_SHADOW_FETCH=1 (shadows every host).

use std::sync::{Arc, OnceLock};

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::ghost::cache::GhostState;
use crate::profile::RequestClass;

use super::client::Fetcher;

/// Max subresources per page (a real page-load burst is 5-15).
const MAX_ASSETS: usize = 15;
/// Total byte budget for one page's burst.
const MAX_TOTAL_BYTES: usize = 2 << 20;
/// Hard wall clock for the whole burst.
const BURST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
/// Concurrent asset fetches (browser-ish; h2 multiplexes anyway).
const BURST_CONCURRENCY: usize = 6;
/// Tolerated failures before the burst abandons (a broken page
/// must not burn the whole budget).
const MAX_FAILURES: usize = 3;

fn deadline() -> std::time::Duration {
    std::env::var("DONSETCH_SHADOW_DEADLINE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(BURST_DEADLINE)
}

/// Should this host get page-load realism? Route memory policy:
/// any wall signal (known-walled, ever-challenged, vendor memory,
/// or mixed tier-1 history) marks the host defended enough to be
/// worth the bytes. Open hosts stay fast and document-only.
pub fn policy_shadows(state: &GhostState, host: &str) -> bool {
    if crate::config::env_flag("DONSETCH_NO_SHADOW_FETCH") {
        return false;
    }
    if crate::config::env_flag("DONSETCH_SHADOW_FETCH") {
        return true;
    }
    let Some(p) = state.profiles.get(host) else {
        return false;
    };
    p.needs_tier2
        || p.walled_count > 0
        || p.wall_vendor.is_some()
        || (p.t1_samples >= 2 && p.t1_ewma < 1.0)
}

/// Fire the burst in the background. Called after a successful
/// HTML navigation; never blocks the agent's response. Returns
/// without spawning when the policy or switches say no.
pub async fn maybe_shadow(
    fetcher: &Arc<Fetcher>,
    state: &Arc<Mutex<GhostState>>,
    page_url: &str,
    page: &super::client::FetchOutcome,
) {
    if !matches!(page.verdict, crate::detect::walls::Verdict::ContentOk) {
        return;
    }
    let is_html = page.headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("content-type") && v.to_lowercase().contains("text/html")
    });
    if !is_html {
        return;
    }
    let host = match url::Url::parse(page_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
    {
        Some(h) => h.to_lowercase(),
        None => return,
    };
    let (shadow, assets) = {
        let st = match state.try_lock() {
            Ok(st) => st,
            // State contended: skip rather than hold the response.
            Err(_) => return,
        };
        if !policy_shadows(&st, &host) {
            return;
        }
        (true, extract_assets(page_url, &page.body))
    };
    if !shadow || assets.is_empty() {
        return;
    }
    let fetcher = Arc::clone(fetcher);
    let state = Arc::clone(state);
    let page_url = page_url.to_string();
    let handle = tokio::spawn(async move {
        let fetched = shadow_burst(&fetcher, &page_url, assets).await;
        if fetched > 0 {
            let mut st = state.lock().await;
            st.note_shadow(fetched);
        }
    });
    pending().lock().await.push(handle);
}

/// In-flight bursts, tracked so short-lived processes (one-shot
/// CLI fetches) can let the page-load realism finish before the
/// runtime exits. Daemons never drain (perpetual).
fn pending() -> &'static Mutex<Vec<JoinHandle<()>>> {
    static PENDING: OnceLock<Mutex<Vec<JoinHandle<()>>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

/// Wait for in-flight bursts, bounded. CLI entry points call this
/// before exit; it is a no-op when nothing is shadowing.
pub async fn drain_pending() {
    let handles: Vec<JoinHandle<()>> = {
        let mut slot = pending().lock().await;
        std::mem::take(&mut *slot)
    };
    if handles.is_empty() {
        return;
    }
    let _ = tokio::time::timeout(deadline(), futures_util::future::join_all(handles)).await;
}

/// One page's burst, budgeted.
async fn shadow_burst(
    fetcher: &Arc<Fetcher>,
    page_url: &str,
    assets: Vec<(String, RequestClass)>,
) -> usize {
    use futures_util::stream::{self, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let total_bytes = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let fetched = Arc::new(AtomicUsize::new(0));

    let work = stream::iter(
        assets
            .into_iter()
            .take(MAX_ASSETS)
            .map(|(asset_url, class)| {
                let total_bytes = Arc::clone(&total_bytes);
                let failures = Arc::clone(&failures);
                let fetched = Arc::clone(&fetched);
                async move {
                    if failures.load(Ordering::Relaxed) >= MAX_FAILURES
                        || total_bytes.load(Ordering::Relaxed) >= MAX_TOTAL_BYTES
                    {
                        return;
                    }
                    let out = fetcher
                        .fetch_once_via_class(&asset_url, &[], None, true, Some(page_url), class)
                        .await;
                    match out {
                        Ok(o) => {
                            total_bytes.fetch_add(o.body.len(), Ordering::Relaxed);
                            fetched.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            failures.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }),
    );
    let _ = tokio::time::timeout(
        deadline(),
        work.for_each_concurrent(BURST_CONCURRENCY, |f| f),
    )
    .await;
    fetched.load(Ordering::Relaxed)
}

/// Byte-scan the document for the subresources a browser would
/// fetch, in document order: stylesheets, scripts, preloaded
/// fonts, the first images, and the favicon (browsers always ask
/// for /favicon.ico). Media (video/audio) is excluded: browsers
/// range-request it lazily, and it is heavy.
pub fn extract_assets(page_url: &str, html: &[u8]) -> Vec<(String, RequestClass)> {
    let base = match url::Url::parse(page_url) {
        Ok(u) => u,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(html);
    let mut out: Vec<(String, RequestClass)> = Vec::new();
    let mut images = 0;

    for (attr_value, class) in scan_candidates(&text) {
        if out.len() >= MAX_ASSETS {
            break;
        }
        if class == RequestClass::Image {
            images += 1;
            if images > 3 {
                continue;
            }
        }
        let Ok(abs) = base.join(&attr_value) else {
            continue;
        };
        if !matches!(abs.scheme(), "http" | "https") {
            continue;
        }
        if out.iter().any(|(u, _)| u == abs.as_str()) {
            continue;
        }
        out.push((abs.to_string(), class));
    }

    // The favicon: browsers fetch it on every first visit to an
    // origin, linked or not.
    if let Ok(favicon) = base.join("/favicon.ico")
        && !out.iter().any(|(u, _)| u == favicon.as_str())
    {
        out.push((favicon.to_string(), RequestClass::Favicon));
    }
    out
}

/// Raw (attribute value, class) candidates in document order.
/// Byte-scan over the tag soup: no DOM, no allocation beyond the
/// candidates. Looks at <link>, <script>, <img> only.
fn scan_candidates(html: &str) -> Vec<(String, RequestClass)> {
    // Offsets found here index back into `html`, and only
    // to_ascii_lowercase preserves byte length and char boundaries.
    // It is also the folding HTML specifies: tag and attribute names
    // match ASCII-case-insensitively. A slice that lands mid-codepoint
    // panics, and the release profile aborts rather than unwinding, so
    // the cost of breaking this is the whole daemon, not one fetch.
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = Vec::new();
    let mut pos = 0usize;

    while pos < bytes.len() {
        // Next tag open of interest.
        let link = find_from(bytes, b"<link", pos);
        let script = find_from(bytes, b"<script", pos);
        let img = find_from(bytes, b"<img", pos);
        let (tag_at, kind) = match (link, script, img) {
            (Some(l), Some(s), Some(i)) => {
                if l <= s && l <= i {
                    (l, 0)
                } else if s <= i {
                    (s, 1)
                } else {
                    (i, 2)
                }
            }
            (Some(l), Some(s), None) => {
                if l <= s {
                    (l, 0)
                } else {
                    (s, 1)
                }
            }
            (Some(l), None, Some(i)) => {
                if l <= i {
                    (l, 0)
                } else {
                    (i, 2)
                }
            }
            (None, Some(s), Some(i)) => {
                if s <= i {
                    (s, 1)
                } else {
                    (i, 2)
                }
            }
            (Some(l), None, None) => (l, 0),
            (None, Some(s), None) => (s, 1),
            (None, None, Some(i)) => (i, 2),
            (None, None, None) => break,
        };
        let Some(tag_end) = find_from(bytes, b">", tag_at) else {
            break;
        };
        let tag = &html[tag_at..tag_end + 1];

        match kind {
            0 => {
                // <link>: stylesheet / icon / preload-as-font.
                let rel = attr(tag, "rel").map(|r| r.to_lowercase());
                let as_ = attr(tag, "as").map(|a| a.to_lowercase());
                let class = if rel.as_deref() == Some("stylesheet") {
                    Some(RequestClass::Style)
                } else if rel.as_deref().is_some_and(|r| r.contains("icon")) {
                    Some(RequestClass::Favicon)
                } else if rel.as_deref() == Some("preload") && as_.as_deref() == Some("font") {
                    Some(RequestClass::Font)
                } else {
                    None
                };
                if let (Some(class), Some(href)) = (class, attr(tag, "href")) {
                    out.push((href, class));
                }
            }
            1 => {
                if let Some(src) = attr(tag, "src") {
                    out.push((src, RequestClass::Script));
                }
            }
            _ => {
                if let Some(src) = attr(tag, "src") {
                    out.push((src, RequestClass::Image));
                }
            }
        }
        pos = tag_end + 1;
    }
    out
}

fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.len() > hay.len() {
        return None;
    }
    memchr::memmem::find(&hay[from..], needle).map(|p| from + p)
}

/// Pull an attribute value out of a single tag's text. Handles
/// double quotes, single quotes, and unquoted values.
fn attr(tag: &str, name: &str) -> Option<String> {
    // ASCII-only folding, same reason as scan_candidates: `at` indexes
    // back into `tag`.
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let at = lower.find(&needle)? + needle.len();
    let rest = &tag[at..];
    let first = rest.chars().next()?;
    match first {
        '"' => rest[1..].find('"').map(|e| rest[1..e + 1].to_string()),
        '\'' => rest[1..].find('\'').map(|e| rest[1..e + 1].to_string()),
        _ => {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(rest.len());
            Some(rest[..end].to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!DOCTYPE html><html><head>
        <link rel="stylesheet" href="/css/app.css">
        <link rel="preload" as="font" href="/fonts/main.woff2" crossorigin>
        <link rel="icon" href="/favicon-32.png">
        <script src="/js/app.js"></script>
        </head><body>
        <img src="/img/hero.png">
        <img src="https://cdn.example.com/x.png">
        <script src="https://static.example.com/lib.js" async></script>
        <video src="/video/intro.mp4"></video>
        <a href="/about">about</a>
        </body></html>"#;

    /// Fails if the folding in `scan_candidates` or `attr` returns to
    /// `to_lowercase`. The markers are picked for their byte-length
    /// deltas: `Ω` (3->2) shifts the offsets onto ASCII and the assets
    /// vanish, `K` (3->1) shifts onto a continuation byte and the
    /// slice panics. `İ` (2->3) still parses either way, kept so the
    /// growth direction is covered too.
    #[test]
    fn non_ascii_before_a_tag_does_not_shift_offsets() {
        for marker in ["\u{130}", "\u{212A}", "\u{2126}"] {
            let html = format!(
                "<html><head><p>{marker}{marker}{marker}</p>\
                 <link rel=\"stylesheet\" href=\"/css/App.css\">\
                 <script src=\"/js/App.js\"></script></head></html>"
            );
            let assets = extract_assets("https://example.com/p", html.as_bytes());
            let urls: Vec<&str> = assets.iter().map(|(u, _)| u.as_str()).collect();
            assert!(
                urls.contains(&"https://example.com/css/App.css"),
                "stylesheet lost after {marker:?}: {urls:?}"
            );
            // Fails if a fix slices `lower` instead of `tag`: URL
            // paths are case-sensitive.
            assert!(
                urls.contains(&"https://example.com/js/App.js"),
                "script lost or case-folded after {marker:?}: {urls:?}"
            );
        }
    }

    #[test]
    fn extracts_browser_subresources_in_order() {
        let assets = extract_assets("https://example.com/page", PAGE.as_bytes());
        let urls: Vec<&str> = assets.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/css/app.css",
                "https://example.com/fonts/main.woff2",
                "https://example.com/favicon-32.png",
                "https://example.com/js/app.js",
                "https://example.com/img/hero.png",
                "https://cdn.example.com/x.png",
                "https://static.example.com/lib.js",
                "https://example.com/favicon.ico",
            ]
        );
        let classes: Vec<RequestClass> = assets.iter().map(|(_, c)| *c).collect();
        assert_eq!(
            classes,
            vec![
                RequestClass::Style,
                RequestClass::Font,
                RequestClass::Favicon,
                RequestClass::Script,
                RequestClass::Image,
                RequestClass::Image,
                RequestClass::Script,
                RequestClass::Favicon,
            ]
        );
        // Media and plain links are not subresources.
        assert!(!urls.iter().any(|u| u.contains("intro.mp4")));
        assert!(!urls.iter().any(|u| u.contains("about")));
    }

    #[test]
    fn resolves_relative_and_skips_non_http() {
        let page = r#"<script src="js/a.js"></script>
            <script src="//cdn.example.com/b.js"></script>
            <script src="data:text/javascript,void(0)"></script>"#;
        let assets = extract_assets("https://example.com/dir/page", page.as_bytes());
        let urls: Vec<&str> = assets.iter().map(|(u, _)| u.as_str()).collect();
        assert!(urls.contains(&"https://example.com/dir/js/a.js"));
        assert!(urls.contains(&"https://cdn.example.com/b.js"));
        assert!(!urls.iter().any(|u| u.starts_with("data:")));
    }

    #[test]
    fn dedupes_and_caps() {
        let mut page = String::new();
        for i in 0..40 {
            page.push_str(&format!("<script src=\"/js/{i}.js\"></script>"));
        }
        page.push_str("<script src=\"/js/0.js\"></script>");
        let assets = extract_assets("https://example.com/", page.as_bytes());
        // 40 unique scripts but the burst caps at MAX_ASSETS, and
        // the duplicate /js/0.js never appears twice. +1 favicon.
        assert_eq!(assets.len(), MAX_ASSETS + 1);
        let zero_count = assets
            .iter()
            .filter(|(u, _)| u.ends_with("/js/0.js"))
            .count();
        assert_eq!(zero_count, 1);
    }

    #[test]
    fn images_capped_at_three() {
        let page = (0..10)
            .map(|i| format!("<img src=\"/img/{i}.png\">"))
            .collect::<String>();
        let assets = extract_assets("https://example.com/", page.as_bytes());
        let images = assets
            .iter()
            .filter(|(_, c)| *c == RequestClass::Image)
            .count();
        assert_eq!(images, 3);
    }

    #[test]
    fn single_quoted_and_unquoted_attrs() {
        let page = r#"<link rel='stylesheet' href='/a.css'>
            <script src=/b.js></script>"#;
        let assets = extract_assets("https://example.com/", page.as_bytes());
        let urls: Vec<&str> = assets.iter().map(|(u, _)| u.as_str()).collect();
        assert!(urls.contains(&"https://example.com/a.css"));
        assert!(urls.contains(&"https://example.com/b.js"));
    }
}
