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
    crate::sandbox();
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
async fn stealth_v3_freshness_includes_actual_response_delay_and_validation_age() {
    use donsetch::fetch::client::CacheState;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/owned-delay", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for ordinal in 1..=3 {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(tcp.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap();
            if ordinal == 1 {
                assert!(!head.contains("if-none-match:"));
            } else {
                assert!(head.contains("if-none-match: \"delay\"\r\n"));
            }
            let response = if ordinal == 1 {
                tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nETag: \"delay\"\r\nCache-Control: max-age=1\r\nAge: 0\r\nContent-Length: 10\r\nConnection: close\r\n\r\ndelay body"
            } else if ordinal == 2 {
                "HTTP/1.1 304 Not Modified\r\nETag: \"delay\"\r\nCache-Control: max-age=1\r\nAge: 2\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 304 Not Modified\r\nETag: \"delay\"\r\nCache-Control: max-age=600\r\nAge: 0\r\nConnection: close\r\n\r\n"
            };
            tcp.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    assert_eq!(fetcher.fetch(&url).await.unwrap().body, b"delay body");
    for _ in 0..2 {
        let out = fetcher.fetch(&url).await.unwrap();
        assert_eq!(
            out.cache,
            CacheState::Revalidated,
            "expired wire time/Age requires actual validation"
        );
        assert_eq!(out.body, b"delay body");
    }
    assert_eq!(fetcher.fetch(&url).await.unwrap().cache, CacheState::Fresh);
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn stealth_v3_304_changed_validators_fail_without_merging_a_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    for (old, updated, valid) in [
        ("ETag: \"v1\"", "ETag: \"v2\"", false),
        ("ETag: W/\"v1\"", "ETag: \"v1\"", false),
        ("ETag: \"v1\"", "ETag: \"v1\"\r\nETag: \"v1\"", false),
        (
            "Last-Modified: Sun, 06 Nov 1994 08:49:37 GMT",
            "Last-Modified: Sun, 06 Nov 1994 08:49:38 GMT",
            false,
        ),
        (
            "Last-Modified: Sun, 06 Nov 1994 08:49:37 GMT",
            "Last-Modified: Sun, 06 Nov 1994 08:49:37 GMT",
            true,
        ),
        ("ETag: \"v1\"", "ETag: W/\"v1\"", true),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/owned-validator", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for ordinal in 1..=2 {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 16384);
                    head.push(tcp.read_u8().await.unwrap());
                }
                let head = String::from_utf8(head).unwrap();
                if ordinal == 2 {
                    let expected = if old.starts_with("ETag:") {
                        "if-none-match:"
                    } else {
                        "if-modified-since:"
                    };
                    assert!(head.contains(expected));
                }
                let response = if ordinal == 1 {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n{old}\r\nContent-Length: 10\r\nConnection: close\r\n\r\nowned body"
                    )
                } else {
                    format!(
                        "HTTP/1.1 304 Not Modified\r\n{updated}\r\nCache-Control: max-age=600\r\nConnection: close\r\n\r\n"
                    )
                };
                tcp.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
        assert_eq!(fetcher.fetch(&url).await.unwrap().body, b"owned body");
        let result = fetcher.fetch(&url).await;
        if valid {
            assert_eq!(
                result.unwrap().body,
                b"owned body",
                "matching validator {updated}"
            );
        } else {
            let Err(error) = result else {
                panic!("changed validator must not authorize old body: {updated}");
            };
            assert!(error.to_string().contains("304"));
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn stealth_v3_304_refreshes_metadata_and_freshness_without_refetching_body() {
    use donsetch::fetch::client::CacheState;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/owned", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let server = tokio::spawn(async move {
        for ordinal in 1..=2 {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(tcp.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap();
            log.lock().unwrap().push(head.clone());
            let response = if ordinal == 1 {
                assert!(!head.contains("if-none-match:"));
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: max-age=0\r\nETag: \"v1\"\r\nX-Revision: old\r\nContent-Length: 15\r\nConnection: close\r\n\r\nowned body data"
            } else {
                assert!(head.contains("if-none-match: \"v1\"\r\n"));
                "HTTP/1.1 304 Not Modified\r\nCache-Control: max-age=600\r\nETag: \"v1\"\r\nX-Revision: validated\r\nConnection: close\r\n\r\n"
            };
            tcp.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    assert_eq!(fetcher.fetch(&url).await.unwrap().body, b"owned body data");
    let validated = fetcher.fetch(&url).await.unwrap();
    assert_eq!(validated.cache, CacheState::Revalidated);
    assert_eq!(validated.status, 200);
    assert_eq!(validated.body, b"owned body data");
    assert!(
        validated
            .headers
            .iter()
            .any(|(name, value)| name == "x-revision" && value == "validated")
    );
    let fresh = fetcher.fetch(&url).await.unwrap();
    assert_eq!(fresh.cache, CacheState::Fresh);
    assert_eq!(fresh.body, b"owned body data");
    assert_eq!(seen.lock().unwrap().len(), 2);
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn stealth_v3_304_uses_its_validator_snapshot_without_overwriting_newer_body() {
    use donsetch::fetch::client::CacheState;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/owned", listener.local_addr().unwrap());
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut entered = Some(entered);
        let mut released = Some(released);
        let mut peers = tokio::task::JoinSet::new();
        for ordinal in 1..=3 {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let barrier = if ordinal == 2 {
                Some((entered.take().unwrap(), released.take().unwrap()))
            } else {
                None
            };
            peers.spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 16384);
                    head.push(tcp.read_u8().await.unwrap());
                }
                let head = String::from_utf8(head).unwrap();
                if ordinal > 1 {
                    assert!(head.contains("if-none-match: \"v1\"\r\n"));
                }
                if let Some((entered, released)) = barrier {
                    entered.send(()).unwrap();
                    released.await.unwrap();
                }
                let response = match ordinal {
                    1 => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nETag: \"v1\"\r\nCache-Control: max-age=0\r\nContent-Length: 11\r\nConnection: close\r\n\r\nold version",
                    2 => "HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nCache-Control: max-age=600\r\nX-Revision: old-validated\r\nConnection: close\r\n\r\n",
                    _ => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nETag: \"v2\"\r\nCache-Control: max-age=600\r\nX-Revision: new\r\nContent-Length: 11\r\nConnection: close\r\n\r\nnew version",
                };
                tcp.write_all(response.as_bytes()).await.unwrap();
                head
            });
        }
        let mut heads = Vec::new();
        while let Some(result) = peers.join_next().await {
            heads.push(result.unwrap());
        }
        heads
    });
    let fetcher = Arc::new(Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap());
    assert_eq!(fetcher.fetch(&url).await.unwrap().body, b"old version");
    let delayed = {
        let fetcher = fetcher.clone();
        let url = url.clone();
        tokio::spawn(async move { fetcher.fetch(&url).await.unwrap() })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetcher.fetch(&url).await.unwrap().body, b"new version");
    release.send(()).unwrap();
    let validated = delayed.await.unwrap();
    assert_eq!(validated.cache, CacheState::Revalidated);
    assert_eq!(
        validated.body, b"old version",
        "304 validates the exact body sent in its precondition, not a concurrent replacement"
    );
    let fresh = fetcher.fetch(&url).await.unwrap();
    assert_eq!(fresh.cache, CacheState::Fresh);
    assert_eq!(
        fresh.body, b"new version",
        "old validation must not overwrite a newer committed representation"
    );
    let heads = tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(heads.len(), 3);
}

