//! Minimal CDP (Chrome DevTools Protocol) client.
//!
//! Browser-level + page-session JSON-RPC over the DevTools ws
//! endpoint. No Runtime/Console/Debugger domains : DOM and Page
//! only. Message framing per RFC 6455 via tokio-tungstenite.
//!
//! Upstream (master) made `Cdp` cloneable by wrapping every field
//! in Arc; this branch additionally exposes `call_with_timeout`
//! so the ghost hot path can bound each response wait. On Debian 12
//! chromium 151, session-scoped CDP responses queue behind a
//! settling navigation and can lag the URL advance by tens of
//! seconds : unbounded waits there turn a recoverable stall into a
//! failed fetch (see ghost::navigate docs).

use crate::error::FetchError;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Pending = std::sync::Mutex<HashMap<u64, oneshot::Sender<Value>>>;

const GUARD_INFLIGHT: usize = 16;
const GUARD_QUEUE: usize = 256;

struct PausedRequest {
    id: String,
    url: String,
}

type GuardQueue = std::sync::Mutex<Option<(String, mpsc::Sender<PausedRequest>)>>;

// No await happens under this short map lock. Synchronous removal lets
// cancellation retire a request immediately, including during a send.
struct PendingCall<'a> {
    pending: &'a Pending,
    id: u64,
    cancelled: &'a AtomicBool,
    finished: bool,
}

impl Drop for PendingCall<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.cancelled.store(true, Ordering::Release);
        }
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

/// Largest DevTools message the client accepts, and the frame limit
/// with it: a full-page screenshot or a large document's outerHTML
/// is tens of MB, well over tungstenite's 16 MiB default frame.
pub(crate) const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

pub struct Cdp {
    write: Arc<Mutex<futures_util::stream::SplitSink<Ws, Message>>>,
    pending: Arc<Pending>,
    /// Event stream (targetInfoChanged = title/url
    /// changes : challenge progression without Runtime).
    /// Consumed by the daemon's smarter wait loop.
    #[allow(dead_code)]
    events: broadcast::Sender<Value>,
    next_id: Arc<AtomicU64>,
    /// Set once the demux reader has ended: the link is gone even
    /// if the browser process is not.
    dead: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    guard_queue: Arc<GuardQueue>,
    document: Arc<std::sync::Mutex<super::document::Tracker>>,
    failure: Arc<std::sync::Mutex<Option<&'static str>>>,
}

impl Clone for Cdp {
    fn clone(&self) -> Self {
        Self {
            write: Arc::clone(&self.write),
            pending: Arc::clone(&self.pending),
            events: self.events.clone(),
            next_id: Arc::clone(&self.next_id),
            dead: Arc::clone(&self.dead),
            cancelled: Arc::clone(&self.cancelled),
            guard_queue: Arc::clone(&self.guard_queue),
            document: Arc::clone(&self.document),
            failure: Arc::clone(&self.failure),
        }
    }
}

impl Cdp {
    /// Connect to a browser-level ws endpoint and spawn the
    /// demux reader task.
    pub async fn connect(ws_url: &str) -> Result<Self, FetchError> {
        Self::connect_with_limit(ws_url, MAX_MESSAGE_BYTES).await
    }

