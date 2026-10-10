//! `web_screenshot` MCP tool (issue #171).
//!
//! A rendered PNG of a page, via the same tier-2 browser the fetch
//! tool escalates to. No new trust surface: the URL passes the same
//! guards, the browser is the same pool, the bytes stay in this
//! process for the caller's token budget to decide on.

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
    daemon: &Arc<Daemon>,
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
    // The render path honors cancellation and a hard ceiling: the
    // pool-slot acquire alone can wait on another call's 20-40s
    // render, and the tool used to observe neither the deadline nor
    // notifications/cancelled while it did.
    let work = async {
        let inner: Result<serde_json::Value, serde_json::Value> = async {
            let target = ensure_url_safe(target.as_str())
                .await
                .map_err(|error| fetch_failure(&error, &url_in, ToString::to_string))?;
            let (wire, route) = {
                let mut state = daemon.state.lock().await;
                let caps = crate::persona::PersonaCaps::from_profile(daemon.fetcher.profile());
                state.ensure_persona(&host, &caps);
                state.ensure_persona_egress(&host);
                let mut wire = state
                    .personas
                    .get(&host)
                    .filter(|p| p.quarantine_reason.is_none())
                    .map(|p| p.ghost_wire())
                    .unwrap_or_default();
                let route = daemon.fetcher.route_for_fetch(target.as_str());
                wire.route = Some(route.clone());
                (wire, route)
            };
            let ghost = {
                // v4 E2: screenshot must claim the same persona wire
                // as tier-1 (viewport + locale), or the capture is a
                // different identity than the page we just fetched.
                match daemon
                    .ghost_mgr
                    .acquire_for_wire(&daemon.profile, Some(&host), wire)
                    .await
                {
                    Ok(g) => g,
                    Err(e) => {
                        return Err(tool_error(format!("web_screenshot: no browser: {e}")));
                    }
                }
            };

            let read = daemon
                .ghost_mgr
                .read_document(
                    ghost,
                    &daemon.profile,
                    target.as_str(),
                    budget
                        .saturating_sub(started.elapsed())
                        .min(Duration::from_secs(20)),
                )
                .await
                .map_err(|error| {
                    fetch_failure(&error, &url_in, |e| {
                        format!("web_screenshot: page failed to render: {e}")
                    })
                })?;
            let ghost = read.guard;
            if wait_ms > 0 {
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
            let (png, document) = match ghost.screenshot_document(full_page).await {
                Ok(b) => b,
                Err(e) => return Err(tool_error(format!("web_screenshot: capture failed: {e}"))),
            };
            validate_url_basic(&document.url)
                .map_err(|error| fetch_failure(&error, &document.url, ToString::to_string))?;
            let filename = format!("capture-{}.png", crate::handles::random_base62(16));
            let path = crate::paths::resolve_screenshot_path(&filename).map_err(tool_error)?;
            crate::ghost::save_screenshot(&path, &png)
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
                            "Captured {} ({} view, {} PNG bytes)",
                            document.url,
                            if full_page { "full-page" } else { "viewport" },
                            png.len()
                        )
                    }
                ],
                "structuredContent": {
                    "ok": true,
                    "url": document.url,
                    "full_page": full_page,
                    "bytes": png.len(),
                    "path": path
                },
                "isError": false,
                "_meta": {
                    "com.donsetch/screenshot-debug": {
                        "requested_url": url_in,
                        "route": route.id(),
                        "document": document
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
