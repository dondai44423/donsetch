//! Parallel AI search provider adapter.
//!
//! POST https://api.parallel.ai/v1/search
//! Auth: x-api-key <key>
//! Body: { objective, search_queries, mode: "fast" }
//! Response: { results: [{ url, title, excerpts, publish_date }] }
//!
//! "fast" mode: high quality within a 1-second latency budget.
//! Best mix of quality and speed per the user's request.

use super::ProviderOutcome;
use std::time::Instant;

use serde_json::{Value, json};

use super::{KeyError, ProviderResult, SearchHit};

const ENDPOINT: &str = "https://api.parallel.ai/v1/search";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn search(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    max: usize,
    intent: &crate::search::intent::Intent,
) -> ProviderResult {
    let started = Instant::now();

    // Parallel's API takes an objective + search_queries.
    // The query is both the objective and the single search query.
    // For news intent, add tbm=nws equivalent via the objective.
    let objective = match intent {
        crate::search::intent::Intent::News => format!("Find recent news about: {query}"),
        crate::search::intent::Intent::Paper => format!("Find academic papers about: {query}"),
        _ => query.to_string(),
    };

    let body = json!({
        "objective": objective,
        "search_queries": [query],
        "mode": "fast",
    });

    let resp = client
        .post(ENDPOINT)
        .header("x-api-key", key)
        .header("Content-Type", "application/json")
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
        if lower.contains("invalid") && (lower.contains("key") || lower.contains("api")) {
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
                .take(max)
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
                    // Excerpts is an array of strings. Join them
                    // into a single snippet for the agent.
                    let snippet = r
                        .get("excerpts")
                        .and_then(Value::as_array)
                        .map(|excerpts| {
                            excerpts
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    // Score by position: 1st result = 1.0, decay.
                    let score = 1.0 / (arr.iter().position(|x| x == r).unwrap_or(0) as f32 + 1.0);
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
