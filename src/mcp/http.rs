//! HTTP transport for the MCP server.
//!
//! POST /mcp speaks JSON-RPC 2.0 exactly like stdio: every request is
//! dispatched through the same daemon handler, so initialize
//! negotiation, tools/list and full tool calls behave identically on
//! both transports. GET /health is a liveness probe for orchestrators
//! and load balancers.
//!
//! GET /mcp opens the server-initiated SSE stream that streamable-HTTP
//! clients expect after initialize (OpenCode among them fails the whole
//! connection on a bare 405 here). DonSeTch never pushes unsolicited
//! messages : no server tools, no sampling requests : so the stream
//! carries keep-alive comments only. It self-terminates just inside
//! SESSION_TTL: graceful shutdown is never blocked by an idle stream,
//! clients simply reconnect per SSE convention, and each reconnect
//! refreshes the session's idle timer. DELETE /mcp terminates a session
//! per the spec.
//!
//! Sessions: the server assigns an `Mcp-Session-Id` response header on
//! `initialize`. Clients that echo it back get a dedicated cancellation
//! registry, so a `notifications/cancelled` posted while a tool call is
//! in flight reaches it (mirroring stdio's shared registry). Clients
//! that ignore sessions : curl, simple integrators : share one default
//! registry; cancellation still works, they just share a request-id
//! namespace with other session-less clients. Unknown or expired
//! session ids get a 404, per the MCP streamable-HTTP convention.
//!
//! Hardening: optional bearer auth via `DONSETCH_HTTP_TOKEN`, a
//! configurable per-request timeout via `DONSETCH_HTTP_TIMEOUT_SECS`
//! (default 300), and JSON-RPC error envelopes for malformed bodies.
//! The server drains in-flight requests on SIGTERM/SIGINT before
//! shutting the daemon down.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::Stream;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

use crate::mcp::server::{CancelMap, Daemon, handle};

/// Idle sessions are dropped after this long, so cancelled-tool
/// registries cannot leak forever for clients that vanish mid-call.
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
/// Upper bound on tracked sessions; the oldest are evicted beyond it.
const MAX_SESSIONS: usize = 1024;
/// Default per-request timeout when DONSETCH_HTTP_TIMEOUT_SECS is unset.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// The GET /mcp SSE stream closes itself after this long (just inside
/// SESSION_TTL): an unbounded idle stream would hold graceful shutdown
/// open forever, and a closed stream is a routine reconnect for SSE
/// clients : one that usefully refreshes the session idle timer.
const SSE_MAX_LIFETIME: Duration = Duration::from_secs(25 * 60);

struct Session {
    cancels: CancelMap,
    last_seen: Instant,
}

/// Session-id → cancellation-registry table with idle-TTL GC and a
/// bounded size. Split out of the server state so the registry logic
/// can be tested without constructing a Daemon.
struct SessionTable {
    sessions: Mutex<HashMap<String, Session>>,
}

/// Shared HTTP server state.
#[derive(Clone)]
struct HttpState {
    daemon: Arc<Daemon>,
    sessions: Arc<SessionTable>,
    /// When set, requests must carry `Authorization: Bearer <token>`.
    auth_token: Option<String>,
    timeout: Duration,
}

