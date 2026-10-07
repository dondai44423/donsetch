//! End-to-end proof for issue #154: a TLS-intercepting egress proxy.
//!
//! The test stands up a real MITM in-process: an HTTP CONNECT proxy
//! that re-signs the connection with its own CA for a fake host
//! (`console.example`), exactly like a corp/cloud sandbox egress.
//! Three assertions on the real Fetcher:
//!   1. env-proxy honored + interception-safe handshake + the CA
//!      bundle from SSL_CERT_FILE => 200 through the tunnel;
//!   2. the same tunnel WITHOUT the CA trusted => an honest
//!      certificate-verification failure, nothing hanging or vague;
//!   3. plaintext http:// through the proxy uses absolute-form
//!      request targets.
//!
//! CI note: env mutations are process-scoped and nextest gives each
//! test its own process, so tests cannot race each other here.

use std::net::SocketAddr;
use std::sync::Arc;

use donsetch::fetch::client::Fetcher;
use donsetch::profile::BrowserProfile;
use rustls_pki_types::PrivateKeyDer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

/// Env vars are process-global and tokio runs tests in parallel:
/// two tests setting HTTPS_PROXY concurrently would race each
/// other onto different MITM ports. One lock serializes the file.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Mitm {
    addr: SocketAddr,
    ca_pem: Vec<u8>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn spawn_mitm() -> Mitm {
    run_mitm().await
}

async fn run_mitm() -> Mitm {
    // Self-signed leaf for console.example: trusting the LEAF itself via
    // SSL_CERT_FILE is the smallest trust closure that exercises every
    // stage of the interception path (bundle load, verify, tunnel).
    let certified = rcgen::generate_simple_self_signed(vec!["console.example".to_string()])
        .expect("self signed");

    // rustls crypto provider: installed once here.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let key_der = PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key_der)
        .expect("server config");
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            let seen = seen2.clone();
            tokio::spawn(async move {
                // 1. Read the CONNECT head (or absolute-form http).
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match tcp.read(&mut byte).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => head.push(byte[0]),
                    }
                    if head.len() > 8192 {
                        return;
                    }
                }
                let head_s = String::from_utf8_lossy(&head).to_string();
                seen.lock().unwrap().push(head_s.clone());

                if head_s.starts_with("CONNECT") {
                    // Assert CONNECT carries the right authority.
                    tcp.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await
                        .ok();
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    serve_one(tls).await;
                } else if head_s.starts_with("GET http://") || head_s.starts_with("POST http://") {
                    // Absolute-form plaintext : respond in the clear.
                    tcp.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 10\r\nConnection: close\r\n\r\nplain-http",
                    )
                    .await
                    .ok();
                } else {
                    tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
                        .await
                        .ok();
                }
            });
        }
    });

    Mitm {
        addr,
        ca_pem: certified.cert.pem().into_bytes(),
        seen,
    }
}

async fn serve_one(mut tls: tokio_rustls::server::TlsStream<TcpStream>) {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        match tls.read(&mut byte).await {
            Ok(0) | Err(_) => return,
            Ok(_) => buf.push(byte[0]),
        }
        if buf.len() > 8192 {
            return;
        }
    }
    tls.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 12\r\nConnection: close\r\n\r\nthrough-mitm",
    )
    .await
    .ok();
}

fn set_env_https_proxy(mitm: &Mitm) {
    // SAFETY: each nextest test runs in its own process; no other
    // thread reads these vars concurrently in this test binary.
    unsafe {
        std::env::set_var("HTTPS_PROXY", format!("http://{}", mitm.addr));
        std::env::set_var("ALL_PROXY", format!("http://{}", mitm.addr));
        // Test-only: the fake host cannot resolve, and the guard is
        // otherwise fail-closed (that is the whole point of it).
        std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1");
    }
}

fn write_ca_bundle(pem: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("donsetch-mitm-ca-{}.pem", std::process::id()));
    std::fs::write(&path, pem).expect("write ca");
    path
}

