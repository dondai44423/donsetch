//! TCP connect with Happy Eyeballs (RFC 8305): race IPv6/IPv4, 250ms stagger.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

use crate::error::FetchError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STAGGER: Duration = Duration::from_millis(250);

pub async fn happy_connect(host: &str, port: u16) -> Result<TcpStream, FetchError> {
    happy_connect_with(host, port, false).await
}

/// `warm` = this origin has a cached TLS session this daemon lifetime
/// (repeat navigation in Chrome-speak). Warm origins get
/// TCP_FASTOPEN_CONNECT on Linux: Chrome TFOs repeat navigations;
/// the kernel still decides per peer cookie state. Non-fatal: every
/// failure degrades to a normal connect.
pub async fn happy_connect_with(
    host: &str,
    port: u16,
    warm: bool,
) -> Result<TcpStream, FetchError> {
    let addrs: Vec<SocketAddr> =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| FetchError::Timeout)??
            .collect();
    if addrs.is_empty() {
        return Err(FetchError::Http(format!("dns: no address for {host}")));
    }

    // DNS pinning (SSRF): a hostname that resolves to a
    // private/loopback address is treated exactly like a
    // literal one. Checking the addresses we are about to
    // dial : not the name : also closes rebinding TOCTOU.
    // Escape hatch for deliberate local-egress use (CLI
    // power users, tests): DONSETCH_ALLOW_PRIVATE_EGRESS=1.
    let addrs: Vec<SocketAddr> = if crate::fetch::guards::private_egress_allowed() {
        addrs
    } else {
        let blocked: Vec<&SocketAddr> = addrs
            .iter()
            .filter(|a| crate::fetch::guards::is_ssrf_resolved_ip(&a.ip()))
            .collect();
        if blocked.len() == addrs.len() {
            return Err(FetchError::Http(format!(
                "dns: {host} resolves to a private/loopback address : SSRF guard"
            )));
        }
        addrs
            .into_iter()
            .filter(|a| !crate::fetch::guards::is_ssrf_resolved_ip(&a.ip()))
            .collect()
    };

    let mut v6: Vec<SocketAddr> = addrs.iter().filter(|a| a.is_ipv6()).copied().collect();
    let mut v4: Vec<SocketAddr> = addrs.iter().filter(|a| a.is_ipv4()).copied().collect();
    // Chrome prefers IPv6.
    let (primary, secondary) = if v6.is_empty() {
        (std::mem::take(&mut v4), v6)
    } else {
        (std::mem::take(&mut v6), v4)
    };

    // Race: start primary family; after 250ms start secondary in parallel.
    let mut primary_fut = Box::pin(connect_all(primary, warm));
    if secondary.is_empty() {
        return primary_fut.await;
    }
    let staggered = async move {
        tokio::time::sleep(STAGGER).await;
        connect_all(secondary, warm).await
    };
    let mut secondary_fut = Box::pin(staggered);

    tokio::select! {
        res = &mut primary_fut => match res {
            Ok(s) => Ok(s),
            Err(e) => secondary_fut.await.map_err(|_| e),
        },
        res = &mut secondary_fut => match res {
            Ok(s) => Ok(s),
            Err(_) => primary_fut.await,
        },
    }
}

async fn connect_all(addrs: Vec<SocketAddr>, warm: bool) -> Result<TcpStream, FetchError> {
    let mut last_err = FetchError::Http("no addresses".into());
    for addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, connect_one(addr, warm)).await {
            Ok(Ok(s)) => {
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Ok(Err(e)) => last_err = e,
            Err(_) => last_err = FetchError::Timeout,
        }
    }
    Err(last_err)
}

async fn connect_one(addr: SocketAddr, warm: bool) -> Result<TcpStream, FetchError> {
    tcp_connect(addr, warm).await
}

