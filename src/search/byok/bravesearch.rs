//! Brave Search API (official, keyed) adapter.
//!
//! Distinct from the keyless `brave` SERP scraper in
//! `src/search/engines.rs` (which scrapes search.brave.com's HTML,
//! no key required) : this hits Brave's own REST API and needs a
//! subscription token. Named `bravesearch` here to keep the two
//! unambiguous in the provider list.
//!
//! GET https://api.search.brave.com/res/v1/web/search
//! Auth: X-Subscription-Token: <key>  (+ Accept: application/json)
//! Params: q, count (max 20)
//! Response: { web: { results: [{ title, url, description }] } }
//!
//! News intent uses a separate endpoint:
//! GET https://api.search.brave.com/res/v1/news/search
//! Response: { results: [{ title, url, description }] }
//!
//! Brave doesn't return a per-result position/rank field, so the
//! score is derived from array order.

use super::ProviderOutcome;
use std::time::Instant;

use serde_json::Value;

use super::{KeyError, ProviderResult, SearchHit};

const WEB_BASE: &str = "https://api.search.brave.com/res/v1/web/search";
const NEWS_BASE: &str = "https://api.search.brave.com/res/v1/news/search";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Extract hits from an array of `{ title, url, description }`
/// objects (the shape both the `web.results` and top-level
/// `results` arrays share). Pure function so it's testable without
/// a live API call.
fn parse_results(arr: &[Value]) -> Vec<SearchHit> {
    arr.iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let title = r
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let url = r
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if url.is_empty() {
                return None;
            }
            let snippet = r
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // No position field from Brave : derive from array
            // order (0 → ~1.0, 9 → ~0.1).
            let score = 1.0 / (i as f32 + 1.0);
            Some(SearchHit {
                title,
                url,
                snippet,
                score,
            })
        })
        .collect()
}

pub async fn search(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    max: usize,
    intent: &crate::search::intent::Intent,
) -> ProviderResult {
    let started = Instant::now();

    let is_news = matches!(intent, crate::search::intent::Intent::News);
    let endpoint = if is_news { NEWS_BASE } else { WEB_BASE };
    let count = max.clamp(1, 20).to_string();

    let resp = client
        .get(endpoint)
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .query(&[("q", query), ("count", &count)])
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(KeyError::from_transport)?;

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();

    if status == 401 || status == 403 {
        return Err(KeyError::InvalidKey);
    }
    // No 402 branch: unlike Tavily/SerpApi/Serper/Brightdata's
    // underlying services, Brave's API doesn't document a 402.
    // Its rate-limit policy expresses both per-second throttling
    // and monthly-quota exhaustion under the same 429 status (see
    // the X-RateLimit-Policy header's 1s/30-day windows), so quota
    // exhaustion is inferred here via message-sniffing only.
    if status == 429 {
        let lower = text.to_lowercase();
        if lower.contains("quota") || lower.contains("exceeded") || lower.contains("plan") {
            return Err(KeyError::CreditDepleted);
        }
        return Err(KeyError::RateLimited);
    }
    if status >= 500 {
        return Err(KeyError::ServerError(format!("HTTP {status}")));
    }
    if status >= 400 {
        let lower = text.to_lowercase();
        if lower.contains("invalid") && (lower.contains("key") || lower.contains("token")) {
            return Err(KeyError::InvalidKey);
        }
        if lower.contains("quota") || lower.contains("exceeded") || lower.contains("plan") {
            return Err(KeyError::CreditDepleted);
        }
        return Err(KeyError::UnknownError(format!("HTTP {status}: {text}")));
    }

    let json: Value = serde_json::from_str(&text)
        .map_err(|e| KeyError::UnknownError(format!("parse error: {e}")))?;

    let results = if is_news {
        json.get("results")
            .and_then(Value::as_array)
            .map(|arr| parse_results(arr))
            .unwrap_or_default()
    } else {
        json.get("web")
            .and_then(|w| w.get("results"))
            .and_then(Value::as_array)
            .map(|arr| parse_results(arr))
            .unwrap_or_default()
    };

    let ms = started.elapsed().as_millis() as u64;
    Ok(ProviderOutcome {
        hits: results,
        ms,
        degraded: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_results_basic() {
        let arr = json!([
            { "title": "Rust", "url": "https://rust-lang.org", "description": "a systems language" },
            { "title": "Rust book", "url": "https://doc.rust-lang.org/book", "description": "the book" },
        ]);
        let hits = parse_results(arr.as_array().unwrap());
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust");
        assert_eq!(hits[0].url, "https://rust-lang.org");
        assert_eq!(hits[0].snippet, "a systems language");
        assert!(
            hits[0].score > hits[1].score,
            "earlier array position scores higher"
        );
    }

    #[test]
    fn parse_results_drops_entries_without_url() {
        let arr = json!([
            { "title": "No URL" },
            { "title": "Has URL", "url": "https://example.com" },
        ]);
        let hits = parse_results(arr.as_array().unwrap());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Has URL");
    }

    #[test]
    fn parse_results_empty_array() {
        let arr = json!([]);
        assert!(parse_results(arr.as_array().unwrap()).is_empty());
    }

    #[test]
    fn parse_results_missing_description_defaults_empty() {
        let arr = json!([
            { "title": "No description", "url": "https://example.com" },
        ]);
        let hits = parse_results(arr.as_array().unwrap());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "");
    }
}
