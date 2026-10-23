//! Process-local DNS cache.
//!
//! The host resolver on a typical box is a network round trip: this one
//! answers from public resolvers with no local caching daemon, and one
//! lookup costs ~55ms. A single fetch resolves the same name at least
//! twice (the SSRF guard, then the connect), a redirect hop adds another
//! pair, and a crawl of one host used to pay it per page. Chrome keeps a
//! host-resolver cache for exactly this reason; so does DonSeTch.
//!
//! Safety rules, both deliberate:
//!
//! - Only POSITIVE results are cached, for the short TTL
//!   `fetch.dns_cache_ttl_secs` (default 30, 0 disables). A failure or a
//!   timeout is never cached: a resolver blip must not block a host for
//!   the whole TTL.
//! - Caching changes NOTHING about the SSRF decision. Every caller still
//!   filters the addresses it is about to dial (`is_ssrf_resolved_ip`),
//!   so a name that starts answering with a private address inside the
//!   TTL is still refused at the dial.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::FetchError;

/// Matches the guard's original DNS deadline; a resolver that answers
/// later than this is useless to a tool whose whole pitch is speed.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Ceiling on cached names. A daemon that crawls the web for days must
/// not grow one entry per host forever; the oldest entry is evicted once
/// the map is full and nothing has expired.
const MAX_ENTRIES: usize = 512;

struct Entry {
    addrs: Vec<SocketAddr>,
    at: Instant,
}

type Key = (String, u16);

fn cache() -> &'static Mutex<HashMap<Key, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<Key, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

// A lock poisoned by a panic elsewhere still holds usable data (a map of
// addresses), so the cache keeps working instead of degrading to a
// permanent miss.
fn lock() -> std::sync::MutexGuard<'static, HashMap<Key, Entry>> {
    match cache().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// (hits, misses) since process start. The receipts for the cache's
/// effect read this rather than guessing from wall clock.
pub fn stats() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

fn ttl() -> Option<Duration> {
    match crate::config::cfg().fetch.dns_cache_ttl_secs {
        0 => None,
        s => Some(Duration::from_secs(s)),
    }
}

