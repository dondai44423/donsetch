//! Keyless image search (`web_search` with `intent: "image"`), rank-badged
//! contact-sheet rendering, and direct raster image inspection (`web_fetch`).
//!
//! Queries Bing Images async HTML, DuckDuckGo Images (`i.js`), and the
//! Wikimedia Commons MediaWiki API in parallel using DonSeTch's trusted TLS
//! roots, configured proxies/timeouts, and per-hop SSRF-guarded redirects.
//! Merges and ranks results by cross-source agreement, query relevance,
//! resolution, and watermark-host demotion, and optionally composites a crisp
//! rank-badged PNG contact sheet for vision-capable agents.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Lang;

static BING_CARD_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.iuscp").expect("static selector"));
static BING_LINK_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.iusc").expect("static selector"));
static BING_DIM_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".img_info .nowrap, span.nowrap").expect("static selector"));

/// Route-cached HTTP clients for image search and thumbnails so TLS root
/// loading and connection pools are reused across calls.
static IMAGE_HTTP: LazyLock<Mutex<HashMap<String, reqwest::Client>>> =
    LazyLock::new(Default::default);

/// Maximum decoded image allocation per thumbnail (defends against PNG/WebP
/// decompression bombs across concurrent contact-sheet tiles).
const MAX_DECODE_DIM: u32 = 8_192;
const MAX_DECODE_ALLOC_BYTES: u64 = 32 * 1024 * 1024;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0";
const MAX_API_BYTES: usize = 2 * 1024 * 1024;
const MAX_THUMB_BYTES: usize = 4 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const THUMB_TIMEOUT: Duration = Duration::from_secs(4);
const SHEET_BUDGET_CAP: Duration = Duration::from_secs(8);
pub const MAX_DIRECT_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Hosts whose public image URLs are almost always watermarked stock previews.
/// Demoted (never dropped) so clean photos and CC media rank ahead of them.
const WATERMARK_HOSTS: &[&str] = &[
    "shutterstock.com",
    "gettyimages.com",
    "istockphoto.com",
    "alamy.com",
    "dreamstime.com",
    "123rf.com",
    "depositphotos.com",
    "vecteezy.com",
    "stock.adobe.com",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageHit {
    pub title: String,
    pub image_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSourceReport {
    pub id: String,
    pub items: usize,
    pub status: String,
    pub state: &'static str,
}

#[derive(Debug, Clone)]
pub struct ImageSearchOutcome {
    pub query: String,
    pub results: Vec<ImageHit>,
    pub sources: Vec<ImageSourceReport>,
    pub elapsed: Duration,
    pub sheet_png: Option<Vec<u8>>,
    pub sheet_tiles: usize,
    pub sheet_attempted: usize,
    pub sheet_note: Option<String>,
}

fn is_http_url(u: &str) -> bool {
    let trimmed = u.trim();
    trimmed.starts_with("https://") || trimmed.starts_with("http://")
}

fn encode_query(q: &str) -> String {
    url::form_urlencoded::byte_serialize(q.as_bytes()).collect()
}

fn source_state(status: &str) -> &'static str {
    match status {
        "ok" => "answered",
        "empty" => "empty",
        "timeout" => "late",
        "rate-limited" => "rested",
        _ => "failed",
    }
}

/// Strip Wikimedia's `utm_*` tracking parameters from `imageinfo.url` while
/// leaving every other query parameter intact.
fn clean_wikimedia_url(raw: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return raw.to_string();
    };
    if parsed.query().is_none() {
        return raw.to_string();
    }
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !k.to_ascii_lowercase().starts_with("utm_"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        parsed.set_query(None);
    } else {
        let q = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(kept)
            .finish();
        parsed.set_query(Some(&q));
    }
    parsed.to_string()
}

fn report(id: &str, items: usize, status: impl Into<String>) -> ImageSourceReport {
    let status = status.into();
    let state = source_state(&status);
    ImageSourceReport {
        id: id.into(),
        items,
        status,
        state,
    }
}

/// Parse pixel dimensions like `1600×1067`, `1600 x 1067`, or `1600X1067`.
pub(crate) fn parse_dims_text(raw: &str) -> Option<(u32, u32)> {
    let trimmed = raw.trim();
    let sep = trimmed.find(['\u{00d7}', 'x', 'X'])?;
    let sep_len = trimmed[sep..].chars().next()?.len_utf8();
    let w: u32 = trimmed[..sep].trim().parse().ok()?;
    let h: u32 = trimmed[sep + sep_len..].trim().parse().ok()?;
    (w > 0 && h > 0 && w <= 50_000 && h <= 50_000).then_some((w, h))
}

/// Parse Bing Images async HTML (`div.iuscp` cards or bare `a.iusc[m]`) into
/// structured hits, extracting pixel dimensions from `.img_info .nowrap` when
/// present.
pub fn parse_bing_images_html(body: &str, max: usize) -> Vec<ImageHit> {
    let doc = Html::parse_document(body);
    let card_sel = &*BING_CARD_SEL;
    let link_sel = &*BING_LINK_SEL;
    let dim_sel = &*BING_DIM_SEL;

    let mut hits = Vec::new();
    let mut seen = HashSet::new();

    let mut push_link = |el: scraper::ElementRef<'_>, dims: Option<(u32, u32)>| -> bool {
        if hits.len() >= max {
            return true;
        }
        let Some(raw_m) = el.value().attr("m") else {
            return false;
        };
        let Ok(meta) = serde_json::from_str::<Value>(raw_m) else {
            return false;
        };
        let Some(murl) = meta.get("murl").and_then(Value::as_str).map(str::trim) else {
            return false;
        };
        if !is_http_url(murl) || !seen.insert(murl.to_string()) {
            return false;
        }
        let thumb = meta
            .get("turl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| is_http_url(s))
            .map(str::to_string);
        let page = meta
            .get("purl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| is_http_url(s))
            .map(str::to_string);
        let title = meta
            .get("t")
            .and_then(Value::as_str)
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let (width, height) = match dims {
            Some((w, h)) => (Some(w), Some(h)),
            None => (None, None),
        };
        hits.push(ImageHit {
            title,
            image_url: murl.to_string(),
            thumb_url: thumb,
            page_url: page,
            width,
            height,
            source: "bing-images".to_string(),
        });
        hits.len() >= max
    };

    for card in doc.select(card_sel) {
        let Some(link) = card.select(link_sel).next() else {
            continue;
        };
        let dims = card
            .select(dim_sel)
            .find_map(|n| parse_dims_text(&n.text().collect::<String>()));
        if push_link(link, dims) {
            break;
        }
    }

    // Fallback for bare `a.iusc` markup without `div.iuscp` wrappers.
    for el in doc.select(link_sel) {
        if push_link(el, None) {
            break;
        }
    }

    hits
}

/// Extract DuckDuckGo's `vqd` token from the HTML shell page.
pub fn extract_ddg_vqd(html: &str) -> Option<String> {
    for needle in ["vqd=\"", "vqd='", "vqd=", "vqd\":\""] {
        let mut rest = html;
        while let Some(pos) = rest.find(needle) {
            let after = &rest[pos + needle.len()..];
            let token: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            // Real vqd tokens carry digits and a hyphen (e.g. `4-123456...`),
            // never tiny placeholder keys.
            if token.len() >= 12 && token.contains('-') && token.chars().any(|c| c.is_ascii_digit())
            {
                return Some(token);
            }
            rest = after;
        }
    }
    None
}

/// Parse DuckDuckGo Images `i.js` JSON payload into structured hits.
pub fn parse_ddg_images_json(val: &Value, max: usize) -> Vec<ImageHit> {
    let Some(items) = val.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        let Some(image_url) = item.get("image").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if !is_http_url(image_url) || !seen.insert(image_url.to_string()) {
            continue;
        }
        let thumb_url = item
            .get("thumbnail")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| is_http_url(s))
            .map(str::to_string);
        let page_url = item
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| is_http_url(s))
            .map(str::to_string);
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let width = item
            .get("width")
            .and_then(Value::as_u64)
            .filter(|&w| w > 0 && w <= 50_000)
            .map(|w| w as u32);
        let height = item
            .get("height")
            .and_then(Value::as_u64)
            .filter(|&h| h > 0 && h <= 50_000)
            .map(|h| h as u32);
        hits.push(ImageHit {
            title,
            image_url: image_url.to_string(),
            thumb_url,
            page_url,
            width,
            height,
            source: "ddg-images".to_string(),
        });
        if hits.len() >= max {
            break;
        }
    }
    hits
}