/// Run the HTTP MCP server until SIGTERM/SIGINT.
pub async fn run(host: String, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let daemon = Arc::new(Daemon::new().await.map_err(|e| e.to_string())?);
    let auth_token = std::env::var("DONSETCH_HTTP_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    let auth_enabled = auth_token.is_some();
    let timeout = Duration::from_secs(
        std::env::var("DONSETCH_HTTP_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS),
    );

    // CORS is off by default: MCP clients are processes, not browsers,
    // and a permissive layer would let any webpage open in a local
    // browser POST to a localhost instance and read the responses.
    // Browser-based clients can opt in with DONSETCH_HTTP_CORS.
    let cors_enabled = matches!(
        std::env::var("DONSETCH_HTTP_CORS").as_deref(),
        Ok("1" | "true" | "on")
    );
    validate_http_config(cors_enabled, auth_enabled)?;

    let state = HttpState {
        daemon: Arc::clone(&daemon),
        sessions: Arc::new(SessionTable {
            sessions: Mutex::new(HashMap::new()),
        }),
        auth_token,
        timeout,
    };

    let app = Router::new()
        .route("/health", get(health_handler))
        .route(
            "/mcp",
            post(mcp_handler).get(sse_handler).delete(delete_handler),
        )
        .layer(TraceLayer::new_for_http());
    let app = if cors_enabled {
        app.layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
    } else {
        app
    };
    let app = app.with_state(state);

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    if auth_enabled {
        eprintln!("[mcp] HTTP server listening on http://{host}:{port} (bearer auth enabled)");
    } else {
        eprintln!("[mcp] HTTP server listening on http://{host}:{port}");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Drain done; stop the daemon so no orphan browsers survive.
    eprintln!("[mcp] draining complete, shutting daemon down");
    daemon.shutdown().await;
    Ok(())
}

/// Resolve SIGTERM/SIGINT into a future that completes on either.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    eprintln!("[mcp] shutdown signal received; draining in-flight requests");
}

/// Liveness probe for orchestrators and load balancers.
async fn health_handler() -> impl IntoResponse {
    Json(json!({ "status": "ok", "transport": "http" }))
}

/// Refuse the classic "localhost server + permissive CORS = drive-by"
/// footgun: `DONSETCH_HTTP_CORS` and `DONSETCH_HTTP_TOKEN` are
/// independently optional, so nothing stopped a user from enabling
/// permissive CORS (any origin) while leaving auth off. With that
/// combination, any webpage open in the same local browser could
/// POST arbitrary MCP tool calls — fetch/crawl/search, including the
/// `actions` browser-automation surface — to this server with no
/// authentication. Fail closed instead of just warning: a startup
/// error is impossible to miss, unlike an eprintln! in a background
/// daemon's log.
fn validate_http_config(cors_enabled: bool, auth_enabled: bool) -> Result<(), String> {
    if cors_enabled && !auth_enabled {
        return Err(
            "DONSETCH_HTTP_CORS is enabled without DONSETCH_HTTP_TOKEN: any \
             webpage open in a local browser could drive this MCP server \
             with no authentication. Set DONSETCH_HTTP_TOKEN to a random \
             secret before enabling CORS, or leave DONSETCH_HTTP_CORS unset \
             if you don't need browser-based clients."
                .into(),
        );
    }
    Ok(())
}

/// Bearer-auth check shared by every /mcp method (health stays open).
fn authorized(state: &HttpState, headers: &HeaderMap) -> bool {
    token_ok(state.auth_token.as_deref(), headers)
}

/// Pure form of the bearer check so tests don't need server state.
fn token_ok(expected: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(expected) = expected else {
        return true; // no token configured: auth is off
    };
    match headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        Some(presented) => constant_time_eq(presented.as_bytes(), expected.as_bytes()),
        None => false,
    }
}

/// Constant-time equality (XOR fold, no early exit on the bytes). The
/// length check up front is deliberate: token length is not a secret
/// worth protecting, and folding equal-length buffers without a
/// byte-wise early exit removes the timing oracle on the token itself.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A stream that stays silent for `max` and then ends. While silent, the
/// SSE body emits keep-alive comments; ending triggers the client's
/// routine reconnect.
fn silent_stream(max: Duration) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold((), move |()| async move {
        tokio::time::sleep(max).await;
        None
    })
}

/// Server-initiated stream half of the streamable-HTTP transport.
///
/// DonSeTch has nothing server-initiated to say, so this exists purely
/// so clients that open the stream after initialize (per spec they MAY)
/// get a live SSE connection instead of a 405 that some clients :
/// OpenCode notably : treat as fatal. See the module docs for the
/// lifetime/reconnect reasoning.
async fn sse_handler(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Unknown or expired session ids get a 404, per the same convention
    // as POST. No header (curl, plain probes) opens an anonymous stream.
    if let Some(sid) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok())
        && !sid.is_empty()
        && state.sessions.registry(sid).is_none()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    Sse::new(silent_stream(SSE_MAX_LIFETIME))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

