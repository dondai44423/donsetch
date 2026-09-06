//! Bright Data SERP API provider adapter.
//!
//! POST https://api.brightdata.com/request
//! Auth: Bearer <token>
//! Body: { zone, url, format: "raw" }
//!
//! The URL carries `brd_json=1` so Bright Data parses Google's
//! HTML into structured JSON before returning. The response has
//! an `organic` array with rank, title, link, description.
//!
//! Key format:
//!   token                    uses zone "serp_api1" (default)
//!   token::zone_name         uses the specified zone
//!
//! Zone can also be set via DONSETCH_BRIGHTDATA_ZONE env var,
//! which takes priority over the default but not over `::`.

use super::ProviderOutcome;
use std::time::Instant;

use serde_json::{Value, json};

use super::{KeyError, ProviderResult, SearchHit};

const ENDPOINT: &str = "https://api.brightdata.com/request";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DEFAULT_ZONE: &str = "serp_api1";

/// Split the key into (token, zone). Supports `token::zone`
/// encoding. Falls back to the env var, then the default. Empty
/// token/zone is rejected: the user typed something wrong and
/// the API would bill nothing but return a confusing error.
pub(crate) fn parse_key(key: &str) -> Result<(String, String), String> {
    if key.trim().is_empty() {
        return Err("brightdata key is empty".to_string());
    }
    if let Some((token, zone)) = key.split_once("::") {
        if token.trim().is_empty() {
            return Err("empty token before `::`".to_string());
        }
        if zone.trim().is_empty() {
            return Err("empty zone after `::` (add a zone name or drop the suffix)".to_string());
        }
        return Ok((token.to_string(), zone.to_string()));
    }
    let zone = std::env::var("DONSETCH_BRIGHTDATA_ZONE")
        .ok()
        .filter(|z| !z.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ZONE.to_string());
    Ok((key.trim().to_string(), zone))
}

/// Percent-encode the query exactly per RFC 3986 over its UTF-8
/// bytes (unreserved: A-Z a-z 0-9 - _ . ~). A char-code formatter
/// like `%{:02X} c` is wrong for any non-ASCII input: 'é' encodes
/// as %C3%A9 (two UTF-8 bytes), never %E9, and astral chars need
/// four bytes. Spaces become '+' which Google accepts in queries.
fn encode_query(q: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(q.len() * 3);
    for b in q.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'*' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

pub(crate) async fn search(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    max: usize,
    intent: &crate::search::intent::Intent,
) -> ProviderResult {
    let started = Instant::now();
    let (token, zone) = parse_key(key).map_err(|_| KeyError::InvalidKey)?;

    // Build the Google search URL with brd_json=1 for parsed JSON.
    // q must come first per Bright Data's docs.
    let encoded_q = encode_query(query);
    let mut google_url =
        format!("https://www.google.com/search?q={encoded_q}&brd_json=1&gl=us&hl=en");
    // News intent: use Google News vertical.
    if matches!(intent, crate::search::intent::Intent::News) {
        google_url.push_str("&tbm=nws");
    }

    let body = json!({
        "zone": zone,
        "url": google_url,
        "format": "raw",
    });

    let resp = client
        .post(ENDPOINT)
        .bearer_auth(&token)
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
        if lower.contains("invalid")
            && (lower.contains("key") || lower.contains("token") || lower.contains("zone"))
        {
            return Err(KeyError::InvalidKey);
        }
        return Err(KeyError::UnknownError(format!("HTTP {status}: {text}")));
    }

    // Bright Data returns parsed JSON when brd_json=1 is in the URL.
    // The response has an `organic` array with rank, title, link, description.
    let json: Value = serde_json::from_str(&text)
        .map_err(|e| KeyError::UnknownError(format!("parse error: {e}")))?;

    let results = json
        .get("organic")
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
        .unwrap_or_default();

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

    #[test]
    fn parse_key_simple() {
        let (token, zone) = parse_key("my-token-123").unwrap();
        assert_eq!(token, "my-token-123");
        assert_eq!(zone, "serp_api1");
    }

    #[test]
    fn parse_key_with_zone() {
        let (token, zone) = parse_key("my-token-123::my_zone").unwrap();
        assert_eq!(token, "my-token-123");
        assert_eq!(zone, "my_zone");
    }

    #[test]
    fn parse_key_rejects_empty_parts() {
        assert!(parse_key("").is_err());
        assert!(parse_key("  ").is_err());
        assert!(parse_key("::").is_err());
        assert!(parse_key("tok::").is_err());
        assert!(parse_key("::zon").is_err());
    }

    #[test]
    fn parse_key_env_zone_fallback() {
        unsafe { std::env::set_var("DONSETCH_BRIGHTDATA_ZONE", "env_zone") };
        let (_, zone) = parse_key("abc").unwrap();
        assert_eq!(zone, "env_zone");
        let (_, zone2) = parse_key("abc::explicit").unwrap();
        assert_eq!(zone2, "explicit");
        unsafe { std::env::remove_var("DONSETCH_BRIGHTDATA_ZONE") };
    }

    #[test]
    fn encode_query_basic() {
        assert_eq!(encode_query("hello world"), "hello+world");
        assert_eq!(encode_query("rust async"), "rust+async");
    }

    #[test]
    fn encode_query_special() {
        assert_eq!(encode_query("a+b/c"), "a%2Bb%2Fc");
    }

    #[test]
    fn encode_query_utf8_multibyte() {
        // 'é' is U+00E9 = 0xC3 0xA9 in UTF-8. A char-code encode
        // would emit %E9, which Google decodes as Latin-1 garbage.
        assert_eq!(encode_query("café"), "caf%C3%A9");
        // CJK: 日本語 = E6 97 A5 E6 9C AC E8 AA 9E
        assert_eq!(encode_query("日本語"), "%E6%97%A5%E6%9C%AC%E8%AA%9E");
        // Astral: emoji is 4 UTF-8 bytes.
        assert_eq!(encode_query("a 🦀"), "a+%F0%9F%A6%80");
    }
}
