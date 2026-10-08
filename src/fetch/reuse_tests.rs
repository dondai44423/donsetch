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
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
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

/// Accept one connection from whichever family the resolver picked.
async fn accept_any(
    v4: &tokio::net::TcpListener,
    v6: Option<&tokio::net::TcpListener>,
) -> TcpStream {
    match v6 {
        Some(v6) => {
            tokio::select! {
                r = v4.accept() => r.unwrap().0,
                r = v6.accept() => r.unwrap().0,
            }
        }
        None => v4.accept().await.unwrap().0,
    }
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
    // The URL host must stay `localhost` (the owned certificate's SAN),
    // so the dial follows the resolver: mirror each port on [::1] when
    // the stack allows it - a runner whose `localhost` answers ::1 only
    // would otherwise refuse the primary family with no IPv4 fallback.
    let first6 = tokio::net::TcpListener::bind(("::1", ports[0])).await.ok();
    let second6 = tokio::net::TcpListener::bind(("::1", ports[1])).await.ok();
    let server = tokio::spawn(async move {
        let mut resumed = Vec::new();
        // One TLS context deliberately serves both ports, so an incorrectly
        // shared client ticket would be accepted and observable at the peer.
        for (v4, v6) in [(first, first6), (second, second6)] {
            let tcp = accept_any(&v4, v6.as_ref()).await;
            let mut tls = tokio_boring::accept(&acceptor, tcp).await.unwrap();
            resumed.push(tls.ssl().session_reused());
            start_h2(&mut tls, b"owned-port").await;
        }
        resumed
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    for port in ports {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fetcher.fetch(&format!("https://localhost:{port}/")),
        )
        .await
        .unwrap()
        .unwrap();
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
        // The client fetches alice, then bob: serve strictly in that
        // order. The old select raced bob's accept against alice's
        // remaining frames and, on slower peers, returned before the
        // second tunnel existed (connection refused for the second
        // fetch).
        let mut last = "none";
        for (expected, body, label) in [
            (
                "Proxy-Authorization: Basic YWxpY2U6b25l\r\n",
                b"lane-alice".as_slice(),
                "alice",
            ),
            (
                "Proxy-Authorization: Basic Ym9iOnR3bw==\r\n",
                b"lane-bob".as_slice(),
                "bob",
            ),
        ] {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let head = http_head(&mut tcp).await;
            assert!(head.contains(expected));
            tcp.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut tls = tokio_boring::accept(&acceptor, tcp).await.unwrap();
            start_h2(&mut tls, body).await;
            last = label;
        }
        last
    });
    let fetcher = Fetcher::new(BrowserProfile::chrome_150(Platform::Linux)).unwrap();
    for (credentials, body) in [
        ("alice:one", b"lane-alice".as_slice()),
        ("bob:two", b"lane-bob".as_slice()),
    ] {
        let proxy = Proxy::parse(&format!("http://{credentials}@127.0.0.1:{port}")).unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fetcher.fetch_via_jar_opts(
                "https://localhost/",
                Some(&proxy),
                false,
                None,
                true,
                false,
                None,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.alpn, "h2");
        assert_eq!(
            response.body, body,
            "proxy credentials selected a different egress identity"
        );
    }
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap(),
        "bob"
    );
}
