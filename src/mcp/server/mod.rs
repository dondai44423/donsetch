//! The stdio server: read loop, dispatch, writer task,
//! and the tool handlers. The per-tool implementations live in
//! child modules: `errors` (the structured error contract),
//! `crawl_tool`, `fetch_tool`, `search_tool`. This file keeps
//! the Daemon, the dispatch (`handle`), and the shared glue.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use futures_util::FutureExt;

mod answer_tool;
mod crawl_tool;
mod errors;
mod fetch_tool;
mod search_tool;
#[cfg(feature = "rerank")]
mod web_memory_tool;
mod web_screenshot_tool;

use crate::crawl::real as crawl_real;
use crate::crawl::{CrawlMode, CrawlOptions, Crawler};
use crate::detect::walls::{Vendor, Verdict};
use crate::error::FetchError;
use crate::extract::{self, ExtractOptions};
use crate::fetch::client::Fetcher;
use crate::ghost::cache::{CookieRecord, GhostState, RouteDecision};
use crate::ghost::manager::GhostManager;
use crate::ghost::ops;
use crate::profile::BrowserProfile;
use crate::search::byok::ByokSearcher;
use crate::search::egress::EgressPool;
use crate::search::intent::Intent;
use crate::search::{self, Searcher};
use errors::*;
use fetch_tool::*;
use search_tool::*;

use super::tools;

/// Shared daemon state, built once, lives forever.
pub struct Daemon {
    fetcher: Arc<Fetcher>,
    profile: BrowserProfile,
    ghost_mgr: Arc<GhostManager>,
    state: Arc<Mutex<GhostState>>,
    searcher: Arc<Searcher>,
    byok: ByokSearcher,
    crawler: Crawler,
    handles: Arc<Mutex<crate::handles::HandleTable>>,
    history: Arc<std::sync::Mutex<crate::pages::history::PageHistory>>,
    /// (modified, len) of ghost-state.json at the last vault refresh:
    /// a login or logout CLI write flips this and the next tool call
    /// resyncs the cookie jar. mtime-only would miss same-second
    /// login+logout pairs, hence the length pair.
    vault_seen: tokio::sync::Mutex<Option<(u64, u64)>>,
    /// One background pre-solve at a time: search hints for a walled
    /// domain trigger a solve while the agent is still reading
    /// results. Cheap spinlock: a lost race just skips the win.
    pre_solve_busy: std::sync::atomic::AtomicBool,
    /// Background route-memory prober handle (v4 phase 0.2);
    /// aborted on shutdown.
    probe_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Daemon {
    pub async fn new() -> Result<Self, crate::error::FetchError> {
        let profile = BrowserProfile::host_default();
        let fetcher = Arc::new(Fetcher::new(profile.clone())?);
        let proxies = crate::transport::proxy::load_all();
        let ghost_mgr = GhostManager::new().await;
        let state = Arc::new(Mutex::new(GhostState::load()));

        // Tier 1 starts the session with the vault too: a domain
        // that serves without JS gets an authenticated plain-HTTP
        // fetch on the very first request after a restart, not
        // only after the browser has visited it once.
        {
            let sessions = crate::ghost::cache::load_session_cookies();
            fetcher.import_cookies(&sessions).await;
            // Tier-1 jar persistence (v4 phase 1.4), kill-switched.
            if !crate::config::env_flag("DONSETCH_NO_COOKIE_VAULT") {
                let jar = state.lock().await.tier1_cookies.clone();
                fetcher.import_cookies(&jar).await;
            }
        }

        // Build ghost escalation hook for the crawl: renders
        // JS-only pages in the headless browser so SPA sites
        // yield real content instead of empty shells. Capped at
        // 3 per crawl by the orchestrator. The search clone runs
        // the SAME machinery but never serves from the render
        // cache: a cached walled SERP would replay as "no
        // results" forever inside one TTL window.
        let ghost_hook = make_ghost_hook(
            Arc::clone(&ghost_mgr),
            profile.clone(),
            Arc::clone(&fetcher),
            Arc::clone(&state),
            false,
        );
        let search_ghost = make_ghost_hook(
            Arc::clone(&ghost_mgr),
            profile.clone(),
            Arc::clone(&fetcher),
            Arc::clone(&state),
            true,
        );

        let (crawler, _gov) = crawl_real::build(Arc::clone(&fetcher), proxies);
        let crawler = crawler.with_ghost(ghost_hook);

        let searcher = Arc::new(
            Searcher::new(Fetcher::new(profile.clone())?, EgressPool::from_env())
                .with_ghost(search_ghost),
        );
        searcher.preflight();

        Ok(Self {
            fetcher,
            profile,
            ghost_mgr,
            state,
            searcher,
            byok: ByokSearcher::new(),
            crawler,
            handles: Arc::new(Mutex::new(crate::handles::HandleTable::load())),
            history: Arc::new(std::sync::Mutex::new(
                crate::pages::history::PageHistory::load(),
            )),
            vault_seen: tokio::sync::Mutex::new(None),
            pre_solve_busy: std::sync::atomic::AtomicBool::new(false),
            probe_task: std::sync::Mutex::new(None),
        })
    }

    /// Spawn the background route-memory prober (v4 phase 0.2).
    /// Called once the daemon is inside its runtime; one-shot CLI
    /// paths never call it, so short-lived processes stay clean.
    pub fn start_prober(self: &Arc<Self>) {
        if crate::config::env_flag("DONSETCH_NO_ROUTE_PROBES") {
            return;
        }
        let handle = crate::ghost::probe::spawn(Arc::clone(&self.fetcher), Arc::clone(&self.state));
        if let Ok(mut slot) = self.probe_task.lock() {
            *slot = Some(handle);
        }
    }

    /// Shutdown: kill ghost browser + Xvfb (if owned).
    /// Called by the CLI before exit; by the MCP daemon on close.
    pub async fn shutdown(&self) {
        self.ghost_mgr.shutdown().await;
    }

    /// Resync the tier-1 cookie jar from the session vault when the
    /// on-disk file moved (login/logout/rotation). Stat-only in the
    /// hot path; parse only after a real change.
    pub async fn refresh_vault(&self) {
        let meta = std::fs::metadata(crate::paths::cache_dir().join("ghost-state.json"))
            .ok()
            .map(|m| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    (m.mtime() as u64, m.len())
                }
                #[cfg(not(unix))]
                {
                    (0u64, m.len())
                }
            });
        let Some(sig) = meta else { return };
        let changed = {
            let mut seen = self.vault_seen.lock().await;
            let changed = *seen != Some(sig);
            if changed {
                *seen = Some(sig);
            }
            changed
        };
        if changed {
            let mut cookies = crate::ghost::cache::load_session_cookies();
            // Reset is wholesale: keep the tier-1 jar (device /
            // analytics cookies the browser-real daemon already
            // holds) so a login resync does not erase the session.
            if !crate::config::env_flag("DONSETCH_NO_COOKIE_VAULT") {
                cookies.extend(self.state.lock().await.tier1_cookies.clone());
            }
            self.fetcher.reset_to(&cookies).await;
        }
    }
}