/// Session termination: drop the cancellation registry so a vanished
/// client's entry cannot linger until the idle TTL collects it.
async fn delete_handler(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let status = match headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        Some(sid) => {
            let removed = state.sessions.remove(sid);
            if removed {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::NOT_FOUND
            }
        }
        None => StatusCode::NO_CONTENT, // nothing to terminate
    };
    status.into_response()
}

fn rpc_error(status: StatusCode, code: i32, message: &str) -> Response {
    (
        status,
        Json(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": code, "message": message }
        })),
    )
        .into_response()
}

/// Generate an opaque, unguessable session id: 16 base62 chars (~95
/// bits) from the same entropy pool as handle IDs. Sequential or
/// timestamp-derived ids would let a client guess other sessions' ids;
/// the id is not itself an auth token, but unpredictability costs
/// nothing and removes the question.
fn new_session_id() -> String {
    crate::handles::random_base62(16)
}

impl SessionTable {
    /// Registry for an existing session id, or None if unknown.
    fn registry(&self, id: &str) -> Option<CancelMap> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.gc_locked(&mut sessions);
        let s = sessions.get_mut(id)?;
        s.last_seen = Instant::now();
        Some(Arc::clone(&s.cancels))
    }

    /// Registry for `id`, creating it on first use. The empty id is
    /// the shared default registry for session-less clients; it is
    /// never a candidate for MAX_SESSIONS eviction.
    fn registry_or_create(&self, id: &str) -> CancelMap {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.gc_locked(&mut sessions);
        if let Some(s) = sessions.get_mut(id) {
            s.last_seen = Instant::now();
            return Arc::clone(&s.cancels);
        }
        if sessions.len() >= MAX_SESSIONS && !id.is_empty() {
            // Evict the oldest session to bound memory.
            if let Some(oldest) = sessions
                .iter()
                .filter(|(k, _)| !k.is_empty())
                .min_by_key(|(_, s)| s.last_seen)
                .map(|(k, _)| k.clone())
            {
                sessions.remove(&oldest);
            }
        }
        let cancels: CancelMap = Arc::new(Mutex::new(HashMap::new()));
        sessions.insert(
            id.to_string(),
            Session {
                cancels: Arc::clone(&cancels),
                last_seen: Instant::now(),
            },
        );
        cancels
    }

    /// Remove a session; true if it existed.
    fn remove(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .is_some()
    }

    /// Drop sessions idle beyond TTL. Called with the lock held.
    fn gc_locked(&self, sessions: &mut HashMap<String, Session>) {
        self.gc_locked_with(sessions, SESSION_TTL);
    }

    /// TTL-parameterized form so tests can sweep with a tiny window
    /// (production TTLs are longer than a fresh host's uptime, which
    /// makes backdating an `Instant` by the real TTL unrepresentable).
    fn gc_locked_with(&self, sessions: &mut HashMap<String, Session>, ttl: Duration) {
        sessions.retain(|_, s| s.last_seen.elapsed() < ttl);
    }
}

impl HttpState {
    /// Registry for session-less clients; created on first use.
    fn default_cancels(&self) -> CancelMap {
        self.sessions.registry_or_create("")
    }
}

