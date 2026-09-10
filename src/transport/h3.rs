//! Tier-1 HTTP/3 + QUIC transport (v4 phase 5.1).
//!
//! One Chrome-true boring context (see tls.rs) under the donsetch
//! fork of quiche 0.29.3. One QUIC connection serves one request in
//! this build: the agent's own fetch shape does not overlap streams,
//! so multiplexing inside one connection is an explicit v2 question,
//! not a silent one.
//!
//! Wire shape:
//! - TLS 1.3 only, h3 ALPN, Chrome's cipher/curve/sigalg/GREASE core
//!   (the same builder our h1/h2 path uses).
//! - QUIC transport params from Chrome's public defaults (v1; a
//!   byte-exact param capture is recorded in design/v4.md's edge
//!   ledger for the v2 parity pass).
//! - 0-RTT only when a serialized session exists in route memory; the
//!   config arms `enable_early_data` only when a ticket is present.
//!
//! Route policy lives in routes.rs: h3 only when the origin's alt-svc
//! vouched for it, no h3 behind a CONNECT proxy, DONSETCH_NO_H3 as the
//! hard kill switch.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use crate::error::FetchError;

pub use quiche::h3;
use quiche::h3::NameValue;

const PROTOCOL_VERSION: u32 = quiche::PROTOCOL_VERSION;
const UDP_BUF: usize = 65_527;

static QUIC_TOTAL_CONN: AtomicU64 = AtomicU64::new(0);

pub fn stats() -> u64 {
    QUIC_TOTAL_CONN.load(Ordering::Relaxed)
}

#[derive(Default, Debug)]
pub struct QuicStats {
    pub handshake_ms: u128,
    pub early_data: bool,
    pub resumed: bool,
    pub pkts_in: u64,
    pub pkts_out: u64,
    pub total_ms: u128,
}

pub struct H3Out {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub altsvc: Option<String>,
}

pub struct H3Request<'a> {
    pub egress: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str,
    pub authority: &'a str,
    pub headers: Vec<(String, String)>,
    pub user_agent: &'a str,
    pub accept: &'a str,
    pub timeout: Duration,
}