    /// `connect` with the message and frame limit as a parameter.
    pub(crate) async fn connect_with_limit(
        ws_url: &str,
        max_bytes: usize,
    ) -> Result<Self, FetchError> {
        // The only unguarded network primitive in the ghost stack :
        // a browser that accepts TCP but stalls the WS handshake
        // would hang the tool call forever.
        let mut ws_config = WebSocketConfig::default();
        ws_config.max_message_size = Some(max_bytes);
        ws_config.max_frame_size = Some(max_bytes);
        let (ws, _) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_async_with_config(ws_url, Some(ws_config), false),
        )
        .await
        .map_err(|_| FetchError::ghost("cdp connect: ws handshake timeout"))?
        .map_err(|e| FetchError::ghost(format!("cdp connect: {e}")))?;
        let (write, mut read) = ws.split();
        let pending = Arc::new(Pending::new(HashMap::new()));
        let pending_task = Arc::clone(&pending);
        let (events_tx, _) = broadcast::channel(256);
        let events_task = events_tx.clone();
        let dead = Arc::new(AtomicBool::new(false));
        let dead_task = Arc::clone(&dead);
        let guard_queue = Arc::new(GuardQueue::new(None));
        let guard_task = Arc::clone(&guard_queue);
        let document = Arc::new(std::sync::Mutex::new(super::document::Tracker::default()));
        let document_task = Arc::clone(&document);
        let failure = Arc::new(std::sync::Mutex::new(None));
        let failure_task = Arc::clone(&failure);
        tokio::spawn(async move {
            let cause = loop {
                let msg = match read.next().await {
                    Some(Ok(msg)) => msg,
                    // Closed by the peer, or a reply the client will
                    // not frame: either way nothing more arrives here.
                    Some(Err(_)) => break "websocket framing/read error",
                    None => break "websocket EOF",
                };
                if matches!(msg, Message::Close(_)) {
                    break "peer closed websocket";
                }
                let Message::Text(text) = msg else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    let mut map = pending_task.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(tx) = map.remove(&id) {
                        let _ = tx.send(v);
                    }
                } else {
                    if matches!(
                        v.get("method").and_then(Value::as_str),
                        Some(
                            "Page.frameNavigated"
                                | "Page.navigatedWithinDocument"
                                | "Network.responseReceived"
                        )
                    ) {
                        document_task
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .observe(&v);
                    }
                    // Paused requests cannot share a lossy broadcast ring
                    // with Network/DOM chatter: losing one stalls Chrome.
                    if v.get("method").and_then(Value::as_str) == Some("Fetch.requestPaused") {
                        let queue = guard_task.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some((session, tx)) = queue.as_ref()
                            && v.get("sessionId").and_then(Value::as_str) == Some(session)
                        {
                            let id = v.pointer("/params/requestId").and_then(Value::as_str);
                            let url = v.pointer("/params/request/url").and_then(Value::as_str);
                            // Keep only bounded routing data, never post bodies.
                            let Some(id) = id.filter(|id| !id.is_empty() && id.len() <= 1024)
                            else {
                                break "malformed paused request ID";
                            };
                            let Some(url) = url.filter(|url| url.len() <= 64 * 1024) else {
                                break "malformed paused request URL";
                            };
                            if tx
                                .try_send(PausedRequest {
                                    id: id.into(),
                                    url: url.into(),
                                })
                                .is_err()
                            {
                                // Overflow/closed owner fails this generation.
                                // Never forget a pause and return a usable tab.
                                break "request guard queue overflow or owner closed";
                            }
                        }
                        continue;
                    }
                    let _ = events_task.send(v);
                }
            };
            // The link is gone while the browser process may not be.
            // Fail every waiter now (dropping a sender ends its
            // receiver) instead of at each call's own timeout, and
            // let the holder see it before it serves another job.
            failure_task
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get_or_insert(cause);
            dead_task.store(true, Ordering::Release);
            pending_task
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
        });
        Ok(Self {
            write: Arc::new(Mutex::new(write)),
            pending,
            events: events_tx,
            next_id: Arc::new(AtomicU64::new(1)),
            dead,
            cancelled: Arc::new(AtomicBool::new(false)),
            guard_queue,
            document,
            failure,
        })
    }

    /// True once the DevTools link has ended (the browser exited,
    /// the socket closed, or a reply broke the framing limits). A
    /// dead link answers nothing; the holder relaunches.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    pub fn failure(&self) -> Option<&'static str> {
        *self.failure.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn was_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(super) fn track_document(&self, session: String) {
        *self.document.lock().unwrap_or_else(|p| p.into_inner()) =
            super::document::Tracker::new(session);
    }

    pub(super) fn document(&self) -> super::document::Document {
        self.document
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .current
            .clone()
    }

    pub(super) fn navigation_committed(
        &self,
        before: &super::document::Document,
        expected_loader: Option<&str>,
        requested: &str,
    ) -> Option<super::document::Document> {
        let tracker = self.document.lock().unwrap_or_else(|p| p.into_inner());
        tracker
            .navigation_committed(before, expected_loader, requested)
            .then(|| tracker.current.clone())
    }

    /// Call a method. `session` scopes it to an attached
    /// target (page); None = browser-level.
    pub async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, FetchError> {
        self.call_with_timeout(session, method, params, 20).await
    }

    /// Call with an explicit response timeout (seconds). The ghost
    /// hot path uses short bounds so one queued/deferred response
    /// costs a single poll iteration instead of the whole render
    /// window; detached warmup traffic uses longer bounds so late
    /// responses still land cleanly and free their pending slot.
    pub async fn call_with_timeout(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout_secs: u64,
    ) -> Result<Value, FetchError> {
        if self.is_dead() {
            return Err(FetchError::ghost(format!(
                "cdp link closed: {method} ({})",
                self.failure().unwrap_or("unavailable")
            )));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            if self.is_dead() {
                return Err(FetchError::ghost(format!("cdp link closed: {method}")));
            }
            pending.insert(id, tx);
        }
        let mut pending_call = PendingCall {
            pending: &self.pending,
            id,
            cancelled: &self.cancelled,
            finished: false,
        };
        let started = std::time::Instant::now();
        let resp = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
            {
                let mut w = self.write.lock().await;
                w.send(Message::Text(msg.to_string().into()))
                    .await
                    .map_err(|e| FetchError::ghost(format!("cdp send: {e}")))?;
            }
            rx.await
                .map_err(|_| FetchError::ghost(format!("cdp dropped: {method}")))
        })
        .await;
        if crate::config::cfg().debug.ghost && started.elapsed().as_millis() >= 1000 {
            eprintln!(
                "[cdp] {method} response wait {}ms",
                started.elapsed().as_millis()
            );
        }
        // A locally handled response timeout is different from the caller
        // cancelling an operation halfway through send/wait. The latter
        // retires the browser; the former keeps the bounded navigation retry.
        pending_call.finished = true;
        let resp = resp.map_err(|_| FetchError::ghost(format!("cdp timeout: {method}")))??;
        if let Some(err) = resp.get("error") {
            return Err(FetchError::ghost(format!(
                "cdp {method}: {}",
                err.get("message").and_then(Value::as_str).unwrap_or("?")
            )));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Subscribe to CDP events (targetInfoChanged, loadEvent).
    #[allow(dead_code)] // daemon wait loop (MCP milestone)
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Spawn a cancellable request-guard task for one page session.
    ///
    /// Subscribes to CDP events, filters `Fetch.requestPaused` events
    /// for the given `session`, reads `params.requestId` and
    /// `params.request.url`, calls `fetch::guards::ensure_url_safe`
    /// on every paused URL, then issues `Fetch.continueRequest` for
    /// safe URLs or `Fetch.failRequest` with `errorReason`
    /// `BlockedByClient` for unsafe, non-http, or DNS-failed URLs.
    ///
    /// The demux feeds a separate bounded queue; at most 16 validation
    /// calls run together. The owning task owns and cancels its children.
    ///
    /// # DNS rebinding residual limitation
    ///
    /// This is a point-in-time check. DNS can change between
    /// validation and the actual network stack's resolution (DNS
    /// rebinding / TOCTOU). Without full DNS pinning (reusing the
    /// validated IPs for the connect), there is a residual window.
    /// The explicit preflight (`ensure_url_safe` before `Page.navigate`)
    /// and redirect/post-action checks are retained as defence-in-depth
    /// alongside this in-browser Fetch guard.
    pub fn spawn_fetch_guard(&self, session: String) -> tokio::task::JoinHandle<()> {
        let cdp = self.clone();
        let (tx, mut requests) = mpsc::channel::<PausedRequest>(GUARD_QUEUE);
        *self.guard_queue.lock().unwrap_or_else(|p| p.into_inner()) = Some((session.clone(), tx));
        tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    result = tasks.join_next(), if !tasks.is_empty() => {
                        if !matches!(result, Some(Ok(Ok(())))) {
                            cdp.failure.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert("request guard disposition failed");
                            cdp.dead.store(true, Ordering::Release);
                            cdp.pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
                            break;
                        }
                    }
                    request = requests.recv(), if tasks.len() < GUARD_INFLIGHT => {
                        let Some(PausedRequest { id: request_id, url }) = request else { break };
                        let cdp2 = cdp.clone();
                        let session2 = session.clone();
                        tasks.spawn(async move {
                    // Fail-closed on DNS failure / non-http / private IP.
                    // file: and data: URLs are safe (no network
                    // request) and needed for the selftest.
                    let safe = if url.is_empty() {
                        false
                    } else if url.starts_with("file:") || url.starts_with("data:") {
                        true
                    } else {
                        crate::fetch::guards::ensure_url_safe(&url).await.is_ok()
                    };
                    if safe {
                        cdp2
                            .call(
                                Some(&session2),
                                "Fetch.continueRequest",
                                json!({ "requestId": request_id }),
                            )
                            .await?;
                    } else {
                        cdp2
                            .call(
                                Some(&session2),
                                "Fetch.failRequest",
                                json!({
                                    "requestId": request_id,
                                    "errorReason": "BlockedByClient"
                                }),
                            )
                            .await?;
                    }
                            Ok::<_, FetchError>(())
                        });
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    // A DevTools endpoint stand-in: accepts one websocket and runs
    // `serve` on it.
    async fn endpoint<F, Fut>(serve: F) -> String
    where
        F: FnOnce(WebSocketStream<TcpStream>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = accept_async(stream).await.unwrap();
            serve(ws).await;
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn stealth_v3_fetch_guard_bounds_inflight_and_owns_children() {
        let (sent, received) = oneshot::channel();
        let (start, started) = oneshot::channel();
        let url = endpoint(|mut ws| async move {
            started.await.unwrap();
            for id in 0..100 {
                ws.send(Message::Text(
                    json!({
                        "sessionId": "owned", "method": "Fetch.requestPaused",
                        "params": {"requestId": format!("req-{id}"),
                            "request": {"url": "data:text/plain,owned"}}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            }
            let mut dispatched = 0;
            while let Ok(Some(Ok(_))) =
                tokio::time::timeout(Duration::from_millis(250), ws.next()).await
            {
                dispatched += 1;
            }
            sent.send(dispatched).unwrap();
            // Do not answer: these are real pending calls when the owner
            // cancels, not completed tasks that happen to disappear.
            let _ = ws.next().await;
        })
        .await;
        let cdp = Cdp::connect(&url).await.unwrap();
        let guard = cdp.spawn_fetch_guard("owned".into());
        start.send(()).unwrap();
        let dispatched = tokio::time::timeout(Duration::from_secs(3), received)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            dispatched, 16,
            "guard must dispatch a bounded batch while replies are held"
        );
        assert_eq!(cdp.pending.lock().unwrap().len(), 16);
        guard.abort();
        assert!(guard.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !cdp.pending.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("guard left detached request handlers alive");
    }

    #[tokio::test]
    async fn stealth_v3_fetch_guard_keeps_pauses_through_unrelated_event_flood() {
        let (start, started) = oneshot::channel();
        let (done, finished) = oneshot::channel();
        let url = endpoint(|mut ws| async move {
            started.await.unwrap();
            for id in 0..2000 {
                ws.send(Message::Text(
                    json!({"sessionId":"owned",
                    "method":"Network.dataReceived", "params":{"requestId":id}})
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            }
            for id in 0..101 {
                let url = if id == 100 {
                    "http://127.0.0.1/private"
                } else {
                    "data:text/plain,owned"
                };
                ws.send(Message::Text(
                    json!({"sessionId":"owned", "method":"Fetch.requestPaused",
                    "params":{"requestId":format!("req-{id}"), "request":{"url":url}}})
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            }
            let mut seen = std::collections::HashSet::new();
            while seen.len() < 101 {
                let Some(Ok(Message::Text(text))) = ws.next().await else {
                    panic!("guard lost a paused request")
                };
                let call: Value = serde_json::from_str(&text).unwrap();
                let id = call["params"]["requestId"].as_str().unwrap();
                assert!(
                    seen.insert(id.to_owned()),
                    "duplicate paused request disposition"
                );
                assert_eq!(
                    call["method"],
                    if id == "req-100" {
                        "Fetch.failRequest"
                    } else {
                        "Fetch.continueRequest"
                    }
                );
                ws.send(Message::Text(
                    json!({"id":call["id"], "result":{}}).to_string().into(),
                ))
                .await
                .unwrap();
            }
            done.send(seen.len()).unwrap();
            let _ = ws.next().await;
        })
        .await;
        let cdp = Cdp::connect(&url).await.unwrap();
        let guard = cdp.spawn_fetch_guard("owned".into());
        start.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), finished)
                .await
                .expect("unanswered paused request")
                .unwrap(),
            101
        );
        assert!(!cdp.is_dead());
        guard.abort();
        let _ = guard.await;
    }

    #[tokio::test]
    async fn stealth_v3_fetch_guard_overflow_invalidates_generation() {
        let (start, started) = oneshot::channel();
        let url = endpoint(|mut ws| async move {
            started.await.unwrap();
            for id in 0..=GUARD_QUEUE {
                if ws.send(Message::Text(json!({"sessionId":"owned", "method":"Fetch.requestPaused",
                    "params":{"requestId":format!("req-{id}"), "request":{"url":"data:text/plain,owned"}}}).to_string().into())).await.is_err() { break; }
            }
            let _ = ws.next().await;
        }).await;
        let cdp = Cdp::connect(&url).await.unwrap();
        let (tx, rx) = mpsc::channel(GUARD_QUEUE);
        *cdp.guard_queue.lock().unwrap() = Some(("owned".into(), tx));
        start.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cdp.is_dead() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("overflow silently abandoned a paused request");
        assert_eq!(rx.len(), GUARD_QUEUE, "queue really reached its bound");
        assert_eq!(
            cdp.failure(),
            Some("request guard queue overflow or owner closed")
        );
        assert!(
            cdp.call(None, "Target.getTargets", json!({}))
                .await
                .unwrap_err()
                .to_string()
                .contains("link closed")
        );
    }

    // The browser process can outlive its DevTools link (a proxy in
    // between resets, a reply breaks the framing limits). Every call
    // in flight has to fail when the link ends, not when its own
    // timeout runs out, and the holder has to be able to see it.
    #[tokio::test]
    async fn a_cancelled_call_releases_its_pending_slot() {
        let (sent, received) = oneshot::channel();
        let url = endpoint(|mut ws| async move {
            assert!(ws.next().await.is_some());
            sent.send(()).unwrap();
            let _ = ws.next().await;
        })
        .await;
        let cdp = Cdp::connect(&url).await.unwrap();
        let caller = cdp.clone();
        let task =
            tokio::spawn(async move { caller.call(None, "Target.getTargets", json!({})).await });
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            cdp.pending.lock().unwrap().len(),
            1,
            "the call really dispatched"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            cdp.pending.lock().unwrap().is_empty(),
            "cancelled request leaked a sender"
        );
        assert!(
            cdp.was_cancelled(),
            "a dispatched cancelled call must retire its browser generation"
        );
    }

    #[tokio::test]
    async fn a_call_timeout_includes_waiting_for_the_writer() {
        let url = endpoint(|mut ws| async move {
            let _ = ws.next().await;
        })
        .await;
        let cdp = Cdp::connect(&url).await.unwrap();
        let _writer = cdp.write.lock().await;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            cdp.call_with_timeout(None, "Target.getTargets", json!({}), 1),
        )
        .await;
        assert!(
            result.is_ok(),
            "the call exceeded its deadline waiting for the writer"
        );
        assert!(result.unwrap().unwrap_err().to_string().contains("timeout"));
        assert!(cdp.pending.lock().unwrap().is_empty());
        assert!(
            !cdp.was_cancelled(),
            "a handled response timeout retains the bounded warm retry"
        );
    }

    #[tokio::test]
    async fn a_closed_link_fails_every_pending_call_at_once() {
        let url = endpoint(|mut ws| async move {
            let _ = ws.next().await;
            let _ = ws.next().await;
            drop(ws);
        })
        .await;
        let cdp = Cdp::connect(&url).await.unwrap();
        let started = Instant::now();
        let (a, b) = tokio::join!(
            cdp.call_with_timeout(None, "Target.getTargets", json!({}), 6),
            cdp.call_with_timeout(None, "Target.getTargets", json!({}), 6),
        );
        assert!(a.is_err() && b.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the waiters outlived the link by {:?}",
            started.elapsed()
        );
        assert!(cdp.is_dead(), "a closed link is dead");
        let started = Instant::now();
        let later = cdp
            .call_with_timeout(None, "Target.getTargets", json!({}), 6)
            .await;
        assert!(later.is_err() && started.elapsed() < Duration::from_secs(1));
        assert!(
            later.unwrap_err().to_string().contains("link closed"),
            "a call on a dead link says so"
        );
    }

    // A page can make its own DOM as large as it likes, and
    // outer_html asks for all of it. A reply the client will not
    // frame must fail that call, not silently end the reader with
    // the browser still alive.
    #[tokio::test]
    async fn an_oversized_reply_fails_the_call_and_marks_the_link_dead() {
        let url = endpoint(|mut ws| async move {
            let Some(Ok(Message::Text(req))) = ws.next().await else {
                return;
            };
            let id = serde_json::from_str::<Value>(&req).unwrap()["id"].clone();
            let huge = "a".repeat((1 << 20) + 1024);
            let reply = json!({ "id": id, "result": { "outerHTML": huge } }).to_string();
            let _ = ws.send(Message::Text(reply.into())).await;
            // Stay open: the browser is alive, only the reply was too big.
            tokio::time::sleep(Duration::from_secs(8)).await;
        })
        .await;
        let cdp = Cdp::connect_with_limit(&url, 1 << 20).await.unwrap();
        let started = Instant::now();
        let r = cdp
            .call_with_timeout(None, "DOM.getOuterHTML", json!({}), 6)
            .await;
        assert!(r.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the call waited for its timeout ({:?}) instead of failing with the link",
            started.elapsed()
        );
        assert!(cdp.is_dead());
    }
}