/// Parse Wikimedia Commons `action=query&prop=imageinfo` JSON into structured hits.
pub fn parse_wikimedia_images_json(val: &Value, max: usize) -> Vec<ImageHit> {
    let Some(pages) = val
        .get("query")
        .and_then(|q| q.get("pages"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    for page in pages {
        let Some(info) = page
            .get("imageinfo")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
        else {
            continue;
        };
        let Some(raw_url) = info.get("url").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if !is_http_url(raw_url) {
            continue;
        }
        let clean_url = clean_wikimedia_url(raw_url);
        if !seen.insert(clean_url.clone()) {
            continue;
        }
        let mime = info.get("mime").and_then(Value::as_str).unwrap_or("");
        if !mime.is_empty() && !mime.starts_with("image/") {
            continue;
        }
        let thumb = info
            .get("thumburl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| is_http_url(s))
            .map(str::to_string);
        let page_url = info
            .get("descriptionurl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| is_http_url(s))
            .map(str::to_string);
        let raw_title = page
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let title = raw_title
            .strip_prefix("File:")
            .unwrap_or(raw_title)
            .replace('_', " ");
        let width = info.get("width").and_then(Value::as_u64).map(|n| n as u32);
        let height = info.get("height").and_then(Value::as_u64).map(|n| n as u32);
        hits.push(ImageHit {
            title,
            image_url: clean_url,
            thumb_url: thumb,
            page_url,
            width,
            height,
            source: "wikimedia".to_string(),
        });
        if hits.len() >= max {
            break;
        }
    }
    hits
}

/// Canonical deduplication key for an image URL: ignores scheme (`http` vs
/// `https`), lowercases host, trims trailing slash, drops fragments and common
/// tracking query parameters (`utm_*`, `fbclid`, `gclid`).
pub fn canonical_image_key(raw: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(raw.trim()) else {
        return raw.trim().to_ascii_lowercase();
    };
    parsed.set_fragment(None);
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let path = parsed.path().trim_end_matches('/');
    let kept_query: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| {
            let kl = k.to_ascii_lowercase();
            !kl.starts_with("utm_") && kl != "fbclid" && kl != "gclid" && kl != "ref"
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut key = format!("{host}{path}");
    if !kept_query.is_empty() {
        let q = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(kept_query)
            .finish();
        key.push('?');
        key.push_str(&q);
    }
    key
}

fn is_watermark_host(url_str: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    WATERMARK_HOSTS
        .iter()
        .any(|w| host == *w || host.ends_with(&format!(".{w}")))
}

fn query_tokens(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| s.chars().count() >= 2)
        .collect()
}

/// Merge primary and secondary image hit lists, deduplicating by canonical URL.
pub fn merge_image_hits(
    primary: Vec<ImageHit>,
    secondary: Vec<ImageHit>,
    max: usize,
) -> Vec<ImageHit> {
    let mut out = Vec::with_capacity(max);
    let mut seen = HashSet::new();
    for hit in primary.into_iter().chain(secondary) {
        if seen.insert(canonical_image_key(&hit.image_url)) {
            out.push(hit);
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

/// Merge hits across multiple independent image sources, fuse duplicate URLs
/// (preserving the richest dimensions, thumbnail, and title), and rank by:
/// 1. Reciprocal Rank Fusion (cross-source agreement floats to the top),
/// 2. Query-token lexical overlap on title / URL / source page,
/// 3. Resolution & aspect-ratio quality (penalizing tiny icons and extreme strips),
/// 4. Watermarked stock-preview host demotion.
pub fn merge_and_rank_image_hits(
    query: &str,
    lists: Vec<Vec<ImageHit>>,
    max: usize,
) -> Vec<ImageHit> {
    struct Entry {
        hit: ImageHit,
        rrf: f64,
        sources: HashSet<String>,
        best_rank: usize,
        first_seen: usize,
    }

    let tokens = query_tokens(query);
    let mut by_key: HashMap<String, Entry> = HashMap::new();
    let mut seq = 0usize;

    for list in lists {
        for (rank, hit) in list.into_iter().enumerate() {
            let key = canonical_image_key(&hit.image_url);
            // Source-specific gentle prior: web indexes carry broad coverage,
            // Wikimedia gets a slightly smaller base RRF weight unless its
            // lexical match is strong so generic queries stay visually on-topic.
            let weight = if hit.source == "wikimedia" { 0.85 } else { 1.0 };
            let contrib = weight / (12.0 + (rank as f64) + 1.0);

            if let Some(existing) = by_key.get_mut(&key) {
                if existing.sources.insert(hit.source.clone()) {
                    existing.rrf += contrib;
                } else {
                    // Same source via a query variant: small diminishing boost.
                    existing.rrf += contrib * 0.35;
                }
                existing.best_rank = existing.best_rank.min(rank);
                // Prefer https over http and enrich missing metadata.
                if existing.hit.image_url.starts_with("http://")
                    && hit.image_url.starts_with("https://")
                {
                    existing.hit.image_url = hit.image_url;
                }
                if existing.hit.width.is_none() && hit.width.is_some() {
                    existing.hit.width = hit.width;
                    existing.hit.height = hit.height;
                }
                if existing.hit.thumb_url.is_none() && hit.thumb_url.is_some() {
                    existing.hit.thumb_url = hit.thumb_url;
                }
                if existing.hit.page_url.is_none() && hit.page_url.is_some() {
                    existing.hit.page_url = hit.page_url;
                }
                if existing.hit.title.len() < hit.title.len() {
                    existing.hit.title = hit.title;
                }
            } else {
                let mut sources = HashSet::new();
                sources.insert(hit.source.clone());
                by_key.insert(
                    key,
                    Entry {
                        hit,
                        rrf: contrib,
                        sources,
                        best_rank: rank,
                        first_seen: seq,
                    },
                );
                seq += 1;
            }
        }
    }

    let mut scored: Vec<(f64, usize, usize, ImageHit)> = by_key
        .into_values()
        .map(|entry| {
            let mut score = entry.rrf;

            // Cross-source consensus bonus.
            if entry.sources.len() >= 2 {
                score += 0.04 * (entry.sources.len() - 1) as f64;
            }

            // Query-token match against title + URL + page URL.
            if !tokens.is_empty() {
                let hay = format!(
                    "{} {} {}",
                    entry.hit.title,
                    entry.hit.image_url,
                    entry.hit.page_url.as_deref().unwrap_or("")
                )
                .to_ascii_lowercase();
                let matched = tokens.iter().filter(|t| hay.contains(t.as_str())).count();
                score += 0.055 * (matched as f64 / tokens.len() as f64);
            }

            // Resolution & aspect-ratio quality signal.
            if let (Some(w), Some(h)) = (entry.hit.width, entry.hit.height) {
                let min_d = w.min(h);
                let max_d = w.max(h);
                let aspect = max_d as f64 / min_d.max(1) as f64;
                if min_d < 160 {
                    score -= 0.07;
                } else if aspect > 6.0 {
                    score -= 0.05;
                } else if min_d >= 400 && max_d >= 600 && aspect <= 4.0 {
                    score += 0.028;
                } else if min_d >= 240 {
                    score += 0.01;
                }
            }

            // Demote watermarked stock-preview hosts.
            if is_watermark_host(&entry.hit.image_url)
                || entry.hit.page_url.as_deref().is_some_and(is_watermark_host)
            {
                score -= 0.085;
            }

            // Slight preference for direct raster extensions over opaque scripts.
            let lower_url = entry.hit.image_url.to_ascii_lowercase();
            let path_part = lower_url.split('?').next().unwrap_or(&lower_url);
            if path_part.ends_with(".jpg")
                || path_part.ends_with(".jpeg")
                || path_part.ends_with(".png")
                || path_part.ends_with(".webp")
            {
                score += 0.006;
            }

            (score, entry.best_rank, entry.first_seen, entry.hit)
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    scored
        .into_iter()
        .take(max)
        .map(|(_, _, _, hit)| hit)
        .collect()
}

fn remaining_budget(cap: Duration) -> Duration {
    match super::SEARCH_DEADLINE.try_with(|d| *d).ok().flatten() {
        Some(deadline) => deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(cap)
            .max(Duration::from_millis(250)),
        None => cap,
    }
}

fn image_source_timeout() -> Duration {
    remaining_budget(super::fanout_hard_mark())
}

fn cached_http_client(
    proxy: Option<&crate::egress::proxy::Proxy>,
) -> Result<reqwest::Client, String> {
    let route_key = proxy
        .map(|p| p.connection_key())
        .unwrap_or_else(|| "direct".into());
    if let Some(existing) = IMAGE_HTTP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&route_key)
        .cloned()
    {
        return Ok(existing);
    }
    let mut builder = crate::egress::trust::with_trusted_roots(reqwest::Client::builder())
        .user_agent(USER_AGENT)
        .connect_timeout(super::search_connect_timeout())
        // Redirects are followed manually via `fetch_bounded_safe` so every hop
        // passes DNS/IP SSRF validation (`ensure_url_safe`).
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(4)
        .no_proxy();
    if let Some(p) = proxy {
        // Fail closed on SOCKS5 rather than silently bypassing to the host IP.
        if p.scheme == crate::egress::proxy::ProxyScheme::Socks5 {
            return Err(
                "invalid-config: image search requires an HTTP proxy or a direct route".into(),
            );
        }
        let rp = reqwest::Proxy::all(p.wire_url())
            .map_err(|_| "invalid-config: unsupported proxy configuration".to_string())?;
        builder = builder.proxy(rp);
    }
    let built = builder.build().map_err(|e| e.to_string())?;
    let mut map = IMAGE_HTTP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() >= 16 {
        map.clear();
    }
    map.insert(route_key, built.clone());
    Ok(built)
}

/// Returns `(proxy, from_pool)`: pool proxies only apply to search-engine
/// queries, whereas configured/ambient forward proxies must stay active for
/// thumbnail fetches too.
fn pick_search_proxy(url: &str) -> Result<(Option<crate::egress::proxy::Proxy>, bool), String> {
    if let Some(pool) = super::egress::global()
        && let Some(egress) = pool.pick("bing", &[], true)
        && let Some(p) = egress.proxy
        && p.scheme != crate::egress::proxy::ProxyScheme::Socks5
    {
        return Ok((Some(p), true));
    }
    let env_proxy = crate::egress::proxy::from_env_for(url).map_err(|e| e.to_string())?;
    Ok((env_proxy, false))
}

/// Fetch a URL following up to `MAX_REDIRECTS` redirects while enforcing
/// full DNS/IP SSRF safety (`ensure_url_safe`) on EVERY hop and streaming the
/// response body under a strict `max_bytes` cap so oversized responses cannot
/// OOM the daemon.
async fn fetch_bounded_safe(
    client: &reqwest::Client,
    initial_url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
    max_bytes: usize,
) -> Result<(u16, Vec<u8>), String> {
    let mut current = initial_url.to_string();
    let deadline = tokio::time::Instant::now() + timeout;

    for hop in 0..=MAX_REDIRECTS {
        crate::fetch::guards::ensure_url_safe(&current)
            .await
            .map_err(|e| format!("ssrf:{e}"))?;
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        if rem.is_zero() {
            return Err("timeout".into());
        }

        let mut req = client.get(&current).timeout(rem);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut resp = req
            .send()
            .await
            .map_err(|e| if e.is_timeout() { "timeout" } else { "error" }.to_string())?;
        let status = resp.status();

        if status.is_redirection() {
            if hop >= MAX_REDIRECTS {
                return Err("too-many-redirects".into());
            }
            let Some(loc) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return Err(format!("http-{}", status.as_u16()));
            };
            let base = url::Url::parse(&current).map_err(|_| "bad-url".to_string())?;
            let next = base.join(loc).map_err(|_| "bad-redirect".to_string())?;
            current = next.to_string();
            continue;
        }

        if !status.is_success() {
            return Ok((status.as_u16(), Vec::new()));
        }

        if let Some(cl) = resp.content_length()
            && cl as usize > max_bytes
        {
            return Err("too-large".into());
        }

        let mut buf = Vec::with_capacity(
            resp.content_length()
                .map(|n| (n as usize).min(max_bytes).min(256 * 1024))
                .unwrap_or(16 * 1024),
        );
        while let Some(chunk) = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .map_err(|_| "timeout".to_string())?
            .map_err(|_| "read-error".to_string())?
        {
            if buf.len().saturating_add(chunk.len()) > max_bytes {
                return Err("too-large".into());
            }
            buf.extend_from_slice(&chunk);
        }
        return Ok((status.as_u16(), buf));
    }
    Err("too-many-redirects".into())
}

async fn fetch_bing_images(
    client: &reqwest::Client,
    query: &str,
    max: usize,
    lang: Option<&Lang>,
    timeout: Duration,
) -> (Vec<ImageHit>, ImageSourceReport) {
    let count = (max * 2).clamp(12, 30);
    let mut url = format!(
        "https://www.bing.com/images/async?q={}&first=1&count={count}&mmasync=1&adlt=moderate",
        encode_query(query)
    );
    let accept_lang = match lang {
        Some(l) => {
            url.push_str("&setlang=");
            url.push_str(l.code());
            if l.region().is_some() {
                url.push_str("&mkt=");
                url.push_str(&l.tag());
            }
            format!("{},{};q=0.9", l.tag(), l.code())
        }
        None => "en-US,en;q=0.9".to_string(),
    };
    let headers = [
        ("Accept", "text/html,application/xhtml+xml"),
        ("Accept-Language", accept_lang.as_str()),
    ];
    match fetch_bounded_safe(client, &url, &headers, timeout, MAX_API_BYTES).await {
        Ok((status, bytes)) if (200..300).contains(&status) => {
            let body = String::from_utf8_lossy(&bytes);
            let hits = parse_bing_images_html(&body, max * 2);
            let st = if hits.is_empty() { "empty" } else { "ok" };
            let n = hits.len();
            (hits, report("bing-images", n, st))
        }
        Ok((status, _)) => (
            Vec::new(),
            report("bing-images", 0, format!("http-{status}")),
        ),
        Err(st) => (Vec::new(), report("bing-images", 0, st)),
    }
}

async fn fetch_ddg_images(
    client: &reqwest::Client,
    query: &str,
    max: usize,
    lang: Option<&Lang>,
    timeout: Duration,
) -> (Vec<ImageHit>, ImageSourceReport) {
    let deadline = tokio::time::Instant::now() + timeout;
    let token_url = format!(
        "https://duckduckgo.com/?q={}&iax=images&ia=images",
        encode_query(query)
    );
    let token_headers = [("Accept", "text/html,application/xhtml+xml")];
    let vqd = match fetch_bounded_safe(client, &token_url, &token_headers, timeout, MAX_API_BYTES)
        .await
    {
        Ok((202 | 429, _)) => return (Vec::new(), report("ddg-images", 0, "rate-limited")),
        Ok((status, bytes)) if (200..300).contains(&status) => {
            let html = String::from_utf8_lossy(&bytes);
            match extract_ddg_vqd(&html) {
                Some(v) => v,
                None => return (Vec::new(), report("ddg-images", 0, "vqd-missing")),
            }
        }
        Ok((status, _)) => {
            return (
                Vec::new(),
                report("ddg-images", 0, format!("http-{status}")),
            );
        }
        Err(st) => return (Vec::new(), report("ddg-images", 0, st)),
    };

    let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
    if rem.is_zero() {
        return (Vec::new(), report("ddg-images", 0, "timeout"));
    }

    let mut api_url = format!(
        "https://duckduckgo.com/i.js?o=json&q={}&vqd={}&f=,,,&p=1",
        encode_query(query),
        encode_query(&vqd)
    );
    // Respect DonSeTch's explicit-region rule: only pass `l=<kl>` when the
    // caller explicitly included a region subtag (e.g. `fr-FR`, `pt-BR`).
    if let Some(kl) = lang.and_then(Lang::kl) {
        api_url.push_str("&l=");
        api_url.push_str(&kl);
    }
    let api_headers = [
        ("Accept", "application/json, text/javascript, */*; q=0.01"),
        ("Referer", "https://duckduckgo.com/"),
    ];
    match fetch_bounded_safe(client, &api_url, &api_headers, rem, MAX_API_BYTES).await {
        Ok((202 | 429, _)) => (Vec::new(), report("ddg-images", 0, "rate-limited")),
        Ok((status, bytes)) if (200..300).contains(&status) => {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(val) => {
                    let hits = parse_ddg_images_json(&val, max * 2);
                    let st = if hits.is_empty() { "empty" } else { "ok" };
                    let n = hits.len();
                    (hits, report("ddg-images", n, st))
                }
                Err(_) => (Vec::new(), report("ddg-images", 0, "parse-error")),
            }
        }
        Ok((status, _)) => (
            Vec::new(),
            report("ddg-images", 0, format!("http-{status}")),
        ),
        Err(st) => (Vec::new(), report("ddg-images", 0, st)),
    }
}

async fn fetch_wikimedia_images(
    client: &reqwest::Client,
    query: &str,
    max: usize,
    timeout: Duration,
) -> (Vec<ImageHit>, ImageSourceReport) {
    let gsrlimit = max.clamp(4, 12);
    let url = format!(
        "https://commons.wikimedia.org/w/api.php?action=query&generator=search&gsrsearch={}&gsrnamespace=6&gsrlimit={gsrlimit}&prop=imageinfo&iiprop=url%7Csize%7Cmime&iiurlwidth=640&format=json&formatversion=2",
        encode_query(query)
    );
    let headers = [("Accept", "application/json")];
    match fetch_bounded_safe(client, &url, &headers, timeout, MAX_API_BYTES).await {
        Ok((status, bytes)) if (200..300).contains(&status) => {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(val) => {
                    let hits = parse_wikimedia_images_json(&val, max);
                    let st = if hits.is_empty() { "empty" } else { "ok" };
                    let n = hits.len();
                    (hits, report("wikimedia", n, st))
                }
                Err(_) => (Vec::new(), report("wikimedia", 0, "parse-error")),
            }
        }
        Ok((status, _)) => (Vec::new(), report("wikimedia", 0, format!("http-{status}"))),
        Err(st) => (Vec::new(), report("wikimedia", 0, st)),
    }
}

/// Recognize direct raster image payloads (PNG/JPEG/WebP/GIF) strictly by
/// magic bytes so an HTML/text error response mislabeled `Content-Type: image/*`
/// is never emitted as a broken MCP image block. Returns the canonical MIME
/// type and pixel dimensions when decodable from headers.
pub fn inspect_raster_image(
    body: &[u8],
    _content_type: &str,
) -> Option<(&'static str, Option<(u32, u32)>)> {
    let mime = if body.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        "image/png"
    } else if body.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if body.len() >= 12 && &body[0..4] == b"RIFF" && &body[8..12] == b"WEBP" {
        "image/webp"
    } else if body.starts_with(b"GIF87a") || body.starts_with(b"GIF89a") {
        "image/gif"
    } else {
        return None;
    };

    let dims = image::ImageReader::new(Cursor::new(body))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok());

    Some((mime, dims))
}

/// Tiny 3x5 bitmap glyphs for `#` and `0`-`9` (zero external font deps).
fn glyph_3x5(ch: char) -> [u8; 5] {
    match ch {
        '#' => [0b010, 0b111, 0b010, 0b111, 0b010],
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        _ => [0; 5],
    }
}

/// Stamp a high-contrast `#N` rank badge at `(x0, y0)` on a contact-sheet tile.
fn draw_rank_badge(canvas: &mut image::RgbaImage, x0: u32, y0: u32, rank: usize) {
    use image::Rgba;

    let label = format!("#{rank}");
    let scale: u32 = 3;
    let char_w = 3 * scale;
    let char_h = 5 * scale;
    let gap = scale;
    let pad_x: u32 = 6;
    let pad_y: u32 = 5;
    let n = label.chars().count() as u32;
    let box_w = pad_x * 2 + n * char_w + n.saturating_sub(1) * gap;
    let box_h = pad_y * 2 + char_h;

    let bx = x0 + 6;
    let by = y0 + 6;
    let bg = Rgba([10, 12, 16, 255]);
    let border = Rgba([225, 65, 55, 255]);
    let fg = Rgba([245, 247, 250, 255]);

    for y in by..(by + box_h).min(canvas.height()) {
        for x in bx..(bx + box_w).min(canvas.width()) {
            let is_edge = x == bx || x + 1 == bx + box_w || y == by || y + 1 == by + box_h;
            canvas.put_pixel(x, y, if is_edge { border } else { bg });
        }
    }

    for (ci, ch) in label.chars().enumerate() {
        let rows = glyph_3x5(ch);
        let gx = bx + pad_x + (ci as u32) * (char_w + gap);
        let gy = by + pad_y;
        for (ry, bits) in rows.iter().enumerate() {
            for rx in 0..3u32 {
                if (bits >> (2 - rx)) & 1 == 1 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let px = gx + rx * scale + sx;
                            let py = gy + (ry as u32) * scale + sy;
                            if px < canvas.width() && py < canvas.height() {
                                canvas.put_pixel(px, py, fg);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Composite up to 9 rank-tagged images into a crisp dark 3-column PNG contact
/// sheet. Each tile carries a `#N` badge matching its 1-based rank in the
/// result list.
pub fn build_contact_sheet_png(images: &[(usize, image::DynamicImage)]) -> Result<Vec<u8>, String> {
    use image::{GenericImage, ImageFormat, Rgba, RgbaImage, imageops::FilterType};

    if images.is_empty() {
        return Err("no images decoded for contact sheet".into());
    }
    let cols: u32 = images.len().min(3) as u32;
    let rows: u32 = (images.len() as u32).div_ceil(cols);
    // 360x240 keeps a 3x3 contact sheet crisp for vision models while cutting
    // PNG wire size by ~35% vs 420x280.
    let cell_w: u32 = 360;
    let cell_h: u32 = 240;
    let pad: u32 = 10;

    let sheet_w = cols * cell_w + (cols + 1) * pad;
    let sheet_h = rows * cell_h + (rows + 1) * pad;
    let mut canvas = RgbaImage::from_pixel(sheet_w, sheet_h, Rgba([12, 13, 18, 255]));
    let cell_bg = Rgba([22, 24, 32, 255]);
    let cell_border = Rgba([46, 51, 68, 255]);

    for (slot, (rank, img)) in images.iter().enumerate() {
        let r = (slot as u32) / cols;
        let c = (slot as u32) % cols;
        let x0 = pad + c * (cell_w + pad);
        let y0 = pad + r * (cell_h + pad);

        // Cell backdrop with subtle 1px frame separating dark photos from canvas.
        for y in y0..(y0 + cell_h) {
            for x in x0..(x0 + cell_w) {
                let is_frame = x == x0 || x + 1 == x0 + cell_w || y == y0 || y + 1 == y0 + cell_h;
                canvas.put_pixel(x, y, if is_frame { cell_border } else { cell_bg });
            }
        }
        let inner_w = cell_w - 2;
        let inner_h = cell_h - 2;
        let thumb = img
            .resize(inner_w, inner_h, FilterType::CatmullRom)
            .to_rgba8();
        let ox = x0 + 1 + (inner_w.saturating_sub(thumb.width())) / 2;
        let oy = y0 + 1 + (inner_h.saturating_sub(thumb.height())) / 2;
        let _ = canvas.copy_from(&thumb, ox, oy);
        draw_rank_badge(&mut canvas, x0, y0, *rank);
    }

    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(canvas)
        .write_to(&mut out, ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// Decode untrusted thumbnail bytes under strict pixel/allocation limits so a
/// tiny compressed PNG/WebP bomb cannot exhaust daemon memory.
fn decode_bounded_thumbnail(bytes: &[u8]) -> Option<image::DynamicImage> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIM);
    limits.max_image_height = Some(MAX_DECODE_DIM);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let img = reader.decode().ok()?;
    (img.width() >= 32 && img.height() >= 32).then_some(img)
}

async fn fetch_contact_sheet(
    client: &reqwest::Client,
    hits: &[ImageHit],
) -> (Option<Vec<u8>>, usize, usize, Option<String>) {
    use futures_util::future::join_all;

    let attempted = hits.len().min(9);
    let sheet_budget = remaining_budget(SHEET_BUDGET_CAP);
    let sheet_deadline = tokio::time::Instant::now() + sheet_budget;

    let futs = hits.iter().take(attempted).enumerate().map(|(idx, hit)| {
        let rank = idx + 1;
        let client = client.clone();
        let page_ref = hit.page_url.clone();
        // Deduplicate candidate URLs so a hit without a separate thumbnail
        // never wastes a second timeout retrying the identical URL.
        let mut candidates: Vec<(String, bool)> = Vec::with_capacity(2);
        if let Some(t) = &hit.thumb_url {
            candidates.push((t.clone(), false));
        }
        if candidates.first().map(|(u, _)| u.as_str()) != Some(hit.image_url.as_str()) {
            candidates.push((hit.image_url.clone(), true));
        }
        async move {
            for (candidate, is_origin) in candidates {
                let rem = sheet_deadline.saturating_duration_since(tokio::time::Instant::now());
                if rem < Duration::from_millis(200) {
                    break;
                }
                let per_try = rem.min(THUMB_TIMEOUT);
                let mut hdrs: Vec<(&str, &str)> = vec![(
                    "Accept",
                    "image/webp,image/png,image/jpeg,image/gif,*/*;q=0.8",
                )];
                // Sending the source page as Referer when falling back to the
                // origin image URL clears common hotlink-protection checks.
                if is_origin && let Some(ref_url) = page_ref.as_deref() {
                    hdrs.push(("Referer", ref_url));
                }
                let Ok((status, bytes)) =
                    fetch_bounded_safe(&client, &candidate, &hdrs, per_try, MAX_THUMB_BYTES).await
                else {
                    continue;
                };
                if !(200..300).contains(&status) || bytes.is_empty() {
                    continue;
                }
                // Decode on a blocking worker with tight pixel/RAM limits so
                // CPU-bound decompression never stalls Tokio async workers.
                if let Ok(Some(img)) =
                    tokio::task::spawn_blocking(move || decode_bounded_thumbnail(&bytes)).await
                {
                    return Some((rank, img));
                }
            }
            None
        }
    });

    let decoded: Vec<(usize, image::DynamicImage)> =
        join_all(futs).await.into_iter().flatten().collect();
    let tiles = decoded.len();
    if decoded.is_empty() {
        return (
            None,
            0,
            attempted,
            Some("contact_sheet requested, but no thumbnails could be fetched cleanly".into()),
        );
    }
    let partial_note = (tiles < attempted).then(|| {
        format!(
            "contact_sheet rendered {tiles}/{attempted} tiles ({} failed to fetch or decode)",
            attempted - tiles
        )
    });
    match tokio::task::spawn_blocking(move || build_contact_sheet_png(&decoded)).await {
        Ok(Ok(png)) => (Some(png), tiles, attempted, partial_note),
        Ok(Err(e)) => (
            None,
            0,
            attempted,
            Some(format!("contact_sheet encoding failed: {e}")),
        ),
        Err(e) => (
            None,
            0,
            attempted,
            Some(format!("contact_sheet worker failed: {e}")),
        ),
    }
}

/// Run a keyless image search across Bing Images, DuckDuckGo Images, and
/// Wikimedia Commons for a single query.
pub async fn search_images(query: &str, max: usize, contact_sheet: bool) -> ImageSearchOutcome {
    search_images_multi(&[query.to_string()], max, None, contact_sheet).await
}

/// Run a keyless image search across Bing Images, DuckDuckGo Images, and
/// Wikimedia Commons for one or more query formulations, with optional
/// explicit `lang` and rank-badged contact sheet generation.
pub async fn search_images_multi(
    queries: &[String],
    max: usize,
    lang: Option<&Lang>,
    contact_sheet: bool,
) -> ImageSearchOutcome {
    search_images_multi_with_searxng(queries, max, lang, contact_sheet, false).await
}

/// Run an image search across SearXNG (when `prefer_searxng` is true and
/// `[search] searxng_url` is set, with automatic keyless fallback on empty or
/// error) or across Bing Images, DuckDuckGo Images, and Wikimedia Commons.
pub async fn search_images_multi_with_searxng(
    queries: &[String],
    max: usize,
    lang: Option<&Lang>,
    contact_sheet: bool,
    prefer_searxng: bool,
) -> ImageSearchOutcome {
    use futures_util::future::join_all;

    let started = Instant::now();
    let max = max.clamp(1, 12);
    let primary_query = queries.first().map(String::as_str).unwrap_or("");
    let timeout = image_source_timeout();

    let mut all_lists: Vec<Vec<ImageHit>> = Vec::new();
    let mut merged_reports: Vec<ImageSourceReport> = Vec::new();
    let mut searxng_answered = false;

    let sequential = crate::config::cfg().search.sequential;

    if prefer_searxng && let Some(base_url) = super::byok::searxng::configured_url() {
        let sx_timeout = timeout.max(Duration::from_secs(6));
        let per_query = if sequential {
            let mut out = Vec::with_capacity(queries.len());
            for q in queries {
                out.push(
                    super::byok::searxng::search_images(&base_url, q, max, lang, sx_timeout).await,
                );
            }
            out
        } else {
            join_all(
                queries.iter().map(|q| {
                    let base = base_url.clone();
                    async move {
                        super::byok::searxng::search_images(&base, q, max, lang, sx_timeout).await
                    }
                }),
            )
            .await
        };
        for (hits, rep) in per_query {
            if !hits.is_empty() {
                searxng_answered = true;
                all_lists.push(hits);
            }
            if let Some(existing) = merged_reports.iter_mut().find(|r| r.id == rep.id) {
                existing.items += rep.items;
                if existing.state != "answered" && rep.state == "answered" {
                    existing.status = rep.status;
                    existing.state = rep.state;
                }
            } else {
                merged_reports.push(rep);
            }
        }
    }

    let mut thumb_env_proxy: Option<crate::egress::proxy::Proxy> = None;
    let mut cached_search_client: Option<reqwest::Client> = None;
    let mut search_used_pool = false;

    if !searxng_answered {
        let (proxy, from_pool) = match pick_search_proxy("https://www.bing.com/images/async") {
            Ok(pair) => pair,
            Err(e) => {
                return ImageSearchOutcome {
                    query: primary_query.to_string(),
                    results: Vec::new(),
                    sources: vec![report("proxy", 0, format!("invalid-config: {e}"))],
                    elapsed: started.elapsed(),
                    sheet_png: None,
                    sheet_tiles: 0,
                    sheet_attempted: 0,
                    sheet_note: None,
                };
            }
        };
        search_used_pool = from_pool;
        if !from_pool {
            thumb_env_proxy = proxy.clone();
        }
        let search_client = match cached_http_client(proxy.as_ref()) {
            Ok(c) => c,
            Err(e) => {
                return ImageSearchOutcome {
                    query: primary_query.to_string(),
                    results: Vec::new(),
                    sources: vec![report("http", 0, e)],
                    elapsed: started.elapsed(),
                    sheet_png: None,
                    sheet_tiles: 0,
                    sheet_attempted: 0,
                    sheet_note: None,
                };
            }
        };
        cached_search_client = Some(search_client.clone());

        // Query all formulations across the three keyless sources (sequentially
        // when `[search] sequential = true` to avoid saturating slow links).
        let per_query = if sequential {
            let mut out = Vec::with_capacity(queries.len());
            for q in queries {
                let (bing_hits, bing_rep) =
                    fetch_bing_images(&search_client, q, max, lang, timeout).await;
                let (ddg_hits, ddg_rep) =
                    fetch_ddg_images(&search_client, q, max, lang, timeout).await;
                let (wiki_hits, wiki_rep) =
                    fetch_wikimedia_images(&search_client, q, max, timeout).await;
                out.push((
                    vec![bing_hits, ddg_hits, wiki_hits],
                    vec![bing_rep, ddg_rep, wiki_rep],
                ));
            }
            out
        } else {
            join_all(queries.iter().map(|q| {
                let client = search_client.clone();
                async move {
                    let ((bing_hits, bing_rep), (ddg_hits, ddg_rep), (wiki_hits, wiki_rep)) = tokio::join!(
                        fetch_bing_images(&client, q, max, lang, timeout),
                        fetch_ddg_images(&client, q, max, lang, timeout),
                        fetch_wikimedia_images(&client, q, max, timeout),
                    );
                    (
                        vec![bing_hits, ddg_hits, wiki_hits],
                        vec![bing_rep, ddg_rep, wiki_rep],
                    )
                }
            }))
            .await
        };

        for (lists, reps) in per_query {
            all_lists.extend(lists);
            for rep in reps {
                if let Some(existing) = merged_reports.iter_mut().find(|r| r.id == rep.id) {
                    existing.items += rep.items;
                    if existing.state != "answered" && rep.state == "answered" {
                        existing.status = rep.status;
                        existing.state = rep.state;
                    }
                } else {
                    merged_reports.push(rep);
                }
            }
        }
    }

    let results = merge_and_rank_image_hits(primary_query, all_lists, max);

    // Thumbnails ride direct egress unless an explicit/ambient forward proxy is
    // configured for the thumbnail host (never strip a mandatory corporate proxy).
    let (sheet_png, sheet_tiles, sheet_attempted, sheet_note) =
        if contact_sheet && !results.is_empty() {
            let thumb_client = if !search_used_pool && let Some(c) = cached_search_client {
                Ok(c)
            } else {
                let first_url = results[0]
                    .thumb_url
                    .as_deref()
                    .unwrap_or(&results[0].image_url);
                let env_p = thumb_env_proxy
                    .or_else(|| crate::egress::proxy::from_env_for(first_url).ok().flatten());
                cached_http_client(env_p.as_ref())
            };
            match thumb_client {
                Ok(c) => fetch_contact_sheet(&c, &results).await,
                Err(e) => (
                    None,
                    0,
                    results.len().min(9),
                    Some(format!("contact_sheet client error: {e}")),
                ),
            }
        } else {
            (None, 0, 0, None)
        };

    ImageSearchOutcome {
        query: primary_query.to_string(),
        results,
        sources: merged_reports,
        elapsed: started.elapsed(),
        sheet_png,
        sheet_tiles,
        sheet_attempted,
        sheet_note,
    }
}

/// Render compact Markdown for image search results matching DonSeTch's output shape.
pub fn render_image_markdown(out: &ImageSearchOutcome) -> String {
    render_image_markdown_with_handles(out, None)
}

/// Render compact Markdown for image search results, optionally displaying
/// session-local S-handles for 1-step `web_fetch` follow-up in MCP mode.
pub fn render_image_markdown_with_handles(
    out: &ImageSearchOutcome,
    handles: Option<&[String]>,
) -> String {
    let mut lines = vec!["# Image search results".to_string()];
    for (idx, hit) in out.results.iter().enumerate() {
        let title = if hit.title.is_empty() {
            "(untitled image)"
        } else {
            hit.title.as_str()
        };
        let dims = match (hit.width, hit.height) {
            (Some(w), Some(h)) => format!(" · {w}x{h}"),
            _ => String::new(),
        };
        let handle_suffix = handles
            .and_then(|hs| hs.get(idx))
            .map(|h| format!(" ({h})"))
            .unwrap_or_default();
        lines.push(format!(
            "{}. {} · {}{} [{}]{}",
            idx + 1,
            hit.image_url,
            title,
            dims,
            hit.source,
            handle_suffix
        ));
        if let Some(page) = &hit.page_url {
            lines.push(format!("   source: {page}"));
        }
    }
    if out.results.is_empty() {
        lines.push("No matching images returned.".to_string());
    }
    if let Some(note) = &out.sheet_note {
        lines.push(format!("note: {note}"));
    }
    let census = out
        .sources
        .iter()
        .map(|s| format!("{} {} ({})", s.id, s.items, s.status))
        .collect::<Vec<_>>()
        .join(" · ");
    lines.push(format!("sources: {census}"));
    format!("{}\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bing_parser_extracts_valid_hits_dimensions_and_drops_non_http_and_dupes() {
        let html = r#"
        <html><body>
          <div class="iuscp">
            <a class="iusc" m='{"murl":"https://img.example.com/a.jpg","turl":"https://t.example.com/a.jpg","purl":"https://example.com/page-a","t":"Nine-Ball   Unit"}'></a>
            <div class="img_info"><span class="nowrap">1600&#215;1067</span></div>
          </div>
          <a class="iusc" m='{"murl":"https://img.example.com/a.jpg","t":"Duplicate"}'></a>
          <a class="iusc" m='{"murl":"javascript:alert(1)","t":"Bad Scheme"}'></a>
          <a class="iusc" m='not-json'></a>
          <a class="iusc" m='{"murl":"http://img.example.com/b.png","purl":"https://example.com/page-b","t":"Garage HUD"}'></a>
        </body></html>
        "#;
        let hits = parse_bing_images_html(html, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Nine-Ball Unit");
        assert_eq!(hits[0].image_url, "https://img.example.com/a.jpg");
        assert_eq!(hits[0].width, Some(1600));
        assert_eq!(hits[0].height, Some(1067));
        assert_eq!(
            hits[0].thumb_url.as_deref(),
            Some("https://t.example.com/a.jpg")
        );
        assert_eq!(
            hits[0].page_url.as_deref(),
            Some("https://example.com/page-a")
        );
        assert_eq!(hits[1].image_url, "http://img.example.com/b.png");
        assert_eq!(hits[1].width, None);
    }

    #[test]
    fn ddg_vqd_and_json_parser_extract_structured_hits() {
        let shell =
            r#"<script>var x = 1; vqd="4-336654097568711921202995830734462689616";</script>"#;
        assert_eq!(
            extract_ddg_vqd(shell).as_deref(),
            Some("4-336654097568711921202995830734462689616")
        );
        assert_eq!(extract_ddg_vqd("<html>no token here</html>"), None);

        let payload = json!({
            "results": [
                {
                    "title": "Golden   Retriever Photo",
                    "image": "https://dogs.example/golden.jpg",
                    "thumbnail": "https://tse1.mm.bing.net/th?id=abc",
                    "url": "https://dogs.example/gallery",
                    "width": 1200,
                    "height": 800
                },
                {
                    "title": "Duplicate",
                    "image": "https://dogs.example/golden.jpg",
                    "width": 1200,
                    "height": 800
                },
                {
                    "title": "Bad scheme",
                    "image": "data:image/png;base64,AAA"
                }
            ]
        });
        let hits = parse_ddg_images_json(&payload, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Golden Retriever Photo");
        assert_eq!(hits[0].width, Some(1200));
        assert_eq!(hits[0].height, Some(800));
        assert_eq!(hits[0].source, "ddg-images");
    }

    #[test]
    fn wikimedia_parser_extracts_imageinfo_and_skips_non_images() {
        let payload = json!({
            "query": {
                "pages": [
                    {
                        "title": "File:Mecha_diagram.png",
                        "imageinfo": [{
                            "url": "https://upload.wikimedia.org/mecha.png?utm_source=commons.wikimedia.org&utm_campaign=imageinfo",
                            "thumburl": "https://upload.wikimedia.org/thumb/mecha.png",
                            "descriptionurl": "https://commons.wikimedia.org/wiki/File:Mecha_diagram.png",
                            "width": 1200,
                            "height": 800,
                            "mime": "image/png"
                        }]
                    },
                    {
                        "title": "File:Audio_clip.ogg",
                        "imageinfo": [{
                            "url": "https://upload.wikimedia.org/clip.ogg",
                            "mime": "audio/ogg"
                        }]
                    },
                    {
                        "title": "File:Missing_info.jpg"
                    }
                ]
            }
        });
        let hits = parse_wikimedia_images_json(&payload, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Mecha diagram.png");
        assert_eq!(hits[0].image_url, "https://upload.wikimedia.org/mecha.png");
        assert_eq!(hits[0].width, Some(1200));
        assert_eq!(hits[0].height, Some(800));
        assert_eq!(hits[0].source, "wikimedia");
    }

    #[test]
    fn merge_deduplicates_across_sources_and_respects_limit() {
        let mk = |u: &str, src: &str| ImageHit {
            title: u.into(),
            image_url: u.into(),
            thumb_url: None,
            page_url: None,
            width: None,
            height: None,
            source: src.into(),
        };
        let merged = merge_image_hits(
            vec![
                mk("https://a.test/1.jpg", "bing"),
                mk("http://a.test/2.jpg?utm_source=x", "bing"),
            ],
            vec![
                mk("https://a.test/2.jpg", "wikimedia"),
                mk("https://a.test/3.jpg", "wikimedia"),
            ],
            2,
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].image_url, "https://a.test/1.jpg");
        assert_eq!(merged[1].image_url, "http://a.test/2.jpg?utm_source=x");
    }

    #[test]
    fn ranker_promotes_cross_source_consensus_and_demotes_watermarks_and_icons() {
        let bing = vec![
            ImageHit {
                title: "Golden retriever stock preview".into(),
                image_url: "https://www.shutterstock.com/image-photo/golden.jpg".into(),
                thumb_url: None,
                page_url: None,
                width: Some(1000),
                height: Some(700),
                source: "bing-images".into(),
            },
            ImageHit {
                title: "Tiny icon".into(),
                image_url: "https://example.org/icon.png".into(),
                thumb_url: None,
                page_url: None,
                width: Some(64),
                height: Some(64),
                source: "bing-images".into(),
            },
            ImageHit {
                title: "Golden retriever portrait".into(),
                image_url: "http://clean.example/golden-portrait.jpg".into(),
                thumb_url: Some("https://t.example/1.jpg".into()),
                page_url: None,
                width: None,
                height: None,
                source: "bing-images".into(),
            },
        ];
        let ddg = vec![ImageHit {
            title: "Golden retriever high-res portrait".into(),
            image_url: "https://clean.example/golden-portrait.jpg?utm_medium=ref".into(),
            thumb_url: None,
            page_url: Some("https://clean.example/dogs".into()),
            width: Some(1600),
            height: Some(1067),
            source: "ddg-images".into(),
        }];

        let ranked = merge_and_rank_image_hits("golden retriever portrait", vec![bing, ddg], 5);
        assert_eq!(ranked.len(), 3);
        // Cross-source consensus hit wins #1 and merges HTTPS URL, dimensions,
        // thumb_url, page_url, and richer title.
        assert_eq!(
            ranked[0].image_url,
            "https://clean.example/golden-portrait.jpg?utm_medium=ref"
        );
        assert_eq!(ranked[0].width, Some(1600));
        assert_eq!(ranked[0].height, Some(1067));
        assert_eq!(
            ranked[0].thumb_url.as_deref(),
            Some("https://t.example/1.jpg")
        );
        assert_eq!(
            ranked[0].page_url.as_deref(),
            Some("https://clean.example/dogs")
        );
        // Watermarked stock photo is demoted below clean consensus hit.
        assert_ne!(
            ranked[0].image_url,
            "https://www.shutterstock.com/image-photo/golden.jpg"
        );
    }

    #[test]
    fn contact_sheet_encodes_valid_png_with_rank_badges() {
        let a = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            64,
            48,
            image::Rgba([200, 30, 40, 255]),
        ));
        let b = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            48,
            64,
            image::Rgba([40, 220, 90, 255]),
        ));
        // Non-contiguous ranks (1 and 3) simulate thumbnail #2 failing while
        // preserving exact list numbering on the contact sheet.
        let png = build_contact_sheet_png(&[(1, a), (3, b)]).expect("valid contact sheet png");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
        let decoded = image::load_from_memory(&png)
            .expect("png decodes")
            .to_rgba8();
        assert!(decoded.width() > 400 && decoded.height() > 200);
        // Badge border at tile 0 top-left (pad=10, offset=6 -> x=16, y=16).
        assert_eq!(decoded.get_pixel(16, 16), &image::Rgba([225, 65, 55, 255]));
    }

    #[test]
    fn inspect_raster_image_requires_real_magic_bytes_and_rejects_mislabeled_html() {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            40,
            24,
            image::Rgba([10, 20, 30, 255]),
        ));
        let mut png = Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let (mime, dims) =
            inspect_raster_image(png.get_ref(), "application/octet-stream").expect("png detected");
        assert_eq!(mime, "image/png");
        assert_eq!(dims, Some((40, 24)));

        let (webp_mime, _) =
            inspect_raster_image(b"RIFF\x00\x00\x00\x00WEBPxxxx", "image/webp").expect("webp");
        assert_eq!(webp_mime, "image/webp");

        assert!(inspect_raster_image(b"PK\x03\x04zipbytes", "application/zip").is_none());
        assert!(inspect_raster_image(b"<html>hi</html>", "text/html").is_none());
        // Regression: an HTML error page mislabeled `Content-Type: image/png`
        // must NOT pass as a raster image.
        assert!(
            inspect_raster_image(b"<html><body>404 Not Found</body></html>", "image/png").is_none()
        );
    }
}
