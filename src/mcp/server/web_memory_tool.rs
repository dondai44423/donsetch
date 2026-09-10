//! web_memory MCP tool (v4 phase 5.2).
//!
//! One-call semantic search over the local page memory. This handler
//! does nothing else: ingest is attached to the other tools (a
//! successful fetch/search/crawl row lands in the store); searching
//! is a query embed plus a cosine scan over the local index.
//! Everything stays on this box.

use std::sync::Arc;

use serde_json::{Value, json};

use super::Daemon;
use super::errors::tool_error;
use crate::memory::{self, MemoryHit};

fn render_hits_markdown(query: &str, hits: &[MemoryHit], rows: usize, kill: bool) -> String {
    if kill {
        return format!(
            "# web_memory disabled: {query}\n\nThe kill switch DONSETCH_NO_WEB_MEMORY is active:\n\
             the local page memory neither reads nor writes. Unset it to query the index\n\
             (it currently holds {rows} rows).\n"
        );
    }
    if rows == 0 {
        return format!(
            "# No local memory yet for: {query}\n\nThe local page memory is empty.\n\
             Fetch pages (web_fetch / web_crawl) and they land here automatically;\n\
             then ask the same query again.\n"
        );
    }
    if hits.is_empty() {
        return format!(
            "# No local memory hits for: {query}\n\nThe local index has {rows} rows\n\
             but none match this query yet.\n"
        );
    }
    let mut buf = String::new();
    for h in hits {
        let title = if h.title.is_empty() {
            h.url.as_str()
        } else {
            h.title.as_str()
        };
        buf.push_str(&format!(
            "- {:.3} [{title}]({})\n\n  {}\n\n",
            h.score, h.url, h.snippet
        ));
    }
    buf
}

/// MCP tool: query is required, limit optional (1..=50, default 6).
pub async fn web_memory_tool(
    _daemon: &Arc<Daemon>,
    args: &Value,
    _ctx: Option<super::ToolCtx>,
) -> Value {
    let limit_opt = args.get("limit").and_then(Value::as_u64);
    if let Some(problem) = memory::guard(limit_opt) {
        return tool_error(problem);
    }
    let Some(query) = args
        .get("query")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return tool_error("web_memory: query is required");
    };
    if query.chars().count() > 1024 {
        return tool_error("web_memory: query too long (over 1024 chars)");
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(6);
    // Kill switch: answer honestly without running the model.
    let started = std::time::Instant::now();
    let kill = memory::kill_switch();
    let hits = if kill {
        Vec::new()
    } else {
        match memory::search(&query, limit) {
            Ok(h) => h,
            Err(e) => return tool_error(format!("web_memory: search failed: {e}")),
        }
    };
    let rows = memory::rows();
    let took_ms = started.elapsed().as_millis() as u64;
    let md = render_hits_markdown(&query, &hits, rows, kill);
    let structured = json!({
        "query": query,
        "rows": rows,
        "took_ms": took_ms,
        "hits": hits,
        "kill_switch": memory::kill_switch(),
    });
    json!({
        "content": [{ "type": "text", "text": md }],
        "structuredContent": structured,
        "_meta": {
            "rows": rows,
            "took_ms": took_ms,
        },
    })
}