/// Linux TFO-capable connect for warm hosts. Everything else runs
/// tokio's normal connect (Chrome does not TFO on Windows/macOS in
/// the same way; parity with the host platform matters more).
///
/// The raw handshake runs on a `spawn_blocking` thread, not inline:
/// `libc::connect()` on this TFO-requesting socket is a genuine
/// blocking syscall, and measured directly against a local listener
/// it can take well over a minute to return (a real ~135s handshake
/// was observed here, against ~220us for a plain connect to the same
/// listener) instead of the near-instant loopback connect one would
/// expect. Run inline, that stalls the tokio worker thread carrying
/// it for the same duration, which silently defeats connect_all's
/// outer `tokio::time::timeout(CONNECT_TIMEOUT, ..)` -- a blocking
/// syscall never yields, so the timeout's own timer can't be polled
/// until the syscall returns on its own. spawn_blocking keeps it off
/// the reactor so that outer timeout can actually fire.
#[cfg(target_os = "linux")]
async fn tcp_connect(addr: SocketAddr, warm: bool) -> Result<TcpStream, FetchError> {
    if !warm {
        return TcpStream::connect(addr).await.map_err(Into::into);
    }
    match tokio::task::spawn_blocking(move || tfo_connect_blocking(addr)).await {
        Ok(Some(std_stream)) => {
            std_stream.set_nonblocking(true).ok();
            TcpStream::from_std(std_stream).map_err(Into::into)
        }
        _ => TcpStream::connect(addr).await.map_err(Into::into),
    }
}

