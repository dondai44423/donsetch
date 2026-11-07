//! Minimal CDP (Chrome DevTools Protocol) client.
//!
//! Page-level JSON-RPC over the DevTools ws endpoint: navigation,
//! `Runtime.evaluate`, `Page.captureScreenshot`, the `Storage` /
//! `Browser` calls the login harvest needs, and the request guard that
//! answers the page's `Fetch.requestPaused` events. Message framing per
//! RFC 6455 via tokio-tungstenite.
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Pending = std::sync::Mutex<HashMap<u64, oneshot::Sender<Value>>>;

/// URL checks the request guard runs at once.
const GUARD_INFLIGHT: usize = 16;
/// Bytes of routing data the request guard may hold for paused
/// requests it has not answered yet, queued or being checked.
const GUARD_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// The routing data of one `Fetch.requestPaused` event.
struct PausedRequest {
    id: String,
    url: String,
    /// A document request of the page's main frame: the navigation
    /// itself or one of its redirect hops.
    main_document: bool,
}

/// What one paused request holds of [`GUARD_BUDGET_BYTES`].
fn paused_cost(id: &str, url: &str) -> usize {
    std::mem::size_of::<PausedRequest>() + id.len() + url.len()
}

/// The installed guard's inlet, while it runs.
struct GuardInlet {
    main_frame: String,
    requests: mpsc::UnboundedSender<PausedRequest>,
    /// Budget bytes of the paused requests not answered yet.
    held: Arc<AtomicUsize>,
}

type GuardQueue = std::sync::Mutex<Option<GuardInlet>>;

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
    /// Set once the link has ended: the demux reader stopped, or the
    /// request guard could not deliver an answer. The browser process
    /// may still be running.
    dead: Arc<AtomicBool>,
    failure: Arc<std::sync::Mutex<Option<&'static str>>>,
    guard_queue: Arc<GuardQueue>,
}

impl Clone for Cdp {
    fn clone(&self) -> Self {
        Self {
            write: Arc::clone(&self.write),
            pending: Arc::clone(&self.pending),
            next_id: Arc::clone(&self.next_id),
            dead: Arc::clone(&self.dead),
            failure: Arc::clone(&self.failure),
            guard_queue: Arc::clone(&self.guard_queue),
        }
    }
}

/// The running request guard of one page (see
/// [`Cdp::spawn_request_guard`]). Dropping it stops the guard.
pub struct RequestGuard {
    task: tokio::task::JoinHandle<()>,
    refusal: Arc<std::sync::Mutex<Option<FetchError>>>,
}

