//! Owned peers prove that transport reuse preserves scheme and proxy identity.

use super::client::Fetcher;
use crate::profile::{BrowserProfile, Platform};
use crate::transport::h2::frame::*;
use crate::transport::proxy::Proxy;
use boring::ssl::{AlpnError, SslAcceptor, SslMethod};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_boring::SslStream;

fn owned_acceptor() -> SslAcceptor {
    // Both names are acceptable hosts for these tests: the literal IPv4
    // keeps the dial deterministic on every platform (no resolver in the
    // path), and `localhost` stays covered for the resolver-path test.
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let directory = crate::paths::cache_dir().join("owned-reuse");
    std::fs::create_dir(&directory).unwrap();
    let bundle = directory.join("ca.pem");
    std::fs::write(&bundle, cert.cert.pem()).unwrap();
    // These tests require nextest's per-test process isolation. Only owned
    // loopback traffic and this generated certificate are permitted here.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &bundle);
        std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1");
        std::env::set_var("DONSETCH_NO_ENV_PROXY", "1");
    }
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor
        .set_certificate(&boring::x509::X509::from_der(cert.cert.der()).unwrap())
        .unwrap();
    acceptor
        .set_private_key(
            &boring::pkey::PKey::private_key_from_der(&cert.signing_key.serialize_der()).unwrap(),
        )
        .unwrap();
    acceptor.set_alpn_select_callback(|_, protocols| {
        boring::ssl::select_next_proto(b"\x02h2", protocols).ok_or(AlpnError::NOACK)
    });
    acceptor.build()
}

async fn start_h2(stream: &mut SslStream<TcpStream>, body: &[u8]) {
    let mut preface = [0; 24];
    stream.read_exact(&mut preface).await.unwrap();
    assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let id = loop {
        let (header, _) = read_frame(stream).await.unwrap();
        if header.ty == HEADERS {
            break header.stream_id;
        }
    };
    write_frame(stream, SETTINGS, 0, 0, &[]).await.unwrap();
    write_frame(stream, SETTINGS, FLAG_ACK, 0, &[])
        .await
        .unwrap();
    answer_h2(stream, id, body).await;
}

async fn answer_h2(stream: &mut SslStream<TcpStream>, id: u32, body: &[u8]) {
    write_frame(stream, HEADERS, FLAG_END_HEADERS, id, &[0x88])
        .await
        .unwrap();
    write_frame(stream, DATA, FLAG_END_STREAM, id, body)
        .await
        .unwrap();
    stream.flush().await.unwrap();
}

async fn http_head(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(
            bytes.len() < 8192,
            "owned fixture request exceeded its bound"
        );
        bytes.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
}

/// Close a served peer without provoking a Windows RST: read whatever the
/// client left inbound (bounded) and close with a TLS shutdown. A close
/// with unread inbound data answers RST on Windows, which surfaces at the
/// client as an abort or retry instead of a clean end of stream.
async fn quiet_close(tls: &mut SslStream<TcpStream>) {
    let _ = tokio::time::timeout(std::time::Duration::from_millis(150), async {
        let mut buf = [0u8; 64];
        while matches!(tls.read(&mut buf).await, Ok(n) if n > 0) {}
    })
    .await;
    let _ = tls.shutdown().await;
}

/// Accept whichever listener receives the next dial.
async fn accept_either(listeners: &[tokio::net::TcpListener; 2]) -> TcpStream {
    tokio::select! {
        r = listeners[0].accept() => r.unwrap().0,
        r = listeners[1].accept() => r.unwrap().0,
    }
}

/// Accept the next dial, or give up after the idle window - the bounded
/// drain that serves a late (retried) dial instead of letting it land on
/// a closed port as a refusal.
async fn accept_either_idle(
    listeners: &[tokio::net::TcpListener; 2],
    idle: std::time::Duration,
) -> Option<TcpStream> {
    tokio::time::timeout(idle, async {
        tokio::select! {
            r = listeners[0].accept() => r.unwrap().0,
            r = listeners[1].accept() => r.unwrap().0,
        }
    })
    .await
    .ok()
}