/// Resolve `host:port`, serving a cached answer inside the TTL.
///
/// Errors are typed: `Dns` for a name that does not resolve or resolves
/// to nothing, `DnsTimeout` for a resolver that does not answer. Both are
/// name failures, never policy blocks (see `mcp::server::errors`).
pub async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, FetchError> {
    // A literal needs no resolver. `Url::host_str` keeps the brackets
    // on an IPv6 literal, and getaddrinfo does not know "[::1]", so
    // every v6-literal URL failed here with a DNS error; the guard's
    // v6 rules were never reached end to end. Same answer set as a
    // lookup would give, and nothing to cache.
    if let Ok(ip) = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .parse::<std::net::IpAddr>()
    {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let ttl = ttl();
    if let Some(ttl) = ttl
        && let Some(addrs) = cached(host, port, ttl)
    {
        HITS.fetch_add(1, Ordering::Relaxed);
        return Ok(addrs);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    let addrs: Vec<SocketAddr> =
        tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| {
                FetchError::DnsTimeout(format!(
                    "the resolver did not answer within {}s for {host}",
                    LOOKUP_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|e| classify_lookup_error(host, &e))?
            .collect();
    if addrs.is_empty() {
        return Err(FetchError::Dns(format!("{host} resolved to no addresses")));
    }
    if let Some(ttl) = ttl {
        store(host, port, &addrs, ttl);
    }
    Ok(addrs)
}

/// A resolver that could not answer NOW (EAI_AGAIN: resolv.conf
/// unreachable, SERVFAIL, a VPN flap) is the same transient signal as
/// one that did not answer in time, not a name that does not exist.
/// #248 made `Dns` permanent, so this arm has to stay `DnsTimeout` or
/// an outage reads as a dead name and the caller is told not to
/// retry. getaddrinfo's EAI code never reaches std, only its text:
/// glibc/macOS "Temporary failure in name resolution", musl "Try
/// again", Windows WSATRY_AGAIN 11002.
fn classify_lookup_error(host: &str, e: &std::io::Error) -> FetchError {
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    let again = lower.contains("temporary failure")
        || lower.contains("try again")
        || e.raw_os_error() == Some(11002);
    if again {
        FetchError::DnsTimeout(format!("the resolver could not answer for {host}: {text}"))
    } else {
        FetchError::Dns(format!("could not resolve {host}: {text}"))
    }
}

fn cached(host: &str, port: u16, ttl: Duration) -> Option<Vec<SocketAddr>> {
    let key = (host.to_ascii_lowercase(), port);
    let mut c = lock();
    match c.get(&key) {
        Some(e) if e.at.elapsed() < ttl => Some(e.addrs.clone()),
        // Expired: drop it now so a stale answer cannot be resurrected.
        Some(_) => {
            c.remove(&key);
            None
        }
        None => None,
    }
}

fn store(host: &str, port: u16, addrs: &[SocketAddr], ttl: Duration) {
    let mut c = lock();
    if c.len() >= MAX_ENTRIES {
        c.retain(|_, e| e.at.elapsed() < ttl);
        if c.len() >= MAX_ENTRIES
            && let Some(oldest) = c.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone())
        {
            c.remove(&oldest);
        }
    }
    c.insert(
        (host.to_ascii_lowercase(), port),
        Entry {
            addrs: addrs.to_vec(),
            at: Instant::now(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // The point of the module: the second lookup inside the window must
    // not touch the resolver. Both calls resolve a real name here, so the
    // test fails if the cache is bypassed or the TTL reads as zero.
    #[tokio::test]
    async fn a_second_lookup_inside_the_window_is_served_from_the_cache() {
        let host = "localhost";
        let (h0, m0) = stats();
        let first = resolve(host, 8443).await.expect("localhost must resolve");
        let (h1, m1) = stats();
        let second = resolve(host, 8443).await.expect("localhost must resolve");
        let (h2, m2) = stats();
        assert_eq!(m1 - m0, 1, "first call must be a miss");
        assert_eq!(h1 - h0, 0, "first call cannot be a hit");
        assert_eq!(h2 - h1, 1, "second call must be a hit: {h2}/{m2}");
        assert_eq!(first, second, "a cached answer must be the same answer");
    }

    // A failure must never be cached: one resolver blip must not block a
    // host for the whole TTL.
    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_never_cached() {
        let host = "no-such-host-for-donsetch-probe.invalid";
        let (h0, m0) = stats();
        assert!(resolve(host, 443).await.is_err());
        assert!(resolve(host, 443).await.is_err());
        let (h1, m1) = stats();
        assert_eq!(m1 - m0, 2, "both calls must miss");
        assert_eq!(h1 - h0, 0, "a failure must not be cached");
    }

    // The cache holds addresses, not decisions: a name that resolves to a
    // private address is refused on the FIRST call and on the cached one.
    #[tokio::test]
    async fn a_cached_answer_is_still_ssrf_filtered_at_every_use() {
        for _ in 0..2 {
            let err = crate::fetch::guards::ensure_url_safe("http://localhost/")
                .await
                .expect_err("loopback must be refused");
            assert!(matches!(err, FetchError::Ssrf(_)), "got {err:?}");
        }
    }

    // #248 made `Dns` permanent ("do not retry"). getaddrinfo's
    // EAI_AGAIN (resolver unreachable, SERVFAIL) arrives as the same
    // io::Error type as NXDOMAIN; only the text tells them apart, so
    // the transient one must be routed to the transient variant.
    #[test]
    fn a_resolver_that_cannot_answer_now_is_transient_not_a_dead_name() {
        use std::io::Error;
        let again = [
            "failed to lookup address information: Temporary failure in name resolution",
            "failed to lookup address information: Try again",
        ];
        for msg in again {
            let e = classify_lookup_error("example.com", &Error::other(msg));
            assert!(matches!(e, FetchError::DnsTimeout(_)), "{msg} -> {e:?}");
        }
        let win = Error::from_raw_os_error(11002);
        assert!(matches!(
            classify_lookup_error("example.com", &win),
            FetchError::DnsTimeout(_)
        ));
        let nx = Error::other("failed to lookup address information: Name or service not known");
        let e = classify_lookup_error("nope.invalid", &nx);
        assert!(matches!(e, FetchError::Dns(_)), "{e:?}");
        assert!(e.to_string().contains("nope.invalid"));
    }

    // The bracketed form is what Url::host_str hands over; getaddrinfo
    // refuses it, so a v6-literal URL never dialed.
    #[tokio::test]
    async fn a_bracketed_v6_literal_is_answered_without_the_resolver() {
        let got = resolve("[::1]", 8080).await.unwrap();
        assert_eq!(got, vec!["[::1]:8080".parse::<SocketAddr>().unwrap()]);
        let got = resolve("[2606:4700::1111]", 443).await.unwrap();
        assert_eq!(
            got[0].ip(),
            "2606:4700::1111".parse::<std::net::IpAddr>().unwrap()
        );
        let got = resolve("127.0.0.1", 80).await.unwrap();
        assert_eq!(got, vec!["127.0.0.1:80".parse::<SocketAddr>().unwrap()]);
        let (_, misses_before) = stats();
        let _ = resolve("[::1]", 1).await.unwrap();
        assert_eq!(stats().1, misses_before, "a literal is not a cache miss");
    }

    // Long-lived daemon, unbounded map: the entry cap has to hold.
    #[test]
    fn the_map_stays_bounded() {
        let addr: SocketAddr = "93.184.216.34:443".parse().unwrap();
        let ttl = Duration::from_secs(30);
        for i in 0..(MAX_ENTRIES + 40) {
            store(&format!("host-{i}.example"), 443, &[addr], ttl);
        }
        let len = lock().len();
        assert!(len <= MAX_ENTRIES, "cache grew to {len} entries");
    }
}
