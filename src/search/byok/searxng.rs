//! Built-in SearXNG / SearXNG-compatible (e.g. Degoog) direct HTTP search
//! adapter (#476).
//!
//! Configured via `[search] searxng_url` (`DONSETCH_SEARCH__SEARXNG_URL`), e.g.
//! `http://searxng:8080` or `https://search.example.com`. Calls
//! `GET <base>/search?q=...&format=json` directly without spawning a plugin
//! subprocess, so minimal containers without Python/Node work out of the box.
//!
//! Supports explicit BCP-47 `lang` (`&language=<tag>`), `Intent::News`
//! (`&categories=news` with automatic general fallback when empty), and
//! `Intent::Image` (`&categories=images`), honors `[tls]` trusted roots,
//! `[proxy]` settings, `[rules.url]` deny rules, and `[search] timeout_ms`,
//! and masks embedded URL credentials in diagnostics.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, LOCATION};
use reqwest::{Client as HttpClient, StatusCode};
use serde_json::Value;

use super::{KeyError, ProviderFailure, ProviderOutcome, SearchHit};
use crate::search::Lang;
use crate::search::images::{ImageHit, ImageSourceReport, parse_dims_text};
use crate::search::intent::Intent;

pub(crate) const SOURCE: &str = "searxng";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;
const MAX_TITLE_CHARS: usize = 512;
const MAX_SNIPPET_CHARS: usize = 8 * 1024;
const MAX_URL_CHARS: usize = 4096;

/// Cached HTTP clients keyed by resolved egress route (`"direct"` or proxy id).
static HTTP: LazyLock<Mutex<HashMap<String, HttpClient>>> = LazyLock::new(Default::default);

/// Configured SearXNG base URL (`[search] searxng_url`), if set.
pub fn configured_url() -> Option<String> {
    let raw = crate::config::cfg().search.searxng_url.trim();
    (!raw.is_empty()).then(|| raw.to_string())
}

/// Effective timeout for SearXNG HTTP requests: scales up with `[search] timeout_ms`
/// (with a 10 s floor so slower metasearch fan-outs inside SearXNG finish cleanly)
/// and obeys any active caller `SEARCH_DEADLINE`.
fn request_timeout() -> Duration {
    let base = crate::search::fanout_hard_mark().max(Duration::from_secs(10));
    crate::search::stage_budget(base, Duration::from_millis(200)).max(Duration::from_millis(500))
}

/// Normalize an operator-configured SearXNG base URL into a concrete `/search`
/// request URL carrying `q`, `format=json`, and optional `categories`/`language`.
///
/// Preserves any operator-supplied base path prefix (e.g. `/searxng`) and
/// extra query parameters (e.g. `engines=google,duckduckgo`, `safesearch=0`).
pub(crate) fn build_search_url(
    base_raw: &str,
    query: &str,
    intent: &Intent,
    lang: Option<&Lang>,
    category_override: Option<&str>,
) -> Result<(url::Url, bool), String> {
    let mut url =
        url::Url::parse(base_raw.trim()).map_err(|e| format!("invalid search.searxng_url: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return Err("search.searxng_url must use http:// or https:// with a host".into());
    }
    url.set_fragment(None);

    // Append `/search` unless the operator already pointed at `.../search`.
    let trimmed_path = url.path().trim_end_matches('/').to_string();
    if !trimmed_path.ends_with("/search") {
        let new_path = if trimmed_path.is_empty() {
            "/search".to_string()
        } else {
            format!("{trimmed_path}/search")
        };
        url.set_path(&new_path);
    } else if url.path() != trimmed_path {
        url.set_path(&trimmed_path);
    }

    let existing: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let has_user_categories = existing
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("categories"));

    let mut pairs: Vec<(String, String)> = existing
        .into_iter()
        .filter(|(k, _)| {
            let kl = k.to_ascii_lowercase();
            kl != "q"
                && kl != "format"
                && (category_override.is_none() || kl != "categories")
                && (lang.is_none() || kl != "language")
        })
        .collect();

    pairs.push(("q".into(), query.to_string()));
    pairs.push(("format".into(), "json".into()));

    let mut auto_news_category = false;
    if let Some(cat) = category_override {
        pairs.push(("categories".into(), cat.to_string()));
    } else if matches!(intent, Intent::News) && !has_user_categories {
        pairs.push(("categories".into(), "news".into()));
        auto_news_category = true;
    }

    if let Some(l) = lang {
        pairs.push(("language".into(), l.tag()));
    }

    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    url.set_query(Some(&encoded));
    Ok((url, auto_news_category))
}