#[tokio::test]
async fn stealth_v3_transport_tls_tickets_do_not_cross_origin_ports() {
    let acceptor = owned_acceptor();
    let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ports = [
        first.local_addr().unwrap().port(),
        second.local_addr().unwrap().port(),
    ];
    // Literal-host URLs dial IPv4 directly on every platform; both
    // listeners stay bound for the whole task and a bounded drain serves
    // any late dial, so a transient peer abort plus retry can never land
    // on a closed port as a refusal.
    let listeners = [first, second];
    let server = tokio::spawn(async move {
        let mut resumed = Vec::new();
        // One TLS context deliberately serves both ports, so an incorrectly
        // shared client ticket would be accepted and observable at the peer.
        for _ in 0..2 {
            let tcp = accept_either(&listeners).await;
            let mut tls = tokio_boring::accept(&acceptor, tcp).await.unwrap();
            resumed.push(tls.ssl().session_reused());
            start_h2(&mut tls, b"owned-port").await;
            quiet_close(&mut tls).await;
        }
        while let Some(tcp) =
            accept_either_idle(&listeners, std::time::Duration::from_millis(1200)).await
        {
            if let Ok(mut tls) = tokio_boring::accept(&acceptor, tcp).await {
                start_h2(&mut tls, b"owned-port").await;
                quiet_close(&mut tls).await;
            }
        }
        resumed
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    for port in ports {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fetcher.fetch(&format!("https://127.0.0.1:{port}/")),
        )
        .await
        .unwrap_or_else(|_| panic!("fetch of port {port} timed out"))
        .unwrap_or_else(|e| panic!("fetch of port {port} failed: {e}"));
        assert_eq!(response.body, b"owned-port");
    }
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap(),
        [false, false],
        "a ticket for one origin port must not be offered to another"
    );
}

#[tokio::test]
async fn stealth_v3_transport_plain_http_never_rides_a_pooled_tls_connection() {
    let acceptor = owned_acceptor();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut tls = tokio_boring::accept(&acceptor, tcp).await.unwrap();
        start_h2(&mut tls, b"tls-owned").await;
        loop {
            tokio::select! {
                frame = read_frame(&mut tls) => {
                    let (header, _) = frame.unwrap();
                    if header.ty == HEADERS {
                        answer_h2(&mut tls, header.stream_id, b"wrong-tls-origin").await;
                        return "tls";
                    }
                }
                accepted = listener.accept() => {
                    let (mut plain, _) = accepted.unwrap();
                    assert!(http_head(&mut plain).await.starts_with("GET /plain HTTP/1.1\r\n"));
                    plain.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nplain-owned").await.unwrap();
                    return "plain";
                }
            }
        }
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fetcher.fetch(&format!("https://localhost:{port}/tls")),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.alpn, "h2");
    assert_eq!(first.body, b"tls-owned");
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fetcher.fetch(&format!("http://localhost:{port}/plain")),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        second.alpn, "h1",
        "plaintext URL must not use a TLS H2 stream"
    );
    assert_eq!(second.body, b"plain-owned");
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap(),
        "plain"
    );
}

#[tokio::test]
async fn stealth_v3_transport_new_proxy_credentials_require_their_own_tunnel() {
    let acceptor = owned_acceptor();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        // Serve each tunnel by the credentials its CONNECT carried and
        // record the observed head. The bounded idle window keeps an extra
        // dial (a peer-side retry after a transient abort) from panicking
        // the fixture - the property is asserted on the recording instead
        // of inside the peer loop, so a failure reports the full wire.
        let mut tunnels: Vec<(String, String)> = Vec::new();
        loop {
            let accepted =
                tokio::time::timeout(std::time::Duration::from_millis(2500), listener.accept())
                    .await;
            let (mut tcp, _) = match accepted {
                Ok(Ok(conn)) => conn,
                _ => break,
            };
            let head = http_head(&mut tcp).await;
            let who = if head.contains("Proxy-Authorization: Basic YWxpY2U6b25l") {
                "alice"
            } else if head.contains("Proxy-Authorization: Basic Ym9iOnR3bw==") {
                "bob"
            } else {
                "unknown"
            };
            tunnels.push((who.to_string(), head));
            tcp.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut tls = tokio_boring::accept(&acceptor, tcp).await.unwrap();
            let body: &[u8] = match who {
                "alice" => b"lane-alice".as_slice(),
                "bob" => b"lane-bob".as_slice(),
                _ => b"lane-unknown".as_slice(),
            };
            start_h2(&mut tls, body).await;
            quiet_close(&mut tls).await;
        }
        tunnels
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    let cases = [
        ("alice:one", b"lane-alice".as_slice()),
        ("bob:two", b"lane-bob".as_slice()),
    ];
    let mut observed: Vec<(String, String, Vec<u8>)> = Vec::new();
    for (credentials, _) in cases.iter() {
        let proxy = Proxy::parse(&format!("http://{credentials}@127.0.0.1:{port}")).unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fetcher.fetch_via_jar_opts(
                "https://127.0.0.1/",
                Some(&proxy),
                false,
                None,
                true,
                false,
                None,
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("fetch via {credentials} timed out"))
        .unwrap_or_else(|e| panic!("fetch via {credentials} failed: {e}"));
        observed.push((
            credentials.to_string(),
            response.alpn.clone(),
            response.body.clone(),
        ));
    }
    let tunnels = tokio::time::timeout(std::time::Duration::from_secs(6), server)
        .await
        .expect("the owned peer did not finish")
        .expect("the owned peer panicked");
    for ((credentials, alpn, body), (_, expected)) in observed.iter().zip(cases.iter()) {
        assert_eq!(
            alpn, "h2",
            "fetch via {credentials} negotiated {alpn}; tunnels: {tunnels:?}"
        );
        assert_eq!(
            body.as_slice(),
            *expected,
            "fetch via {credentials} received {:?}; tunnels: {tunnels:?}",
            String::from_utf8_lossy(body)
        );
    }
    let alice_at = tunnels.iter().position(|(who, _)| who == "alice");
    let bob_at = tunnels.iter().position(|(who, _)| who == "bob");
    assert!(
        alice_at.is_some(),
        "no CONNECT carried the alice credentials: {tunnels:?}"
    );
    assert!(
        bob_at.is_some(),
        "no CONNECT carried the bob credentials: {tunnels:?}"
    );
    assert!(
        alice_at < bob_at,
        "alice's tunnel must precede bob's: {tunnels:?}"
    );
}

