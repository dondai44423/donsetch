//! Serper.dev search provider adapter.
//!
//! POST https://google.serper.dev/search
//! Auth: X-API-KEY <key>
//! Body: { q, num, gl, hl }
//! Response: { organic: [{ title, link, snippet, position }], knowledgeGraph }
//!
//! For news/paper intent, uses /news or /scholar endpoints.

use super::ProviderOutcome;
use std::time::Instant;

use serde_json::{Value, json};

use super::{KeyError, ProviderResult, SearchHit};

const BASE: &str = "https://google.serper.dev";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn search(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    max: usize,
    intent: &crate::search::intent::Intent,
) -> ProviderResult {
    let started = Instant::now();

    // Route by intent: news → /news, paper → /scholar, else /search.
    let path = match intent {
        crate::search::intent::Intent::News => "/news",
        crate::search::intent::Intent::Paper => "/scholar",
        _ => "/search",
    };
    let endpoint = format!("{BASE}{path}");

    let body = json!({
        "q": query,
        "num": max.min(10),
    });

    let resp = client
        .post(&endpoint)
        .header("X-API-KEY", key)
        .json(&body)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(KeyError::from_transport)?;

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
        if lower.contains("rate") || lower.contains("excessive") {
            return Err(KeyError::RateLimited);
        }
        if lower.contains("credit") || lower.contains("quota") || lower.contains("billing") {
            return Err(KeyError::CreditDepleted);
        }
        if lower.contains("invalid") && lower.contains("key") {
            return Err(KeyError::InvalidKey);
        }
        return Err(KeyError::UnknownError(format!("HTTP {status}: {text}")));
    }

    let json: Value = serde_json::from_str(&text)
        .map_err(|e| KeyError::UnknownError(format!("parse error: {e}")))?;

    // Serper returns `organic` for /search and /scholar,
    // and `organic` or `news` for /news.
    let arr_key = if path == "/news" { "news" } else { "organic" };
    let results = json
        .get(arr_key)
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
                    // Serper doesn't return a relevance score;
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
        .unwrap_or_default();

    let ms = started.elapsed().as_millis() as u64;
    Ok(ProviderOutcome {
        hits: results,
        ms,
        degraded: false,
    })
}
