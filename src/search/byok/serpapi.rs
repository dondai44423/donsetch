//! SerpApi (serpapi.com) search provider adapter.
//!
//! GET https://serpapi.com/search
//! Auth: api_key=<key> (query parameter, not a header)
//! Params: q, engine, num, api_key, [tbm=nws for news]
//! Response: { organic_results: [{ position, title, link, snippet }] }
//!           news (tbm=nws) uses `news_results` instead.
//!           paper intent switches engine to google_scholar, still
//!           returns `organic_results`.

use std::time::Instant;

use serde_json::Value;

use super::{KeyError, ProviderResult, SearchHit};

const BASE: &str = "https://serpapi.com/search";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Extract hits from the results array named by `key`
/// (`organic_results` or `news_results`). Pure function so it's
/// testable without a live API call.
fn parse_results(json: &Value, key: &str) -> Vec<SearchHit> {
    json.get(key)
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
                    // SerpApi doesn't return a relevance score;
                    // derive one from position (1 → ~1.0, 10 → ~0.1).
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

    // Route by intent: paper → google_scholar engine, news → the
    // google engine's news vertical (tbm=nws), else plain google.
    let mut params = vec![
        ("q".to_string(), query.to_string()),
        ("num".to_string(), max.min(10).to_string()),
        ("api_key".to_string(), key.to_string()),
    ];
    let results_key = match intent {
        crate::search::intent::Intent::Paper => {
            params.push(("engine".to_string(), "google_scholar".to_string()));
            "organic_results"
        }
        crate::search::intent::Intent::News => {
            params.push(("engine".to_string(), "google".to_string()));
            params.push(("tbm".to_string(), "nws".to_string()));
            "news_results"
        }
        _ => {
            params.push(("engine".to_string(), "google".to_string()));
            "organic_results"
        }
    };

    let resp = client
        .get(BASE)
        .query(&params)
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
    if status == 429 {
        let lower = text.to_lowercase();
        if lower.contains("run out") || lower.contains("plan") || lower.contains("quota") {
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
        if lower.contains("run out") || lower.contains("quota") || lower.contains("plan") {
            return Err(KeyError::CreditDepleted);
        }
        return Err(KeyError::UnknownError(format!("HTTP {status}: {text}")));
    }

    let json: Value = serde_json::from_str(&text)
        .map_err(|e| KeyError::UnknownError(format!("parse error: {e}")))?;

    // SerpApi returns HTTP 200 with an `error` field for some
    // failure modes (e.g. unsupported engine/param combos) instead
    // of a non-2xx status.
    if let Some(err) = json.get("error").and_then(Value::as_str) {
        let lower = err.to_lowercase();
        if lower.contains("invalid") && lower.contains("key") {
            return Err(KeyError::InvalidKey);
        }
        return Err(KeyError::UnknownError(err.to_string()));
    }

    let results = parse_results(&json, results_key);

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
            "organic_results": [
                { "position": 1, "title": "Rust", "link": "https://rust-lang.org", "snippet": "a systems language" },
                { "position": 2, "title": "Rust book", "link": "https://doc.rust-lang.org/book", "snippet": "the book" },
            ]
        });
        let hits = parse_results(&body, "organic_results");
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
    fn parse_results_news_key() {
        let body = json!({
            "news_results": [
                { "position": 1, "title": "Breaking", "link": "https://news.example.com/a", "snippet": "s" },
            ]
        });
        let hits = parse_results(&body, "news_results");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Breaking");
    }

    #[test]
    fn parse_results_drops_entries_without_link() {
        let body = json!({
            "organic_results": [
                { "position": 1, "title": "No URL" },
                { "position": 2, "title": "Has URL", "link": "https://example.com" },
            ]
        });
        let hits = parse_results(&body, "organic_results");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Has URL");
    }

    #[test]
    fn parse_results_missing_key_returns_empty() {
        let body = json!({ "search_metadata": {} });
        assert!(parse_results(&body, "organic_results").is_empty());
    }

    #[test]
    fn parse_results_defaults_missing_position_to_one() {
        let body = json!({
            "organic_results": [
                { "title": "No position field", "link": "https://example.com" },
            ]
        });
        let hits = parse_results(&body, "organic_results");
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 1.0).abs() < 0.001);
    }
}
