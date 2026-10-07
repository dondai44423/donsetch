//! Local SOCKS5 relay that lets Chrome use authenticated egress
//! lanes.
//!
//! Chrome cannot authenticate SOCKS or HTTP proxies from
//! `--proxy-server` (no credential support, no dialog we can
//! drive headless), so every authenticated lane turned into
//! Chrome's own `ERR_SOCKS_CONNECTION_FAILED` error page: the
//! render loop saw a stable, tiny-text DOM, extraction found
//! nothing, and the whole tier-2 attempt died with "no real
//! content was extractable" (live case: an authenticated socks5
//! lane on a daemon fetch). The relay removes that class
//! entirely: Chrome connects to a local, credential-free SOCKS5
//! listener; the relay performs the upstream handshake with the
//! lane's own credentials (SOCKS5 user/pass or HTTP CONNECT
//! basic auth, exactly what the tier-1 client does) and then
//! pipes bytes in both directions. Chrome believes it is talking
//! to an unauthenticated SOCKS5 proxy; the upstream sees our
//! normal authenticated client.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

/// A running relay bound to one upstream lane. Dropping its owner closes
/// the listener and all owned in-flight handshakes and tunnels.
pub struct Relay {
    pub port: u16,
    handle: Option<JoinHandle<()>>,
}

/// Per-relay upstream-failure memory: a host the lane keeps
/// refusing (ACL: tiktokcdn-class hosts on a search-oriented
/// lane) fails FAST after a few strikes instead of burning a full
/// dial timeout on every browser request. That is what made the
/// ghost-solve ladder take 40s on tiktok: every CDN request
/// retried the refused dial.
#[derive(Default)]
struct StrikeCache {
    strikes: std::collections::HashMap<String, (u8, std::time::Instant)>,
}

impl StrikeCache {
    /// The host is fast-rejected only while its strikes are at the
    /// limit AND fresh; anything older than the TTL counts as healed
    /// (the next dial failure re-stamps from scratch).
    fn refused(&self, host_key: &str) -> bool {
        self.strikes
            .get(host_key)
            .is_some_and(|(n, at)| *n >= STRIKE_LIMIT && at.elapsed() < STRIKE_TTL)
    }

    fn note_failure(&mut self, host_key: &str) {
        self.prune();
        if !self.strikes.contains_key(host_key)
            && self.strikes.len() >= 1024
            && let Some(oldest) = self
                .strikes
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(host, _)| host.clone())
        {
            self.strikes.remove(&oldest);
        }
        let now = std::time::Instant::now();
        let (n, at) = self.strikes.entry(host_key.to_owned()).or_insert((0, now));
        // A strike that already expired counts as healed: restart the
        // limit instead of inheriting the stale count, so a single
        // failure right after recovery cannot fast-reject again.
        if at.elapsed() >= STRIKE_TTL {
            *n = 0;
        }
        *n = n.saturating_add(1);
        *at = now;
    }

    fn note_success(&mut self, host_key: &str) {
        self.strikes.remove(host_key);
    }

    /// Drop entries whose TTL already passed. Without this the map only
    /// ever grew: a refusal is checked for freshness on read, so nothing
    /// ever removed the healed rows, and a long browser session dialing
    /// thousands of hosts kept every one of them forever. Runs on the
    /// failure path (the only writer), so it cannot touch a hot read.
    fn prune(&mut self) {
        self.strikes.retain(|_, (_, at)| at.elapsed() < STRIKE_TTL);
    }
}

const STRIKE_LIMIT: u8 = 3;

/// A strike expires after this long: a host that hiccups three
/// times must recover. Without a TTL the refused host stays
/// fast-rejected for the whole process lifetime even after the
/// network heals (each dial failure stamps the refusal time).
const STRIKE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