#[tokio::test]
async fn stealth_v3_late_304_cannot_resurrect_content_after_a_new_404() {
    use donsetch::fetch::client::CacheState;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/deleted", listener.local_addr().unwrap());
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut entered = Some(entered);
        let mut released = Some(released);
        let mut peers = tokio::task::JoinSet::new();
        for ordinal in 1..=4 {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let barrier = if ordinal == 2 {
                Some((entered.take().unwrap(), released.take().unwrap()))
            } else {
                None
            };
            peers.spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 16384);
                    head.push(tcp.read_u8().await.unwrap());
                }
                let head = String::from_utf8(head).unwrap();
                if ordinal == 2 || ordinal == 3 {
                    assert!(head.contains("if-none-match: \"v1\"\r\n"));
                } else { assert!(!head.contains("if-none-match:")); }
                if let Some((entered, released)) = barrier {
                    entered.send(()).unwrap(); released.await.unwrap();
                }
                let response = match ordinal {
                    1 => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nETag: \"v1\"\r\nContent-Length: 10\r\nConnection: close\r\n\r\nowned body",
                    2 => "HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nCache-Control: max-age=600\r\nConnection: close\r\n\r\n",
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                };
                tcp.write_all(response.as_bytes()).await.unwrap();
            });
        }
        while let Some(result) = peers.join_next().await {
            result.unwrap();
        }
    });
    let fetcher = Arc::new(Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap());
    assert_eq!(fetcher.fetch(&url).await.unwrap().body, b"owned body");
    let delayed = {
        let fetcher = fetcher.clone();
        let url = url.clone();
        tokio::spawn(async move { fetcher.fetch(&url).await.unwrap() })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetcher.fetch(&url).await.unwrap().status, 404);
    release.send(()).unwrap();
    assert_eq!(
        delayed.await.unwrap().body,
        b"owned body",
        "old read can finish from its own snapshot"
    );
    let current = fetcher.fetch(&url).await.unwrap();
    assert_eq!(
        current.status, 404,
        "late validation must not cache content known to be deleted"
    );
    assert_eq!(current.cache, CacheState::None);
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn stealth_v3_unsolicited_304_after_redirect_cannot_reuse_original_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/original", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for ordinal in 1..=3 {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(tcp.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap();
            if ordinal == 3 {
                assert!(head.starts_with("GET /other "));
                assert!(!head.contains("if-none-match:") && !head.contains("if-modified-since:"));
            }
            let response = match ordinal {
                1 => {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nETag: \"v1\"\r\nContent-Length: 13\r\nConnection: close\r\n\r\noriginal body"
                }
                2 => {
                    "HTTP/1.1 302 Found\r\nLocation: /other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                }
                _ => "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n",
            };
            tcp.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    assert_eq!(fetcher.fetch(&url).await.unwrap().body, b"original body");
    let Err(error) = fetcher.fetch(&url).await else {
        panic!("an unsolicited redirected 304 has no validator snapshot");
    };
    assert!(error.to_string().contains("304"));
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn stealth_v3_revalidation_new_200_replaces_the_original_request_context() {
    use donsetch::fetch::client::CacheState;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/owned", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(tcp.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap();
            let count = {
                let mut log = log.lock().unwrap();
                log.push(head);
                log.len()
            };
            let (body, age, tag) = if count == 1 {
                ("old representation", 0, "v1")
            } else {
                ("new representation", 600, "v2")
            };
            tcp.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: max-age={age}\r\nETag: \"{tag}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    assert_eq!(
        fetcher.fetch(&url).await.unwrap().body,
        b"old representation"
    );
    assert_eq!(
        fetcher.fetch(&url).await.unwrap().body,
        b"new representation"
    );
    let fresh = fetcher.fetch(&url).await.unwrap();
    assert_eq!(fresh.body, b"new representation");
    assert_eq!(
        fresh.cache,
        CacheState::Fresh,
        "a conditional request returning 200 must replace the initial entry"
    );
    let log = seen.lock().unwrap().clone();
    assert_eq!(log.len(), 2);
    assert!(!log[0].contains("if-none-match:"));
    assert!(log[1].contains("if-none-match: \"v1\"\r\n"));
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn stealth_v3_cache_tracks_sent_cookie_and_referer_not_jar_mode() {
    use donsetch::fetch::client::CacheState;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/article", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(tcp.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap();
            log.lock().unwrap().push(head.clone());
            let body = if head.contains("cookie: session=alice\r\n") {
                "alice representation"
            } else if head.contains("referer: ") {
                "linked representation"
            } else {
                "anonymous representation"
            };
            tcp.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: max-age=600\r\nVary: Cookie, Referer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    let anonymous = fetcher.fetch(&url).await.unwrap();
    assert_eq!(anonymous.body, b"anonymous representation");
    assert_eq!(fetcher.fetch(&url).await.unwrap().cache, CacheState::Fresh);
    let cookie = donsetch::ghost::cache::CookieRecord {
        name: "session".into(),
        value: "alice".into(),
        domain: "127.0.0.1".into(),
        path: "/".into(),
        expires_at: None,
        secure: false,
        http_only: true,
        same_site: "Lax".into(),
    };
    fetcher.reset_to(std::slice::from_ref(&cookie)).await;
    let alice = fetcher.fetch(&url).await.unwrap();
    assert_eq!(
        alice.body, b"alice representation",
        "a login must not read the anonymous fresh entry"
    );
    assert_eq!(fetcher.fetch(&url).await.unwrap().cache, CacheState::Fresh);
    // Unsent cookies must not invalidate this origin's fresh representation.
    let unrelated = donsetch::ghost::cache::CookieRecord {
        domain: "other.invalid".into(),
        ..cookie.clone()
    };
    fetcher.import_cookies(&[unrelated]).await;
    assert_eq!(fetcher.fetch(&url).await.unwrap().cache, CacheState::Fresh);
    fetcher.reset_to(&[]).await;
    let logged_out = fetcher.fetch(&url).await.unwrap();
    assert_eq!(
        logged_out.body, b"anonymous representation",
        "logout must not serve Alice's body"
    );
    let linked = fetcher
        .fetch_via_jar_ref(&url, None, true, Some("https://referrer.test/path"))
        .await
        .unwrap();
    assert_eq!(
        linked.body, b"linked representation",
        "Vary Referer requires a distinct entry"
    );
    assert_eq!(
        fetcher
            .fetch_via_jar_ref(&url, None, true, Some("https://referrer.test/path"))
            .await
            .unwrap()
            .cache,
        CacheState::Fresh
    );
    let requests = seen.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        3,
        "each actual request context must warm exactly once"
    );
    assert!(requests[1].contains("cookie: session=alice\r\n"));
    assert!(requests[2].contains("referer: https://referrer.test/\r\n"));
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn stealth_v3_cache_tracks_exact_proxy_credentials_and_direct_route() {
    use donsetch::fetch::client::CacheState;
    use donsetch::transport::proxy::Proxy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(tcp.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap();
            let body = if head.contains("proxy-authorization: Basic YWxpY2U6b25l\r\n") {
                "alice exit"
            } else if head.contains("proxy-authorization: Basic Ym9iOnR3bw==\r\n") {
                "bob exit"
            } else {
                "direct exit"
            };
            seen.push(head);
            tcp.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: max-age=600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        seen
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    let url = format!("http://{addr}/article");
    for (credentials, body) in [
        ("alice:one", b"alice exit".as_slice()),
        ("bob:two", b"bob exit".as_slice()),
    ] {
        let proxy = Proxy::parse(&format!("http://{credentials}@{addr}")).unwrap();
        let response = fetcher
            .fetch_via_jar_ref(&url, Some(&proxy), false, None)
            .await
            .unwrap();
        assert_eq!(
            response.body, body,
            "fresh content must belong to this exact route"
        );
        assert_eq!(
            fetcher
                .fetch_via_jar_ref(&url, Some(&proxy), false, None)
                .await
                .unwrap()
                .cache,
            CacheState::Fresh
        );
    }
    let direct = fetcher
        .fetch_via_jar_ref(&url, None, false, None)
        .await
        .unwrap();
    assert_eq!(
        direct.body, b"direct exit",
        "direct must not inherit a proxy representation"
    );
    assert_eq!(
        fetcher
            .fetch_via_jar_ref(&url, None, false, None)
            .await
            .unwrap()
            .cache,
        CacheState::Fresh
    );
    let seen = tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen[..2].iter().all(|h| h.starts_with("GET http://")));
    assert!(seen[2].starts_with("GET /article "));
}

#[tokio::test]
async fn env_proxy_is_rechecked_against_no_proxy_per_hop() {
    crate::sandbox();
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
