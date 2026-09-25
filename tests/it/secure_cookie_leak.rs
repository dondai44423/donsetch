//! Live regression: a cookie jar that honors the `Secure`
//! attribute, end to end.
//!
//! The report (GHSA draft, mnaza) observed the tier-1 jar drops the
//! secure flag when it parses Set-Cookie and when it imports
//! harvested cookies, so a Secure cookie replays over plain HTTP.
//! This test runs a real TCP server and a real Fetcher: /set hands
//! out a Secure session cookie, /check reports whether the cookie
//! came back on a plain-HTTP request. Pre-fix this test fails with
//! `LEAK:...` in the /check body; post-fix it is `CLEAN`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use donsetch::fetch::client::Fetcher;
use donsetch::profile::{BrowserProfile, Platform};

fn serve(listener: TcpListener, saw_secret: Arc<AtomicBool>) {
    for idx in 0..2 {
        let (mut conn, _) = listener.accept().expect("accept");
        let mut req = [0u8; 4096];
        let n = conn.read(&mut req).expect("read request");
        let req = String::from_utf8_lossy(&req[..n]);
        let line = req.lines().next().unwrap_or_default();
        let (path, cookie_header) = {
            let mut cookie = String::new();
            for l in req.lines() {
                let ll = l.to_ascii_lowercase();
                if ll.starts_with("cookie:") {
                    cookie = l[7..].trim().to_string();
                }
            }
            (line.split_whitespace().nth(1).unwrap_or("/"), cookie)
        };
        let (body, set_cookie) = match (idx, path) {
            (0, "/set") => ("set", Some("Set-Cookie: sess=SECRET; Secure; Path=/\r\n")),
            (1, "/check") => (
                if cookie_header.contains("sess=SECRET") {
                    saw_secret.store(true, Ordering::SeqCst);
                    "LEAK:sess=SECRET"
                } else {
                    "CLEAN"
                },
                None,
            ),
            _ => ("unexpected", None),
        };
        let extra = set_cookie.unwrap_or("");
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
            body.len(),
            extra,
            body
        );
        conn.write_all(resp.as_bytes()).expect("write response");
    }
}

#[tokio::test]
async fn secure_cookie_never_leaks_over_plain_http() {
    crate::sandbox();
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    // Drop the guard before any await: the fetch runs on the
    // runtime and must not hold the env lock across it.
    {
        let _guard = ENV_LOCK.lock().unwrap();
        // 127.0.0.1 is normally rejected by the egress guard; the
        // documented hatch re-enables private egress for this test.
        unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let saw_secret = Arc::new(AtomicBool::new(false));
    let srv_flag = saw_secret.clone();
    let handle = std::thread::spawn(move || serve(listener, srv_flag));

    let profile = BrowserProfile::chrome_150(Platform::Linux);
    let fetcher = Fetcher::new(profile).expect("fetcher");
    let base = format!("http://127.0.0.1:{port}");

    let set_out = fetcher
        .fetch(&format!("{base}/set"))
        .await
        .expect("fetch /set");
    assert_eq!(set_out.status, 200, "setup fetch failed");
    let check_out = fetcher
        .fetch(&format!("{base}/check"))
        .await
        .expect("fetch /check");
    handle.join().expect("server join");

    let body = String::from_utf8_lossy(&check_out.body);
    assert!(
        !body.contains("LEAK") && body.contains("CLEAN"),
        "plain-HTTP response leaked the Secure cookie: {body}"
    );
    assert!(
        !saw_secret.load(Ordering::SeqCst),
        "server saw the Secure cookie on a plain-HTTP request"
    );
    unsafe { std::env::remove_var("DONSETCH_ALLOW_PRIVATE_EGRESS") };
}