impl Relay {
    /// Bind a loopback listener and own the bounded tunnel tasks.
    /// Bind failures propagate; callers must retain the selected route.
    pub async fn spawn(proxy: Arc<crate::transport::proxy::Proxy>) -> std::io::Result<Relay> {
        Self::spawn_with_bypass(proxy, String::new()).await
    }

    pub(crate) async fn spawn_with_bypass(
        proxy: Arc<crate::transport::proxy::Proxy>,
        bypass: String,
    ) -> std::io::Result<Relay> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let handle = tokio::spawn(async move {
            accept_loop(listener, proxy, Arc::new(bypass)).await;
        });
        Ok(Relay {
            port,
            handle: Some(handle),
        })
    }

    /// The proxy string Chrome is pointed at.
    pub fn chrome_arg(&self) -> String {
        format!("--proxy-server=socks5://127.0.0.1:{}", self.port)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

const MAX_TUNNELS: usize = 64;
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn accept_loop(
    listener: TcpListener,
    proxy: Arc<crate::transport::proxy::Proxy>,
    bypass: Arc<String>,
) {
    let strikes = std::sync::Arc::new(std::sync::Mutex::new(StrikeCache::default()));
    let mut tunnels = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            Some(_) = tunnels.join_next(), if !tunnels.is_empty() => {}
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { return };
                if tunnels.len() >= MAX_TUNNELS {
                    // Refuse excess work without retaining sockets or waiters.
                    drop(stream);
                    continue;
                }
                let proxy = Arc::clone(&proxy);
                let strikes = Arc::clone(&strikes);
                let bypass = Arc::clone(&bypass);
                tunnels.spawn(async move {
                    let _ = serve_socks5_client(stream, proxy, strikes, bypass).await;
                });
            }
        }
    }
}

async fn serve_socks5_client(
    mut client: TcpStream,
    proxy: Arc<crate::transport::proxy::Proxy>,
    strikes: std::sync::Arc<std::sync::Mutex<StrikeCache>>,
    bypass: Arc<String>,
) -> std::io::Result<()> {
    let Some((host, port)) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, read_socks5_target(&mut client))
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "relay SOCKS handshake timed out",
                )
            })??
    else {
        return Ok(());
    };

    let host_key = format!("{host}:{port}");
    let refused = strikes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .refused(&host_key);
    if refused {
        // Refused fast: Chrome skips the asset, the page hydrates.
        let _ = client
            .write_all(&[5u8, 1u8, 0u8, 1u8, 0, 0, 0, 0, 0, 0])
            .await;
        return Ok(());
    }

    let connected = if crate::transport::proxy::no_proxy_match_value(&host, &bypass) {
        crate::transport::tcp::happy_connect(&host, port).await
    } else {
        proxy.connect(&host, port).await
    };
    match connected {
        Ok(mut upstream) => {
            // The route has healed when upstream connect succeeds, even if
            // this browser tunnel remains active for the rest of the page.
            strikes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .note_success(&host_key);
            if client
                .write_all(&[5u8, 0u8, 0u8, 1u8, 0, 0, 0, 0, 0, 0])
                .await
                .is_err()
            {
                return Ok(());
            }
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "[relay] upstream dial failed for {host}:{port} through {}: {e}",
                proxy.host
            );
            strikes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .note_failure(&host_key);
            let _ = client
                .write_all(&[5u8, 1u8, 0u8, 1u8, 0, 0, 0, 0, 0, 0])
                .await;
            Ok(())
        }
    }
}

