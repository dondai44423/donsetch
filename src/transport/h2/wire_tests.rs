//! Owned TLS peer: protocol assertions exercise the production H2 connection.

use super::conn::H2Conn;
use super::frame::*;
use crate::profile::{BrowserProfile, Platform};
use boring::ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_boring::SslStream;

async fn fixture<F, Fut>(profile: BrowserProfile, serve: F) -> (H2Conn, tokio::task::JoinHandle<()>)
where
    F: FnOnce(SslStream<TcpStream>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let cert = boring::x509::X509::from_pem(include_bytes!("../../../tests/landmarks/h2cert.pem"))
        .unwrap();
    let key = boring::pkey::PKey::private_key_from_pem(include_bytes!(
        "../../../tests/landmarks/h2key.pem"
    ))
    .unwrap();
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    acceptor.set_private_key(&key).unwrap();
    let acceptor = acceptor.build();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut tls = tokio_boring::accept(&acceptor, tcp).await.unwrap();
        let mut preface = [0; 24];
        tls.read_exact(&mut preface).await.unwrap();
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        assert_eq!(read_frame(&mut tls).await.unwrap().0.ty, SETTINGS);
        assert_eq!(read_frame(&mut tls).await.unwrap().0.ty, WINDOW_UPDATE);
        let (request, payload) = read_frame(&mut tls).await.unwrap();
        assert_eq!(request.ty, HEADERS);
        assert_eq!(request.stream_id, 1);
        assert_eq!(
            request.flags,
            FLAG_PRIORITY | FLAG_END_HEADERS | FLAG_END_STREAM
        );
        assert_eq!(&payload[..5], &[0x80, 0, 0, 0, 0xff]);
        let fields = super::hpack::Decoder::new().decode(&payload[5..]).unwrap();
        assert_eq!(
            fields
                .iter()
                .take(4)
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            [":method", ":authority", ":scheme", ":path"]
        );
        assert_eq!(
            &fields[..3],
            &[
                (":method".into(), "GET".into()),
                (":authority".into(), "localhost".into()),
                (":scheme".into(), "https".into())
            ]
        );
        write_frame(&mut tls, SETTINGS, 0, 0, &[]).await.unwrap();
        tls.flush().await.unwrap();
        let (ack, payload) = read_frame(&mut tls).await.unwrap();
        assert_eq!(
            (ack.ty, ack.flags, ack.stream_id, payload.len()),
            (SETTINGS, FLAG_ACK, 0, 0)
        );
        serve(tls).await;
    });
    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    // Trust bypass exists only for this self-signed owned loopback fixture.
    connector.set_verify(SslVerifyMode::NONE);
    let tcp = TcpStream::connect(addr).await.unwrap();
    let tls = tokio_boring::connect(connector.build().configure().unwrap(), "localhost", tcp)
        .await
        .unwrap();
    (H2Conn::start(tls, &profile).await.unwrap(), server)
}

