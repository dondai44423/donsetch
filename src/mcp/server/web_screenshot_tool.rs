//! `web_screenshot` MCP tool (issue #171).
//!
//! One rendered PNG of a page, captured through the locally installed
//! browser in a throwaway headless profile. No new trust surface: the
//! URL passes the same guards, one launch per call, and the bytes stay
//! in this process for the caller's token budget to decide on.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::{Value, json};

use super::Daemon;
use super::errors::{policy_error_value, tool_error, tool_error_structured};
use crate::error::FetchError;
use crate::fetch::guards::{ensure_url_safe, validate_url_basic};

const WAIT_MS_MAX: u64 = 5000;

/// A guard or render error as the tool result: a rule denial keeps its
/// rule, kind and the operator's message through `policy_error_value`;
/// anything else is the plain text `wrap` builds from the error.
fn fetch_failure(error: &FetchError, url: &str, wrap: impl FnOnce(&FetchError) -> String) -> Value {
    if matches!(error, FetchError::Denied { .. }) {
        return policy_error_value(error, url, None);
    }
    tool_error(wrap(error))
}

/// Viewport is the cheap default (matches CLI --full-page SetTrue:
/// omitted flag = viewport). Omitting full_page on MCP must NOT
/// silently mean full-page: that is the expensive path and it made
/// CLI/MCP disagree on the same call shape.
fn full_page_arg(args: &Value) -> bool {
    args.get("full_page")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The whole-call budget: `deadline_ms` with the fetch tool's clamp
/// (500-600000 ms) and default (60s), so the two tools agree on what
/// a deadline means. V12: the value used to be a fixed 60s the
/// caller could not change.
fn deadline_arg(args: &Value) -> Duration {
    Duration::from_millis(
        args.get("deadline_ms")
            .and_then(Value::as_u64)
            .unwrap_or(60_000)
            .clamp(500, 600_000),
    )
}

pub async fn web_screenshot_tool(
    _daemon: &Arc<Daemon>,
    args: &Value,
    mut ctx: Option<super::ToolCtx>,
) -> Value {
    let started = std::time::Instant::now();
    let budget = deadline_arg(args);
    let url_in = match args.get("url").and_then(Value::as_str) {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => {
            return tool_error("web_screenshot needs a url string");
        }
    };
    let full_page = full_page_arg(args);
    let wait_ms = args
        .get("wait_ms")
        .and_then(Value::as_u64)
        .unwrap_or(600)
        .min(WAIT_MS_MAX);
    // Cloned before the work future takes `url_in` for its debug block.
    let deadline_error_url = url_in.clone();

    let target = match validate_url_basic(&url_in) {
        Ok(u) => u,
        Err(e) => return fetch_failure(&e, &url_in, ToString::to_string),
    };
    let host = target.host_str().unwrap_or("").to_string();
    if host.is_empty() {
        return tool_error("web_screenshot: the url has no host");
    }
    // The capture observes the caller's budget end to end: launch,
    // load and shoot all run under the tool's deadline and
    // notifications/cancelled (the launch is killed on drop).
    let work = async {
        let inner: Result<serde_json::Value, serde_json::Value> = async {
            let target = ensure_url_safe(target.as_str())
                .await
                .map_err(|error| fetch_failure(&error, &url_in, ToString::to_string))?;
            let (png, final_url) =
                crate::browser::capture_page_png(target.as_str(), full_page, wait_ms)
                    .await
                    .map_err(|e| tool_error(format!("web_screenshot: {e}")))?;
            let filename = format!("capture-{}.png", crate::handles::random_base62(16));
            let path = crate::paths::resolve_screenshot_path(&filename).map_err(tool_error)?;
            crate::browser::save_screenshot(&path, &png)
                .map_err(|e| tool_error(format!("web_screenshot: saving capture failed: {e}")))?;
            let b64 = BASE64_STANDARD.encode(&png);
            Ok(json!({
                "content": [
                    {
                        "type": "image",
                        "data": b64,
                        "mimeType": "image/png"
                    },
                    {
                        "type": "text",
                        "text": format!(
                            "Captured {final_url} ({} view, {} PNG bytes)",
                            if full_page { "full-page" } else { "viewport" },
                            png.len()
                        )
                    }
                ],
                "structuredContent": {
                    "ok": true,
                    "url": final_url,
                    "full_page": full_page,
                    "bytes": png.len(),
                    "path": path
                },
                "isError": false,
                "_meta": {
                    "com.donsetch/screenshot-debug": {
                        "requested_url": url_in
                    }
                }
            }))
        }
        .await;
        match inner {
            Ok(v) | Err(v) => v,
        }
    };
    super::run_with_budget(
        work,
        Some(budget.saturating_sub(started.elapsed())),
        ctx.as_mut(),
        || {
            tool_error_structured(
                format!(
                    "web_screenshot: deadline exceeded after {}ms",
                    budget.as_millis()
                ),
                "transient",
                Some(json!({
                    "code": "deadline.hit",
                    "url": deadline_error_url,
                    "next_action": "raise deadline_ms or wait_ms, or capture the viewport instead of the full page",
                })),
            )
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{deadline_arg, fetch_failure, full_page_arg};
    use crate::fetch::guards::validate_url_basic_with_policy;
    use crate::rules::{RuleAction, RuleSet, RulesSection, UrlRule};
    use serde_json::json;
    use std::time::Duration;

    fn deny_example_com() -> RuleSet {
        let mut section = RulesSection::default();
        section.url.insert(
            "example.com".to_string(),
            UrlRule {
                action: RuleAction::Deny,
                message: Some("ask the human operator to download it".to_string()),
                reason: Some("ip_ban".to_string()),
                ..Default::default()
            },
        );
        RuleSet::compile(&section).unwrap()
    }

    // The first guard of web_screenshot is the sync validate_url_basic.
    // A rule denial there must reach the caller as the structured policy
    // error (rule, code, the operator's message), not as plain text.
    #[test]
    fn a_denied_url_surfaces_as_the_policy_error() {
        let rules = deny_example_com();
        let url = "https://www.example.com/page";
        let error = validate_url_basic_with_policy(url, false, &rules).unwrap_err();
        let value = fetch_failure(&error, url, ToString::to_string);
        assert_eq!(value["structuredContent"]["ok"], false);
        let structured = &value["structuredContent"];
        assert_eq!(structured["code"], "policy.denied.ip_ban");
        assert_eq!(structured["rule"], "example.com");
        assert_eq!(
            structured["next_action"],
            "ask the human operator to download it"
        );
    }

    // The negative case: a host no rule names passes the guard, and an
    // error that is not a denial keeps the plain-text shape.
    #[test]
    fn other_guard_errors_stay_plain_text() {
        let rules = deny_example_com();
        assert!(validate_url_basic_with_policy("https://example.org/", false, &rules).is_ok());
        let url = "ftp://example.org/";
        let error = validate_url_basic_with_policy(url, false, &rules).unwrap_err();
        let value = fetch_failure(&error, url, ToString::to_string);
        assert_eq!(value["structuredContent"]["ok"], false);
        assert!(
            !value["structuredContent"]["code"]
                .as_str()
                .unwrap_or("")
                .starts_with("policy.denied"),
            "{value}"
        );
    }

    #[test]
    fn omitted_full_page_defaults_to_viewport_not_full_page() {
        // CLI --full-page is SetTrue (omitted = false). MCP omitting
        // the field used to default true: same call shape, different
        // expensive path. Viewport is the cheap default on both.
        assert!(!full_page_arg(&json!({})), "omit = viewport");
        assert!(!full_page_arg(&json!({"full_page": false})));
        assert!(full_page_arg(&json!({"full_page": true})));
    }

    // V12: deadline_ms mirrors web_fetch: default 60s, clamp
    // 500-600000 ms, so both tools bound a call the same way.
    #[test]
    fn deadline_ms_defaults_and_clamps_like_fetch() {
        assert_eq!(deadline_arg(&json!({})), Duration::from_secs(60));
        assert_eq!(
            deadline_arg(&json!({"deadline_ms": 4000})),
            Duration::from_secs(4)
        );
        assert_eq!(
            deadline_arg(&json!({"deadline_ms": 100})),
            Duration::from_millis(500)
        );
        assert_eq!(
            deadline_arg(&json!({"deadline_ms": 10_000_000})),
            Duration::from_secs(600)
        );
    }
}
