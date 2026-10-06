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
    let key = key.trim();
    if key.trim().is_empty() {
        return Err("brightdata key is empty".to_string());
    }
    if let Some((token, zone)) = key.split_once("::") {
        let (token, zone) = (token.trim(), zone.trim());
        if token.trim().is_empty() {
            return Err("empty token before `::`".to_string());
        }
        if zone.trim().is_empty() {
            return Err("empty zone after `::` (add a zone name or drop the suffix)".to_string());
        }
        if token.chars().any(|c| c.is_whitespace() || c.is_control())
            || zone
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == ':')
        {
            return Err(
                "invalid token/zone shape: whitespace, control characters or an extra `::` suffix"
                    .into(),
            );
        }
        return Ok((token.to_string(), zone.to_string()));
    }
    let configured = crate::config::cfg()
        .search
        .brightdata_zone
        .trim()
        .to_string();
    let zone = if configured.is_empty() {
        DEFAULT_ZONE.to_string()
    } else {
        configured
    };
    if key.chars().any(|c| c.is_whitespace() || c.is_control())
        || zone
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == ':')
    {
        return Err("invalid token/zone shape: whitespace or control characters".into());
    }
    Ok((key.to_string(), zone))
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
    // A body-read failure is a transport error, not an empty body
    // (#286): keep what reqwest saw instead of mapping it to "".
    let text = read_serp_body(resp).await?;

    let results = parse_results(status, &text, max, intent).map_err(|mut e| {
        match &mut e {
            KeyError::UnknownError(detail) | KeyError::ServerError(detail) => {
                *detail = detail.replace(&token, "[redacted]")
            }
            _ => {}
        }
        e
    })?;
    Ok(ProviderOutcome {
        hits: results,
        ms: started.elapsed().as_millis() as u64,
        degraded: false,
    })
}

async fn read_serp_body(mut resp: reqwest::Response) -> Result<String, KeyError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(KeyError::from_transport)? {
        if bytes.len() + chunk.len() > crate::transport::MAX_BODY {
            return Err(KeyError::UnknownError(
                "SERP response exceeded the body cap".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map_err(|_| KeyError::UnknownError("SERP response is not UTF-8".into()))
}

fn parse_results(
    status: u16,
    text: &str,
    max: usize,
    intent: &crate::search::intent::Intent,
) -> Result<Vec<SearchHit>, KeyError> {
    if status == 401 {
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
        if status == 403 {
            return Err(KeyError::UnknownError(format!(
                "HTTP 403: check SERP zone type and token permissions: {}",
                super::err_body(text)
            )));
        }
        let lower = text.to_lowercase();
        if lower.contains("rate") || lower.contains("excessive") {
            return Err(KeyError::RateLimited);
        }
        if lower.contains("credit") || lower.contains("quota") || lower.contains("billing") {
            return Err(KeyError::CreditDepleted);
        }
        if lower.contains("invalid") && (lower.contains("key") || lower.contains("token")) {
            return Err(KeyError::InvalidKey);
        }
        return Err(KeyError::UnknownError(format!(
            "HTTP {status}: {}{}",
            super::err_body(text),
            if lower.contains("zone") {
                "; use <token>::<SERP zone name> from the Bright Data dashboard (docs/brightdata.md)"
            } else {
                ""
            }
        )));
    }

    // Bright Data returns parsed JSON when brd_json=1 is in the URL.
    // The response has an `organic` array with rank, title, link, description.
    let json: Value = super::parse_provider_json(status, text)?;

    let field =
        if matches!(intent, crate::search::intent::Intent::News) && json.get("news").is_some() {
            "news"
        } else {
            "organic"
        };
    let arr = json.get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| KeyError::UnknownError(format!("SERP JSON missing {field} results; check the SERP zone and output format (docs/brightdata.md)")))?;
    let results = arr
        .iter()
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
            if !url::Url::parse(&url)
                .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
            {
                return None;
            }
            let snippet = r
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let rank = r
                .get("rank")
                .or_else(|| r.get("global_rank"))
                .and_then(Value::as_u64)
                .unwrap_or(1) as f32;
            let score = 1.0 / rank.max(1.0);
            Some(SearchHit {
                title,
                url,
                snippet,
                score,
            })
        })
        .take(max)
        .collect();
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn news_results_and_global_ranks_survive_normalization() {
        let data = r#"{"news":[{"link":"javascript:bad"},{"link":"https://example.com/news","title":"News","description":"Story","global_rank":4}],"organic":[]}"#;
        let hits = parse_results(200, data, 1, &crate::search::intent::Intent::News).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "invalid links must not consume the result limit"
        );
        assert_eq!(hits[0].url, "https://example.com/news");
        assert_eq!(hits[0].snippet, "Story");
        assert_eq!(hits[0].score, 0.25);
        assert!(
            parse_results(
                200,
                r#"{"error":"wrong product"}"#,
                5,
                &crate::search::intent::Intent::Web
            )
            .is_err()
        );
        assert!(
            parse_results(
                200,
                r#"{"organic":[]}"#,
                5,
                &crate::search::intent::Intent::Web
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            parse_results(
                403,
                "zone not allowed",
                5,
                &crate::search::intent::Intent::Web
            )
            .unwrap_err()
            .to_key_state()
            .is_none()
        );
    }

    #[test]
    fn paid_key_parts_are_trimmed_and_controls_rejected() {
        assert_eq!(
            parse_key(" token :: zone ").unwrap(),
            ("token".into(), "zone".into())
        );
        assert!(parse_key("tok\n en::zone").is_err());
        assert!(parse_key("token::zone::extra").is_err());
    }

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
