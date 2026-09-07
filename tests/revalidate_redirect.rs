//! E2E tests for the redirect-hop fixes from the 3.6.7 refactor audit:
//!
//! - B2: conditional revalidation headers (If-None-Match/If-Modified-Since)
//!   minted for the ORIGINAL url must never ride a redirect hop. A colliding
//!   ETag on the target would otherwise yield a false 304 and merge the
//!   wrong cached body.
//!
//! - E15: the env proxy (HTTP_PROXY) is re-evaluated per hop against
//!   NO_PROXY. A redirect to a NO_PROXY-covered host dials direct
//!   (origin-form request), not through the proxy (absolute-form).

use donsetch::fetch::client::Fetcher;
use donsetch::profile::{BrowserProfile, Platform};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// Minimal HTTP server: records the first request line per connection
/// (into `log`), answers from `routes` (first matching token, "" =
/// fallback), then closes. One connection per request: our h1 tier
/// never pools plaintext connections.
fn serve(listener: TcpListener, log: Arc<Mutex<Vec<String>>>, routes: Vec<(String, String)>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let raw = String::from_utf8_lossy(&buf).to_string();
            let first = raw.lines().next().unwrap_or("").to_string();
            log.lock().unwrap().push(first.clone());
            let token = first.split(' ').nth(1).unwrap_or("");
            let response = routes
                .iter()
                .find(|(k, _)| k.is_empty() || token.contains(k.as_str()))
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                });
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
}

#[tokio::test]
async fn revalidation_conditionals_never_ride_a_redirect_hop() {
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };

    let origin = TcpListener::bind("127.0.0.1:0").expect("bind origin");
    let port = origin.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    // The collision path:
    //   request 1: GET /a (no conditionals) -> 200 + ETag "collide"
    //              (enters the revalidation cache on this seed fetch).
    //   request 2: GET /a carrying If-None-Match: "collide" (the
    //              revalidation) -> the origin answers 302 -> /b
    //              INSTEAD of 304. The validator was minted for /a.
    //   request 3: GET /b on the redirect hop.
    // Pre-fix: the stale If-None-Match rides hop 3; /b matches the
    // colliding ETag and answers 304 -> the caller merges /a's OLD
    // cached body into /b's outcome. Post-fix: hop 3 carries no
    // conditionals -> a real 200 with /b's body.
    std::thread::spawn(move || {
        for stream in origin.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let raw = String::from_utf8_lossy(&buf).to_string();
            let first = raw.lines().next().unwrap_or("").to_string();
            log.lock().unwrap().push(first.clone());
            // Our h1 tier sends lowercase header names (Chrome h2 truth).
            let lower = raw.to_lowercase();
            let has_conditional =
                lower.contains("if-none-match:") || lower.contains("if-modified-since:");
            let resp = if first.contains("/b") {
                if has_conditional {
                    // The smuggled validator matched /b's colliding ETag.
                    "HTTP/1.1 304 Not Modified\r\nETag: \"collide\"\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 6\r\n\r\nb-body"
                }
            } else if first.contains("/a") && has_conditional {
                // Revalidation request on /a: redirect instead of
                // answering 304 (the collision trigger).
                "HTTP/1.1 302 Found\r\nLocation: /b\r\nContent-Length: 0\r\n\r\n"
            } else {
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: max-age=0\r\nETag: \"collide\"\r\nContent-Length: 6\r\n\r\na-body"
            };
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });

    let profile = BrowserProfile::chrome_150(Platform::Linux);
    let fetcher = Fetcher::new(profile).expect("fetcher");
    let base = format!("http://127.0.0.1:{port}");

    // 1) Seed: /a answers 200 + ETag "collide" (cached, validators kept).
    let first = fetcher
        .fetch(&format!("{base}/a"))
        .await
        .expect("seed fetch");
    assert_eq!(first.status, 200);
    assert!(first.body.starts_with(b"a-body"), "seed body");

    // 2) Refetch the SAME url: the cache is in Revalidate state, so the
    //    client sends If-None-Match: "collide"; the origin 302s to /b.
    //    Pre-fix the conditional rides hop 2 and /b's 304 merges /a's
    //    cached body; post-fix /b is a real 200 with its own body.
    let out = fetcher
        .fetch(&format!("{base}/a"))
        .await
        .expect("redirect fetch");
    println!(
        "OUT: status={} body={:?} cache={:?}",
        out.status, out.body, out.cache
    );
    assert_eq!(
        out.status, 200,
        "hop 2 must be a real 200, not a merged 304"
    );
    assert!(
        out.body.starts_with(b"b-body"),
        "expected the redirect target's fresh body, got {:?}",
        out.body
    );
    let requests = seen.lock().unwrap().clone();
    println!("REQUESTS: {requests:?}");
    assert!(
        !requests
            .iter()
            .any(|l| l.contains("/b") && l.to_lowercase().contains("if-none-match")),
        "conditionals leaked onto a redirect hop: {:?}",
        requests
    );
}