impl RequestGuard {
    /// The check's error for the first main-frame document request the
    /// guard refused, if any; taken, so a second call returns `None`.
    pub fn take_refusal(&self) -> Option<FetchError> {
        self.refusal
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.task.abort();
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
        let guard_queue = Arc::new(GuardQueue::new(None));
        let guard_task = Arc::clone(&guard_queue);
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
                } else if v.get("method").and_then(Value::as_str) == Some("Fetch.requestPaused") {
                    // Chrome holds a paused request until it is answered,
                    // so none may be dropped: a pause the guard cannot
                    // take ends the link instead of stalling the page.
                    // The reader never waits for the guard, whose answers
                    // it has to deliver; the byte budget bounds what a
                    // page's burst of requests can make it hold.
                    let queue = guard_task.lock().unwrap_or_else(|p| p.into_inner());
                    let Some(inlet) = queue.as_ref() else {
                        continue;
                    };
                    let id = v.pointer("/params/requestId").and_then(Value::as_str);
                    let url = v.pointer("/params/request/url").and_then(Value::as_str);
                    // Keep only routing data, never post bodies.
                    let Some(id) = id.filter(|id| !id.is_empty() && id.len() <= 1024) else {
                        break "malformed paused request ID";
                    };
                    let Some(url) = url else {
                        break "malformed paused request URL";
                    };
                    let cost = paused_cost(id, url);
                    if inlet.held.fetch_add(cost, Ordering::AcqRel) + cost > GUARD_BUDGET_BYTES {
                        break "request guard budget exceeded";
                    }
                    let main_document = v.pointer("/params/resourceType").and_then(Value::as_str)
                        == Some("Document")
                        && v.pointer("/params/frameId").and_then(Value::as_str)
                            == Some(inlet.main_frame.as_str());
                    if inlet
                        .requests
                        .send(PausedRequest {
                            id: id.into(),
                            url: url.into(),
                            main_document,
                        })
                        .is_err()
                    {
                        break "request guard closed";
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
            // The guard's clone of this link also owns the queue's
            // sender: drop it here so an idle guard sees the end.
            *guard_task.lock().unwrap_or_else(|p| p.into_inner()) = None;
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
            guard_queue,
        })
    }

    /// Start answering the page's `Fetch.requestPaused` events; the
    /// caller enables `Fetch` afterwards, so no pause is missed.
    ///
    /// Each paused URL goes through `fetch::guards::ensure_url_safe`
    /// (scheme, credentials, the operator's rules, SSRF and DNS checks;
    /// a `data:` URL is let through unchecked) and is answered with
    /// `Fetch.continueRequest`, or with `Fetch.failRequest`
    /// (`BlockedByClient`) when the check refuses it. A refused document
    /// request of `main_frame` (the navigation or a redirect hop) also
    /// keeps the check's error for [`RequestGuard::take_refusal`]; the
    /// first such error is kept.
    ///
    /// At most 16 checks run at once; the pauses waiting for one are
    /// held in arrival order. A disposition the browser rejects, as it
    /// does for a request the page has cancelled, ends nothing. The link
    /// ends (`is_dead`), since an unanswered pause would stall the page,
    /// when a disposition cannot be delivered (send failure, no reply
    /// within 20 s) or when the unanswered pauses' ids and URLs would
    /// pass 16 MiB.
    ///
    /// The check is point-in-time: the browser resolves the host again
    /// when it connects, so a name that changes its answer in between
    /// (DNS rebinding) is not caught.
    pub fn spawn_request_guard(&self, main_frame: String) -> RequestGuard {
        let cdp = self.clone();
        let refusal = Arc::new(std::sync::Mutex::new(None));
        let refusal_task = Arc::clone(&refusal);
        let (tx, mut requests) = mpsc::unbounded_channel::<PausedRequest>();
        let held = Arc::new(AtomicUsize::new(0));
        *self.guard_queue.lock().unwrap_or_else(|p| p.into_inner()) = Some(GuardInlet {
            main_frame,
            requests: tx,
            held: Arc::clone(&held),
        });
        let task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    result = tasks.join_next(), if !tasks.is_empty() => {
                        if !matches!(result, Some(Ok(Ok(())))) {
                            cdp.failure
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .get_or_insert("request guard disposition failed");
                            cdp.dead.store(true, Ordering::Release);
                            cdp.pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
                            break;
                        }
                    }
                    request = requests.recv(), if tasks.len() < GUARD_INFLIGHT => {
                        let Some(request) = request else { break };
                        let cdp = cdp.clone();
                        let refusal = Arc::clone(&refusal_task);
                        let held = Arc::clone(&held);
                        tasks.spawn(async move {
                            let cost = paused_cost(&request.id, &request.url);
                            let answered = cdp.answer_paused(request, &refusal).await;
                            held.fetch_sub(cost, Ordering::AcqRel);
                            answered
                        });
                    }
                }
            }
        });
        RequestGuard { task, refusal }
    }

    /// Check one paused request and answer it. `Err` only when the
    /// answer could not be delivered.
    async fn answer_paused(
        &self,
        request: PausedRequest,
        refusal: &std::sync::Mutex<Option<FetchError>>,
    ) -> Result<(), FetchError> {
        // A data: URL carries its bytes and opens no connection.
        let checked = if request.url.starts_with("data:") {
            Ok(())
        } else {
            crate::fetch::guards::ensure_url_safe(&request.url)
                .await
                .map(drop)
        };
        let (method, params) = match checked {
            Ok(()) => ("Fetch.continueRequest", json!({ "requestId": request.id })),
            Err(error) => {
                // Kept before the answer goes out, so it is in place by
                // the time the browser acts on the failure.
                if request.main_document {
                    refusal
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get_or_insert(error);
                }
                (
                    "Fetch.failRequest",
                    json!({ "requestId": request.id, "errorReason": "BlockedByClient" }),
                )
            }
        };
        let reply = self.exchange(None, method, params, 20).await?;
        // An error reply is how Chrome rejects a disposition for a
        // request it no longer holds ("Invalid InterceptionId." once the
        // page has cancelled it): nothing is left paused, so the guard
        // carries on.
        if let Some(error) = reply.get("error")
            && crate::config::cfg().debug.ghost
        {
            crate::diag!("[cdp] {method} rejected: {error}");
        }
        Ok(())
    }

    /// True once the DevTools link has ended (the browser exited,
    /// the socket closed, a reply broke the framing limits, the paused
    /// requests passed the guard's budget, or the guard could not
    /// deliver an answer). A dead link answers nothing.
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
        let resp = self.exchange(session, method, params, timeout_secs).await?;
        if let Some(err) = resp.get("error") {
            return Err(FetchError::browser(format!(
                "cdp {method}: {}",
                err.get("message").and_then(Value::as_str).unwrap_or("?")
            )));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// One call and the browser's whole reply, an error reply included.
    /// `Err` only when the link fails: closed, a send error, no reply
    /// within `timeout_secs`.
    async fn exchange(
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
        resp.map_err(|_| FetchError::browser(format!("cdp timeout: {method}")))?
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

    // Every pause gets an answer: a URL the check accepts continues, one
    // it refuses fails with BlockedByClient, and only a refused document
    // of the main frame is kept as the refusal. Installs the process
    // config, so it depends on nextest's one process per test.
    #[tokio::test]
    async fn the_request_guard_answers_each_pause_and_keeps_the_main_document_refusal() {
        use crate::rules::{RuleAction, UrlRule};
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = false;
        config.rules.url.insert(
            "denied.example".into(),
            UrlRule {
                action: RuleAction::Deny,
                message: Some("not this host".into()),
                ..Default::default()
            },
        );
        crate::config::install(config).unwrap();
        // (requestId, url, resourceType, frameId)
        let paused = [
            ("public", "http://93.184.216.34/", "Document", "main"),
            ("inline", "data:text/plain,hi", "Image", "main"),
            ("image", "https://denied.example/pixel.png", "Image", "main"),
            ("frame", "https://denied.example/embed", "Document", "child"),
            ("loopback", "http://127.0.0.1/", "XHR", "main"),
            ("hop", "https://denied.example/landing", "Document", "main"),
        ];
        let (sent, answered) = oneshot::channel();
        let url = endpoint(move |mut ws| async move {
            // Pauses start once interception is enabled, as in Chrome.
            let Some(Ok(Message::Text(call))) = ws.next().await else {
                return;
            };
            let call: Value = serde_json::from_str(&call).unwrap();
            assert_eq!(call["method"], "Fetch.enable");
            let reply = json!({ "id": call["id"], "result": {} });
            ws.send(Message::Text(reply.to_string().into()))
                .await
                .unwrap();
            for (id, url, kind, frame) in paused {
                let event = json!({
                    "method": "Fetch.requestPaused",
                    "params": {
                        "requestId": id,
                        "request": { "url": url, "method": "GET" },
                        "resourceType": kind,
                        "frameId": frame,
                    },
                });
                ws.send(Message::Text(event.to_string().into()))
                    .await
                    .unwrap();
            }
            let mut answers = HashMap::new();
            while answers.len() < paused.len() {
                let Some(Ok(Message::Text(call))) = ws.next().await else {
                    break;
                };
                let call: Value = serde_json::from_str(&call).unwrap();
                let reply = json!({ "id": call["id"], "result": {} });
                ws.send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
                let answer = match call["params"]["errorReason"].as_str() {
                    Some(reason) => format!("{} {reason}", call["method"].as_str().unwrap()),
                    None => call["method"].as_str().unwrap().to_string(),
                };
                answers.insert(
                    call["params"]["requestId"].as_str().unwrap().to_string(),
                    answer,
                );
            }
            sent.send(answers).unwrap();
            let _ = ws.next().await;
        })
        .await;
        let cdp = Cdp::connect(&url).await.unwrap();
        let guard = cdp.spawn_request_guard("main".into());
        cdp.call(None, "Fetch.enable", json!({})).await.unwrap();
        let answers = tokio::time::timeout(Duration::from_secs(5), answered)
            .await
            .expect("every pause answered")
            .unwrap();
        let fail = "Fetch.failRequest BlockedByClient";
        for (id, expected) in [
            ("public", "Fetch.continueRequest"),
            ("inline", "Fetch.continueRequest"),
            ("image", fail),
            ("frame", fail),
            ("loopback", fail),
            ("hop", fail),
        ] {
            assert_eq!(answers.get(id).map(String::as_str), Some(expected), "{id}");
        }
        match guard.take_refusal() {
            Some(FetchError::Denied { url, .. }) => {
                assert_eq!(url, "https://denied.example/landing");
            }
            other => panic!("expected the main document's denial, got {other:?}"),
        }
        assert!(guard.take_refusal().is_none(), "the refusal is taken once");
        assert!(!cdp.is_dead(), "answered pauses keep the link");
    }

    // Chrome rejects the disposition of a request the page cancelled
    // while it waited ("Invalid InterceptionId."). Nothing is left
    // paused, so the guard answers the next pause and the link stays.
    #[tokio::test]
    async fn a_rejected_disposition_leaves_the_guard_and_the_link_running() {
        let (sent, answered) = oneshot::channel();
        let url = endpoint(move |mut ws| async move {
            let Some(Ok(Message::Text(call))) = ws.next().await else {
                return;
            };
            let call: Value = serde_json::from_str(&call).unwrap();
            assert_eq!(call["method"], "Fetch.enable");
            let reply = json!({ "id": call["id"], "result": {} });
            ws.send(Message::Text(reply.to_string().into()))
                .await
                .unwrap();
            let mut answers = Vec::new();
            for id in ["cancelled", "next"] {
                let event = json!({
                    "method": "Fetch.requestPaused",
                    "params": {
                        "requestId": id,
                        "request": { "url": "data:text/plain,hi", "method": "GET" },
                        "resourceType": "Fetch",
                        "frameId": "main",
                    },
                });
                ws.send(Message::Text(event.to_string().into()))
                    .await
                    .unwrap();
                let Some(Ok(Message::Text(call))) = ws.next().await else {
                    break;
                };
                let call: Value = serde_json::from_str(&call).unwrap();
                let reply = if id == "cancelled" {
                    json!({
                        "id": call["id"],
                        "error": { "code": -32602, "message": "Invalid InterceptionId." },
                    })
                } else {
                    json!({ "id": call["id"], "result": {} })
                };
                ws.send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
                answers.push(format!(
                    "{} {}",
                    call["method"].as_str().unwrap(),
                    call["params"]["requestId"].as_str().unwrap()
                ));
            }
            sent.send(answers).unwrap();
            // An ordinary call after the rejection.
            if let Some(Ok(Message::Text(call))) = ws.next().await {
                let call: Value = serde_json::from_str(&call).unwrap();
                let reply = json!({ "id": call["id"], "result": { "targetInfos": [] } });
                ws.send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
            }
            let _ = ws.next().await;
        })
        .await;
        let cdp = Cdp::connect(&url).await.unwrap();
        let _guard = cdp.spawn_request_guard("main".into());
        cdp.call(None, "Fetch.enable", json!({})).await.unwrap();
        let answers = tokio::time::timeout(Duration::from_secs(5), answered)
            .await
            .expect("the pause after the rejected disposition was answered")
            .unwrap();
        assert_eq!(
            answers,
            [
                "Fetch.continueRequest cancelled",
                "Fetch.continueRequest next"
            ]
        );
        assert!(!cdp.is_dead(), "{:?}", cdp.failure());
        cdp.call(None, "Target.getTargets", json!({}))
            .await
            .expect("the link still carries calls");
    }

    // A page can start far more requests at once than the guard checks
    // at once. Every pause is held until its turn and answered.
    #[tokio::test]
    async fn a_burst_of_pauses_is_held_and_answered_in_full() {
        const BURST: usize = 2000;
        let (sent, answered) = oneshot::channel();
        let url = endpoint(move |mut ws| async move {
            let Some(Ok(Message::Text(call))) = ws.next().await else {
                return;
            };
            let call: Value = serde_json::from_str(&call).unwrap();
            assert_eq!(call["method"], "Fetch.enable");
            let reply = json!({ "id": call["id"], "result": {} });
            ws.send(Message::Text(reply.to_string().into()))
                .await
                .unwrap();
            // Every pause goes out before any answer is read, as when a
            // page's requests outrun the guard.
            for n in 0..BURST {
                let event = json!({
                    "method": "Fetch.requestPaused",
                    "params": {
                        "requestId": format!("burst-{n}"),
                        "request": { "url": format!("data:text/plain,{n}"), "method": "GET" },
                        "resourceType": "Image",
                        "frameId": "main",
                    },
                });
                ws.send(Message::Text(event.to_string().into()))
                    .await
                    .unwrap();
            }
            let mut answers = std::collections::HashSet::new();
            while answers.len() < BURST {
                let Some(Ok(Message::Text(call))) = ws.next().await else {
                    break;
                };
                let call: Value = serde_json::from_str(&call).unwrap();
                let reply = json!({ "id": call["id"], "result": {} });
                ws.send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
                answers.insert(call["params"]["requestId"].as_str().unwrap().to_string());
            }
            sent.send(answers.len()).unwrap();
            let _ = ws.next().await;
        })
        .await;
        let cdp = Cdp::connect(&url).await.unwrap();
        let _guard = cdp.spawn_request_guard("main".into());
        cdp.call(None, "Fetch.enable", json!({})).await.unwrap();
        let answered = tokio::time::timeout(Duration::from_secs(10), answered)
            .await
            .expect("every pause of the burst answered")
            .unwrap();
        assert_eq!(answered, BURST);
        assert!(!cdp.is_dead(), "{:?}", cdp.failure());
    }

    // What the guard holds is bounded: unanswered pauses whose ids and
    // URLs pass the budget end the link, and the link says why.
    #[tokio::test]
    async fn pauses_past_the_guard_budget_end_the_link() {
        let url = endpoint(move |mut ws| async move {
            let Some(Ok(Message::Text(call))) = ws.next().await else {
                return;
            };
            let call: Value = serde_json::from_str(&call).unwrap();
            let reply = json!({ "id": call["id"], "result": {} });
            ws.send(Message::Text(reply.to_string().into()))
                .await
                .unwrap();
            let long = format!("data:text/plain,{}", "a".repeat(1 << 20));
            for n in 0..2 * (GUARD_BUDGET_BYTES >> 20) {
                let event = json!({
                    "method": "Fetch.requestPaused",
                    "params": {
                        "requestId": format!("held-{n}"),
                        "request": { "url": long, "method": "GET" },
                        "resourceType": "Image",
                        "frameId": "main",
                    },
                });
                if ws
                    .send(Message::Text(event.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // Read the dispositions, answer none.
            while ws.next().await.is_some() {}
        })
        .await;
        let cdp = Cdp::connect(&url).await.unwrap();
        let _guard = cdp.spawn_request_guard("main".into());
        cdp.call(None, "Fetch.enable", json!({})).await.unwrap();
        let started = Instant::now();
        while !cdp.is_dead() && started.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(cdp.failure(), Some("request guard budget exceeded"));
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