/// Note: The stdio transport implementation has been moved to `stdio.rs`.
/// This function is kept for backward compatibility but delegates to the stdio module.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    crate::mcp::stdio::run().await
}

/// Per-request tool context (v3): cancellation signal + progress
/// emitter. `None` = CLI invocation (no client to cancel us).
#[derive(Clone)]
pub(crate) struct ToolCtx {
    cancel: tokio::sync::watch::Receiver<bool>,
    /// The raw _meta.progressToken from the request, if the client
    /// asked for progress notifications.
    progress_token: Option<Value>,
    progress_tx: Option<mpsc::UnboundedSender<String>>,
}

/// Standalone progress emission for spawned subtasks (batch fetch
/// workers) that own cloned parts instead of the whole ctx.
pub(crate) fn emit_progress(
    parts: &(Option<Value>, Option<mpsc::UnboundedSender<String>>),
    done: u64,
    total: Option<u64>,
    message: &str,
) {
    let (Some(token), Some(tx)) = (&parts.0, &parts.1) else {
        return;
    };
    let mut params = json!({ "progressToken": token, "progress": done });
    if let Some(t) = total {
        params["total"] = json!(t);
    }
    if !message.is_empty() {
        params["message"] = json!(message);
    }
    let line = json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": params,
    })
    .to_string();
    let _ = tx.send(line);
}

impl ToolCtx {
    pub fn cancelled(&self) -> bool {
        *self.cancel.borrow() || self.cancel.has_changed().unwrap_or(false)
    }

    /// Resolves when the client cancels this request.
    pub async fn cancelled_async(&mut self) -> bool {
        if self.cancelled() {
            return true;
        }
        self.cancel.changed().await.is_err() || *self.cancel.borrow()
    }

    /// Emit an MCP progress notification if the client asked for
    /// progress. Never blocks, never panics : progress is a
    /// courtesy, not a contract.
    /// Cloneable progress parts for subtasks and closures.
    pub fn progress_parts(&self) -> (Option<Value>, Option<mpsc::UnboundedSender<String>>) {
        (self.progress_token.clone(), self.progress_tx.clone())
    }

    /// Clone the cancel receiver (e.g. for the crawl's graceful
    /// stop flag).
    pub fn cancel_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.cancel.clone()
    }
}