/// Same bomb rule as the h1 reader: a body must fail the shared
/// transport cap BEFORE it allocates past it, not after.
fn accept_body_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), FetchError> {
    if body.len().saturating_add(chunk.len()) > super::MAX_BODY {
        return Err(FetchError::Http("h3: body exceeds cap".into()));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn resolve_host(host: &str, port: u16) -> Result<SocketAddr, FetchError> {
    (host, port)
        .to_socket_addrs()
        .map_err(|_| FetchError::Http(format!("dns resolve failed for {host}")))?
        .next()
        .ok_or_else(|| FetchError::Http(format!("dns resolve empty for {host}")))
}

/// All the quiche quoting for one connection: our Chrome-true builder
/// and Chrome's public default transport params. quiche Config is
/// single-use at connect time, so it is rebuilt per request.
#[cfg_attr(test, allow(clippy::map_unwrap_or))]
fn quic_config(profile: &crate::profile::BrowserProfile) -> Result<quiche::Config, FetchError> {
    let builder = crate::transport::tls::build_quic_ctx_builder(profile)?;
    // NOTE(quiche): quiche's Config map_err has clippy's map_err for
    // the small closure body; keep it because it shows the eventual
    // failure surface in one place.
    let cfg = quiche::Config::with_boring_ssl_ctx_builder(PROTOCOL_VERSION, builder)
        .map_err(|e| FetchError::Http(format!("quiche config init failed: {e}")))?;
    let mut cfg = cfg;
    // The boring ctx ALPN already sends the single h3 entry; this call
    // mirrors the negotiated ALPN inside quiche's own connection.
    cfg.set_application_protos(&[b"h3"])
        .map_err(|_| FetchError::Http("quiche alpn failed".into()))?;
    cfg.grease(true);
    cfg.set_max_idle_timeout(30_000);
    cfg.set_initial_max_data(6_291_456);
    cfg.set_initial_max_stream_data_bidi_local(1_048_576);
    cfg.set_initial_max_stream_data_bidi_remote(1_048_576);
    cfg.set_initial_max_stream_data_uni(1_048_576);
    cfg.set_initial_max_streams_bidi(100);
    cfg.set_initial_max_streams_uni(100);
    cfg.set_max_recv_udp_payload_size(65_527);
    cfg.set_max_send_udp_payload_size(1_352);
    cfg.discover_pmtu(true);
    Ok(cfg)
}

pub async fn h3_fetch_heat(
    req: &H3Request<'_>,
    profile: &crate::profile::BrowserProfile,
) -> Result<(H3Out, QuicStats), FetchError> {
    h3_fetch_inner(
        req.egress,
        req.host,
        req.port,
        req.path,
        req.authority,
        req.headers.clone(),
        req.user_agent,
        req.accept,
        profile,
        req.timeout,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn h3_fetch_inner(
    egress: &str,
    host: &str,
    port: u16,
    path: &str,
    authority: &str,
    headers: Vec<(String, String)>,
    user_agent: &str,
    accept: &str,
    profile: &crate::profile::BrowserProfile,
    timeout: Duration,
) -> Result<(H3Out, QuicStats), FetchError> {
    let mut cfg = quic_config(profile)?;
    let peer = resolve_host(host, port)?;
    let socket = if peer.is_ipv4() {
        UdpSocket::bind("0.0.0.0:0").await?
    } else {
        UdpSocket::bind("[::]:0").await?
    };
    socket.connect(&peer).await?;
    let local_addr = socket.local_addr()?;

    // Ticket load: when there is one, arm 0-RTT (a ticket must exist
    // for the early-data to fire) and stamp it before quiche's first
    // packet leaves.
    let resumed = crate::transport::routes::load_h3_session(authority, egress);
    if let Some(bytes) = &resumed {
        if std::env::var_os("DONGHOST_DEBUG").is_some() {
            eprintln!("[h3] session resume armed ({} bytes)", bytes.len());
        }
        cfg.enable_early_data();
    }

    // Long random local CID, Chrome-style local CIDs.
    let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
    boring::rand::rand_bytes(&mut scid)
        .map_err(|_| FetchError::Http("quic cid rand failed".into()))?;

    let mut conn = quiche::connect(
        Some(host),
        &quiche::ConnectionId::from_ref(&scid[..]),
        local_addr,
        peer,
        &mut cfg,
    )
    .map_err(|_| FetchError::Http("quic connect init failed".into()))?;
    if let Some(sess) = &resumed {
        conn.set_session(sess)
            .map_err(|_| FetchError::Http("quic session set failed".into()))?;
    }

    let started = Instant::now();
    let deadline = started + timeout;

    let mut req_headers = vec![
        h3::Header::new(b":method", b"GET"),
        h3::Header::new(b":scheme", b"https"),
        h3::Header::new(b":path", path.as_bytes()),
        h3::Header::new(b":authority", authority.as_bytes()),
        h3::Header::new(b"user-agent", user_agent.as_bytes()),
        h3::Header::new(b"accept", accept.as_bytes()),
    ];
    for (n, v) in &headers {
        req_headers.push(h3::Header::new(n.as_bytes(), v.as_bytes()));
    }

    let mut send_buf = vec![0u8; UDP_BUF];
    let mut recv_buf = vec![0u8; UDP_BUF];

    let mut h3conn: Option<h3::Connection> = None;
    let mut status = 0u16;
    let mut resp_headers: Vec<(String, String)> = Vec::new();
    let mut body: Vec<u8> = Vec::new();
    let mut altsvc: Option<String> = None;
    let mut request_sent = false;
    let mut pkts_in = 0u64;
    let mut pkts_out = 0u64;
    let mut handshake_ms = 0u128;
    let early_data = resumed.is_some();
    let mut rtt_landed = false;

    loop {
        // Flush quiche's outbound packets.
        loop {
            let (write, _info) = match conn.send(&mut send_buf) {
                Ok(v) => v,
                Err(quiche::Error::Done) => break,
                Err(e) => return Err(FetchError::Http(format!("quic send err {e}"))),
            };
            if write == 0 {
                break;
            }
            socket
                .send(&send_buf[..write])
                .await
                .map_err(|e| FetchError::Http(format!("udp send err {e}")))?;
            pkts_out += 1;
        }

        // Drain inbound until the socket reports WouldBlock / Done.
        loop {
            match socket.try_recv(&mut recv_buf) {
                Ok(n) if n > 0 => {
                    let info = quiche::RecvInfo {
                        from: peer,
                        to: local_addr,
                    };
                    match conn.recv(&mut recv_buf[..n], info) {
                        Ok(_) => {
                            pkts_in += 1;
                        }
                        Err(quiche::Error::Done) => break,
                        Err(e) => return Err(FetchError::Http(format!("quic recv err {e}"))),
                    }
                }
                Ok(_) => {}
                Err(kind)
                    if kind.kind() == std::io::ErrorKind::WouldBlock
                        || kind.kind() == std::io::ErrorKind::ConnectionAborted =>
                {
                    break;
                }
                Err(kind) if kind.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(kind) => return Err(FetchError::Http(format!("udp recv err {kind}"))),
            }
        }

        // Handshake done: the h3 layer can speak.
        if conn.is_established() && h3conn.is_none() {
            let h3cfg =
                h3::Config::new().map_err(|_| FetchError::Http("h3 config init failed".into()))?;
            h3conn = Some(
                h3::Connection::with_transport(&mut conn, &h3cfg)
                    .map_err(|_| FetchError::Http("h3 connect failed".into()))?,
            );
            handshake_ms = started.elapsed().as_millis();
            rtt_landed = early_data;
        }

        // Send our request once the h3 layer is live.
        if !request_sent && let Some(hc) = h3conn.as_mut() {
            hc.send_request(&mut conn, &req_headers, true)
                .map_err(|e| FetchError::Http(format!("h3 send err {e}")))?;
            request_sent = true;
        }

        let mut stream_done = false;
        if let Some(hc) = h3conn.as_mut() {
            loop {
                let (sid, event) = match hc.poll(&mut conn) {
                    Ok((sid, ev)) => (sid, ev),
                    Err(h3::Error::Done) => break,
                    Err(e) => return Err(FetchError::Http(format!("h3 poll err {e}"))),
                };
                match event {
                    h3::Event::Headers { list, .. } => {
                        // Early-hints (1xx) over h3 are informational:
                        // the final status and body follow on the same
                        // stream. Record alt-svc hints, keep polling. A
                        // 1xx is only treated as final if it is also
                        // the last... never: non-1xx always wins.
                        let is_informational = list.iter().any(|h| {
                            h.name() == b":status"
                                && std::str::from_utf8(h.value())
                                    .ok()
                                    .and_then(|x| x.trim().parse::<u16>().ok())
                                    .is_some_and(|s| s < 200)
                        });
                        if is_informational {
                            for hdr in &list {
                                if hdr.name() == b"alt-svc" {
                                    altsvc =
                                        Some(String::from_utf8_lossy(hdr.value()).into_owned());
                                }
                            }
                            continue;
                        }
                        for hdr in &list {
                            if hdr.name() == b":status" && status == 0 {
                                status = std::str::from_utf8(hdr.value())
                                    .ok()
                                    .and_then(|x| x.trim().parse::<u16>().ok())
                                    .unwrap_or(0);
                                continue;
                            }
                            if hdr.name() == b"alt-svc" {
                                altsvc = Some(String::from_utf8_lossy(hdr.value()).into_owned());
                                continue;
                            }
                            resp_headers.push((
                                String::from_utf8_lossy(hdr.name()).into_owned(),
                                String::from_utf8_lossy(hdr.value()).into_owned(),
                            ));
                        }
                    }
                    h3::Event::Data => {
                        let mut chunk = [0u8; 65_536];
                        loop {
                            match hc.recv_body(&mut conn, sid, &mut chunk) {
                                Ok(n) => accept_body_chunk(&mut body, &chunk[..n])?,
                                Err(h3::Error::Done) => break,
                                Err(e) => {
                                    return Err(FetchError::Http(format!("h3 body err {e}")));
                                }
                            }
                        }
                    }
                    h3::Event::Finished => stream_done = true,
                    h3::Event::Reset(..) => {
                        return Err(FetchError::Http("h3 stream reset".into()));
                    }
                    h3::Event::GoAway => {}
                    h3::Event::PriorityUpdate => {}
                }
            }
        }

        // Response landed: return it, and keep the fully-local
        // session for the next 0-RTT handshake on this origin. The
        // ticket usually lands within tens of ms of the response, so
        // hold the connection (Chrome-style) for a bounded window:
        // returning early means the next handshake loses 0-RTT.
        if stream_done {
            let hold_until = Instant::now() + Duration::from_millis(400);
            while conn.session().is_none() {
                let now = Instant::now();
                if now >= deadline || now >= hold_until {
                    break;
                }
                // Drain any pending inbound (the ticket may still be
                // on the wire) before the short sleep.
                loop {
                    match socket.try_recv(&mut recv_buf) {
                        Ok(n) if n > 0 => {
                            if conn
                                .recv(
                                    &mut recv_buf[..n],
                                    quiche::RecvInfo {
                                        from: peer,
                                        to: local_addr,
                                    },
                                )
                                .is_ok()
                            {
                                pkts_in += 1;
                            }
                        }
                        _ => break,
                    }
                }
                loop {
                    match conn.send(&mut send_buf) {
                        Ok((write, _)) if write > 0 => {
                            let _ = socket.send(&send_buf[..write]).await;
                        }
                        _ => break,
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = conn.close(true, 0, b"");
            if let Some(sess) = conn.session() {
                if std::env::var_os("DONGHOST_DEBUG").is_some() {
                    eprintln!("[h3] session saved {} bytes", sess.len());
                }
                crate::transport::routes::save_h3_session(authority, egress, sess);
            } else if std::env::var_os("DONGHOST_DEBUG").is_some() {
                eprintln!("[h3] no session ticket before close");
            }
            QUIC_TOTAL_CONN.fetch_add(1, Ordering::Relaxed);
            return Ok((
                H3Out {
                    status,
                    headers: resp_headers,
                    body: std::mem::take(&mut body),
                    altsvc,
                },
                QuicStats {
                    handshake_ms,
                    early_data: rtt_landed,
                    resumed: conn.is_resumed(),
                    pkts_in,
                    pkts_out,
                    total_ms: started.elapsed().as_millis(),
                },
            ));
        }

        // Give up when the QUIC shuts down pre-response.
        if conn.is_closed() || conn.is_draining() {
            if status != 0 {
                QUIC_TOTAL_CONN.fetch_add(1, Ordering::Relaxed);
                return Ok((
                    H3Out {
                        status,
                        headers: resp_headers,
                        body: std::mem::take(&mut body),
                        altsvc,
                    },
                    QuicStats {
                        handshake_ms,
                        early_data: rtt_landed,
                        resumed: conn.is_resumed(),
                        pkts_in,
                        pkts_out,
                        total_ms: started.elapsed().as_millis(),
                    },
                ));
            }
            return Err(FetchError::Http("h3: quic closed before response".into()));
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(FetchError::Http("quic deadline exceeded".into()));
        }
        match conn.timeout() {
            Some(t) => {
                let at = now + t;
                let wait = at
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(20));
                tokio::time::sleep(wait).await;
                conn.on_timeout();
            }
            None => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
}

/// The fetch client's h3 route (v4 phase 5.1): a direct-egress-only
/// quic/h3 attempt over the same Chrome-true builder the h1/h2 path
/// uses. `user_headers` arrive already profile-shaped (the same header
/// set the h1/h2 path builds), so the wire stays coherent across the
/// transports. No 0-RTT on force: force is an explicit re-handshake
/// unless route memory vouches a session, and that is the caller's
/// argument for `session`.
#[allow(clippy::too_many_arguments)]
pub async fn h3_fetch_direct(
    host: &str,
    port: u16,
    path: &str,
    authority: &str,
    user_headers: Vec<(String, String)>,
    timeout: Option<Duration>,
) -> Result<(H3Out, QuicStats), FetchError> {
    if std::env::var_os("DONGHOST_DEBUG").is_some() {
        eprintln!("[h3] attempt {host}:{port}{path}");
    }
    let profile = crate::profile::BrowserProfile::chrome_150(crate::profile::Platform::host());
    let timeout = timeout.unwrap_or(Duration::from_secs(15));
    let accept = user_headers
        .iter()
        .find(|(n, _)| n == "accept")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "text/html,application/xhtml+xml".into());
    let user_agent = user_headers
        .iter()
        .find(|(n, _)| n == "user-agent")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let req = H3Request {
        egress: "direct",
        host,
        port,
        path,
        authority,
        headers: user_headers
            .iter()
            .filter(|(n, _)| n != "user-agent" && n != "accept" && n != "accept-encoding")
            .cloned()
            .collect(),
        user_agent: user_agent.as_str(),
        accept: accept.as_str(),
        timeout,
    };
    let (out, stats) = h3_fetch_heat(&req, &profile).await?;
    if std::env::var_os("DONGHOST_DEBUG").is_some() {
        eprintln!(
            "[h3] stats {}:{} early={} resumed={} hs_ms={} total_ms={} pkts_in={} pkts_out={}",
            host,
            port,
            stats.early_data,
            stats.resumed,
            stats.handshake_ms,
            stats.total_ms,
            stats.pkts_in,
            stats.pkts_out
        );
    }
    Ok((out, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The h1 reader learned this the hard way (#144): a hostile
    // origin streaming an unbounded body must hit the shared cap,
    // not the allocator. The h3 lane reads the same kind of wire.
    #[test]
    fn body_chunks_stop_at_the_shared_transport_cap() {
        let mut body = Vec::new();
        assert!(accept_body_chunk(&mut body, &[0u8; 65_536]).is_ok());
        assert_eq!(body.len(), 65_536);
        // Jump to just under the cap, then cross it.
        let mut near = vec![0u8; super::super::MAX_BODY - 10];
        assert!(accept_body_chunk(&mut near, &[0u8; 10]).is_ok());
        let before = near.len();
        assert!(
            accept_body_chunk(&mut near, &[0u8; 1]).is_err(),
            "the crossing chunk must fail"
        );
        assert_eq!(near.len(), before, "a refused chunk must not allocate");
    }
}
