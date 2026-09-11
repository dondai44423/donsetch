//! `web_screenshot` MCP tool (issue #171).
//!
//! A rendered PNG of a page, via the same tier-2 browser the fetch
//! tool escalates to. No new trust surface: the URL passes the same
//! guards, the browser is the same pool, the bytes stay in this
//! process for the caller's token budget to decide on.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::Daemon;
use super::errors::tool_error;
use crate::fetch::guards::{ensure_url_safe, validate_url_basic};

const WAIT_MS_MAX: u64 = 5000;

pub async fn web_screenshot_tool(
    daemon: &Arc<Daemon>,
    args: &Value,
    _ctx: Option<super::ToolCtx>,
) -> Value {
    let url_in = match args.get("url").and_then(Value::as_str) {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => {
            return tool_error("web_screenshot needs a url string");
        }
    };
    let full_page = args
        .get("full_page")
        .and_then(Value::as_bool)
        .unwrap_or(true);
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

    let mut ghost = match daemon
        .ghost_mgr
        .acquire_for(&daemon.profile, Some(&host))
        .await
    {
        Ok(g) => g,
        Err(e) => return tool_error(format!("web_screenshot: no browser: {e}")),
    };

    if let Err(e) =
        crate::ghost::ops::ghost_fetch(&mut ghost, target.as_str(), Duration::from_secs(20)).await
    {
        return tool_error(format!("web_screenshot: page failed to render: {e}"));
    }
    if wait_ms > 0 {
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    }
    let png = match ghost.screenshot_bytes(full_page).await {
        Ok(b) => b,
        Err(e) => return tool_error(format!("web_screenshot: capture failed: {e}")),
    };
    let b64 = crate::ghost::encode_base64(&png);

    json!({
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
    })
}