/// Run a tool future under an optional deadline and cancellation,
/// collapsing the 2×2 combination into one place. Cancelled
/// results are discarded by the caller (handle suppresses the
/// response); the sentinel just keeps types simple.
pub(crate) async fn run_with_budget<F>(
    fut: F,
    deadline: Option<std::time::Duration>,
    ctx: Option<&mut ToolCtx>,
    on_deadline: impl FnOnce() -> Value,
) -> Value
where
    F: std::future::Future<Output = Value>,
{
    match (deadline, ctx) {
        (Some(d), Some(c)) => tokio::select! {
            r = fut => r,
            _ = tokio::time::sleep(d) => on_deadline(),
            _ = c.cancelled_async() => tool_error("cancelled"),
        },
        (Some(d), None) => tokio::select! {
            r = fut => r,
            _ = tokio::time::sleep(d) => on_deadline(),
        },
        (None, Some(c)) => tokio::select! {
            r = fut => r,
            _ = c.cancelled_async() => tool_error("cancelled"),
        },
        (None, None) => fut.await,
    }
}

/// Cancellation registry: request-id key → cancel sender.
pub type CancelMap =
    Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::sync::watch::Sender<bool>>>>;

/// Registry key for a JSON-RPC request id. Ids are numbers OR
/// strings (uuids, "req-7"); keying on i64 meant a string-id client
/// could never cancel anything. The JSON encoding keeps `7` and
/// `"7"` distinct, as the spec requires.
pub fn cancel_key(id: &Value) -> Option<String> {
    match id {
        Value::Number(_) | Value::String(_) => Some(id.to_string()),
        _ => None,
    }
}

/// Handle one line. Returns Some(response) for requests,
/// None for notifications and cancelled requests (per MCP spec,
/// a cancelled request gets no response).
pub async fn handle(
    daemon: &Arc<Daemon>,
    line: &str,
    cancels: &CancelMap,
    writer_tx: &mpsc::Sender<String>,
    mode: &crate::mcp::compat::ModeCell,
) -> Option<String> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            return Some(
                json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": { "code": -32700, "message": "parse error" }
                })
                .to_string(),
            );
        }
    };
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) that we recognize: stay silent.
    // (cancelled is intercepted in run() before this point.)
    id.as_ref()?;
    let id = id.unwrap();

    // tools/call gets the full context: cancel + progress.
    if method == "tools/call" {
        let rid = cancel_key(&id);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        if let Some(r) = rid.clone() {
            cancels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(r, cancel_tx);
        }
        // Probe kept outside the ctx so the final suppression check
        // still works after the ctx is consumed.
        let cancel_probe = cancel_rx.clone();
        // Progress plumbing: if the request carried a progressToken,
        // give the tool a channel straight to the writer.
        let progress_token = params.pointer("/_meta/progressToken").cloned();
        let (ptx, mut prx) = mpsc::unbounded_channel::<String>();
        let progress_tx = progress_token.as_ref().map(|_| ptx);
        let writer_tx = writer_tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(line) = prx.recv().await {
                let _ = writer_tx.send(line).await;
            }
        });
        let ctx = ToolCtx {
            cancel: cancel_rx,
            progress_token,
            progress_tx,
        };
        let result = call_tool_ctx(daemon, &params, Some(ctx)).await;
        // Deregister + stop forwarding progress. The forwarder ends
        // when ptx drops : it moved into ctx, dropped at await end.
        if let Some(r) = rid {
            cancels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&r);
        }
        let _ = forwarder.await;
        // A cancelled request never gets a response, even if the
        // tool managed to finish before observing the cancel.
        if *cancel_probe.borrow() || cancel_probe.has_changed().unwrap_or(false) {
            return None;
        }
        let result = match result {
            Ok(r) => {
                // Client-compat shaping (issue #27): harnesses that
                // show the model only `structuredContent` get the
                // surfaces merged into the one they render.
                let r = if crate::mcp::compat::effective(mode)
                    == crate::mcp::compat::ClientMode::TextOnly
                {
                    crate::mcp::compat::shape_result(r)
                } else {
                    r
                };
                Ok(r)
            }
            Err((code, message)) => Err((code, message)),
        };
        let resp = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": code, "message": message }
            }),
        };
        return Some(resp.to_string());
    }

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => {
            // Issue #27: remember what kind of client this session
            // speaks for. Lenient by design: the pointer read
            // tolerates a missing clientInfo and Claude Code's
            // object-shaped version.
            mode.set(crate::mcp::compat::mode_from_params(&params));
            Ok(initialize(&params))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools::list()),
        "notifications/initialized" | "notifications/cancelled" => {
            return None;
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    };

    let resp = match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message }
        }),
    };
    Some(resp.to_string())
}

