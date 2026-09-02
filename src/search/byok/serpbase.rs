//! SerpBase (serpbase.dev) Google SERP provider adapter.
//!
//! POST https://api.serpbase.dev/google/search
//! Auth: X-API-Key <key> (header)
//! Body: { q, num, hl, gl }
//! Response: { status, organic: [{ position, title, link, snippet }], ... }
//!
//! `status` is a business-level code: 0 = success, non-zero = error
//! (e.g. 1001 unauthorized). Errors may come back as HTTP 200 with a
//! non-zero `status` envelope, so the status field must be checked
//! even on 2xx responses.
//!
//! Correctness note: this adapter follows the *current* SerpBase API
//! contract (POST + JSON body + `X-API-Key` header, results under
//! `organic`). Earlier public examples used `GET /google/search?api_key=`
//! and an `organic_results` field; that contract is outdated and will
//! return HTTP 200 with a non-zero `status` error envelope.

use std::time::Instant;

use serde_json::{Value, json};

use super::{KeyError, ProviderResult, SearchHit};

const BASE: &str = "https://api.serpbase.dev/google/search";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Extract hits from the `organic` array. Pure function so it's
/// testable without a live API call. Mirrors the serpapi/serper
/// adapters: entries without a URL are dropped, and a relevance
/// score is derived from position (1 → ~1.0, 10 → ~0.1).
fn parse_results(json: &Value) -> Vec<SearchHit> {
    json.get("organic")
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
                        .get("link")
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
                    let position = r.get("position").and_then(Value::as_u64).unwrap_or(1) as f32;
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
    _intent: &crate::search::intent::Intent,
) -> ProviderResult {
    let started = Instant::now();

    let body = json!({
        "q": query,
        "num": max.min(10),
    });

    let resp = client
        .post(BASE)
        .header("X-API-Key", key)
        .json(&body)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                KeyError::NetworkError
            } else {
                KeyError::UnknownError(format!("network: {e}"))
            }
        })?;

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();

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
        return Err(KeyError::UnknownError(format!("HTTP {status}: {text}")));
    }

    let json: Value = serde_json::from_str(&text)
        .map_err(|e| KeyError::UnknownError(format!("parse error: {e}")))?;

    // Business-level error envelope: HTTP 200 with status != 0.
    // 1001 = unauthorized (bad/revoked key), others are query-level
    // errors. Map 1001 to InvalidKey so the key is marked dead and
    // rotation moves to the next key; everything else is unknown.
    if let Some(code) = json.get("status").and_then(Value::as_u64) {
        if code == 1001 {
            return Err(KeyError::InvalidKey);
        }
        if code != 0 {
            let msg = json
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            return Err(KeyError::UnknownError(format!(
                "serpbase status {code}: {msg}"
            )));
        }
    }

    let results = parse_results(&json);

    let ms = started.elapsed().as_millis() as u64;
    Ok((results, ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_results_organic() {
        let body = json!({
            "status": 0,
            "organic": [
                { "position": 1, "title": "Rust", "link": "https://rust-lang.org", "snippet": "a systems language" },
                { "position": 2, "title": "Rust book", "link": "https://doc.rust-lang.org/book", "snippet": "the book" },
            ]
        });
        let hits = parse_results(&body);
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
    fn parse_results_drops_entries_without_link() {
        let body = json!({
            "organic": [
                { "position": 1, "title": "No URL" },
                { "position": 2, "title": "Has URL", "link": "https://example.com" },
            ]
        });
        let hits = parse_results(&body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Has URL");
    }

    #[test]
    fn parse_results_missing_key_returns_empty() {
        let body = json!({ "status": 0 });
        assert!(parse_results(&body).is_empty());
    }

    #[test]
    fn parse_results_defaults_missing_position_to_one() {
        let body = json!({
            "organic": [
                { "title": "No position field", "link": "https://example.com" },
            ]
        });
        let hits = parse_results(&body);
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 1.0).abs() < 0.001);
    }
}