#[tokio::test(flavor = "multi_thread")]
// The guard deliberately spans the test: process-global env vars
// must stay pinned for its whole body.
#[allow(clippy::await_holding_lock)]
async fn fetch_via_intercepting_proxy_succeeds_with_trusted_ca() {
    crate::sandbox();
    let _env = ENV_LOCK.lock().unwrap();
    let mitm = spawn_mitm().await;
    set_env_https_proxy(&mitm);
    let ca_path = write_ca_bundle(&mitm.ca_pem);
    // SAFETY: process-scoped, single-threaded with respect to env.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &ca_path);
        std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1");
    }

    let fetcher = Fetcher::new(BrowserProfile::chrome_150(
        donsetch::profile::Platform::Linux,
    ))
    .expect("fetcher builds");
    let out = fetcher
        .fetch("https://console.example/ok")
        .await
        .expect("fetch through MITM");
    assert_eq!(out.status, 200);
    assert_eq!(String::from_utf8_lossy(&out.body), "through-mitm");
    let seen = mitm.seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|h| h.starts_with("CONNECT console.example:443")),
        "expected CONNECT line, saw: {seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::await_holding_lock)]
async fn fetch_via_intercepting_proxy_fails_honestly_without_ca() {
    crate::sandbox();
    let _env = ENV_LOCK.lock().unwrap();
    let mitm = spawn_mitm().await;
    set_env_https_proxy(&mitm);

    // An unrelated cert: the re-signed chain must NOT verify.
    let other = rcgen::generate_simple_self_signed(vec!["unrelated.example".to_string()])
        .expect("self signed");
    let ca_path = write_ca_bundle(other.cert.pem().as_bytes());
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &ca_path);
    }

    let fetcher = Fetcher::new(BrowserProfile::chrome_150(
        donsetch::profile::Platform::Linux,
    ))
    .expect("fetcher builds");
    let err = match fetcher.fetch("https://console.example/ok").await {
        Ok(out) => panic!(
            "must refuse an untrusted MITM chain, got HTTP {}",
            out.status
        ),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("certificate verification failed")
            || msg.to_lowercase().contains("unknown ca")
            || msg.to_lowercase().contains("certificate")
            || msg.contains("untrusted"),
        "honest verification failure expected, got: {msg}"
    );
}

// The same env-proxy contract has to hold on the tier-1 persona lane,
// which is what the MCP `web_fetch` tool and the CLI `fetch`/`search`
// commands ride. It used to pass `None` down to the redirect driver, so
// on a host whose only route out is HTTP_PROXY (a container, a locked
// down office network) every tool call died with "Network is
// unreachable" while `donsetch doctor` and the archive fallback, which
// ride `fetch()`, reported egress healthy: two call chains into the
// same dialer and only one of them resolved the ambient proxy.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::await_holding_lock)]
async fn persona_fetch_uses_the_env_proxy_like_fetch_does() {
    crate::sandbox();
    let _env = ENV_LOCK.lock().unwrap();
    let mitm = spawn_mitm().await;
    set_env_https_proxy(&mitm);

    let fetcher = Fetcher::new(BrowserProfile::chrome_150(
        donsetch::profile::Platform::Linux,
    ))
    .expect("fetcher builds");
    let out = fetcher
        .fetch_persona("http://console.example/persona", None)
        .await
        .expect("persona fetch through the env proxy");
    assert_eq!(String::from_utf8_lossy(&out.body), "plain-http");
    let seen = mitm.seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|h| h.starts_with("GET http://console.example/persona ")),
        "the persona lane must ride the env proxy, saw: {seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::await_holding_lock)]
async fn plaintext_http_through_env_proxy_uses_absolute_form() {
    crate::sandbox();
    let _env = ENV_LOCK.lock().unwrap();
    let mitm = spawn_mitm().await;
    set_env_https_proxy(&mitm);

    let fetcher = Fetcher::new(BrowserProfile::chrome_150(
        donsetch::profile::Platform::Linux,
    ))
    .expect("fetcher builds");
    let out = fetcher
        .fetch("http://console.example/plain")
        .await
        .expect("plaintext through proxy");
    assert_eq!(String::from_utf8_lossy(&out.body), "plain-http");
    let seen = mitm.seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|h| h.starts_with("GET http://console.example/plain ")),
        "expected absolute-form GET, saw: {seen:?}"
    );
}