/// JSON-RPC request handler.
///
/// Accepts MCP JSON-RPC requests via POST and returns responses. Every
/// request is routed through the same daemon handler as stdio, so
/// initialize, ping, tools/list and tools/call (fetch, search, crawl)
/// all work identically on both transports. See the module docs for
/// the session and cancellation model.
async fn mcp_handler(State(state): State<HttpState>, headers: HeaderMap, body: Bytes) -> Response {
    // Auth gate: applies to /mcp only; /health stays open for probes.
    if !authorized(&state, &headers) {
        return rpc_error(StatusCode::UNAUTHORIZED, -32000, "unauthorized");
    }

    // Parse before dispatch: malformed bodies get a JSON-RPC -32700
    // envelope instead of a bare transport-layer 400.
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return rpc_error(StatusCode::BAD_REQUEST, -32700, "Parse error"),
    };
    let line = req.to_string();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let is_notification = req.get("id").is_none();
    let sid_header = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Cancellation notifications are handled inline : they must reach
    // the running tool NOW, not after a full dispatch round-trip.
    // Mirrors the stdio transport's inline handling.
    if is_notification && method == "notifications/cancelled" {
        let cancels = resolve_cancels(&state, sid_header.as_deref());
        if let Some(rid) = req.pointer("/params/requestId").and_then(Value::as_i64)
            && let Some(sender) = cancels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&rid)
        {
            let _ = sender.send(true);
        }
        // 202 Accepted, not 204: the streamable-HTTP spec mandates 202
        // for notification-only POSTs.
        return StatusCode::ACCEPTED.into_response();
    }

    // Session resolution: header wins; unknown ids are expired per
    // streamable-HTTP convention. initialize without a header mints a
    // new session echoed back on the response.
    let mut new_session: Option<String> = None;
    let cancels = match sid_header.as_deref() {
        Some(sid) if !sid.is_empty() => match state.sessions.registry(sid) {
            Some(c) => c,
            None => return rpc_error(StatusCode::NOT_FOUND, -32001, "session expired or unknown"),
        },
        _ => {
            if method == "initialize" {
                let sid = new_session_id();
                new_session = Some(sid.clone());
                state.sessions.registry_or_create(&sid)
            } else {
                state.default_cancels()
            }
        }
    };

    // Writer sink for in-flight progress notifications. Each request
    // gets a fresh channel; nothing else consumes it.
    let (progress_tx, mut progress_rx) = mpsc::channel::<String>(256);
    let request_id = req.get("id").and_then(Value::as_i64);

    let outcome = tokio::time::timeout(
        state.timeout,
        handle(&state.daemon, &line, &cancels, &progress_tx),
    )
    .await;

    let mut response = match outcome {
        Err(_) => {
            // Timed out: drop any registry entry so the id can be
            // reused without leaking a dead watch sender.
            if let Some(rid) = request_id
                && let Some(sender) = cancels
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&rid)
            {
                drop(sender);
            }
            rpc_error(
                StatusCode::GATEWAY_TIMEOUT,
                -32603,
                &format!("request timed out after {}s", state.timeout.as_secs()),
            )
        }
        Ok(Some(resp_line)) => {
            let resp: Value = serde_json::from_str(&resp_line).unwrap_or_else(|_| {
                json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": { "code": -32603, "message": "internal error" }
                })
            });
            Json(resp).into_response()
        }
        Ok(None) => {
            // Notification: no body, 202 Accepted per the streamable-HTTP
            // spec (204 would also be "no content", but the spec names
            // 202 for notification-only POSTs). The sender MUST be
            // dropped before draining : recv() only returns None once
            // every sender is gone, and progress_tx is still in scope
            // here, so draining first would wait forever. (This exact
            // deadlock hung the notifications/initialized POST and with
            // it every streamable-HTTP client's connect; it also would
            // have hung POSTs for cancelled tools/call requests.)
            drop(progress_tx);
            while progress_rx.recv().await.is_some() {}
            StatusCode::ACCEPTED.into_response()
        }
    };

    if let Some(sid) = new_session
        && let Ok(hv) = sid.parse()
    {
        response.headers_mut().insert("mcp-session-id", hv);
    }
    response
}