async fn finish(server: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(std::time::Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn stealth_v3_h2_malformed_response_fields_are_not_content() {
    for (name, value, valid) in [
        ("x-owned", " leading", false),
        ("x-owned", "trailing\t", false),
        ("te", "trailers", false),
        ("x-owned", "inside whitespace\tis valid", true),
        ("x-owned", "", true),
    ] {
        let (mut conn, server) = fixture(
            BrowserProfile::chrome_150(Platform::Linux),
            move |mut tls| async move {
                // Raw literal fields avoid testing the encoder against itself.
                let mut block = vec![0x88, 0, name.len() as u8];
                block.extend_from_slice(name.as_bytes());
                block.push(value.len() as u8);
                block.extend_from_slice(value.as_bytes());
                write_frame(
                    &mut tls,
                    HEADERS,
                    FLAG_END_HEADERS | FLAG_END_STREAM,
                    1,
                    &block,
                )
                .await
                .unwrap();
                tls.flush().await.unwrap();
            },
        )
        .await;
        let response = conn.get("localhost", "/fields", &[]).await;
        if valid {
            assert_eq!(response.unwrap().headers, vec![(name.into(), value.into())]);
        } else {
            assert!(
                response.is_err(),
                "malformed response field accepted: {name}={value:?}"
            );
        }
        finish(server).await;
    }
}

#[tokio::test]
async fn stealth_v3_h2_padded_priority_headers_and_data_keep_the_exact_content() {
    let (mut conn, server) = fixture(
        BrowserProfile::chrome_150(Platform::Linux),
        |mut tls| async move {
            // Independent raw HPACK: :status=200; literal x-proof=owned.
            let block = b"\x88\x00\x07x-proof\x05owned";
            let mut headers = vec![3, 0, 0, 0, 0, 15];
            headers.extend_from_slice(block);
            headers.extend_from_slice(&[0, 0, 0]);
            write_frame(
                &mut tls,
                HEADERS,
                FLAG_PADDED | FLAG_PRIORITY | FLAG_END_HEADERS,
                1,
                &headers,
            )
            .await
            .unwrap();
            write_frame(
                &mut tls,
                DATA,
                FLAG_PADDED | FLAG_END_STREAM,
                1,
                &[2, b'o', b'k', 0, 0],
            )
            .await
            .unwrap();
            tls.flush().await.unwrap();
        },
    )
    .await;
    let response = conn.get("localhost", "/padded", &[]).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.headers, vec![("x-proof".into(), "owned".into())]);
    assert_eq!(response.body, b"ok");
    finish(server).await;
}

#[tokio::test]
async fn stealth_v3_h2_rejects_interleaved_and_unopened_continuations() {
    for interleaved in [false, true] {
        let (mut conn, server) = fixture(
            BrowserProfile::chrome_150(Platform::Linux),
            move |mut tls| async move {
                if interleaved {
                    write_frame(&mut tls, HEADERS, FLAG_END_STREAM, 1, &[0x88])
                        .await
                        .unwrap();
                    write_frame(&mut tls, PING, FLAG_ACK, 0, &[0; 8])
                        .await
                        .unwrap();
                    write_frame(&mut tls, CONTINUATION, FLAG_END_HEADERS, 1, &[])
                        .await
                        .unwrap();
                } else {
                    write_frame(&mut tls, CONTINUATION, FLAG_END_HEADERS, 1, &[0x88])
                        .await
                        .unwrap();
                    write_frame(&mut tls, DATA, FLAG_END_STREAM, 1, &[])
                        .await
                        .unwrap();
                }
                tls.flush().await.unwrap();
            },
        )
        .await;
        let result = conn.get("localhost", "/continuation", &[]).await;
        assert!(result.is_err(), "invalid continuation framing was accepted");
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .to_ascii_lowercase()
                .contains("continuation")
        );
        finish(server).await;
    }
}

#[tokio::test]
async fn stealth_v3_h2_invalid_padding_and_data_before_headers_fail() {
    for padded in [false, true] {
        let (mut conn, server) = fixture(
            BrowserProfile::chrome_150(Platform::Linux),
            move |mut tls| async move {
                if padded {
                    write_frame(&mut tls, HEADERS, FLAG_END_HEADERS, 1, &[0x88])
                        .await
                        .unwrap();
                }
                write_frame(
                    &mut tls,
                    DATA,
                    FLAG_END_STREAM | if padded { FLAG_PADDED } else { 0 },
                    1,
                    &[9],
                )
                .await
                .unwrap();
                tls.flush().await.unwrap();
            },
        )
        .await;
        let result = conn.get("localhost", "/invalid-data", &[]).await;
        assert!(
            result.is_err(),
            "malformed DATA must never become a complete response"
        );
        let error = result.err().unwrap().to_string();
        assert!(
            error.contains(if padded { "padding" } else { "before headers" }),
            "{error}"
        );
        finish(server).await;
    }
}