// The raw absolute-form hop has no CONNECT to carry credentials:
// the request itself must send Proxy-Authorization, exactly like
// curl does for plaintext http:// through an authenticated proxy.
// Without it, every credentialed HTTP proxy (residential lanes are
// essentially always credentialed) answers 407.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::await_holding_lock)]
async fn plaintext_http_through_credentialed_proxy_sends_proxy_authorization() {
    crate::sandbox();
    let _env = ENV_LOCK.lock().unwrap();
    let mitm = spawn_mitm().await;
    // SAFETY: process-scoped, single-threaded with respect to env.
    unsafe {
        std::env::set_var("HTTP_PROXY", format!("http://u:p@{}", mitm.addr));
        std::env::set_var("ALL_PROXY", format!("http://u:p@{}", mitm.addr));
        std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1");
    }

    let fetcher = Fetcher::new(BrowserProfile::chrome_150(
        donsetch::profile::Platform::Linux,
    ))
    .expect("fetcher builds");
    let out = fetcher
        .fetch("http://console.example/plain")
        .await
        .expect("plaintext through credentialed proxy");
    assert_eq!(String::from_utf8_lossy(&out.body), "plain-http");
    let seen = mitm.seen.lock().unwrap();
    let head = seen
        .iter()
        .find(|h| h.starts_with("GET http://console.example/plain "))
        .expect("absolute-form GET reached the proxy");
    // base64("u:p") = "dTpw"
    assert!(
        head.to_lowercase()
            .contains("proxy-authorization: basic dtpw"),
        "credentialed hop must authenticate per-request, head was: {head}"
    );
}

/// Minimal in-process SOCKS5 "proxy": accepts the no-auth handshake
/// and then plays the ORIGIN itself (reads the HTTP head off the
/// tunnel, records it, answers 200). What it records is exactly what
/// the origin server would see arrive through a real tunnel.
async fn spawn_socks5() -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else {
                continue;
            };
            let seen = seen2.clone();
            tokio::spawn(async move {
                // Greeting: VER NMETHODS METHODS... -> pick no-auth.
                let mut hdr = [0u8; 2];
                if tcp.read_exact(&mut hdr).await.is_err() || hdr[0] != 0x05 {
                    return;
                }
                let mut methods = vec![0u8; hdr[1] as usize];
                if tcp.read_exact(&mut methods).await.is_err() {
                    return;
                }
                if tcp.write_all(&[0x05, 0x00]).await.is_err() {
                    return;
                }
                // CONNECT request: VER CMD RSV ATYP DST PORT.
                let mut req = [0u8; 4];
                if tcp.read_exact(&mut req).await.is_err() || req[3] != 0x03 {
                    return;
                }
                let mut len = [0u8; 1];
                if tcp.read_exact(&mut len).await.is_err() {
                    return;
                }
                let mut dst = vec![0u8; len[0] as usize + 2];
                if tcp.read_exact(&mut dst).await.is_err() {
                    return;
                }
                if tcp
                    .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await
                    .is_err()
                {
                    return;
                }
                // Tunnel established: now be the origin.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match tcp.read(&mut byte).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => head.push(byte[0]),
                    }
                    if head.len() > 8192 {
                        return;
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).to_string());
                tcp.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 11\r\nConnection: close\r\n\r\nsocks-plain",
                )
                .await
                .ok();
            });
        }
    });
    (addr, seen)
}