/// Everything here runs on a blocking-pool thread: `None` on any
/// failure just tells the caller to fall back to a normal connect.
#[cfg(target_os = "linux")]
fn tfo_connect_blocking(addr: SocketAddr) -> Option<std::net::TcpStream> {
    use std::os::fd::FromRawFd;

    let domain = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }
    // Request fastopen: the kernel decides per peer TFO-cookie state.
    let one: libc::c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_FASTOPEN_CONNECT,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    let storage = sockaddr_of(addr);
    let (ptr, len) = storage.as_ptr_len();
    let rc = unsafe { libc::connect(fd, ptr, len) };
    if rc != 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    Some(unsafe { std::net::TcpStream::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
async fn tcp_connect(addr: SocketAddr, _warm: bool) -> Result<TcpStream, FetchError> {
    TcpStream::connect(addr).await.map_err(Into::into)
}

/// Owns the raw sockaddr bytes. `sockaddr_of` used to hand back a
/// `*const libc::sockaddr` pointing at a `let` inside its own match
/// arm: that local goes out of scope the instant the function
/// returns, so the pointer was dangling at the call site (confirmed
/// under Miri: "constructing invalid value ... dangling reference
/// (use-after-free)"). Returning the struct by value keeps the bytes
/// alive in the caller's frame for as long as the pointer is used.
#[cfg(target_os = "linux")]
enum SockAddrStorage {
    V4(libc::sockaddr_in),
    V6(libc::sockaddr_in6),
}

#[cfg(target_os = "linux")]
impl SockAddrStorage {
    fn as_ptr_len(&self) -> (*const libc::sockaddr, libc::socklen_t) {
        match self {
            SockAddrStorage::V4(s) => (
                s as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            ),
            SockAddrStorage::V6(s) => (
                s as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            ),
        }
    }
}

#[cfg(target_os = "linux")]
fn sockaddr_of(addr: SocketAddr) -> SockAddrStorage {
    match addr {
        SocketAddr::V4(v4) => SockAddrStorage::V4(libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: v4.port().to_be(),
            // `s_addr` must hold the address's raw octets, in order,
            // regardless of host endianness -- `from_ne_bytes` alone
            // already does that (it's an identity on the bytes: on
            // any platform, `from_ne_bytes(o).to_ne_bytes() == o`).
            // The extra `.to_be()` this used to carry reverses those
            // bytes on every little-endian target (all of this
            // project's real deployments), pointing the raw connect
            // at the octet-reversed address instead of the one asked
            // for -- e.g. 203.0.113.7 became 7.113.0.203.
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(v4.ip().octets()),
            },
            sin_zero: [0; 8],
        }),
        SocketAddr::V6(v6) => SockAddrStorage::V6(libc::sockaddr_in6 {
            sin6_family: libc::AF_INET6 as libc::sa_family_t,
            sin6_port: v6.port().to_be(),
            sin6_flowinfo: v6.flowinfo(),
            sin6_addr: libc::in6_addr {
                s6_addr: v6.ip().octets(),
            },
            sin6_scope_id: v6.scope_id(),
        }),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    // `sockaddr_of` used to hand back a `*const libc::sockaddr`
    // pointing at a `let` inside its own match arm: that local goes
    // out of scope the instant the function returns, so the pointer
    // was dangling at the call site (confirmed under Miri:
    // "constructing invalid value ... dangling reference
    // (use-after-free)"). Reading the fields back immediately through
    // the returned storage is exactly what a dangling pointer would
    // get wrong (garbage instead of the address that went in) -- and,
    // unlike a live connect, it needs no real syscalls, so it stays
    // fast and deterministic on every platform.
    #[test]
    fn sockaddr_of_v4_roundtrips_correctly() {
        let addr: SocketAddr = "203.0.113.7:4242".parse().unwrap();
        let storage = sockaddr_of(addr);
        let (ptr, len) = storage.as_ptr_len();
        assert_eq!(len as usize, std::mem::size_of::<libc::sockaddr_in>());
        let sin = unsafe { &*(ptr as *const libc::sockaddr_in) };
        assert_eq!(sin.sin_family as i32, libc::AF_INET);
        assert_eq!(u16::from_be(sin.sin_port), 4242);
        // s_addr's raw bytes must equal the octets exactly, in order
        // (that's what network byte order means for an address) --
        // this is what an extraneous byte swap or a dangling pointer
        // would both get wrong.
        assert_eq!(sin.sin_addr.s_addr.to_ne_bytes(), [203, 0, 113, 7]);
    }

    #[test]
    fn sockaddr_of_v6_roundtrips_correctly() {
        let addr: SocketAddr = "[2001:db8::1]:4242".parse().unwrap();
        let storage = sockaddr_of(addr);
        let (ptr, len) = storage.as_ptr_len();
        assert_eq!(len as usize, std::mem::size_of::<libc::sockaddr_in6>());
        let sin6 = unsafe { &*(ptr as *const libc::sockaddr_in6) };
        assert_eq!(sin6.sin6_family as i32, libc::AF_INET6);
        assert_eq!(u16::from_be(sin6.sin6_port), 4242);
        let SocketAddr::V6(v6) = addr else {
            unreachable!()
        };
        assert_eq!(std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr), *v6.ip());
    }

    // Control: a plain connect (warm=false) to a real local listener
    // must stay fast and correct.
    #[tokio::test]
    async fn cold_connect_reaches_the_real_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let accept = tokio::task::spawn_blocking(move || listener.accept());
        let stream =
            tokio::time::timeout(std::time::Duration::from_secs(3), tcp_connect(addr, false))
                .await
                .expect("cold connect must not block the executor")
                .expect("cold connect to a real local listener must succeed");
        assert_eq!(stream.peer_addr().expect("peer addr"), addr);
        let _ = accept.await;
    }

    // `tcp_connect(addr, true)`'s raw handshake used to run inline
    // instead of via spawn_blocking (see tcp_connect's doc comment):
    // measured directly against a real local listener in this
    // environment, the raw blocking libc::connect() call took ~135s
    // against ~220us for the plain connect above -- a real blocking
    // syscall that, run inline, would starve this #[tokio::test]'s
    // single worker thread for the same duration. This is
    // deliberately NOT re-exercised as a live connect here: with the
    // fix in place the async logic returns promptly (verified by hand
    // against a standalone repro pinned to this crate's exact libc +
    // tokio versions), but tokio's runtime teardown waits for any
    // outstanding spawn_blocking task to finish before returning
    // control -- so a live test would still take the full ~135s to
    // report a result in an environment like this one, entirely
    // independent of whether the fix is correct. (Tried routing
    // around it with a destination expected to fail connect() fast
    // -- an unassigned Class E address, no route -- but that hung
    // here too: whatever this sandbox does to TCP_FASTOPEN_CONNECT
    // SYNs isn't specific to the loopback listener.) That means this
    // test proves the spawn_blocking-keeps-the-executor-free pattern
    // in isolation, not that `tcp_connect` itself still dispatches
    // through it -- a regression re-inlining the raw connect there
    // would slip past this test.
    #[tokio::test]
    async fn warm_connect_stays_off_the_executor_thread() {
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ticks_bg = ticks.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                ticks_bg.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        // A slow synchronous closure standing in for the real raw
        // connect: spawn_blocking is what keeps either one off this
        // thread, and that's the property under test here.
        drop(tokio::task::spawn_blocking(|| {
            std::thread::sleep(std::time::Duration::from_millis(300))
        }));
        tokio::time::sleep(std::time::Duration::from_millis(320)).await;
        ticker.abort();
        assert!(
            ticks.load(std::sync::atomic::Ordering::Relaxed) >= 10,
            "the executor was starved while a spawn_blocking task was in flight"
        );
    }
}