#[tokio::test]
async fn stealth_v3_h2_padded_data_replenishes_the_advertised_stream_window() {
    let mut profile = BrowserProfile::chrome_150(Platform::Linux);
    profile.h2.initial_window_size = 512;
    let (mut conn, server) = fixture(profile, |mut tls| async move {
        write_frame(&mut tls, HEADERS, FLAG_END_HEADERS, 1, &[0x88])
            .await
            .unwrap();
        let mut data = vec![0; 300];
        data[0] = 255;
        data[1..45].fill(b'x');
        write_frame(&mut tls, DATA, FLAG_PADDED, 1, &data)
            .await
            .unwrap();
        tls.flush().await.unwrap();
        let update =
            tokio::time::timeout(std::time::Duration::from_secs(1), read_frame(&mut tls)).await;
        let (header, payload) = update
            .expect("padding consumes flow control: replenishment must arrive")
            .unwrap();
        assert_eq!(header.ty, WINDOW_UPDATE);
        assert_eq!(header.stream_id, 1);
        assert_eq!(payload, 300u32.to_be_bytes());
        write_frame(&mut tls, DATA, FLAG_END_STREAM, 1, b"done")
            .await
            .unwrap();
        tls.flush().await.unwrap();
    })
    .await;
    let response = conn.get("localhost", "/flow", &[]).await.unwrap();
    assert_eq!(response.body, [vec![b'x'; 44], b"done".to_vec()].concat());
    finish(server).await;
}

#[tokio::test]
async fn stealth_v3_h2_settings_shrink_encoder_before_the_next_request() {
    let (mut conn, server) = fixture(
        BrowserProfile::chrome_150(Platform::Linux),
        |mut tls| async move {
            write_frame(&mut tls, SETTINGS, 0, 0, &settings_payload(&[(1, 0)]))
                .await
                .unwrap();
            write_frame(
                &mut tls,
                HEADERS,
                FLAG_END_HEADERS | FLAG_END_STREAM,
                1,
                &[0x88],
            )
            .await
            .unwrap();
            tls.flush().await.unwrap();
            let (ack, bytes) = read_frame(&mut tls).await.unwrap();
            assert_eq!(
                (ack.ty, ack.flags, ack.stream_id, bytes.len()),
                (SETTINGS, FLAG_ACK, 0, 0)
            );
            let (next, block) = read_frame(&mut tls).await.unwrap();
            assert_eq!((next.ty, next.stream_id), (HEADERS, 3));
            assert_eq!(
                block[5], 0x20,
                "peer table size zero must be encoded at the next block start"
            );
            write_frame(
                &mut tls,
                HEADERS,
                FLAG_END_HEADERS | FLAG_END_STREAM,
                3,
                &[0x88],
            )
            .await
            .unwrap();
            tls.flush().await.unwrap();
        },
    )
    .await;
    assert_eq!(
        conn.get("localhost", "/one", &[]).await.unwrap().status,
        200
    );
    assert_eq!(
        conn.get("localhost", "/two", &[]).await.unwrap().status,
        200
    );
    finish(server).await;
}

#[tokio::test]
async fn stealth_v3_h2_informational_headers_trailers_and_prior_reset_preserve_content() {
    let (mut conn, server) = fixture(
        BrowserProfile::chrome_150(Platform::Linux),
        |mut tls| async move {
            write_frame(&mut tls, HEADERS, FLAG_END_HEADERS, 1, b"\x08\x03103")
                .await
                .unwrap();
            write_frame(&mut tls, HEADERS, FLAG_END_HEADERS, 1, &[0x88])
                .await
                .unwrap();
            write_frame(&mut tls, DATA, 0, 1, b"owned").await.unwrap();
            write_frame(
                &mut tls,
                HEADERS,
                FLAG_END_HEADERS | FLAG_END_STREAM,
                1,
                b"\x00\x07x-proof\x05owned",
            )
            .await
            .unwrap();
            tls.flush().await.unwrap();
            let (next, _) = read_frame(&mut tls).await.unwrap();
            assert_eq!((next.ty, next.stream_id), (HEADERS, 3));
            write_frame(&mut tls, RST_STREAM, 0, 1, &0u32.to_be_bytes())
                .await
                .unwrap();
            write_frame(
                &mut tls,
                HEADERS,
                FLAG_END_HEADERS | FLAG_END_STREAM,
                3,
                &[0x89],
            )
            .await
            .unwrap();
            tls.flush().await.unwrap();
        },
    )
    .await;
    let response = conn.get("localhost", "/sections", &[]).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"owned");
    assert_eq!(response.headers, vec![("x-proof".into(), "owned".into())]);
    assert_eq!(
        conn.get("localhost", "/next", &[]).await.unwrap().status,
        204
    );
    finish(server).await;
}

