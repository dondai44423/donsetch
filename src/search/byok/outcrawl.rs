//! Outcrawl (outcrawl.ai) search provider adapter.
//!
//! POST https://api.outcrawl.ai/v1/search
//! Auth: Authorization: Bearer <key>
//! Body: { query, limit } (limit 1..=20)
//! Response: { hits: [{ url, title, snippet, rank }], usage: { credits } }
//!
//! Errors are plain HTTP statuses: 401 invalid key, 402 out of
//! credits, 429 rate limited; other non-2xx statuses return a JSON
//! body with `code` and `message`.

use super::ProviderOutcome;
use std::time::Instant;

use serde_json::{Value, json};

use super::{KeyError, ProviderResult, SearchHit};

const ENDPOINT: &str = "https://api.outcrawl.ai/v1/search";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// The API rejects a `limit` above 20.
const MAX_LIMIT: usize = 20;

/// Extract hits from the `hits` array. Pure function so it's
/// testable without a live API call. Mirrors the serpbase/serper
/// adapters: entries without a URL are dropped, and a relevance
/// score is derived from rank (1 → 1.0, 10 → 0.1).
fn parse_results(json: &Value) -> Vec<SearchHit> {
    json.get("hits")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
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
                        .get("snippet")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let rank = r.get("rank").and_then(Value::as_u64).unwrap_or(1) as f32;
                    let score = 1.0 / rank.max(1.0);
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
    _intent: &crate::search::intent::Intent,
) -> ProviderResult {
    let started = Instant::now();

    let body = json!({
        "query": query,
        "limit": max.clamp(1, MAX_LIMIT),
    });

    let resp = client
        .post(ENDPOINT)
        .bearer_auth(key)
        .json(&body)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(KeyError::from_transport)?;

    let status = resp.status().as_u16();
    // A body-read failure is a transport error, not an empty body
    // (#286): keep what reqwest saw instead of mapping it to "".
    let text = resp.text().await.map_err(KeyError::from_transport)?;

    if status == 401 || status == 403 {
        return Err(KeyError::InvalidKey);
    }
    if status == 402 {
        return Err(KeyError::CreditDepleted);
    }
    if status == 429 {
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

    let json: Value = super::parse_provider_json(status, &text)?;

    let mut results = parse_results(&json);
    // `limit` is clamped to the API cap, so a caller asking for less
    // than that already gets at most `max`; this keeps the contract
    // explicit for any max above it as well.
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
    fn parse_results_hits() {
        let body = json!({
            "hits": [
                { "rank": 1, "title": "Rust", "url": "https://rust-lang.org", "snippet": "a systems language" },
                { "rank": 2, "title": "Rust book", "url": "https://doc.rust-lang.org/book", "snippet": "the book" },
            ],
            "usage": { "credits": "0.5" }
        });
        let hits = parse_results(&body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust");
        assert_eq!(hits[0].url, "https://rust-lang.org");
        assert_eq!(hits[0].snippet, "a systems language");
        assert!(hits[0].score > hits[1].score, "earlier rank scores higher");
    }

    #[test]
    fn parse_results_drops_entries_without_url() {
        let body = json!({
            "hits": [
                { "rank": 1, "title": "No URL" },
                { "rank": 2, "title": "Empty URL", "url": "" },
                { "rank": 3, "title": "Has URL", "url": "https://example.com" },
            ]
        });
        let hits = parse_results(&body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Has URL");
    }

    #[test]
    fn parse_results_missing_key_returns_empty() {
        let body = json!({ "usage": { "credits": "0.5" } });
        assert!(parse_results(&body).is_empty());
    }

    #[test]
    fn parse_results_defaults_missing_rank_to_one() {
        let body = json!({
            "hits": [
                { "title": "No rank field", "url": "https://example.com" },
            ]
        });
        let hits = parse_results(&body);
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 1.0).abs() < 0.001);
    }
}
