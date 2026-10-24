//! Minimal CDP (Chrome DevTools Protocol) client.
//!
//! Page-level JSON-RPC over the DevTools ws endpoint: navigation,
//! `Runtime.evaluate`, `Page.captureScreenshot` and the `Storage` /
//! `Browser` calls the login harvest needs. Message framing per RFC
//! 6455 via tokio-tungstenite.
//!
//! `Cdp` is cloneable (every field is an Arc); `call_with_timeout`
//! bounds each response wait, because a page-scoped CDP response can
//! queue behind a settling navigation and lag the URL advance by tens
//! of seconds on some Chromium builds : an unbounded wait there turns
//! a recoverable stall into a failed capture.

use crate::error::FetchError;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Pending = std::sync::Mutex<HashMap<u64, oneshot::Sender<Value>>>;

// No await happens under this short map lock. Synchronous removal
// retires an abandoned request immediately, including during a send.
struct PendingCall<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for PendingCall<'_> {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

/// Largest DevTools message the client accepts, and the frame limit
/// with it: a full-page screenshot or a large document's outerHTML
/// is tens of MB, well over tungstenite's 16 MiB default frame.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

pub struct Cdp {
    write: Arc<Mutex<futures_util::stream::SplitSink<Ws, Message>>>,
    pending: Arc<Pending>,
    next_id: Arc<AtomicU64>,
    /// Set once the demux reader has ended: the link is gone even
    /// if the browser process is not.
    dead: Arc<AtomicBool>,
    failure: Arc<std::sync::Mutex<Option<&'static str>>>,
}

impl Clone for Cdp {
    fn clone(&self) -> Self {
        Self {
            write: Arc::clone(&self.write),
            pending: Arc::clone(&self.pending),
            next_id: Arc::clone(&self.next_id),
            dead: Arc::clone(&self.dead),
            failure: Arc::clone(&self.failure),
        }
    }
}

impl Cdp {
    /// Connect to a page/browser-level ws endpoint and spawn the
    /// demux reader task.
    pub async fn connect(ws_url: &str) -> Result<Self, FetchError> {
        Self::connect_with_limit(ws_url, MAX_MESSAGE_BYTES).await
    }

    /// `connect` with the message and frame limit as a parameter.
    async fn connect_with_limit(ws_url: &str, max_bytes: usize) -> Result<Self, FetchError> {
        // The only unguarded network primitive here : a browser that
        // accepts TCP but stalls the WS handshake would hang the call
        // forever.
        let mut ws_config = WebSocketConfig::default();
        ws_config.max_message_size = Some(max_bytes);
        ws_config.max_frame_size = Some(max_bytes);
        let (ws, _) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_async_with_config(ws_url, Some(ws_config), false),
        )
        .await
        .map_err(|_| FetchError::browser("cdp connect: ws handshake timeout"))?
        .map_err(|e| FetchError::browser(format!("cdp connect: {e}")))?;
        let (write, mut read) = ws.split();
        let pending = Arc::new(Pending::new(HashMap::new()));
        let pending_task = Arc::clone(&pending);
        let dead = Arc::new(AtomicBool::new(false));
        let dead_task = Arc::clone(&dead);
        let failure = Arc::new(std::sync::Mutex::new(None));
        let failure_task = Arc::clone(&failure);
        tokio::spawn(async move {
            let cause = loop {
                let msg = match read.next().await {
                    Some(Ok(msg)) => msg,
                    // Closed by the peer, or a reply the client will
                    // not frame: either way nothing more arrives here.
                    Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                        break "websocket message/frame limit";
                    }
                    Some(Err(tokio_tungstenite::tungstenite::Error::Io(_))) => {
                        break "websocket I/O error";
                    }
                    Some(Err(tokio_tungstenite::tungstenite::Error::Protocol(_))) => {
                        break "websocket protocol error";
                    }
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
            next_id: Arc::new(AtomicU64::new(1)),
            dead,
            failure,
        })
    }

    /// True once the DevTools link has ended (the browser exited,
    /// the socket closed, or a reply broke the framing limits). A
    /// dead link answers nothing.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    pub fn failure(&self) -> Option<&'static str> {
        *self.failure.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Call a method. `session` scopes it to an attached
    /// target (page); None = browser/page-level (default 20s bound).
    pub async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, FetchError> {
        self.call_with_timeout(session, method, params, 20).await
    }

    /// Call with an explicit response timeout (seconds). Short bounds
    /// make one queued/deferred response cost a single poll iteration
    /// instead of the whole wait window; longer bounds let late
    /// responses still land cleanly and free their pending slot.
    pub async fn call_with_timeout(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout_secs: u64,
    ) -> Result<Value, FetchError> {
        if self.is_dead() {
            return Err(FetchError::browser(format!(
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
                return Err(FetchError::browser(format!(
                    "cdp link closed: {method} ({})",
                    self.failure().unwrap_or("unavailable")
                )));
            }
            pending.insert(id, tx);
        }
        // Retire the pending slot on every exit path (timeout, abort):
        // a late duplicate answer must never be delivered twice.
        let _pending_call = PendingCall {
            pending: &self.pending,
            id,
        };
        let started = std::time::Instant::now();
        let resp = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
            {
                let mut w = self.write.lock().await;
                w.send(Message::Text(msg.to_string().into()))
                    .await
                    .map_err(|e| FetchError::browser(format!("cdp send: {e}")))?;
            }
            rx.await.map_err(|_| {
                FetchError::browser(format!(
                    "cdp dropped: {method} ({})",
                    self.failure().unwrap_or("unavailable")
                ))
            })
        })
        .await;
        if crate::config::cfg().debug.ghost && started.elapsed().as_millis() >= 1000 {
            crate::diag!(
                "[cdp] {method} response wait {}ms",
                started.elapsed().as_millis()
            );
        }
        // A response timeout keeps the bounded retry; the drop guard
        // above releases the slot either way.
        let resp = resp.map_err(|_| FetchError::browser(format!("cdp timeout: {method}")))??;
        if let Some(err) = resp.get("error") {
            return Err(FetchError::browser(format!(
                "cdp {method}: {}",
                err.get("message").and_then(Value::as_str).unwrap_or("?")
            )));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

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

    // A page can make its own DOM as large as it likes. A reply the
    // client will not frame must fail that call, not silently end the
    // reader with the browser still alive.
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
        assert_eq!(cdp.failure(), Some("websocket message/frame limit"));
    }

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
}
