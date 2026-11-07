//! The stdio server: read loop, dispatch, writer task,
//! and the tool handlers. The per-tool implementations live in
//! child modules: `errors` (the structured error contract),
//! `crawl_tool`, `fetch_tool`, `search_tool`. This file keeps
//! the Daemon, the dispatch (`handle`), and the shared glue.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

mod crawl_tool;
mod errors;
mod fetch_tool;
mod search_tool;
mod web_screenshot_tool;

use crate::crawl::real as crawl_real;
use crate::crawl::{CrawlMode, CrawlOptions, Crawler};
use crate::detect::walls::{Vendor, Verdict};
use crate::error::FetchError;
use crate::extract::{self, ExtractOptions};
use crate::fetch::client::Fetcher;
use crate::profile::BrowserProfile;
use crate::search::byok::ByokSearcher;
use crate::search::egress::EgressPool;
use crate::search::intent::Intent;
use crate::search::{self, Searcher};
use crate::state::GhostState;
use errors::*;
// The uniform failure predicate (v4.7 envelope) : the CLI's exit-code and
// rendering paths branch on the same signal the wire carries.
pub(crate) use errors::is_failure;
use fetch_tool::*;

use super::tools;

/// Shared daemon state, built once, lives forever.
pub struct Daemon {
    fetcher: Arc<Fetcher>,
    state: Arc<Mutex<GhostState>>,
    searcher: Arc<Searcher>,
    byok: ByokSearcher,
    crawler: Crawler,
    handles: Arc<Mutex<crate::handles::HandleTable>>,
    /// Session-local search references are useful only to a persistent caller.
    pub(crate) search_handles: bool,
    history: Arc<std::sync::Mutex<crate::pages::history::PageHistory>>,
    /// Background route-memory prober handle (v4 phase 0.2);
    /// aborted on shutdown.
    probe_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Owned startup proxy probes; cancelled on shutdown or daemon drop.
    preflight_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Daemon {
    pub async fn new() -> Result<Self, crate::error::FetchError> {
        let profile = BrowserProfile::host_default();
        // One egress fabric for search, crawl, and fetch (v4 A2).
        let egress = std::sync::Arc::new(EgressPool::from_env());
        crate::search::egress::install_global(std::sync::Arc::clone(&egress));
        let fetcher =
            Arc::new(Fetcher::new(profile.clone())?.with_egress(std::sync::Arc::clone(&egress)));
        crate::state::ensure_session_identity();
        let state = Arc::new(Mutex::new(GhostState::load()));

        let (crawler, _gov) =
            crawl_real::build(Arc::clone(&fetcher), std::sync::Arc::clone(&egress));

        let searcher = Arc::new(Searcher::new_shared(
            Fetcher::new(profile.clone())?.with_egress(std::sync::Arc::clone(&egress)),
            egress,
        ));
        let preflight_task = searcher.preflight();

        Ok(Self {
            fetcher,
            state,
            searcher,
            byok: ByokSearcher::new(),
            crawler,
            handles: Arc::new(Mutex::new(crate::handles::HandleTable::load())),
            search_handles: true,
            history: Arc::new(std::sync::Mutex::new(
                crate::pages::history::PageHistory::load(),
            )),
            probe_task: std::sync::Mutex::new(None),
            preflight_task: std::sync::Mutex::new(Some(preflight_task)),
        })
    }

    /// Spawn the background route-memory prober (v4 phase 0.2).
    /// Called once the daemon is inside its runtime; one-shot CLI
    /// paths never call it, so short-lived processes stay clean.
    pub fn start_prober(self: &Arc<Self>) {
        if !crate::config::cfg().browser.route_probes {
            return;
        }
        let handle = crate::state_probe::spawn(Arc::clone(&self.fetcher), Arc::clone(&self.state));
        let mut slot = self
            .probe_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // One prober: a second start replaces (and cancels) the first.
        if let Some(previous) = slot.take() {
            previous.abort();
        }
        *slot = Some(handle);
    }

    /// Shutdown: cancel every daemon-owned helper task and join it.
    /// Called by the CLI before exit; by the MCP daemon on close.
    pub async fn shutdown(&self) {
        // Every daemon-owned helper is cancelled AND joined: the startup
        // probes and the route-memory prober. An orphaned helper would
        // keep working for a retired daemon (and keep its Arcs alive).
        let preflight = self
            .preflight_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = preflight {
            task.abort();
            let _ = task.await;
        }
        let prober = self
            .probe_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = prober {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        for slot in [&mut self.preflight_task, &mut self.probe_task] {
            if let Some(task) = slot
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                task.abort();
            }
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
        // Progress ends at the cancel: the client that cancelled no
        // longer tracks the request's progress token (the TypeScript SDK
        // drops the token's handler before it sends
        // notifications/cancelled), and beats still queued then are
        // dropped. A cancel sender dropped without a cancel is the call
        // ending normally, and every beat it queued is still forwarded.
        let mut cancel_seen = cancel_rx.clone();
        let forwarder = tokio::spawn(async move {
            let cancelled = async move {
                if cancel_seen.wait_for(|cancelled| *cancelled).await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(cancelled);
            loop {
                tokio::select! {
                    biased;
                    () = &mut cancelled => break,
                    line = prx.recv() => match line {
                        Some(line) => {
                            let _ = writer_tx.send(line).await;
                        }
                        None => break,
                    },
                }
            }
        });
        let ctx = ToolCtx {
            cancel: cancel_rx,
            progress_token,
            progress_tx,
        };
        let result = call_tool_ctx(daemon, &params, Some(ctx)).await;
        // Deregister + stop forwarding progress. The forwarder ends at
        // the cancel, or when ptx drops : it moved into ctx, dropped at
        // await end.
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
    // Clients may forward arguments without schema validation. Reject
    // unknown fields and wrong types before any network or browser work.
    if let Some(rejected) = invalid_args_error(name, &args) {
        return Ok(rejected);
    }
    match name {
        "web_fetch" => Ok(Box::pin(fetch_tool::fetch_tool(daemon, &args, ctx)).await),
        "web_search" => Ok(Box::pin(search_tool::search_tool(daemon, &args, ctx)).await),
        "web_crawl" => Ok(crawl_tool::crawl_tool(daemon, &args, ctx).await),
        "web_screenshot" => Ok(web_screenshot_tool::web_screenshot_tool(daemon, &args, ctx).await),
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

#[cfg(test)]
mod preflight_lifecycle_tests {
    use super::Daemon;
    use tokio::io::AsyncReadExt;

    async fn stop_pending_preflight(explicit_shutdown: bool) {
        let mut config = crate::config::DonsetchConfig::default();
        config.fetch.allow_private_egress = true;
        config.proxy.from_environment = false;
        crate::config::install(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::fs::write(
            crate::egress::proxy::config_path(),
            format!("http://{}\n", listener.local_addr().unwrap()),
        )
        .unwrap();
        let daemon = Daemon::new().await.unwrap();
        let (mut socket, _) =
            tokio::time::timeout(std::time::Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            assert!(head.len() < 16384);
            head.push(socket.read_u8().await.unwrap());
        }
        assert!(head.starts_with(b"CONNECT api.ipify.org:443 HTTP/1.1\r\n"));
        let abort = daemon
            .preflight_task
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .abort_handle();
        if explicit_shutdown {
            daemon.shutdown().await;
            assert!(daemon.preflight_task.lock().unwrap().is_none());
            assert!(
                abort.is_finished(),
                "shutdown must join the cancelled probe"
            );
        }
        drop(daemon);
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            socket.read(&mut [0u8; 1]),
        )
        .await
        .expect("owned CONNECT must close on daemon retirement");
        assert_eq!(read.unwrap(), 0);
        assert!(abort.is_finished());
    }

    #[tokio::test]
    async fn stealth_v3_preflight_shutdown_joins_pending_connect() {
        stop_pending_preflight(true).await;
    }

    #[tokio::test]
    async fn stealth_v3_preflight_drop_aborts_pending_connect() {
        stop_pending_preflight(false).await;
    }
}

#[cfg(test)]
mod prober_lifecycle_tests {
    use super::Daemon;
    use std::time::Duration;

    async fn wait_finished(abort: &tokio::task::AbortHandle) {
        for _ in 0..100 {
            if abort.is_finished() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the prober task was not cancelled");
    }

    async fn daemon_with_prober() -> (std::sync::Arc<Daemon>, tokio::task::AbortHandle) {
        let mut config = crate::config::DonsetchConfig::default();
        config.fetch.allow_private_egress = true;
        config.proxy.from_environment = false;
        config.browser.route_probes = true;
        crate::config::install(config).unwrap();
        let daemon = std::sync::Arc::new(Daemon::new().await.unwrap());
        daemon.start_prober();
        let abort = daemon
            .probe_task
            .lock()
            .unwrap()
            .as_ref()
            .expect("route_probes=true must own a prober task")
            .abort_handle();
        (daemon, abort)
    }

    #[tokio::test]
    async fn stealth_v3_prober_shutdown_joins_the_task() {
        let (daemon, abort) = daemon_with_prober().await;
        assert!(!abort.is_finished(), "the prober runs until cancelled");
        daemon.shutdown().await;
        assert!(
            daemon.probe_task.lock().unwrap().is_none(),
            "shutdown must take the owned prober"
        );
        wait_finished(&abort).await;
    }

    #[tokio::test]
    async fn stealth_v3_prober_drop_aborts_the_task() {
        let (daemon, abort) = daemon_with_prober().await;
        assert!(!abort.is_finished());
        drop(daemon);
        wait_finished(&abort).await;
    }
}

#[cfg(test)]
mod crawl_cancel_tests {
    use super::{CancelMap, Daemon, cancel_key, handle};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    /// A loopback site. `/slow/` and `/fast/` each link to eight pages,
    /// and a `/slow/` page answers 1.5 s after its request; `/lone/` links
    /// nowhere. Every request path is reported on the returned channel.
    async fn site() -> (String, mpsc::UnboundedReceiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (seen, requests) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") && head.len() < 16384 {
                        match socket.read_u8().await {
                            Ok(byte) => head.push(byte),
                            Err(_) => return,
                        }
                    }
                    let path = String::from_utf8_lossy(&head)
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    let _ = seen.send(path.clone());
                    let section = path.trim_start_matches('/').split('/').next().unwrap_or("");
                    let index = path.ends_with('/');
                    if section == "slow" && !index {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                    }
                    let links: String = if index && section != "lone" {
                        (1..=8)
                            .map(|i| {
                                format!("<li><a href=\"/{section}/p{i}\">Field notes {i}</a></li>")
                            })
                            .collect()
                    } else {
                        String::new()
                    };
                    let body = format!(
                        "<html><head><title>Notes {path}</title></head><body><article>\
                         <h1>Notes {path}</h1><p>{}</p><ul>{links}</ul></article></body></html>",
                        format!("An observation recorded at {path} for a reader. ").repeat(40)
                    );
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        (base, requests)
    }

    async fn daemon() -> Arc<Daemon> {
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = true;
        crate::config::install(config).unwrap();
        Arc::new(Daemon::new().await.unwrap())
    }

    fn crawl_call(id: i64, args: Value, progress_token: Option<&str>) -> String {
        let mut params = json!({ "name": "web_crawl", "arguments": args });
        if let Some(token) = progress_token {
            params["_meta"] = json!({ "progressToken": token });
        }
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params }).to_string()
    }

    fn progress_params(line: &str) -> Option<Value> {
        let v: Value = serde_json::from_str(line).ok()?;
        (v["method"] == "notifications/progress").then(|| v["params"].clone())
    }

    /// The call's state: `structuredContent`, or the leading `[meta]`
    /// line a text-only session folds it into.
    fn state(response: &Value) -> Value {
        let result = &response["result"];
        if result.get("structuredContent").is_some() {
            return result["structuredContent"].clone();
        }
        let meta = result["content"][0]["text"].as_str().unwrap_or_default();
        serde_json::from_str(meta.trim_start_matches("[meta] ").trim_end())
            .unwrap_or_else(|_| panic!("no state in {response}"))
    }

    fn drain(out: &mut mpsc::Receiver<String>) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(line) = out.try_recv() {
            lines.push(line);
        }
        lines
    }

    /// The token files in the process cache root's resume store.
    fn stored_tokens() -> Vec<String> {
        let dir = crate::paths::cache_dir().join("crawl-resumes");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.ends_with(".json"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// One uncancelled call through `handle`, with the writer lines its
    /// progress forwarder sent.
    async fn answered(daemon: &Arc<Daemon>, line: String) -> (Value, Vec<String>) {
        let cancels: CancelMap = Arc::default();
        let mode = crate::mcp::compat::ModeCell::new();
        let (writer, mut out) = mpsc::channel::<String>(256);
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            handle(daemon, &line, &cancels, &writer, &mode),
        )
        .await
        .expect("the call is answered when its crawl ends")
        .expect("an uncancelled call gets a response");
        (serde_json::from_str(&response).unwrap(), drain(&mut out))
    }

    // Drives `handle` the way the stdio transport does: one cancel
    // registry, one writer channel, and the cancel delivered by taking
    // the call's sender out of the registry and sending true. Installs the
    // process config and reads the process cache root's resume store:
    // depends on nextest's process-per-test.
    #[tokio::test]
    async fn a_cancelled_crawl_sends_no_progress_after_the_cancel_and_stores_no_token() {
        let (base, mut requests) = site().await;
        let daemon = daemon().await;
        let cancels: CancelMap = Arc::default();
        let (writer, mut out) = mpsc::channel::<String>(256);
        let line = crawl_call(
            7,
            json!({
                "url": format!("{base}/slow/"),
                "mode": "content",
                "respect_robots": false,
                "max_pages": 20,
                "deadline_s": 60,
            }),
            Some("pt-7"),
        );
        let call = tokio::spawn({
            let daemon = Arc::clone(&daemon);
            let cancels = Arc::clone(&cancels);
            async move {
                let mode = crate::mcp::compat::ModeCell::new();
                handle(&daemon, &line, &cancels, &writer, &mode).await
            }
        });
        let first = tokio::time::timeout(Duration::from_secs(20), out.recv())
            .await
            .expect("a first progress beat")
            .expect("the writer is open");
        assert!(progress_params(&first).is_some(), "{first}");
        // Cancel while a page fetch is in flight: the fixture holds every
        // /slow/pN answer for 1.5 s, so no page completes before the cancel.
        tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(path) = requests.recv().await {
                if path.starts_with("/slow/p") {
                    return;
                }
            }
        })
        .await
        .expect("the crawl requests a /slow/ page");
        let before = drain(&mut out);
        assert!(before.is_empty(), "a page completed early: {before:?}");
        let sender = cancels
            .lock()
            .unwrap()
            .remove(&cancel_key(&json!(7)).unwrap())
            .expect("the running call is registered");
        let _ = sender.send(true);
        drop(sender);

        let returned = tokio::time::timeout(Duration::from_secs(15), call).await;
        let after = drain(&mut out);
        let beats: Vec<&String> = after
            .iter()
            .filter(|l| progress_params(l).is_some())
            .collect();
        assert!(beats.is_empty(), "progress after the cancel: {beats:?}");
        assert!(after.is_empty(), "lines after the cancel: {after:?}");
        let returned = returned
            .expect("the cancelled call returns once its crawl stops")
            .unwrap();
        assert_eq!(returned, None, "a cancelled request gets no response");
        assert_eq!(
            stored_tokens(),
            Vec::<String>::new(),
            "a cancelled crawl stored a resume token no response carries"
        );
    }

    // Negative: an uncancelled crawl keeps its progress and its response,
    // and a budget stop still stores the token its response carries.
    // Installs the process config and reads the process cache root's
    // resume store: depends on nextest's process-per-test.
    #[tokio::test]
    async fn an_uncancelled_crawl_keeps_its_progress_and_its_resume_token() {
        let (base, _requests) = site().await;
        let daemon = daemon().await;

        let (response, lines) = answered(
            &daemon,
            crawl_call(
                8,
                json!({
                    "url": format!("{base}/lone/"),
                    "mode": "content",
                    "respect_robots": false,
                    "deadline_s": 30,
                }),
                Some("pt-8"),
            ),
        )
        .await;
        assert_eq!(response["id"], 8);
        let sc = &state(&response);
        assert_eq!(sc["ok"], true, "{response}");
        assert_eq!(sc["stop"], "FrontierEmpty", "{response}");
        let beats: Vec<Value> = lines.iter().filter_map(|l| progress_params(l)).collect();
        assert_eq!(
            beats,
            vec![json!({ "progressToken": "pt-8", "progress": 1, "message": "1 pages, 0 queued" })],
            "{lines:?}"
        );

        let (response, _) = answered(
            &daemon,
            crawl_call(
                9,
                json!({
                    "url": format!("{base}/fast/"),
                    "mode": "content",
                    "respect_robots": false,
                    "max_pages": 2,
                    "deadline_s": 30,
                }),
                None,
            ),
        )
        .await;
        let sc = &state(&response);
        assert_eq!(sc["stop"], "MaxPages", "{response}");
        let token = sc["resume"].as_str().unwrap_or_else(|| {
            panic!("a budget stop with queued pages carries a token: {response}")
        });
        assert_eq!(stored_tokens(), vec![format!("{token}.json")]);
    }
}