async fn read_socks5_target(client: &mut TcpStream) -> std::io::Result<Option<(String, u16)>> {
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).await?;
    let [ver, nmethods] = greeting;
    if ver != 5 || nmethods == 0 {
        return Ok(None);
    }
    let mut methods = vec![0u8; nmethods as usize];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        client.write_all(&[5, 255]).await?;
        return Ok(None);
    }
    client.write_all(&[5, 0]).await?;

    let mut head = [0u8; 4];
    client.read_exact(&mut head).await?;
    let [ver, cmd, rsv, atyp] = head;
    if ver != 5 || cmd != 1 || rsv != 0 {
        let _ = client
            .write_all(&[5, if cmd != 1 { 7 } else { 1 }, 0, 1, 0, 0, 0, 0, 0, 0])
            .await;
        return Ok(None);
    }
    let host = match atyp {
        1u8 => {
            let mut octets = [0u8; 4];
            client.read_exact(&mut octets).await?;
            format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3])
        }
        3u8 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            client.read_exact(&mut name).await?;
            let Ok(name) = String::from_utf8(name) else {
                client.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                return Ok(None);
            };
            // Never repair invalid bytes or admit CONNECT request-line delimiters.
            let labels = name.strip_suffix('.').unwrap_or(&name);
            if labels.is_empty()
                || labels.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || !label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                })
            {
                client.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                return Ok(None);
            }
            name
        }
        4u8 => {
            let mut octets = [0u8; 16];
            client.read_exact(&mut octets).await?;
            format_ipv6(&octets)
        }
        _ => {
            client.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Ok(None);
        }
    };
    let mut port_bytes = [0u8; 2];
    client.read_exact(&mut port_bytes).await?;
    let port = u16::from_be_bytes(port_bytes);

    if port == 0 {
        client.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(None);
    }
    Ok(Some((host, port)))
}

