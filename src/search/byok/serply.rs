//! Serply (serply.io) Google SERP provider adapter.
//!
//! GET https://api.serply.io/v1/search
//! GET https://api.serply.io/v1/scholar   (paper intent)
//! Auth: X-Api-Key <key> (header, so the key never lands in a
//! logged request line)
//! Params: q, num, [tbm=nws for news]
//! Response: { results: [{ position, title, description, link }] }
//!           scholar returns its hits under `articles` instead.
//!
//! Two shape notes that differ from the other Google SERP adapters:
//!
//! 1. The snippet field is `description`, not `snippet`.
//! 2. Scholar entries carry no `position`, and the scholar envelope
//!    also ships an empty `results: []` alongside `articles`. So the
//!    results key is passed in explicitly (a parser left pointed at
//!    `results` would silently return zero hits), and the relevance
//!    score falls back to array order when `position` is absent.

use super::ProviderOutcome;
use std::time::Instant;

use serde_json::Value;

use super::{KeyError, ProviderResult, SearchHit};

const BASE: &str = "https://api.serply.io/v1/search";
const SCHOLAR: &str = "https://api.serply.io/v1/scholar";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Extract hits from the results array named by `key` (`results` or
/// `articles`). Pure function so it's testable without a live API
/// call. Mirrors the serpapi/serpbase adapters: entries without a URL
/// are dropped and a relevance score is derived from position
/// (1 gives ~1.0, 10 gives ~0.1). Scholar entries have no `position`,
/// so array order stands in for it.
fn parse_results(json: &Value, key: &str) -> Vec<SearchHit> {
    json.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    let title = r
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let url = r
                        .get("link")
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
                    let position = r
                        .get("position")
                        .and_then(Value::as_u64)
                        .unwrap_or(i as u64 + 1) as f32;
                    let score = 1.0 / position.max(1.0);
                    Some(SearchHit {
                        title,
                        url,
                        snippet,
                        score,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub async fn search(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    max: usize,
    intent: &crate::search::intent::Intent,
) -> ProviderResult {
    let started = Instant::now();

    // Route by intent: paper goes to the scholar endpoint, news to
    // the Google news vertical (tbm=nws), else plain web search.
    // tbm=nws returns the same `results` shape with publisher links,
    // so news needs no second parser. (/v1/news exists but is an RSS
    // feed: it ignores `num` and its links are news.google.com
    // redirects rather than the article.)
    let mut params = vec![
        ("q".to_string(), query.to_string()),
        ("num".to_string(), max.min(10).to_string()),
    ];
    let (url, results_key) = match intent {
        crate::search::intent::Intent::Paper => (SCHOLAR, "articles"),
        crate::search::intent::Intent::News => {
            params.push(("tbm".to_string(), "nws".to_string()));
            (BASE, "results")
        }
        _ => (BASE, "results"),
    };

    let resp = client
        .get(url)
        .query(&params)
        .header("X-Api-Key", key)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(KeyError::from_transport)?;

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();

    // Serply answers both a bad key and a missing key with 401 and a
    // `{"detail": "..."}` body.
    if status == 401 || status == 403 {
        return Err(KeyError::InvalidKey);
    }
    if status == 402 {
        return Err(KeyError::CreditDepleted);
    }
    if status == 429 {
        let lower = text.to_lowercase();
        if lower.contains("credit") || lower.contains("quota") || lower.contains("plan") {
            return Err(KeyError::CreditDepleted);
        }
        return Err(KeyError::RateLimited);
    }
    if status >= 500 {
        return Err(KeyError::ServerError(format!("HTTP {status}")));
    }
    if status >= 400 {
        let lower = text.to_lowercase();
        if lower.contains("invalid") && lower.contains("key") {
            return Err(KeyError::InvalidKey);
        }
        if lower.contains("credit") || lower.contains("quota") || lower.contains("billing") {
            return Err(KeyError::CreditDepleted);
        }
        return Err(KeyError::UnknownError(format!(
            "HTTP {status}: {}",
            super::err_body(&text)
        )));
    }

    let json: Value = serde_json::from_str(&text)
        .map_err(|e| KeyError::UnknownError(format!("parse error: {e}")))?;

    // Some error envelopes arrive as HTTP 200 with a `detail` string
    // instead of a non-2xx status.
    if let Some(detail) = json.get("detail").and_then(Value::as_str) {
        let lower = detail.to_lowercase();
        if lower.contains("key") {
            return Err(KeyError::InvalidKey);
        }
        return Err(KeyError::UnknownError(detail.to_string()));
    }

    let mut results = parse_results(&json, results_key);
    // `num` is honored on both endpoints, but cap locally too so a
    // vertical that ignores it cannot flood past the caller's budget.
    results.truncate(max);

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
    fn parse_results_web() {
        let body = json!({
            "results": [
                { "position": 1, "title": "Rust", "link": "https://rust-lang.org", "description": "a systems language" },
                { "position": 2, "title": "Rust book", "link": "https://doc.rust-lang.org/book", "description": "the book" },
            ]
        });
        let hits = parse_results(&body, "results");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust");
        assert_eq!(hits[0].url, "https://rust-lang.org");
        assert_eq!(hits[0].snippet, "a systems language");
        assert!(
            hits[0].score > hits[1].score,
            "earlier position scores higher"
        );
    }

    #[test]
    fn parse_results_scholar_articles_key() {
        // The scholar envelope carries an empty `results` alongside
        // `articles`: keyed on `results` this would look like a
        // successful empty search instead of a parse miss.
        let body = json!({
            "results": [],
            "articles": [
                { "title": "Attention", "link": "https://doi.org/10.1", "description": "Vaswani et al." },
                { "title": "GNN", "link": "https://doi.org/10.2", "description": "Scarselli et al." },
            ]
        });
        assert!(parse_results(&body, "results").is_empty());
        let hits = parse_results(&body, "articles");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Attention");
        assert!(
            hits[0].score > hits[1].score,
            "array order stands in for the missing position field"
        );
    }

    #[test]
    fn parse_results_drops_entries_without_link() {
        let body = json!({
            "results": [
                { "position": 1, "title": "No URL" },
                { "position": 2, "title": "Has URL", "link": "https://example.com" },
            ]
        });
        let hits = parse_results(&body, "results");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Has URL");
    }

    #[test]
    fn parse_results_missing_key_returns_empty() {
        let body = json!({ "query": "rust" });
        assert!(parse_results(&body, "results").is_empty());
    }

    #[test]
    fn parse_results_defaults_missing_position_to_array_order() {
        let body = json!({
            "results": [
                { "title": "First", "link": "https://a.example" },
                { "title": "Second", "link": "https://b.example" },
            ]
        });
        let hits = parse_results(&body, "results");
        assert_eq!(hits.len(), 2);
        assert!((hits[0].score - 1.0).abs() < 0.001);
        assert!((hits[1].score - 0.5).abs() < 0.001);
    }
}