fn http_client_for(url: &str) -> Result<HttpClient, String> {
    let proxy = crate::egress::proxy::from_env_for(url).map_err(|e| e.to_string())?;
    let route_key = proxy
        .as_ref()
        .map(|p| p.id())
        .unwrap_or_else(|| "direct".into());
    if let Some(existing) = HTTP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&route_key)
        .cloned()
    {
        return Ok(existing);
    }
    let mut builder = crate::egress::trust::with_trusted_roots(HttpClient::builder())
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(crate::search::search_connect_timeout())
        .pool_max_idle_per_host(4)
        .no_proxy();
    if let Some(proxy) = proxy {
        if proxy.scheme == crate::egress::proxy::ProxyScheme::Socks5 {
            return Err("searxng requires an HTTP proxy or a direct route".into());
        }
        builder = builder.proxy(
            reqwest::Proxy::all(proxy.wire_url())
                .map_err(|_| "unsupported proxy configuration".to_string())?,
        );
    }
    let built = builder
        .build()
        .map_err(|e| format!("http client: {}", e.without_url()))?;
    let mut map = HTTP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() >= 16 {
        map.clear();
    }
    map.insert(route_key, built.clone());
    Ok(built)
}

/// Only follow redirects that stay on the exact same host (e.g. `http -> https`
/// upgrade or `/search` -> `/search/` behind Caddy/Traefik/Nginx), blocking
/// cross-host redirect pivots.
fn same_host_redirect(from: &url::Url, to: &url::Url) -> bool {
    if !matches!(to.scheme(), "http" | "https") {
        return false;
    }
    // Never downgrade HTTPS -> HTTP (would expose Basic Auth credentials).
    if from.scheme() == "https" && to.scheme() != "https" {
        return false;
    }
    match (from.host_str(), to.host_str()) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

async fn fetch_json(initial: &url::Url, timeout: Duration) -> Result<Value, ProviderFailure> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut current = initial.clone();

    for hop in 0..=MAX_REDIRECTS {
        if let Some(denial) = crate::rules::rules().denial(&current) {
            return Err(ProviderFailure::of(KeyError::UnknownError(format!(
                "searxng: blocked by local rule ({})",
                denial.rule
            ))));
        }
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        if rem.is_zero() {
            return Err(ProviderFailure::new(
                KeyError::NetworkError,
                "searxng: request timed out".into(),
            ));
        }

        let client = http_client_for(current.as_str())
            .map_err(|e| ProviderFailure::of(KeyError::UnknownError(format!("searxng: {e}"))))?;

        // Send Basic Auth via Authorization header rather than leaving raw
        // credentials in the request URI, so proxies/logs stay clean.
        let mut req_url = current.clone();
        let basic_user = if !current.username().is_empty() || current.password().is_some() {
            let user = current.username().to_string();
            let pass = current.password().map(str::to_string);
            let _ = req_url.set_username("");
            let _ = req_url.set_password(None);
            Some((user, pass))
        } else {
            None
        };

        let mut req = client
            .get(req_url.as_str())
            .header(ACCEPT, "application/json")
            .timeout(rem);
        if let Some((user, pass)) = basic_user {
            req = req.basic_auth(user, pass);
        }

        let mut resp = req.send().await.map_err(|e| {
            let key = KeyError::from_transport(e);
            let detail = match &key {
                KeyError::NetworkError => "searxng: request timed out".into(),
                _ => format!("searxng: {key}"),
            };
            ProviderFailure::new(key, detail)
        })?;

        let status = resp.status();
        if status.is_redirection() {
            if hop >= MAX_REDIRECTS {
                return Err(ProviderFailure::of(KeyError::UnknownError(
                    "searxng: too many redirects".into(),
                )));
            }
            let Some(loc) = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()) else {
                return Err(ProviderFailure::of(KeyError::ServerError(format!(
                    "HTTP {} without Location header",
                    status.as_u16()
                ))));
            };
            let mut next = current.join(loc.trim()).map_err(|_| {
                ProviderFailure::of(KeyError::UnknownError(
                    "searxng: invalid redirect URL".into(),
                ))
            })?;
            if !same_host_redirect(&current, &next) {
                return Err(ProviderFailure::of(KeyError::UnknownError(
                    "searxng: refused cross-host or scheme-downgrade redirect".into(),
                )));
            }
            if !current.username().is_empty() && next.username().is_empty() {
                let _ = next.set_username(current.username());
                let _ = next.set_password(current.password());
            }
            current = next;
            continue;
        }

        let code = status.as_u16();
        let mut bytes = Vec::new();
        while let Some(chunk) = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .map_err(|_| {
                ProviderFailure::new(KeyError::NetworkError, "searxng: read timed out".into())
            })?
            .map_err(|e| ProviderFailure::of(KeyError::from_transport(e)))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ProviderFailure::of(KeyError::UnknownError(
                    "searxng: response exceeded 2 MiB cap".into(),
                )));
            }
            bytes.extend_from_slice(&chunk);
        }

        if status == StatusCode::UNAUTHORIZED {
            return Err(ProviderFailure::new(
                KeyError::InvalidKey,
                "searxng: HTTP 401 (check Basic Auth credentials in search.searxng_url)".into(),
            ));
        }
        if status == StatusCode::FORBIDDEN {
            return Err(ProviderFailure::new(
                KeyError::ServerError("HTTP 403".into()),
                "searxng: HTTP 403 (enable `json` under search.formats in SearXNG settings.yml)"
                    .into(),
            ));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderFailure::new(
                KeyError::RateLimited,
                "searxng: HTTP 429 rate limited".into(),
            ));
        }
        if code >= 500 {
            return Err(ProviderFailure::new(
                KeyError::ServerError(format!("HTTP {code}")),
                format!("searxng: HTTP {code}"),
            ));
        }
        if code >= 400 {
            let text = String::from_utf8_lossy(&bytes);
            return Err(ProviderFailure::of(KeyError::UnknownError(format!(
                "searxng: HTTP {code}: {}",
                super::err_body(&text)
            ))));
        }

        let text = String::from_utf8_lossy(&bytes);
        let trimmed = text.trim_start();
        if trimmed.is_empty() {
            return Err(ProviderFailure::of(KeyError::UnknownError(format!(
                "searxng: empty body at HTTP {code}"
            ))));
        }
        if trimmed.starts_with('<') {
            return Err(ProviderFailure::of(KeyError::UnknownError(
                "searxng: received HTML instead of JSON (enable `json` in SearXNG settings.yml or verify search.searxng_url)".into(),
            )));
        }
        return serde_json::from_str::<Value>(trimmed).map_err(|e| {
            ProviderFailure::of(KeyError::UnknownError(format!(
                "searxng: invalid JSON at HTTP {code}: {e}"
            )))
        });
    }

    Err(ProviderFailure::of(KeyError::UnknownError(
        "searxng: too many redirects".into(),
    )))
}