/// Static Mutex so the two env-mutating tests serialize in-process
/// (cargo test runs them on one runtime; nextest would isolate anyway).
static ENV_LOCK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[tokio::test]
async fn env_proxy_is_rechecked_against_no_proxy_per_hop() {
    // Atomic test-order gate (cargo test runs both tests on one
    // runtime; nextest would isolate anyway).
    if ENV_LOCK.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    unsafe { std::env::remove_var("DONSETCH_NO_ENV_PROXY") };

    // Proxy: hop 1 target. Direct server: hop 2 target (NO_PROXY).
    let proxy_l = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    let proxy_port = proxy_l.local_addr().unwrap().port();
    let direct_l = TcpListener::bind("127.0.0.1:0").expect("bind direct");
    let direct_port = direct_l.local_addr().unwrap().port();

    let proxy_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let direct_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // Plaintext http via proxy: absolute-form request line; the proxy
    // answers with a 302 to the NO_PROXY-covered direct server.
    serve(
        proxy_l,
        proxy_log.clone(),
        vec![(
            "".to_string(),
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{direct_port}/b\r\nContent-Length: 0\r\n\r\n"
            ),
        )],
    );
    serve(
        direct_l,
        direct_log.clone(),
        vec![(
            "".to_string(),
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 6\r\n\r\nb-body"
                .to_string(),
        )],
    );

    unsafe { std::env::set_var("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}")) };
    unsafe { std::env::set_var("NO_PROXY", "127.0.0.1") };

    let profile = BrowserProfile::chrome_150(Platform::Linux);
    let fetcher = Fetcher::new(profile).expect("fetcher");
    // Hop 1: test-host.invalid is NOT covered by NO_PROXY -> through the
    // env proxy. Hop 2: 127.0.0.1 IS covered -> direct (origin-form).
    let out = fetcher
        .fetch("http://test-host.invalid/a")
        .await
        .expect("chained fetch");
    assert_eq!(out.status, 200, "hop 2 status");
    assert!(out.body.starts_with(b"b-body"), "hop 2 body");

    let plog = proxy_log.lock().unwrap().clone();
    assert!(
        !plog.is_empty(),
        "hop 1 must traverse the env proxy (pre-chain)"
    );
    let dlog = direct_log.lock().unwrap().clone();
    assert!(
        !dlog.is_empty(),
        "hop 2 must dial direct (NO_PROXY recheck per hop)"
    );
    // E15 discriminator: a direct dial sends origin-form ("GET /b").
    // Riding the proxy would arrive as absolute-form
    // ("GET http://127.0.0.1:P/b") — the pre-fix behavior.
    assert!(
        dlog.iter().all(|l| !l.starts_with("GET http")),
        "hop 2 went through the proxy (absolute-form): {:?}",
        dlog
    );
    assert!(
        dlog.iter().any(|l| l.starts_with("GET /b")),
        "hop 2 origin-form request missing: {:?}",
        dlog
    );

    unsafe { std::env::remove_var("HTTP_PROXY") };
    unsafe { std::env::remove_var("NO_PROXY") };
}