/// Keep at least one short pending timer alive for the whole test.
/// With paused time, tokio auto-advances to the NEXT pending timer
/// whenever the runtime parks - it does so even while waiting on real
/// socket I/O - so a park with no nearer timer jumps the virtual clock
/// to whatever deadline is pending. A metronome keeps every jump small;
/// the stall bound under test then fires only after 30 virtual seconds
/// without a byte, which is exactly the property under test.
fn metronome() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    })
}

/// Answer the request, then send `abc` and hold the connection open
/// forever: only the client's own stall bound may decide the outcome.
/// Every wire error is tolerated (the client is expected to abandon
/// this exchange), and each connection runs on its own task so one
/// abandoned connection cannot kill the listener a retry dials next.
async fn answer_h2_stalled(tls: &mut SslStream<TcpStream>) -> Result<(), ()> {
    let mut preface = [0; 24];
    tls.read_exact(&mut preface).await.map_err(|_| ())?;
    let id = loop {
        let (header, _) = read_frame(tls).await.map_err(|_| ())?;
        if header.ty == HEADERS {
            break header.stream_id;
        }
    };
    write_frame(tls, SETTINGS, 0, 0, &[])
        .await
        .map_err(|_| ())?;
    write_frame(tls, SETTINGS, FLAG_ACK, 0, &[])
        .await
        .map_err(|_| ())?;
    write_frame(tls, HEADERS, FLAG_END_HEADERS, id, &[0x88])
        .await
        .map_err(|_| ())?;
    write_frame(tls, DATA, 0, id, b"abc")
        .await
        .map_err(|_| ())?;
    tls.flush().await.map_err(|_| ())?;
    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    Ok(())
}

/// The bound that must survive the fix: a body that STOPS moving is
/// still timed out - a dead peer may not hold a fetch forever. (The
/// slow-but-moving direction cannot live in this suite: with paused
/// time the clock can outrun real socket I/O, so a real-time fixture
/// owns that receipt; the h1 side is pinned by the duplex tests in
/// transport::h1.)
#[tokio::test(start_paused = true)]
async fn stealth_v3_fetch_stalled_h2_body_still_times_out() {
    let _beat = metronome();
    let acceptor = owned_acceptor();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let Ok(mut tls) = tokio_boring::accept(&acceptor, tcp).await else {
                continue;
            };
            tokio::spawn(async move {
                let _ = answer_h2_stalled(&mut tls).await;
            });
        }
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        fetcher.fetch(&format!("https://127.0.0.1:{port}/dead")),
    )
    .await
    .unwrap_or_else(|_| panic!("the stall must be decided, not hung"))
    .err()
    .expect("a body that stops moving must time out");
    assert!(matches!(err, crate::error::FetchError::Timeout), "{err}");
    server.abort();
}