fn is_http_url(raw: &str) -> bool {
    url::Url::parse(raw)
        .ok()
        .is_some_and(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
}

fn normalize_score(raw: Option<f64>, rank: usize) -> f32 {
    if let Some(s) = raw
        && s.is_finite()
        && s > 0.0
    {
        return (s / (s + 1.0)).clamp(0.05, 1.0) as f32;
    }
    1.0 / (rank as f32 + 1.0)
}

/// Parse a SearXNG (or Degoog SearXNG-compatible) JSON payload into normalized
/// hits and a degraded flag.
pub(crate) fn parse_web_results(json: &Value) -> (Vec<SearchHit>, bool) {
    let Some(items) = json.get("results").and_then(Value::as_array) else {
        return (Vec::new(), false);
    };
    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    for (idx, item) in items.iter().enumerate() {
        let Some(raw_url) = item.get("url").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if raw_url.is_empty()
            || raw_url.len() > MAX_URL_CHARS
            || raw_url.chars().any(|c| c.is_whitespace() || c.is_control())
            || !is_http_url(raw_url)
        {
            continue;
        }
        if !seen.insert(crate::search::rank::norm_key(raw_url)) {
            continue;
        }
        let raw_title = item
            .get("title")
            .and_then(Value::as_str)
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let title: String = if raw_title.is_empty() {
            crate::search::rank::host_of(raw_url)
        } else {
            raw_title.chars().take(MAX_TITLE_CHARS).collect()
        };
        let snippet: String = item
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                item.get("snippet")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| item.get("description").and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .chars()
            .take(MAX_SNIPPET_CHARS)
            .collect();
        let published = item
            .get("publishedDate")
            .and_then(Value::as_str)
            .or_else(|| item.get("published_at").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 32)
            .map(str::to_string);
        let score = normalize_score(item.get("score").and_then(Value::as_f64), idx);
        hits.push(SearchHit {
            title,
            url: raw_url.to_string(),
            snippet,
            score,
            published,
        });
    }
    let unresponsive = json
        .get("unresponsive_engines")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    let degraded = unresponsive && hits.len() < 3;
    (hits, degraded)
}

/// Parse a SearXNG `categories=images&format=json` payload into image hits.
pub(crate) fn parse_image_results(json: &Value, max: usize) -> Vec<ImageHit> {
    let Some(items) = json.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        let Some(img_url) = item
            .get("img_src")
            .and_then(Value::as_str)
            .or_else(|| item.get("image").and_then(Value::as_str))
            .map(str::trim)
        else {
            continue;
        };
        if !is_http_url(img_url) || !seen.insert(img_url.to_string()) {
            continue;
        }
        let thumb_url = item
            .get("thumbnail_src")
            .and_then(Value::as_str)
            .or_else(|| item.get("thumbnail").and_then(Value::as_str))
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
        let (width, height) = item
            .get("resolution")
            .and_then(Value::as_str)
            .and_then(parse_dims_text)
            .map(|(w, h)| (Some(w), Some(h)))
            .unwrap_or((None, None));
        hits.push(ImageHit {
            title,
            image_url: img_url.to_string(),
            thumb_url,
            page_url,
            width,
            height,
            source: SOURCE.to_string(),
        });
        if hits.len() >= max {
            break;
        }
    }
    hits
}