fn initialize(params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    // Echo theirs if we speak it, else our max.
    let version = if tools::PROTOCOL_VERSIONS.contains(&asked) {
        asked
    } else {
        tools::PROTOCOL_VERSIONS[0]
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "instructions": tools::instructions(),
        "serverInfo": {
            "name": tools::SERVER_NAME,
            "title": tools::SERVER_TITLE,
            "version": tools::SERVER_VERSION
        }
    })
}

pub(crate) async fn call_tool(
    daemon: &Arc<Daemon>,
    params: &Value,
) -> Result<Value, (i64, String)> {
    call_tool_ctx(daemon, params, None).await
}

pub(crate) async fn call_tool_ctx(
    daemon: &Arc<Daemon>,
    params: &Value,
    ctx: Option<ToolCtx>,
) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match name {
        "web_fetch" => Ok(fetch_tool::fetch_tool(daemon, &args, ctx).await),
        "web_search" => Ok(search_tool::search_tool(daemon, &args, ctx).await),
        "web_answer" => Ok(answer_tool::answer_tool(daemon, &args, ctx).await),
        "web_crawl" => Ok(crawl_tool::crawl_tool(daemon, &args, ctx).await),
        "web_screenshot" => Ok(web_screenshot_tool::web_screenshot_tool(daemon, &args, ctx).await),
        #[cfg(feature = "rerank")]
        "web_memory" => Ok(web_memory_tool::web_memory_tool(daemon, &args, ctx).await),
        #[cfg(not(feature = "rerank"))]
        "web_memory" => Err((
            -32603,
            "web_memory requires the rerank feature (a release build); this binary was built without it".to_string(),
        )),
        _ => Err((-32602, format!("unknown tool: {name}"))),
    }
}

/// The crawl tool: two-phase site walk. Phase 1 = sitemap
/// discovery (a map costs ~2 requests instead of N fetches);
/// Phase 2 = Governor-paced frontier walk riding DonShadow +
/// DonSift. Resume tokens make huge sites paginable.
#[allow(clippy::field_reassign_with_default)]
#[cfg(test)]
mod initialize_tests {
    use super::{initialize, tools};
    use serde_json::{Value, json};

    /// The package version moves every release; the fixture holds a
    /// sentinel there so a version bump never touches it.
    const VERSION_SENTINEL: &str = "<CARGO_PKG_VERSION>";

    fn fixture() -> Value {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/initialize.json"
        ))
        .expect("read fixture");
        let mut v: Value = serde_json::from_str(&raw).expect("parse fixture");
        // Patch the one moving field on the parsed value, never on
        // the text: a textual substitution would also hit the
        // version wherever else it appeared.
        let slot = &mut v["serverInfo"]["version"];
        assert_eq!(slot, VERSION_SENTINEL, "fixture lost its version sentinel");
        *slot = json!(tools::SERVER_VERSION);
        v
    }

    /// Golden fixture: the whole initialize result : capabilities,
    /// serverInfo, and the `instructions` blurb the client injects
    /// into every session's context. If this fails, the handshake
    /// an agent sees changed; bless the fixture deliberately.
    #[test]
    fn initialize_matches_fixture() {
        // Unknown protocol version → we answer with our newest.
        let got = initialize(&json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(got, fixture(), "initialize result drifted from fixture");
    }

    /// Generated from the spec table, so a new tool must announce
    /// itself with no prose edit : and announce itself once: zero
    /// means an agent never learns the tool exists (deferred-loading
    /// clients see only names up front), twice is paid-for noise.
    #[test]
    fn instructions_list_every_tool_once() {
        let text = tools::instructions();
        for t in crate::spec::TOOLS {
            let hits = text.matches(t.name).count();
            assert_eq!(hits, 1, "{} announced {hits}x, expected once", t.name);
        }
    }

    #[test]
    fn known_protocol_version_is_echoed() {
        for v in tools::PROTOCOL_VERSIONS {
            assert_eq!(
                initialize(&json!({ "protocolVersion": v }))["protocolVersion"],
                json!(v)
            );
        }
    }
}

#[cfg(test)]
mod cancel_key_tests {
    use super::cancel_key;
    use serde_json::json;

    // JSON-RPC ids are numbers OR strings. The registry was keyed
    // on i64, so a client using string ids ("req-7", uuids) could
    // never cancel anything: its notifications/cancelled found no
    // entry and the crawl ran to completion.
    #[test]
    fn string_and_number_ids_both_get_a_key() {
        assert!(cancel_key(&json!("req-7")).is_some());
        assert!(cancel_key(&json!(7)).is_some());
        assert!(cancel_key(&json!(-1)).is_some());
        assert_ne!(cancel_key(&json!("7")), cancel_key(&json!(7)));
        assert_eq!(cancel_key(&json!("req-7")), cancel_key(&json!("req-7")));
        assert!(cancel_key(&json!(null)).is_none());
        assert!(cancel_key(&json!({"a": 1})).is_none());
    }
}