// A SOCKS5 hop is a transparent tunnel to the ORIGIN: the request
// line the origin sees must be origin-form ("GET /plain"), like
// every browser sends. Absolute-form belongs only on the raw
// HTTP-proxy hop; leaking it through the tunnel is a fingerprint
// (and picky origins/CDNs reject it).
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::await_holding_lock)]
async fn plaintext_http_through_socks5_uses_origin_form() {
    crate::sandbox();
    let _env = ENV_LOCK.lock().unwrap();
    let (addr, seen) = spawn_socks5().await;
    // SAFETY: process-scoped, single-threaded with respect to env.
    unsafe {
        std::env::set_var("HTTP_PROXY", format!("socks5://{addr}"));
        std::env::set_var("ALL_PROXY", format!("socks5://{addr}"));
        std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1");
    }

    let fetcher = Fetcher::new(BrowserProfile::chrome_150(
        donsetch::profile::Platform::Linux,
    ))
    .expect("fetcher builds");
    let out = fetcher
        .fetch("http://console.example/plain")
        .await
        .expect("plaintext through socks5");
    assert_eq!(String::from_utf8_lossy(&out.body), "socks-plain");
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().any(|h| h.starts_with("GET /plain ")),
        "origin-form GET expected through the tunnel, saw: {seen:?}"
    );
}
#[tokio::test]
async fn stealth_v3_invalid_ambient_proxy_never_dials_direct() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe {
        std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1");
        std::env::remove_var("DONSETCH_NO_ENV_PROXY");
        std::env::set_var(
            "HTTP_PROXY",
            "socks4://alice:private-token@broken.invalid:1080",
        );
        std::env::set_var("NO_PROXY", "");
        std::env::remove_var("no_proxy");
    }
    let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/must-stay-proxied", origin.local_addr().unwrap());
    let server = tokio::spawn(async move {
        if let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(std::time::Duration::from_millis(300), origin.accept()).await
        {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 16384);
                request.push(stream.read_u8().await.unwrap());
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 21\r\nConnection: close\r\n\r\ndirect origin reached").await.unwrap();
            true
        } else {
            false
        }
    });
    let fetcher =
        donsetch::fetch::client::Fetcher::new(donsetch::profile::BrowserProfile::host_default())
            .unwrap();
    let result = fetcher.fetch_persona(&url, None).await;
    let direct_reached = server.await.unwrap();
    assert!(
        !direct_reached,
        "invalid required proxy silently reached the direct origin"
    );
    let Err(error) = result else {
        panic!("invalid proxy must be an actionable error")
    };
    assert!(error.to_string().contains("proxy"));
    assert!(
        !error.to_string().contains("private-token") && !error.to_string().contains("alice"),
        "{error}"
    );
}

#[tokio::test]
async fn stealth_v3_invalid_ambient_proxy_keeps_explicit_bypass_and_lane_controls() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crate::sandbox();
    unsafe {
        std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1");
        std::env::remove_var("DONSETCH_NO_ENV_PROXY");
        std::env::set_var("HTTP_PROXY", "socks4://broken.invalid:1080");
        std::env::set_var("NO_PROXY", "127.0.0.1");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for ordinal in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 16384);
                request.push(stream.read_u8().await.unwrap());
            }
            let request = String::from_utf8(request).unwrap();
            let body = if ordinal == 0 {
                assert!(request.starts_with("GET /owned HTTP/1.1\r\n"));
                assert!(!request.contains("proxy-authorization:"));
                "explicit direct bypass"
            } else {
                assert!(request.starts_with(&format!("GET http://{address}/owned HTTP/1.1\r\n")));
                assert!(request.contains("proxy-authorization: Basic YWxpY2U6c2VjcmV0\r\n"));
                "explicit proxy lane"
            };
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: max-age=600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let fetcher =
        donsetch::fetch::client::Fetcher::new(donsetch::profile::BrowserProfile::host_default())
            .unwrap();
    let url = format!("http://{address}/owned");
    assert_eq!(
        fetcher.fetch_persona(&url, None).await.unwrap().body,
        b"explicit direct bypass"
    );
    unsafe { std::env::set_var("NO_PROXY", "") };
    let proxy = donsetch::transport::proxy::Proxy::parse(&format!("http://alice:secret@{address}"))
        .unwrap();
    assert_eq!(
        fetcher
            .fetch_via_jar_ref(&url, Some(&proxy), true, None)
            .await
            .unwrap()
            .body,
        b"explicit proxy lane"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}