/// Query a configured SearXNG / Degoog instance for web/code/paper/news/entity
/// search results.
pub(crate) async fn search(
    base_url: &str,
    query: &str,
    _max: usize,
    intent: &Intent,
    lang: Option<&Lang>,
) -> Result<ProviderOutcome, ProviderFailure> {
    let started = Instant::now();
    let timeout = request_timeout();
    let (url, auto_news) = build_search_url(base_url, query, intent, lang, None)
        .map_err(|e| ProviderFailure::of(KeyError::UnknownError(format!("searxng: {e}"))))?;

    let json = fetch_json(&url, timeout).await?;
    let (mut hits, mut degraded) = parse_web_results(&json);

    // Some minimal SearXNG configs or Degoog setups disable the `news` category;
    // retry once without `categories=news` if the auto-selected news query
    // returned empty.
    if hits.is_empty() && auto_news {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining >= Duration::from_millis(500)
            && let Ok((fallback_url, _)) =
                build_search_url(base_url, query, &Intent::Web, lang, None)
            && let Ok(fallback_json) = fetch_json(&fallback_url, remaining).await
        {
            let (fb_hits, fb_degraded) = parse_web_results(&fallback_json);
            if !fb_hits.is_empty() {
                hits = fb_hits;
                degraded = fb_degraded;
            }
        }
    }

    Ok(ProviderOutcome {
        hits,
        ms: started.elapsed().as_millis() as u64,
        degraded,
    })
}

/// Query a configured SearXNG instance for image search (`categories=images`).
pub(crate) async fn search_images(
    base_url: &str,
    query: &str,
    max: usize,
    lang: Option<&Lang>,
    timeout: Duration,
) -> (Vec<ImageHit>, ImageSourceReport) {
    let Ok((url, _)) = build_search_url(base_url, query, &Intent::Image, lang, Some("images"))
    else {
        return (
            Vec::new(),
            ImageSourceReport {
                id: SOURCE.into(),
                items: 0,
                status: "invalid-url".into(),
                state: "failed",
            },
        );
    };
    match fetch_json(&url, timeout).await {
        Ok(json) => {
            let hits = parse_image_results(&json, max * 2);
            let n = hits.len();
            let (status, state) = if n == 0 {
                ("empty".into(), "empty")
            } else {
                ("ok".into(), "answered")
            };
            (
                hits,
                ImageSourceReport {
                    id: SOURCE.into(),
                    items: n,
                    status,
                    state,
                },
            )
        }
        Err(failure) => {
            let (status, state) = match failure.key {
                KeyError::NetworkError => ("timeout".into(), "late"),
                KeyError::RateLimited => ("rate-limited".into(), "rested"),
                _ => (super::compact_failure(&failure.detail), "failed"),
            };
            (
                Vec::new(),
                ImageSourceReport {
                    id: SOURCE.into(),
                    items: 0,
                    status,
                    state,
                },
            )
        }
    }
}