/// Resolve the cancellation registry for a cancellation notification.
/// Unknown session ids fall back to the shared default registry: a
/// notification cannot carry a JSON-RPC error, so it is absorbed.
fn resolve_cancels(state: &HttpState, sid: Option<&str>) -> CancelMap {
    match sid {
        Some(s) if !s.is_empty() => state
            .sessions
            .registry(s)
            .unwrap_or_else(|| state.default_cancels()),
        _ => state.default_cancels(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_bearer(token: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(t) = token {
            h.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {t}")).unwrap(),
            );
        }
        h
    }

    #[test]
    fn cors_without_auth_is_refused() {
        assert!(validate_http_config(true, false).is_err());
    }

    #[test]
    fn cors_with_auth_is_allowed() {
        assert!(validate_http_config(true, true).is_ok());
    }

    #[test]
    fn no_cors_is_always_allowed_regardless_of_auth() {
        assert!(validate_http_config(false, false).is_ok());
        assert!(validate_http_config(false, true).is_ok());
    }

    #[test]
    fn token_ok_no_token_configured_allows_all() {
        assert!(token_ok(None, &HeaderMap::new()));
        assert!(token_ok(None, &headers_with_bearer(Some("anything"))));
        // run() filters empty env tokens, but the pure function still
        // treats a configured (even empty) token as auth-on.
        assert!(!token_ok(Some(""), &HeaderMap::new()));
    }

    #[test]
    fn token_ok_requires_bearer_when_configured() {
        let expected = Some("s3cret-token");
        assert!(!token_ok(expected, &HeaderMap::new()));
        assert!(!token_ok(expected, &headers_with_bearer(Some("wrong"))));
        assert!(token_ok(
            expected,
            &headers_with_bearer(Some("s3cret-token"))
        ));
        // Non-Bearer schemes don't match.
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str("Basic czM6Y3JldA==").unwrap(),
        );
        assert!(!token_ok(expected, &h));
        // Length mismatch is rejected by the length check.
        assert!(!token_ok(
            expected,
            &headers_with_bearer(Some("s3cret-token "))
        ));
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
    }

    #[test]
    fn session_ids_are_base62_and_unique() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| new_session_id()).collect();
        assert_eq!(ids.len(), 1000, "session ids must not repeat");
        for id in &ids {
            assert_eq!(id.len(), 16);
            assert!(id.bytes().all(|b| b.is_ascii_alphanumeric()));
        }
    }

    fn backdate(table: &SessionTable, id: &str, by: Duration) -> bool {
        // False when the host booted more recently than `by` (Instant
        // arithmetic would underflow); callers skip such assertions.
        let Some(old) = Instant::now().checked_sub(by) else {
            return false;
        };
        table
            .sessions
            .lock()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .last_seen = old;
        true
    }

    fn table_with(ids: &[&str]) -> SessionTable {
        let table = SessionTable {
            sessions: Mutex::new(HashMap::new()),
        };
        for id in ids {
            table.registry_or_create(id);
        }
        table
    }

    #[test]
    fn registry_unknown_is_none_known_roundtrips() {
        let table = table_with(&["sess-1"]);
        assert!(table.registry("sess-1").is_some());
        assert!(table.registry("nope").is_none());
        // Same registry comes back on the second lookup.
        let a = table.registry("sess-1").unwrap();
        let b = table.registry("sess-1").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn remove_reports_existence() {
        let table = table_with(&["sess-1"]);
        assert!(table.remove("sess-1"));
        assert!(!table.remove("sess-1"));
        assert!(table.registry("sess-1").is_none());
    }

    #[test]
    fn gc_drops_idle_sessions() {
        let table = table_with(&["old", "fresh"]);
        if !backdate(&table, "old", Duration::from_secs(2)) {
            return; // host booted <2s ago; underflow, skip
        }
        let mut sessions = table
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        table.gc_locked_with(&mut sessions, Duration::from_secs(1));
        drop(sessions);
        assert!(table.registry("old").is_none(), "idle session must be GC'd");
        assert!(table.registry("fresh").is_some());
    }

    #[test]
    fn eviction_bounds_table_and_spares_default() {
        let table = table_with(&[""]);
        // Fill with one more than MAX_SESSIONS named sessions, oldest
        // first so eviction order is deterministic.
        for i in 0..=MAX_SESSIONS {
            table.registry_or_create(&format!("s{i:04}"));
        }
        // The oldest named entries were evicted as the table filled.
        let live = table
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            live.len() <= MAX_SESSIONS,
            "table must stay bounded, has {}",
            live.len()
        );
        assert!(live.contains_key(""), "default registry is never evicted");
        assert!(!live.contains_key("s0000"), "oldest named entry evicted");
        assert!(live.contains_key(&format!("s{MAX_SESSIONS:04}")));
    }
}
