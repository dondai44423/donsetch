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
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Largest DevTools message the client accepts, and the frame limit
/// with it: a full-page screenshot or a large document's outerHTML
/// is tens of MB, well over tungstenite's 16 MiB default frame.
pub(crate) const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

pub struct Cdp {
    write: Arc<Mutex<futures_util::stream::SplitSink<Ws, Message>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    /// Event stream (targetInfoChanged = title/url
    /// changes : challenge progression without Runtime).
    /// Consumed by the daemon's smarter wait loop.
    #[allow(dead_code)]
    events: broadcast::Sender<Value>,
    next_id: Arc<AtomicU64>,
    /// Set once the demux reader has ended: the link is gone even
    /// if the browser process is not.
    dead: Arc<AtomicBool>,
}

impl Clone for Cdp {
    fn clone(&self) -> Self {
        Self {
            write: Arc::clone(&self.write),
            pending: Arc::clone(&self.pending),
            events: self.events.clone(),
            next_id: Arc::clone(&self.next_id),
            dead: Arc::clone(&self.dead),
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
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_task = Arc::clone(&pending);
        let (events_tx, _) = broadcast::channel(256);
        let events_task = events_tx.clone();
        let dead = Arc::new(AtomicBool::new(false));
        let dead_task = Arc::clone(&dead);
        tokio::spawn(async move {
            loop {
                let msg = match read.next().await {
                    Some(Ok(msg)) => msg,
                    // Closed by the peer, or a reply the client will
                    // not frame: either way nothing more arrives here.
                    Some(Err(_)) | None => break,
                };
                let Message::Text(text) = msg else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    let mut map = pending_task.lock().await;
                    if let Some(tx) = map.remove(&id) {
                        let _ = tx.send(v);
                    }
                } else {
                    let _ = events_task.send(v);
                }
            }
            // The link is gone while the browser process may not be.
            // Fail every waiter now (dropping a sender ends its
            // receiver) instead of at each call's own timeout, and
            // let the holder see it before it serves another job.
            dead_task.store(true, Ordering::Release);
            pending_task.lock().await.clear();
        });
        Ok(Self {
            write: Arc::new(Mutex::new(write)),
            pending,
            events: events_tx,
            next_id: Arc::new(AtomicU64::new(1)),
            dead,
        })
    }

    /// True once the DevTools link has ended (the browser exited,
    /// the socket closed, or a reply broke the framing limits). A
    /// dead link answers nothing; the holder relaunches.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
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
            return Err(FetchError::ghost(format!("cdp link closed: {method}")));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let send_res = async {
            let mut w = self.write.lock().await;
            w.send(Message::Text(msg.to_string().into())).await
        }
        .await;
        if let Err(e) = send_res {
            self.pending.lock().await.remove(&id);
            return Err(FetchError::ghost(format!("cdp send: {e}")));
        }
        let resp =
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
                Ok(r) => r.map_err(|_| FetchError::ghost(format!("cdp dropped: {method}")))?,
                Err(_) => {
                    // Drop our entry: a tab that accepts commands but
                    // never answers used to leak one pending sender per
                    // call for the life of the warm browser.
                    self.pending.lock().await.remove(&id);
                    return Err(FetchError::ghost(format!("cdp timeout: {method}")));
                }
            };
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
    /// Does not block the demux reader; each paused request is
    /// handled in its own spawned task so DNS resolution cannot stall
    /// the event loop.
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
        let mut events = self.subscribe();
        tokio::spawn(async move {
            // NOTE: this is a broadcast::Receiver, so `recv()` fails
            // with `Lagged` whenever this loop is descheduled long
            // enough for the ring buffer (256) to fill. Lagged does
            // NOT mean dead: the receiver has already caught up and
            // the next `recv()` yields the next live event. Dying on
            // Lagged (a `while let Ok` loop) leaves every later
            // Fetch.requestPaused unanswered, which wedges the whole
            // CDP session (issue #76). Skip it, keep listening.
            loop {
                let event = match fetch_guard_step(events.recv().await) {
                    GuardStep::Event(ev) => ev,
                    GuardStep::Skip => continue,
                    GuardStep::Stop => break,
                };
                let method = event.get("method").and_then(Value::as_str).unwrap_or("");
                if method != "Fetch.requestPaused" {
                    continue;
                }
                // Filter for the single session we are guarding.
                // With `flatten: true`, the sessionId is top-level.
                if let Some(sid) = event.get("sessionId").and_then(Value::as_str) {
                    if sid != session {
                        continue;
                    }
                } else {
                    continue;
                }
                let params = event.get("params");
                let request_id = params
                    .and_then(|p| p.get("requestId"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let url = params
                    .and_then(|p| p.get("request"))
                    .and_then(|r| r.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if request_id.is_empty() {
                    continue;
                }
                // Do not block the demux reader / this loop: spawn per-request handling.
                let cdp2 = cdp.clone();
                let session2 = session.clone();
                tokio::spawn(async move {
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
                        let _ = cdp2
                            .call(
                                Some(&session2),
                                "Fetch.continueRequest",
                                json!({ "requestId": request_id }),
                            )
                            .await;
                    } else {
                        let _ = cdp2
                            .call(
                                Some(&session2),
                                "Fetch.failRequest",
                                json!({
                                    "requestId": request_id,
                                    "errorReason": "BlockedByClient"
                                }),
                            )
                            .await;
                    }
                });
            }
        })
    }
}