/// Live health probe for `donsetch doctor --deep`.
pub(crate) async fn probe(base_url: &str) -> Result<usize, String> {
    let (url, _) = build_search_url(base_url, "rust", &Intent::Web, None, None)?;
    let json = fetch_json(&url, Duration::from_secs(6))
        .await
        .map_err(|f| f.detail)?;
    let (hits, _) = parse_web_results(&json);
    Ok(hits.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    #[test]
    fn build_search_url_normalizes_paths_preserves_params_and_explicit_lang() {
        let (u, auto_news) = build_search_url(
            "http://searxng:8080",
            "rust async",
            &Intent::Web,
            None,
            None,
        )
        .unwrap();
        assert_eq!(u.path(), "/search");
        assert!(!auto_news);
        let qs: HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(qs.get("q").map(String::as_str), Some("rust async"));
        assert_eq!(qs.get("format").map(String::as_str), Some("json"));
        assert!(!qs.contains_key("categories"));
        assert!(!qs.contains_key("language"));

        // Subpath + user params + explicit lang + news intent.
        let lang = Lang::parse("de-DE").unwrap();
        let (u2, auto_news2) = build_search_url(
            "https://home.example/searxng/search/?engines=duckduckgo,google&safesearch=0",
            "bundestag",
            &Intent::News,
            Some(&lang),
            None,
        )
        .unwrap();
        assert_eq!(u2.path(), "/searxng/search");
        assert!(auto_news2);
        let qs2: HashMap<_, _> = u2.query_pairs().into_owned().collect();
        assert_eq!(
            qs2.get("engines").map(String::as_str),
            Some("duckduckgo,google")
        );
        assert_eq!(qs2.get("safesearch").map(String::as_str), Some("0"));
        assert_eq!(qs2.get("categories").map(String::as_str), Some("news"));
        assert_eq!(qs2.get("language").map(String::as_str), Some("de-DE"));

        // Operator-supplied categories are never overwritten on News.
        let (u3, auto_news3) = build_search_url(
            "http://degoog:4444/search?categories=general",
            "breaking",
            &Intent::News,
            None,
            None,
        )
        .unwrap();
        assert!(!auto_news3);
        let qs3: HashMap<_, _> = u3.query_pairs().into_owned().collect();
        assert_eq!(qs3.get("categories").map(String::as_str), Some("general"));
    }

    #[test]
    fn parse_web_results_handles_searxng_and_degoog_shapes() {
        let payload = json!({
            "query": "rust tokio",
            "results": [
                {
                    "url": "https://tokio.rs/",
                    "title": "  Tokio - Rust async runtime  ",
                    "content": "An asynchronous Rust runtime.",
                    "publishedDate": "2026-11-20T00:00:00Z",
                    "score": 4.0
                },
                {
                    "url": "https://tokio.rs/",
                    "title": "Duplicate",
                    "content": "dup"
                },
                {
                    "url": "javascript:alert(1)",
                    "title": "Bad"
                },
                {
                    "url": "https://docs.rs/tokio",
                    "title": "tokio - Rust",
                    "snippet": "Degoog fallback snippet field",
                    "score": 1.0
                }
            ],
            "unresponsive_engines": [["bing", "timeout"]]
        });
        let (hits, degraded) = parse_web_results(&payload);
        assert_eq!(hits.len(), 2);
        assert!(degraded, "unresponsive_engines + <3 hits marks degraded");
        assert_eq!(hits[0].title, "Tokio - Rust async runtime");
        assert_eq!(hits[0].snippet, "An asynchronous Rust runtime.");
        assert_eq!(hits[0].published.as_deref(), Some("2026-11-20T00:00:00Z"));
        assert!(hits[0].score > hits[1].score);
        assert_eq!(hits[1].snippet, "Degoog fallback snippet field");
    }

    #[test]
    fn parse_image_results_extracts_img_thumb_page_and_resolution() {
        let payload = json!({
            "results": [
                {
                    "title": "Red Panda",
                    "img_src": "https://img.example/panda.jpg",
                    "thumbnail_src": "https://thumb.example/panda.jpg",
                    "url": "https://example.org/panda",
                    "resolution": "1920x1080"
                },
                {
                    "title": "Bad scheme",
                    "img_src": "file:///etc/passwd"
                }
            ]
        });
        let hits = parse_image_results(&payload, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].image_url, "https://img.example/panda.jpg");
        assert_eq!(
            hits[0].thumb_url.as_deref(),
            Some("https://thumb.example/panda.jpg")
        );
        assert_eq!(
            hits[0].page_url.as_deref(),
            Some("https://example.org/panda")
        );
        assert_eq!(hits[0].width, Some(1920));
        assert_eq!(hits[0].height, Some(1080));
        assert_eq!(hits[0].source, "searxng");
    }

    #[tokio::test]
    async fn live_loopback_searxng_search_with_basic_auth_and_explicit_lang() {
        let mut config = crate::config::DonsetchConfig::default();
        config.state.no_disk_state = true;
        config.proxy.from_environment = false;
        let _ = crate::config::install(config);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("GET /search?"), "{line}");
            assert!(line.contains("format=json"), "{line}");
            assert!(line.contains("language=ja"), "{line}");
            let mut auth = None;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).await.unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.eq_ignore_ascii_case("authorization")
                {
                    auth = Some(v.trim().to_string());
                }
            }
            assert!(
                auth.as_deref().is_some_and(|v| v.starts_with("Basic ")),
                "expected Basic auth header, got {auth:?}"
            );
            let body = json!({
                "results": [
                    {
                        "url": "https://ja.wikipedia.org/wiki/Rust",
                        "title": "Rust",
                        "content": "プログラミング言語Rust",
                        "score": 2.0
                    }
                ]
            })
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            reader.get_mut().write_all(reply.as_bytes()).await.unwrap();
        });

        let base = format!("http://alice:secret@{addr}");
        let lang = Lang::parse("ja").unwrap();
        let out = search(&base, "Rust 言語", 5, &Intent::Web, Some(&lang))
            .await
            .expect("searxng search succeeds");
        server.await.unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].url, "https://ja.wikipedia.org/wiki/Rust");
    }

    #[tokio::test]
    async fn searxng_403_gives_actionable_settings_yml_hint() {
        let mut config = crate::config::DonsetchConfig::default();
        config.state.no_disk_state = true;
        config.proxy.from_environment = false;
        let _ = crate::config::install(config);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).await.unwrap();
                if h.trim().is_empty() {
                    break;
                }
            }
            let reply =
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\nConnection: close\r\n\r\nForbidden";
            let _ = reader.get_mut().write_all(reply.as_bytes()).await;
            let _ = reader.get_mut().shutdown().await;
        });

        let err = search(&format!("http://{addr}"), "test", 5, &Intent::Web, None)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(err.detail.contains("settings.yml"), "{}", err.detail);
    }

    #[tokio::test]
    async fn searxng_news_falls_back_to_default_category_when_news_is_empty() {
        let mut config = crate::config::DonsetchConfig::default();
        config.state.no_disk_state = true;
        config.proxy.from_environment = false;
        let _ = crate::config::install(config);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let body = if attempt == 0 {
                    assert!(line.contains("categories=news"), "{line}");
                    json!({ "results": [] }).to_string()
                } else {
                    assert!(!line.contains("categories=news"), "{line}");
                    json!({
                        "results": [
                            { "url": "https://news.example/a", "title": "News A", "content": "Details" }
                        ]
                    })
                    .to_string()
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                reader.get_mut().write_all(reply.as_bytes()).await.unwrap();
            }
        });

        let out = search(
            &format!("http://{addr}"),
            "latest release",
            5,
            &Intent::News,
            None,
        )
        .await
        .expect("news fallback succeeds");
        server.await.unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].url, "https://news.example/a");
    }
}