fn format_ipv6(octets: &[u8; 16]) -> String {
    octets
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(all(test, unix))]
mod relay_tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn stealth_v3_relay_bypass_keeps_other_hosts_on_selected_proxy() {
        unsafe {
            std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1");
        }
        let direct = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let direct_port = direct.local_addr().unwrap().port();
        let direct_peer = tokio::spawn(async move {
            let (mut stream, _) = direct.accept().await.unwrap();
            let mut ping = [0; 4];
            stream.read_exact(&mut ping).await.unwrap();
            assert_eq!(&ping, b"ping");
            stream.write_all(b"direct-route").await.unwrap();
        });
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = Arc::new(
            crate::transport::proxy::Proxy::parse(&format!(
                "http://{}",
                upstream.local_addr().unwrap()
            ))
            .unwrap(),
        );
        let reached = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = reached.clone();
        let proxy_peer = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(stream.read_u8().await.unwrap());
            }
            assert!(
                head.starts_with(b"CONNECT example.org:80 HTTP/1.1\r\n"),
                "{head:?}"
            );
            stream
                .write_all(b"HTTP/1.1 200 Established\r\n\r\nproxy-route!")
                .await
                .unwrap();
        });
        let relay = Relay::spawn_with_bypass(proxy, "127.0.0.1:1".into())
            .await
            .unwrap();
        async fn connect(port: u16, target: &[u8]) -> TcpStream {
            let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            client.write_all(&[5, 1, 0]).await.unwrap();
            let mut method = [0; 2];
            client.read_exact(&mut method).await.unwrap();
            assert_eq!(method, [5, 0]);
            client.write_all(target).await.unwrap();
            let mut result = [0; 10];
            client.read_exact(&mut result).await.unwrap();
            assert_eq!(&result[..2], &[5, 0]);
            client
        }
        let mut target = vec![5, 1, 0, 1, 127, 0, 0, 1];
        target.extend_from_slice(&direct_port.to_be_bytes());
        let mut client = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            connect(relay.port, &target),
        )
        .await
        .unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut content = [0; 12];
        client.read_exact(&mut content).await.unwrap();
        assert_eq!(&content, b"direct-route");
        assert_eq!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "bypassed origin must never dial the proxy"
        );
        drop(client);
        direct_peer.await.unwrap();
        let mut proxied = connect(relay.port, b"\x05\x01\x00\x03\x0bexample.org\x00\x50").await;
        proxied.read_exact(&mut content).await.unwrap();
        assert_eq!(&content, b"proxy-route!");
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
        proxy_peer.await.unwrap();
        drop(proxied);
        drop(relay);
    }

    #[tokio::test]
    async fn stealth_v3_relay_connected_tunnel_clears_strikes_before_close() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = Arc::new(
            crate::transport::proxy::Proxy::parse(&format!(
                "http://{}",
                upstream.local_addr().unwrap()
            ))
            .unwrap(),
        );
        let (release, released) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16384);
                head.push(stream.read_u8().await.unwrap());
            }
            assert!(head.starts_with(b"CONNECT example.org:443 HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 Established\r\n\r\n")
                .await
                .unwrap();
            let mut ping = [0; 4];
            stream.read_exact(&mut ping).await.unwrap();
            assert_eq!(&ping, b"ping");
            stream.write_all(b"pong").await.unwrap();
            released.await.unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let strikes = Arc::new(std::sync::Mutex::new(StrikeCache::default()));
        strikes.lock().unwrap().note_failure("example.org:443");
        strikes.lock().unwrap().note_failure("example.org:443");
        let observed = strikes.clone();
        let tunnel = tokio::spawn(serve_socks5_client(
            stream,
            proxy,
            strikes,
            Arc::new(String::new()),
        ));
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut answer = [0; 2];
        client.read_exact(&mut answer).await.unwrap();
        assert_eq!(answer, [5, 0]);
        client
            .write_all(b"\x05\x01\x00\x03\x0bexample.org\x01\xbb")
            .await
            .unwrap();
        let mut success = [0; 10];
        client.read_exact(&mut success).await.unwrap();
        assert_eq!(&success[..2], &[5, 0]);
        client.write_all(b"ping").await.unwrap();
        let mut pong = [0; 4];
        client.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");
        assert!(
            !observed
                .lock()
                .unwrap()
                .strikes
                .contains_key("example.org:443"),
            "an actual connected live tunnel must clear prior failures before it closes"
        );
        release.send(()).unwrap();
        drop(client);
        peer.await.unwrap();
        tunnel.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stealth_v3_relay_malformed_targets_never_dial_upstream() {
        let up = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy =
            crate::transport::proxy::Proxy::parse(&format!("http://{}", up.local_addr().unwrap()))
                .unwrap();
        let relay = Relay::spawn(Arc::new(proxy)).await.unwrap();
        let mut invalid = vec![
            vec![5, 1, 1, 1, 127, 0, 0, 1, 1, 187],
            vec![5, 2, 0, 1, 127, 0, 0, 1, 1, 187],
            vec![5, 1, 0, 9],
            vec![5, 1, 0, 1, 127, 0, 0, 1, 0, 0],
        ];
        for name in [
            b"".as_slice(),
            b"bad\r\nhost",
            b"bad/host",
            b"\xffhost",
            b"two..labels",
        ] {
            let mut request = vec![5, 1, 0, 3, name.len() as u8];
            request.extend_from_slice(name);
            request.extend_from_slice(&443u16.to_be_bytes());
            invalid.push(request);
        }
        for request in invalid {
            let mut client = TcpStream::connect(("127.0.0.1", relay.port)).await.unwrap();
            client.write_all(&[5, 1, 0]).await.unwrap();
            let mut greeting = [0; 2];
            client.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 0]);
            client.write_all(&request).await.unwrap();
            let mut reply = [0; 10];
            tokio::time::timeout(
                std::time::Duration::from_millis(300),
                client.read_exact(&mut reply),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(reply[0], 5);
            assert!(
                matches!(reply[1], 1 | 7 | 8),
                "invalid request must receive a real failure: {reply:?}"
            );
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), up.accept())
                    .await
                    .is_err(),
                "invalid request {request:?} must not reach the upstream listener"
            );
        }
    }

    #[tokio::test]
    async fn stealth_v3_relay_stalled_negotiation_expires() {
        let proxy = crate::transport::proxy::Proxy::parse("http://127.0.0.1:809").unwrap();
        let relay = Relay::spawn(Arc::new(proxy)).await.unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", relay.port)).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut greeting = [0; 2];
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 0]);
        let started = std::time::Instant::now();
        let mut byte = [0; 1];
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(12), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_secs(9),
            "normal clients retain the ten-second negotiation allowance"
        );
    }

    #[tokio::test]
    async fn stealth_v3_relay_never_selects_an_unoffered_auth_method() {
        let proxy = crate::transport::proxy::Proxy::parse("http://127.0.0.1:809").unwrap();
        let relay = Relay::spawn(Arc::new(proxy)).await.unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", relay.port)).await.unwrap();
        client.write_all(&[5, 1, 2]).await.unwrap();
        let mut reply = [0; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(
            reply,
            [5, 255],
            "relay cannot select no-auth when only password auth was offered"
        );
    }

    #[tokio::test]
    async fn stealth_v3_relay_drop_closes_actual_active_tunnel() {
        let up = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = crate::transport::proxy::Proxy::parse(&format!(
            "http://alice:secret@{}",
            up.local_addr().unwrap()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut peer, _) = up.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 4096);
                head.push(peer.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap();
            assert!(head.starts_with("CONNECT owned.test:443 HTTP/1.1\r\n"));
            assert!(head.contains("Proxy-Authorization: Basic YWxpY2U6c2VjcmV0\r\n"));
            peer.write_all(b"HTTP/1.1 200 Established\r\n\r\n")
                .await
                .unwrap();
            let mut payload = [0; 4];
            peer.read_exact(&mut payload).await.unwrap();
            assert_eq!(&payload, b"ping");
            peer.write_all(b"pong").await.unwrap();
            let mut extra = [0; 1];
            assert_eq!(
                peer.read(&mut extra).await.unwrap(),
                0,
                "relay retirement closes its upstream too"
            );
        });
        let relay = Relay::spawn(Arc::new(proxy)).await.unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", relay.port)).await.unwrap();
        client.write_all(&[5, 2, 2, 0]).await.unwrap();
        let mut greeting = [0; 2];
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 0]);
        let mut request = vec![5, 1, 0, 3, 10];
        request.extend_from_slice(b"owned.test");
        request.extend_from_slice(&443u16.to_be_bytes());
        client.write_all(&request).await.unwrap();
        let mut reply = [0; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0);
        client.write_all(b"ping").await.unwrap();
        let mut payload = [0; 4];
        client.read_exact(&mut payload).await.unwrap();
        assert_eq!(
            &payload, b"pong",
            "actual tunnel must be live before retirement"
        );
        drop(relay);
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_millis(300),
                client.read(&mut payload)
            )
            .await
            .expect("active relay tunnel must not survive its owner")
            .unwrap(),
            0
        );
        tokio::time::timeout(std::time::Duration::from_millis(300), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn stealth_v3_relay_bounds_stalled_clients_and_releases_them() {
        let up = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy =
            crate::transport::proxy::Proxy::parse(&format!("http://{}", up.local_addr().unwrap()))
                .unwrap();
        let relay = Relay::spawn(Arc::new(proxy)).await.unwrap();
        let mut clients = Vec::new();
        for _ in 0..64 {
            let mut client = TcpStream::connect(("127.0.0.1", relay.port)).await.unwrap();
            client.write_all(&[5, 1, 0]).await.unwrap();
            let mut reply = [0; 2];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [5, 0]);
            clients.push(client);
        }
        let mut excess = TcpStream::connect(("127.0.0.1", relay.port)).await.unwrap();
        let mut byte = [0; 1];
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_millis(300),
                excess.read(&mut byte)
            )
            .await
            .expect("excess connection must be refused immediately")
            .unwrap(),
            0
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), up.accept())
                .await
                .is_err(),
            "stalled local greetings must not dial upstream"
        );
        let mut released = clients.pop().unwrap();
        released.write_all(&[5, 2, 0, 1]).await.unwrap();
        let mut failure = [0; 10];
        released.read_exact(&mut failure).await.unwrap();
        assert_eq!(failure[1], 7);
        assert_eq!(released.read(&mut byte).await.unwrap(), 0);
        let mut replacement = TcpStream::connect(("127.0.0.1", relay.port)).await.unwrap();
        replacement.write_all(&[5, 1, 0]).await.unwrap();
        let mut reply = [0; 2];
        tokio::time::timeout(
            std::time::Duration::from_millis(300),
            replacement.read_exact(&mut reply),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            reply,
            [5, 0],
            "released capacity must accept a new handshake"
        );
        clients.push(replacement);
        drop(relay);
        for mut client in clients {
            assert_eq!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(300),
                    client.read(&mut byte)
                )
                .await
                .unwrap()
                .unwrap(),
                0
            );
        }
    }

    /// The relay turns Chrome's unauthenticated SOCKS5 into the
    /// lane's authenticated upstream handshake, then pipes bytes.
    /// Discriminating: without the relay an authenticated lane is
    /// unusable by Chrome at all (ERR_SOCKS_CONNECTION_FAILED).
    #[tokio::test]
    async fn relay_authenticates_upstream_and_pipes_bytes() {
        let up = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = up.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (s, _) = up.accept().await.unwrap();
            serve_required_auth_upstream(s).await;
        });

        let proxy = crate::transport::proxy::Proxy::parse(&format!(
            "socks5://laneuser:lanepass@127.0.0.1:{up_port}"
        ))
        .expect("authed socks5 lane parses");
        assert!(
            !proxy.user.is_empty(),
            "the test lane must carry credentials"
        );

        let relay = Relay::spawn(std::sync::Arc::new(proxy))
            .await
            .expect("relay binds on loopback");

        // Chrome side: plain no-auth SOCKS5 greeting.
        let mut c = TcpStream::connect(("127.0.0.1", relay.port)).await.unwrap();
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut greeting = [0u8; 2];
        c.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [0x05, 0x00], "relay must accept no-auth");

        // CONNECT example.internal:809.
        let mut req = vec![0x05, 0x01, 0x00, 0x03, 16];
        req.extend_from_slice(b"example.internal");
        req.extend_from_slice(&809u16.to_be_bytes());
        c.write_all(&req).await.unwrap();
        let mut reply = [0u8; 10];
        c.read_exact(&mut reply).await.unwrap();
        assert_eq!(
            reply[1], 0x00,
            "relay must report success after the authed upstream dial"
        );

        // Payload flows both ways through the relay.
        c.write_all(b"ping").await.unwrap();
        let mut echo = [0u8; 4];
        c.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"pong");
        drop(c);

        server.await.unwrap();
    }

    async fn serve_required_auth_upstream(mut s: TcpStream) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut greeting = [0u8; 2];
        s.read_exact(&mut greeting).await.unwrap();
        let n = greeting[1] as usize;
        let mut methods = vec![0u8; n];
        s.read_exact(&mut methods).await.unwrap();
        assert_eq!(greeting[0], 0x05);
        assert!(
            methods.contains(&0x02),
            "our client must offer user/pass to an authed lane"
        );
        s.write_all(&[0x05, 0x02]).await.unwrap();
        let mut hdr = [0u8; 2];
        s.read_exact(&mut hdr).await.unwrap();
        assert_eq!(hdr[0], 0x01);
        let mut name = vec![0u8; hdr[1] as usize];
        s.read_exact(&mut name).await.unwrap();
        let mut plen = [0u8; 1];
        s.read_exact(&mut plen).await.unwrap();
        let mut pass = vec![0u8; plen[0] as usize];
        s.read_exact(&mut pass).await.unwrap();
        assert_eq!(name, b"laneuser");
        assert_eq!(pass, b"lanepass");
        s.write_all(&[0x01, 0x00]).await.unwrap();
        let mut head = [0u8; 4];
        s.read_exact(&mut head).await.unwrap();
        assert_eq!(head, [0x05, 0x01, 0x00, 0x03]);
        let mut len = [0u8; 1];
        s.read_exact(&mut len).await.unwrap();
        let mut host = vec![0u8; len[0] as usize];
        s.read_exact(&mut host).await.unwrap();
        let mut port = [0u8; 2];
        s.read_exact(&mut port).await.unwrap();
        assert_eq!(host, b"example.internal");
        assert_eq!(u16::from_be_bytes(port), 809);
        s.write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let mut ping = [0u8; 4];
        s.read_exact(&mut ping).await.unwrap();
        s.write_all(b"pong").await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stealth_v3_relay_failure_memory_stays_bounded() {
        let mut cache = StrikeCache::default();
        for ordinal in 0..1025 {
            for _ in 0..STRIKE_LIMIT {
                cache.note_failure(&format!("owned-{ordinal}.test:443"));
            }
        }
        assert!(
            cache.strikes.len() <= 1024,
            "fresh hostile hosts must not grow memory for the entire TTL"
        );
        assert!(cache.refused("owned-1024.test:443"));
        assert!(
            !cache.refused("owned-0.test:443"),
            "oldest row is the bounded eviction candidate"
        );
    }

    /// The strike cache must not punish a healed host: three fresh
    /// dial failures fast-reject, strikes expire after the TTL, a
    /// limit that went stale does not reject on its own, and a
    /// successful connect clears the host's strikes entirely.
    #[test]
    fn strikes_stop_at_limit_until_ttl_and_clear_on_success() {
        let mut c = StrikeCache {
            strikes: Default::default(),
        };
        assert!(!c.refused("h:1"));
        for _ in 0..STRIKE_LIMIT {
            c.note_failure("h:1");
        }
        assert!(c.refused("h:1"), "3 fresh failures = fast-reject");

        // Age every strike past the TTL: the host counts as healed.
        for (_, at) in c.strikes.values_mut() {
            *at = std::time::Instant::now() - STRIKE_TTL - std::time::Duration::from_secs(1);
        }
        assert!(!c.refused("h:1"), "an expired strike must not fast-reject");

        // One failure after the TTL re-stamps fresh: the limit restarts.
        c.note_failure("h:1");
        assert!(
            !c.refused("h:1"),
            "a single post-TTL failure must not fast-reject"
        );

        // A successful connect clears the host's strikes outright.
        for _ in 0..STRIKE_LIMIT {
            c.note_failure("h:2");
        }
        assert!(c.refused("h:2"));
        c.note_success("h:2");
        assert!(!c.refused("h:2"), "success must clear strikes");
    }

    /// The map must not accumulate healed rows forever: a browser session
    /// dials thousands of hosts, and `refused` only checks freshness on
    /// read, so nothing used to remove them. Pruning runs on the failure
    /// path (the only writer) and drops exactly the expired rows.
    #[test]
    fn expired_rows_are_pruned_not_kept_forever() {
        let mut c = StrikeCache {
            strikes: Default::default(),
        };
        // One live host and one healed long ago.
        for _ in 0..STRIKE_LIMIT {
            c.note_failure("live:443");
        }
        c.note_failure("healed:443");
        let healed = c.strikes.get_mut("healed:443").expect("row inserted");
        healed.1 = std::time::Instant::now() - STRIKE_TTL - std::time::Duration::from_secs(1);
        assert_eq!(c.strikes.len(), 2);

        // Any later failure prunes the expired row, keeps the live one.
        c.note_failure("other:443");
        assert!(
            !c.strikes.contains_key("healed:443"),
            "an expired strike row must be dropped, not kept for the process lifetime"
        );
        assert!(c.strikes.contains_key("live:443"));
        assert_eq!(c.strikes.len(), 2, "live + other, healed pruned");
    }
}
