//! Exa search provider adapter.
//!
//! POST https://api.exa.ai/search
//! Auth: x-api-key <key>
//! Body: { query, numResults, type, category }
//! Response: { results: [{ title, url, score, text? }] }

use super::ProviderOutcome;
use std::time::Instant;

use serde_json::{Value, json};

use super::{KeyError, ProviderResult, SearchHit};

const ENDPOINT: &str = "https://api.exa.ai/search";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn search(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    max: usize,
    intent: &crate::search::intent::Intent,
) -> ProviderResult {
    let started = Instant::now();

    // Exa's `type` controls search mode. `auto` is the
    // balanced default. Category maps intent → domain focus.
    let category = match intent {
        crate::search::intent::Intent::Paper => Some("publication"),
        _ => None,
    };

    let mut body = json!({
        "query": query,
        "numResults": max.min(10),
        "type": "auto",
        "contents": {
            "text": {
                "maxCharacters": 300
            }
        }
    });
    if let Some(cat) = category {
        body["category"] = json!(cat);
    }

    let resp = client
        .post(ENDPOINT)
        .header("x-api-key", key)
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

    let results = json
        .get("results")
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
                    // Exa doesn't always return snippets in
                    // /search. Use highlights or text if present.
                    let snippet = r
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let score = r.get("score").and_then(Value::as_f64).unwrap_or(1.0) as f32;
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
