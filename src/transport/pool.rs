//! Per-origin connection pool. h2 conns are reused; h1 conns are one-shot
//! (servers close them unpredictably).

use std::collections::HashMap;
use std::time::{Duration, Instant};

// Idle sockets can survive a server/load-balancer timeout without a usable
// peer. Reconnect them; leave the response budget for fresh requests intact.
const MAX_IDLE: Duration = Duration::from_secs(60);

use super::h2::conn::H2Conn;

pub struct Pool {
    h2: HashMap<String, (H2Conn, Instant)>,
}

impl Pool {
    pub fn new() -> Self {
        Self { h2: HashMap::new() }
    }

    pub fn take_h2(&mut self, origin: &str) -> Option<H2Conn> {
        self.h2
            .remove(origin)
            .and_then(|(conn, used)| (used.elapsed() < MAX_IDLE).then_some(conn))
    }

    pub fn put_h2(&mut self, origin: &str, conn: H2Conn) {
        // Cap the pool; evict the least recently used idle connection.
        if self.h2.len() >= 64
            && !self.h2.contains_key(origin)
            && let Some(k) = self
                .h2
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone())
        {
            self.h2.remove(&k);
        }
        self.h2.insert(origin.to_string(), (conn, Instant::now()));
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boring::ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode};
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn report_audit_idle_pool_reconnects_and_releases_the_old_socket() {
        let cert = boring::x509::X509::from_pem(include_bytes!("../../tests/landmarks/h2cert.pem"))
            .unwrap();
        let key = boring::pkey::PKey::private_key_from_pem(include_bytes!(
            "../../tests/landmarks/h2key.pem"
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
            // The peer observes closure after SETTINGS/window frames, rather
            // than a request sent on an expired connection.
            let mut frames = Vec::new();
            let closed = tls.read_to_end(&mut frames).await;
            // Dropping TLS without close_notify can report an EOF error.
            assert!(closed.is_ok() || closed.unwrap_err().to_string().contains("EOF"));
            assert!(!frames.is_empty());
        });
        let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
        // Only this self-signed loopback fixture disables certificate trust.
        connector.set_verify(SslVerifyMode::NONE);
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let tls = tokio_boring::connect(connector.build().configure().unwrap(), "localhost", tcp)
            .await
            .unwrap();
        let profile = crate::profile::BrowserProfile::chrome_150(crate::profile::Platform::Linux);
        let conn = H2Conn::start(tls, &profile).await.unwrap();
        let mut pool = Pool::new();
        pool.put_h2("fixture", conn);
        let fresh = pool
            .take_h2("fixture")
            .expect("fresh connection must be reused");
        pool.put_h2("fixture", fresh);
        pool.h2.get_mut("fixture").unwrap().1 = Instant::now() - MAX_IDLE - Duration::from_secs(1);
        assert!(
            pool.take_h2("fixture").is_none(),
            "idle connection must reconnect"
        );
        assert!(pool.h2.is_empty());
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
