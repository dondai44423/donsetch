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
use super::errors::tool_error;
use crate::fetch::guards::{ensure_url_safe, validate_url_basic};

const WAIT_MS_MAX: u64 = 5000;

/// Viewport is the cheap default (matches CLI --full-page SetTrue:
/// omitted flag = viewport). Omitting full_page on MCP must NOT
/// silently mean full-page: that is the expensive path and it made
/// CLI/MCP disagree on the same call shape.
fn full_page_arg(args: &Value) -> bool {
    args.get("full_page")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub async fn web_screenshot_tool(
    daemon: &Arc<Daemon>,
    args: &Value,
    mut ctx: Option<super::ToolCtx>,
) -> Value {
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

    let mut target = match validate_url_basic(&url_in) {
        Ok(u) => u,
        Err(e) => return tool_error(e.to_string()),
    };
    let host = target.host_str().unwrap_or("").to_string();
    if host.is_empty() {
        return tool_error("web_screenshot: the url has no host");
    }
    target = match ensure_url_safe(target.as_str()).await {
        Ok(u) => u,
        Err(e) => return tool_error(e.to_string()),
    };

    // The render path honors cancellation and a hard ceiling: the
    // pool-slot acquire alone can wait on another call's 20-40s
    // render, and the tool used to observe neither the deadline nor
    // notifications/cancelled while it did.
    let work = async {
        let inner: Result<serde_json::Value, serde_json::Value> = async {
            let mut ghost = {
                // v4 E2: screenshot must claim the same persona wire
                // as tier-1 (viewport + locale), or the capture is a
                // different identity than the page we just fetched.
                let wire = {
                    let state = daemon.state.lock().await;
                    state
                        .personas
                        .get(host.as_str())
                        .filter(|p| p.quarantine_reason.is_none())
                        .map(|p| p.ghost_wire())
                        .unwrap_or_default()
                };
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

            if let Err(e) =
                crate::ghost::ops::ghost_fetch(&mut ghost, target.as_str(), Duration::from_secs(20))
                    .await
            {
                return Err(tool_error(format!(
                    "web_screenshot: page failed to render: {e}"
                )));
            }
            if wait_ms > 0 {
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
            let png = match ghost.screenshot_bytes(full_page).await {
                Ok(b) => b,
                Err(e) => return Err(tool_error(format!("web_screenshot: capture failed: {e}"))),
            };
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
                            url_in,
                            if full_page { "full-page" } else { "viewport" },
                            png.len()
                        )
                    }
                ],
                "structuredContent": {
                    "ok": true,
                    "url": url_in,
                    "full_page": full_page,
                    "bytes": png.len()
                },
                "isError": false
            }))
        }
        .await;
        match inner {
            Ok(v) | Err(v) => v,
        }
    };
    super::run_with_budget(work, Some(Duration::from_secs(60)), ctx.as_mut(), || {
        tool_error("web_screenshot: deadline exceeded (60s)")
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::full_page_arg;
    use serde_json::json;

    #[test]
    fn omitted_full_page_defaults_to_viewport_not_full_page() {
        // CLI --full-page is SetTrue (omitted = false). MCP omitting
        // the field used to default true: same call shape, different
        // expensive path. Viewport is the cheap default on both.
        assert!(!full_page_arg(&json!({})), "omit = viewport");
        assert!(!full_page_arg(&json!({"full_page": false})));
        assert!(full_page_arg(&json!({"full_page": true})));
    }
}