/// One `recv()` step for the fetch guard loop, as a pure decision.
/// Kept as a separate function so the failure mode of issue #76 is
/// regression-testable without a live CDP endpoint.
enum GuardStep {
    Event(Value),
    /// The receiver fell behind the 256-event ring during a burst.
    /// It has already resynced; dying here (a `while let Ok` loop)
    /// leaves every later `Fetch.requestPaused` unanswered, wedging
    /// the whole CDP session. Continue listening.
    Skip,
    /// All senders are gone (CDP link dropped).
    Stop,
}

#[inline]
fn fetch_guard_step(res: Result<Value, broadcast::error::RecvError>) -> GuardStep {
    match res {
        Ok(ev) => GuardStep::Event(ev),
        Err(broadcast::error::RecvError::Lagged(_skipped)) => GuardStep::Skip,
        Err(broadcast::error::RecvError::Closed) => GuardStep::Stop,
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

    // The browser process can outlive its DevTools link (a proxy in
    // between resets, a reply breaks the framing limits). Every call
    // in flight has to fail when the link ends, not when its own
    // timeout runs out, and the holder has to be able to see it.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #76 regression: one `Lagged` used to kill the fetch
    /// guard loop, starving every later requestPaused. The step must
    /// translate Lagged to Skip (never Stop), and the loop must keep
    /// receiving events after the lag.
    #[tokio::test]
    async fn fetch_guard_survives_lag() {
        let (tx, mut laggard) = broadcast::channel::<Value>(4);
        let mut consumer = tx.subscribe();
        let (go, go_rx) = tokio::sync::oneshot::channel::<()>();
        let mut go = Some(go);

        let sender = tokio::spawn(async move {
            for i in 0..50u32 {
                let _ = tx.send(json!({ "i": i }));
                tokio::task::yield_now().await;
            }
            // Hold the sender open until the laggard reports the lag:
            // proves survival-after-lag, not just a clean Closed.
            let _ = go_rx.await;
            for i in 100..110u32 {
                let _ = tx.send(json!({ "i": i }));
                tokio::task::yield_now().await;
            }
        });

        // A fast consumer drains the first wave while the laggard,
        // never polled, overflows its 4-slot ring.
        for _ in 0..50 {
            let _ = consumer.recv().await;
        }

        let mut events_seen = 0usize;
        let mut lags = 0usize;
        loop {
            match fetch_guard_step(laggard.recv().await) {
                GuardStep::Event(_) => events_seen += 1,
                GuardStep::Skip => {
                    lags += 1;
                    if let Some(g) = go.take() {
                        let _ = g.send(());
                    }
                }
                GuardStep::Stop => break,
            }
        }
        sender.await.unwrap();
        assert_eq!(lags, 1, "lag swallowed exactly once, loop must not die");
        assert!(
            events_seen >= 1,
            "post-lag events must still flow (got {events_seen})"
        );
    }
}