#[tokio::test]
async fn stealth_v3_h2_inconsistent_lengths_and_bodyless_responses_fail() {
    for (status, length, body, allowed) in [
        (0x88, Some(b"9".as_slice()), b"short".as_slice(), false),
        (0x88, Some(b"nine".as_slice()), b"short".as_slice(), false),
        (0x89, None, b"bad".as_slice(), false),
        (0x8b, Some(b"9".as_slice()), b"".as_slice(), true),
        (0x88, Some(b"5".as_slice()), b"short".as_slice(), true),
    ] {
        let (mut conn, server) = fixture(
            BrowserProfile::chrome_150(Platform::Linux),
            move |mut tls| async move {
                let mut block = vec![status];
                if let Some(value) = length {
                    // Literal without indexing, static name28 = content-length.
                    block.extend_from_slice(&[0x0f, 0x0d, value.len() as u8]);
                    block.extend_from_slice(value);
                }
                write_frame(&mut tls, HEADERS, FLAG_END_HEADERS, 1, &block)
                    .await
                    .unwrap();
                write_frame(&mut tls, DATA, FLAG_END_STREAM, 1, body)
                    .await
                    .unwrap();
                tls.flush().await.unwrap();
            },
        )
        .await;
        let result = conn.get("localhost", "/length", &[]).await;
        if allowed {
            assert_eq!(result.unwrap().body, body);
        } else {
            assert!(
                result.is_err(),
                "inconsistent response was accepted: status index={status}, length={length:?}"
            );
            let error = result.err().unwrap().to_string();
            assert!(
                error.contains("content-length") || error.contains("bodyless"),
                "{error}"
            );
        }
        finish(server).await;
    }
}

#[tokio::test]
async fn stealth_v3_h2_valid_zero_settings_and_invalid_peer_limits_are_distinct() {
    for (settings, allowed) in [
        (vec![(2, 0), (4, 0), (0x1234, 42)], true),
        (vec![(2, 1)], false),
        (vec![(4, 0x8000_0000)], false),
        (vec![(5, 16_383)], false),
        (vec![(5, 0x100_0000)], false),
    ] {
        let (mut conn, server) = fixture(
            BrowserProfile::chrome_150(Platform::Linux),
            move |mut tls| async move {
                write_frame(&mut tls, SETTINGS, 0, 0, &settings_payload(&settings))
                    .await
                    .unwrap();
                write_frame(
                    &mut tls,
                    HEADERS,
                    FLAG_END_HEADERS | FLAG_END_STREAM,
                    1,
                    &[0x88],
                )
                .await
                .unwrap();
                tls.flush().await.unwrap();
            },
        )
        .await;
        let result = conn.get("localhost", "/settings", &[]).await;
        if allowed {
            assert_eq!(
                result
                    .expect("RFC9113 allows server ENABLE_PUSH=0 and an empty sending window")
                    .status,
                200
            );
        } else {
            assert!(
                result.is_err(),
                "illegal peer setting must fail before accepting content"
            );
            assert!(result.err().unwrap().to_string().contains("setting"));
        }
        finish(server).await;
    }
}

