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
use tokio::task::JoinHandle;

/// A running relay bound to one upstream lane. Dropping it aborts
/// the accept loop; Chrome dies with it.
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
    /// Bind 127.0.0.1:0 and start the accept loop. `None` when the
    /// listener cannot bind (port exhaustion?); the caller then
    /// falls back to the raw, credential-less proxy arg so the
    /// launch still proceeds exactly as before.
    pub async fn spawn(proxy: Arc<crate::transport::proxy::Proxy>) -> Option<Relay> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
        let port = listener.local_addr().ok()?.port();
        let handle = tokio::spawn(async move {
            accept_loop(listener, proxy).await;
        });
        Some(Relay {
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

async fn accept_loop(listener: TcpListener, proxy: Arc<crate::transport::proxy::Proxy>) {
    let strikes = std::sync::Arc::new(std::sync::Mutex::new(StrikeCache::default()));
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let proxy = Arc::clone(&proxy);
                let strikes = std::sync::Arc::clone(&strikes);
                tokio::spawn(async move {
                    let _ = serve_socks5_client(stream, proxy, strikes).await;
                });
            }
            Err(_) => return,
        }
    }
}

async fn serve_socks5_client(
    mut client: TcpStream,
    proxy: Arc<crate::transport::proxy::Proxy>,
    strikes: std::sync::Arc<std::sync::Mutex<StrikeCache>>,
) -> std::io::Result<()> {
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).await?;
    let [ver, nmethods] = greeting;
    if ver != 5 || nmethods == 0 {
        return Ok(());
    }
    let mut methods = vec![0u8; nmethods as usize];
    client.read_exact(&mut methods).await?;
    client.write_all(&[5u8, 0u8]).await?;

    let mut head = [0u8; 4];
    client.read_exact(&mut head).await?;
    let [ver, cmd, _rsv, atyp] = head;
    if ver != 5 || cmd != 1 {
        let _ = client
            .write_all(&[5u8, cmd.max(1), 0u8, 1u8, 0, 0, 0, 0, 0, 0])
            .await;
        return Ok(());
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
            String::from_utf8_lossy(&name).into_owned()
        }
        4u8 => {
            let mut octets = [0u8; 16];
            client.read_exact(&mut octets).await?;
            format_ipv6(&octets)
        }
        _ => return Ok(()),
    };
    let mut port_bytes = [0u8; 2];
    client.read_exact(&mut port_bytes).await?;
    let port = u16::from_be_bytes(port_bytes);

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

    match proxy.connect(&host, port).await {
        Ok(mut upstream) => {
            if client
                .write_all(&[5u8, 0u8, 0u8, 1u8, 0, 0, 0, 0, 0, 0])
                .await
                .is_err()
            {
                return Ok(());
            }
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            // A successful connect clears the host's strikes: the
            // outage that struck it is over, and a fast-reject held
            // past the healing would starve the page forever.
            strikes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .note_success(&host_key);
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
