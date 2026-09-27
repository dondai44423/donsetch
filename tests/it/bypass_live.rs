//! End-to-end unlock() pipeline against a local fake Bright Data
//! API. This exercises the REAL client, payload, header parsing,
//! retry ladder and solve-cache without spending a cent: the
//! endpoint override points at 127.0.0.1 and the fake server plays
//! the documented Web Unlocker response shapes (legacy JSON wrapper
//! and the current x-brd-status-code header contract).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use donsetch::fetch::bypass::{BypassConfig, BypassFail, unlock};

fn spin<F: Fn(&str) -> String + Send + Sync + 'static>(handler: F) -> (String, Arc<AtomicUsize>) {
    // A couple of retries: parallel tests can exhaust the backlog.
    let listener = (0..5)
        .find_map(|_| TcpListener::bind("127.0.0.1:0").ok())
        .expect("no free port");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            hits2.fetch_add(1, Ordering::SeqCst);
            let mut s = stream;
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let spec = handler(&req);
            // Layout: status int line, optional headers, blank
            // line, then the raw body.
            let mut lines = spec.splitn(2, "\n\n");
            let head = lines.next().unwrap_or("200");
            let mut hlines = head.split('\n');
            let status = hlines
                .next()
                .and_then(|l| l.parse::<u16>().ok())
                .unwrap_or(200);
            let extra_headers: Vec<&str> = hlines.collect();
            let body = lines.next().unwrap_or("");
            let need_cap = !extra_headers
                .iter()
                .any(|h| h.to_lowercase().starts_with("content-length"));
            let length_hdr = if need_cap {
                format!("Content-Length: {}\r\n", body.len())
            } else {
                String::new()
            };
            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
                429 => "Too Many Requests",
                _ => "OK",
            };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\n{}{}Connection: close\r\n\r\n{body}",
                extra_headers
                    .iter()
                    .map(|h| format!("{h}\r\n"))
                    .collect::<String>(),
                length_hdr
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    (format!("http://{addr}/request"), hits)
}

fn cfg_with_endpoint(endpoint: &str) -> BypassConfig {
    BypassConfig {
        endpoint: endpoint.to_string(),
        cache_ttl: std::time::Duration::from_secs(60),
        ..BypassConfig::default()
    }
}

fn tmp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("donsetch-bypass-live-{tag}"))
}

async fn run_one(
    ep: &str,
    url: &str,
    dir: &std::path::Path,
) -> (donsetch::fetch::bypass::BypassOutcome, u32) {
    let cfg = cfg_with_endpoint(ep);
    let out = unlock("test-tok", url, &cfg, dir).await.unwrap();
    let counter: u32 = std::fs::read_to_string(donsetch::fetch::bypass::bypass_count_path(dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    (out, counter)
}

#[tokio::test]
async fn unlock_legacy_json_wrapper() {
    crate::sandbox();
    let dir = tmp_dir("legacy");
    let _ = std::fs::remove_dir_all(&dir);
    let (ep, _hits) = spin(|req| {
        assert!(req.contains("POST /request"), "{req}");
        assert!(req.contains("\"zone\":\"web_unlocker1\""), "{req}");
        assert!(
            req.contains("\"url\":\"https://walled.example/a\""),
            "{req}"
        );
        let body = "200\n\n{\"status\":200,\"headers\":{\"content-type\":\"text/html\"},\"body\":\"<html>solved</html>\"}";
        eprintln!("LEGACY-HANDLER spec: {body}");
        body.to_string()
    });
    let (outcome, _) = run_one(&ep, "https://walled.example/a", &dir).await;
    assert_eq!(outcome.status, 200);
    assert_eq!(outcome.body, b"<html>solved</html>");
    assert_eq!(outcome.content_type, "text/html");
    assert!(!outcome.cached);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unlock_header_contract() {
    crate::sandbox();
    // Current docs: outer 200, target status in x-brd-status-code
    // header, body still in the JSON wrapper.
    let dir = tmp_dir("header");
    let _ = std::fs::remove_dir_all(&dir);
    let (ep, _hits) =
        spin(|_req| "200\nx-brd-status-code: 200\n\n{\"headers\":{},\"body\":\"hi\"}".to_string());
    let (outcome, _) = run_one(&ep, "https://walled.example/b", &dir).await;
    assert_eq!(outcome.status, 200);
    assert_eq!(outcome.body, b"hi");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unlock_auth_rejection_maps_to_api() {
    crate::sandbox();
    let dir = tmp_dir("auth");
    let _ = std::fs::remove_dir_all(&dir);
    let (ep, hits) = spin(|req| {
        let lower = req.to_lowercase();
        assert!(
            lower.contains("authorization: bearer test-tok"),
            "auth header missing: {req}"
        );
        "401\n\n{\"error\":\"user is not authorized\"}".to_string()
    });
    let err = run_one_err(&ep, "https://walled.example/c", &dir).await;
    assert!(
        matches!(err, BypassFail::Api { status: 401, .. }),
        "{err:?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "auth failures never retry");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unlock_transient_solve_failure_retries_once_then_succeeds() {
    crate::sandbox();
    let dir = tmp_dir("retry");
    let _ = std::fs::remove_dir_all(&dir);
    let (ep, hits) = spin({
        let first = Arc::new(AtomicBool::new(true));
        move |_req| {
            if first.swap(false, Ordering::SeqCst) {
                "200\nx-brd-status-code: 502\nx-brd-error-code: reject_block\nx-brd-error: challenge blocked\n\n"
                    .to_string()
            } else {
                "200\nx-brd-status-code: 200\n\n{\"headers\":{},\"body\":\"second try\"}"
                    .to_string()
            }
        }
    });
    let (outcome, counter_after) = run_one(&ep, "https://walled.example/d", &dir).await;
    assert_eq!(outcome.body, b"second try");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one retry for reject_block");
    // The retry must not re-bill the daily counter: one unlock()
    // call = one cap unit even when the network retry doubles up.
    assert_eq!(
        counter_after, 1,
        "one unlock call consumes exactly one cap unit"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unlock_zone_not_found_is_config() {
    crate::sandbox();
    let dir = tmp_dir("zone");
    let _ = std::fs::remove_dir_all(&dir);
    let (ep, hits) = spin(|_req| "400\n\nzone \"mcp_unlocker\" not found".to_string());
    let err = run_one_err(&ep, "https://walled.example/e", &dir).await;
    assert!(matches!(err, BypassFail::Config(_)), "got {err:?}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn solve_cache_second_hit_never_calls_api() {
    crate::sandbox();
    let dir = tmp_dir("cache");
    let _ = std::fs::remove_dir_all(&dir);
    let (ep, hits) = spin(|_req| {
        "200\n\n{\"status\":200,\"headers\":{\"content-type\":\"text/html\"},\"body\":\"<html>once</html>\"}"
            .to_string()
    });
    run_one(&ep, "https://walled.example/cached", &dir).await;
    let (outcome2, _) = run_one(&ep, "https://walled.example/cached", &dir).await;
    assert_eq!(outcome2.body, b"<html>once</html>");
    assert!(outcome2.cached);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "cache hit = zero API calls");
    let _ = std::fs::remove_dir_all(&dir);
}

async fn run_one_err(ep: &str, url: &str, dir: &std::path::Path) -> BypassFail {
    let cfg = cfg_with_endpoint(ep);
    unlock("test-tok", url, &cfg, dir).await.unwrap_err()
}