#[tokio::test]
async fn stealth_v3_h2_large_request_fields_are_fragmented_on_the_same_stream() {
    let value: String = (0..32_000)
        .map(|i| char::from(33 + (i % 94) as u8))
        .collect();
    let expected = value.clone();
    let (mut conn, server) = fixture(
        BrowserProfile::chrome_150(Platform::Linux),
        move |mut tls| async move {
            write_frame(
                &mut tls,
                HEADERS,
                FLAG_END_HEADERS | FLAG_END_STREAM,
                1,
                &[0x88],
            )
            .await
            .unwrap();
            tls.flush().await.unwrap();
            let (header, payload) = read_frame(&mut tls).await.unwrap();
            assert_eq!((header.ty, header.stream_id), (HEADERS, 3));
            assert_eq!(header.flags, FLAG_PRIORITY | FLAG_END_STREAM);
            assert_eq!(payload.len(), DEFAULT_MAX_FRAME_SIZE);
            let mut block = payload[5..].to_vec();
            let mut continuations = 0;
            loop {
                let (header, payload) = read_frame(&mut tls).await.unwrap();
                assert_eq!((header.ty, header.stream_id), (CONTINUATION, 3));
                continuations += 1;
                block.extend_from_slice(&payload);
                if header.flags & FLAG_END_HEADERS != 0 {
                    break;
                }
            }
            assert!(continuations > 0);
            // Independent first-request block establishes the authority/path
            // entries retained from GET /first on this connection.
            let mut decoder = super::hpack::Decoder::new();
            decoder
                .decode(b"\x82\x41\x09localhost\x87\x44\x06/first")
                .unwrap();
            let fields = decoder.decode(&block).unwrap();
            assert!(fields.contains(&("x-owned-large".into(), expected)));
            write_frame(
                &mut tls,
                HEADERS,
                FLAG_END_HEADERS | FLAG_END_STREAM,
                3,
                &[0x88],
            )
            .await
            .unwrap();
            tls.flush().await.unwrap();
        },
    )
    .await;
    assert_eq!(
        conn.get("localhost", "/first", &[]).await.unwrap().status,
        200
    );
    assert_eq!(
        conn.get("localhost", "/second", &[("x-owned-large".into(), value)])
            .await
            .unwrap()
            .status,
        200
    );
    finish(server).await;
}

#[tokio::test]
async fn stealth_v3_h2_graceful_goaway_lets_the_inflight_stream_finish() {
    let (mut conn, server) = fixture(
        BrowserProfile::chrome_150(Platform::Linux),
        |mut tls| async move {
            // Two-phase graceful shutdown: the first GOAWAY vouches the
            // whole stream space, so the in-flight stream still finishes.
            write_frame(&mut tls, HEADERS, FLAG_END_HEADERS, 1, &[0x88])
                .await
                .unwrap();
            write_frame(&mut tls, DATA, 0, 1, b"part-").await.unwrap();
            let mut goaway = [0u8; 8];
            goaway[0..4].copy_from_slice(&0x7fff_ffffu32.to_be_bytes());
            write_frame(&mut tls, GOAWAY, 0, 0, &goaway).await.unwrap();
            write_frame(&mut tls, DATA, FLAG_END_STREAM, 1, b"rest")
                .await
                .unwrap();
            tls.flush().await.unwrap();
        },
    )
    .await;
    let response = conn.get("localhost", "/drain", &[]).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"part-rest");
    assert!(
        conn.is_draining(),
        "a GOAWAY'd connection must never return to the pool"
    );
    assert!(
        conn.get("localhost", "/after", &[]).await.is_err(),
        "a draining connection must refuse a new stream"
    );
    finish(server).await;
}

#[tokio::test]
async fn stealth_v3_h2_goaway_below_the_stream_aborts_the_response() {
    let (mut conn, server) = fixture(
        BrowserProfile::chrome_150(Platform::Linux),
        |mut tls| async move {
            write_frame(&mut tls, HEADERS, FLAG_END_HEADERS, 1, &[0x88])
                .await
                .unwrap();
            write_frame(&mut tls, DATA, 0, 1, b"part-").await.unwrap();
            // last-stream-id 0 < stream 1: the peer will not process it.
            let goaway = [0u8; 8];
            write_frame(&mut tls, GOAWAY, 0, 0, &goaway).await.unwrap();
            tls.flush().await.unwrap();
        },
    )
    .await;
    let error = conn
        .get("localhost", "/gone", &[])
        .await
        .err()
        .expect("a GOAWAY below the stream must abort the response");
    assert!(format!("{error:?}").contains("goaway"), "{error:?}");
    finish(server).await;
}
