//! v4 phase 1.2 over-the-wire proof: a subresource-class fetch
//! carries the subresource header set on the actual socket, not
//! the navigation set. The rig records the raw headers it receives.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;

fn serve_once() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut raw = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            raw.push_str(&line);
        }

        let body = b"body{}";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        sock.write_all(head.as_bytes()).unwrap();
        sock.write_all(body).unwrap();
        let _ = tx.send(raw);
    });
    (port, rx)
}

fn header<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    raw.lines().find_map(|l| {
        l.to_lowercase()
            .starts_with(&format!("{name}:"))
            .then(|| l[name.len() + 1..].trim())
    })
}

#[tokio::test]
async fn legacy_user_agent_is_single_and_request_local() {
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let fetcher =
        donsetch::fetch::client::Fetcher::new(donsetch::profile::BrowserProfile::host_default())
            .unwrap();
    let ua = donsetch::search::engines::google_wml::USER_AGENT;
    let (legacy_port, legacy_rx) = serve_once();
    let (normal_port, normal_rx) = serve_once();
    let legacy_url = format!("http://127.0.0.1:{legacy_port}/search");
    let normal_url = format!("http://127.0.0.1:{normal_port}/page");
    let (legacy, normal) = tokio::join!(
        fetcher.fetch_once_via_user_agent(&legacy_url, None, ua),
        fetcher.fetch_once_via(&normal_url, &[], None, false, None),
    );
    assert_eq!(legacy.unwrap().status, 200);
    assert_eq!(normal.unwrap().status, 200);
    let raw = legacy_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(header(&raw, "user-agent"), Some(ua));
    assert_eq!(
        raw.lines()
            .filter(|l| l.to_lowercase().starts_with("user-agent:"))
            .count(),
        1
    );
    assert!(!raw.to_lowercase().contains("sec-ch-ua"));
    assert!(!raw.to_lowercase().contains("sec-fetch-"));
    assert!(header(&raw, "cookie").is_none());
    let raw = normal_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        header(&raw, "user-agent"),
        Some(fetcher.profile().user_agent.as_str())
    );
    assert!(header(&raw, "sec-ch-ua").is_some());
}

#[tokio::test]
async fn legacy_user_agent_rejects_header_injection_before_network() {
    let fetcher =
        donsetch::fetch::client::Fetcher::new(donsetch::profile::BrowserProfile::host_default())
            .unwrap();
    for ua in ["", "Nokia\r\nCookie: injected", "Nokia\0", "Nokià"] {
        let error = fetcher
            .fetch_once_via_user_agent("not-a-url", None, ua)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("invalid User-Agent"));
    }
}

#[tokio::test]
async fn subresource_class_goes_out_on_the_wire() {
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let (port, rx) = serve_once();
    let fetcher =
        donsetch::fetch::client::Fetcher::new(donsetch::profile::BrowserProfile::host_default())
            .unwrap();
    let url = format!("http://127.0.0.1:{port}/style.css");
    // Same-origin page (same port): browser-true expectations are
    // sec-fetch-site same-origin and the FULL page URL as referer
    // (strict-origin-when-cross-origin: same-origin = full URL).
    let page = format!("http://127.0.0.1:{port}/page");
    let out = fetcher
        .fetch_once_via_class(
            &url,
            &[],
            None,
            false,
            Some(&page),
            donsetch::profile::RequestClass::Style,
        )
        .await
        .unwrap();
    assert_eq!(out.status, 200);
    let raw = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(header(&raw, "sec-fetch-dest"), Some("style"), "{raw}");
    assert_eq!(header(&raw, "sec-fetch-mode"), Some("no-cors"), "{raw}");
    assert_eq!(header(&raw, "sec-fetch-site"), Some("same-origin"), "{raw}");
    assert!(
        header(&raw, "accept").unwrap().starts_with("text/css"),
        "{raw}"
    );
    assert_eq!(header(&raw, "referer"), Some(page.as_str()), "{raw}");
    assert!(header(&raw, "upgrade-insecure-requests").is_none(), "{raw}");
    assert!(header(&raw, "sec-fetch-user").is_none(), "{raw}");
}

#[tokio::test]
async fn navigation_class_stays_the_historical_set() {
    unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
    let (port, rx) = serve_once();
    let fetcher =
        donsetch::fetch::client::Fetcher::new(donsetch::profile::BrowserProfile::host_default())
            .unwrap();
    let url = format!("http://127.0.0.1:{port}/page");
    fetcher
        .fetch_once_via(&url, &[], None, false, None)
        .await
        .unwrap();
    let raw = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(header(&raw, "sec-fetch-dest"), Some("document"), "{raw}");
    assert_eq!(header(&raw, "sec-fetch-mode"), Some("navigate"), "{raw}");
    assert_eq!(header(&raw, "sec-fetch-user"), Some("?1"), "{raw}");
    assert_eq!(
        header(&raw, "upgrade-insecure-requests"),
        Some("1"),
        "{raw}"
    );
}
