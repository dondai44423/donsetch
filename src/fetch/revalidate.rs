//! Conditional revalidation cache (ETag / Last-Modified / Cache-Control).
//!
//! Browser-true cache behavior: honor fresh windows without a request,
//! otherwise send conditional headers and accept 304. Scrapers never do
//! this; browsers always do.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct CacheEntry {
    pub body: Vec<u8>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub fresh_until: Option<Instant>,
}

pub enum CacheCheck {
    /// Serve from cache, no request needed.
    Fresh(Vec<u8>, u16, Vec<(String, String)>),
    /// Send these conditional headers; a 304 means serve stored body.
    Revalidate(Vec<(String, String)>),
    /// No usable entry.
    None,
}

pub struct RevalidationCache {
    map: HashMap<String, CacheEntry>,
    /// Insert order for FIFO eviction (the map's own iteration order
    /// is arbitrary and can pick a hot victim).
    queue: std::collections::VecDeque<String>,
    /// Sum of `body.len()` over `map`, kept in step by `store`.
    total_bytes: usize,
}

const MAX_ENTRIES: usize = 512;
const MAX_BODY: usize = 8 << 20; // 8 MiB
/// Resident body bytes across all entries. The entry cap alone let
/// 512 × 8 MiB = 4 GiB sit in one long-lived daemon; a Fetcher lives
/// as long as the process.
const MAX_TOTAL_BYTES: usize = 64 << 20; // 64 MiB

impl RevalidationCache {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            queue: std::collections::VecDeque::new(),
            total_bytes: 0,
        }
    }

    pub fn check(&self, url: &str) -> CacheCheck {
        let Some(entry) = self.map.get(url) else {
            return CacheCheck::None;
        };
        if let Some(until) = entry.fresh_until
            && Instant::now() < until
        {
            return CacheCheck::Fresh(entry.body.clone(), entry.status, entry.headers.clone());
        }
        let mut cond = Vec::new();
        if let Some(e) = &entry.etag {
            cond.push(("if-none-match".to_string(), e.clone()));
        }
        if let Some(m) = &entry.last_modified {
            cond.push(("if-modified-since".to_string(), m.clone()));
        }
        if cond.is_empty() {
            CacheCheck::None
        } else {
            CacheCheck::Revalidate(cond)
        }
    }

    /// Stored body for a 304 merge.
    #[allow(clippy::type_complexity)]
    pub fn stored(&self, url: &str) -> Option<(Vec<u8>, u16, Vec<(String, String)>)> {
        self.map
            .get(url)
            .map(|e| (e.body.clone(), e.status, e.headers.clone()))
    }

    pub fn store(&mut self, url: &str, status: u16, headers: &[(String, String)], body: &[u8]) {
        if status != 200 || body.len() > MAX_BODY {
            return;
        }
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        // Vary: * means every request gets a different representation;
        // browsers never store it. Storing one arbitrary variant would
        // serve it to every future request until eviction (E12).
        let vary = get("vary").unwrap_or_default();
        if vary.trim() == "*" {
            return;
        }
        let cache_control = get("cache-control").unwrap_or_default().to_lowercase();
        if cache_control.contains("no-store") || cache_control.contains("private") {
            return;
        }
        let etag = get("etag");
        let last_modified = get("last-modified");
        // RFC 9111 §5.2.2.4: no-cache means "store, but never reuse
        // without validation". Serving it inside a max-age fresh
        // window defied the origin's explicit instruction; drop the
        // fresh window so every hit sends the conditional (the
        // validators below still make the entry worth storing).
        let fresh_until = if cache_control.contains("no-cache") {
            None
        } else {
            parse_max_age(&cache_control)
                .map(|secs| Instant::now() + Duration::from_secs(secs.min(3600)))
        };
        // Cache only when there's a reason: a validator or a fresh window.
        if etag.is_none() && last_modified.is_none() && fresh_until.is_none() {
            return;
        }
        // Evict the oldest inserts (FIFO) until the new entry fits
        // both the entry cap and the byte budget. The arbitrary
        // keys().next() victim a HashMap iteration order yields
        // could evict a hot entry; FIFO is the browser-near default
        // for a bounded cache this small. A re-store of an existing
        // key replaces its bytes, so its old size is not counted.
        let replacing = self.map.get(url).map_or(0, |e| e.body.len());
        let mut resident = self.total_bytes - replacing;
        while (self.map.len() >= MAX_ENTRIES && !self.map.contains_key(url))
            || resident + body.len() > MAX_TOTAL_BYTES
        {
            let Some(k) = self.queue.pop_front() else {
                break;
            };
            if k == url {
                // The key being re-stored is the oldest: dropping its
                // queue slot alone is enough, the insert re-queues it.
                self.map.remove(&k);
                continue;
            }
            if let Some(gone) = self.map.remove(&k) {
                resident -= gone.body.len();
            }
        }
        if !self.map.contains_key(url) {
            self.queue.push_back(url.to_string());
        }
        self.total_bytes = resident + body.len();
        self.map.insert(
            url.to_string(),
            CacheEntry {
                body: body.to_vec(),
                status,
                headers: headers.to_vec(),
                etag,
                last_modified,
                fresh_until,
            },
        );
    }
}

impl Default for RevalidationCache {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_max_age(cache_control: &str) -> Option<u64> {
    for part in cache_control.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("max-age=") {
            return v.trim_matches('"').parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    #[test]
    fn vary_star_is_never_stored() {
        let mut c = RevalidationCache::new();
        c.store(
            "https://x.test/a",
            200,
            &[
                ("etag".into(), "\"v1\"".into()),
                ("vary".into(), "*".into()),
            ],
            b"body",
        );
        assert!(
            matches!(c.check("https://x.test/a"), CacheCheck::None),
            "Vary: * must not be stored (E12)"
        );
    }

    // RFC 9111 §5.2.2.4: no-cache is stored but must NEVER be served
    // without validation. "no-cache, max-age=3600" used to be served
    // fresh for an hour, defying the origin's explicit instruction.
    #[test]
    fn no_cache_stores_but_never_serves_fresh() {
        let mut c = RevalidationCache::new();
        c.store(
            "https://x.test/nc",
            200,
            &[
                ("etag".into(), "\"v1\"".into()),
                ("cache-control".into(), "no-cache, max-age=3600".into()),
            ],
            b"body",
        );
        assert!(
            matches!(c.check("https://x.test/nc"), CacheCheck::Revalidate(_)),
            "no-cache must send the conditional on every hit"
        );
    }

    // 512 entries of 8 MiB each was 4 GiB resident. The byte budget
    // evicts oldest-first until the new body fits.
    #[test]
    fn resident_bytes_stay_under_the_budget() {
        let mut c = RevalidationCache::new();
        let big = vec![b'x'; MAX_BODY];
        for i in 0..16 {
            c.store(
                &format!("https://x.test/big/{i}"),
                200,
                &[("etag".into(), format!("\"e{i}\""))],
                &big,
            );
            let sum: usize = c.map.values().map(|e| e.body.len()).sum();
            assert!(sum <= MAX_TOTAL_BYTES, "after {i}: {sum} resident");
            assert_eq!(c.total_bytes, sum, "the counter tracks the map");
        }
        assert_eq!(c.map.len(), MAX_TOTAL_BYTES / MAX_BODY);
        assert!(
            !c.map.contains_key("https://x.test/big/0"),
            "oldest evicted first"
        );
        assert!(c.map.contains_key("https://x.test/big/15"), "newest kept");
        // A re-store of an existing key swaps its bytes, no double count.
        c.store(
            "https://x.test/big/15",
            200,
            &[("etag".into(), "\"e15b\"".into())],
            b"small",
        );
        let sum: usize = c.map.values().map(|e| e.body.len()).sum();
        assert_eq!(c.total_bytes, sum);
        assert_eq!(c.map.len(), MAX_TOTAL_BYTES / MAX_BODY);
    }

    #[test]
    fn eviction_is_fifo() {
        let mut c = RevalidationCache::new();
        for i in 0..MAX_ENTRIES {
            c.store(
                &format!("https://x.test/{i}"),
                200,
                &[("etag".into(), format!("\"e{i}\""))],
                b"b",
            );
        }
        // Refresh entry 0 to mark it hot, then overflow by one: FIFO
        // evicts the OLDEST INSERT (entry 0), not an arbitrary victim.
        c.store(
            "https://x.test/0",
            200,
            &[("etag".into(), "\"hot\"".into())],
            b"b",
        );
        c.store(
            &format!("https://x.test/{MAX_ENTRIES}"),
            200,
            &[("etag".into(), "\"new\"".into())],
            b"b",
        );
        assert_eq!(c.map.len(), MAX_ENTRIES, "capped");
        assert!(
            !c.map.contains_key("https://x.test/0"),
            "FIFO evicts the oldest entry"
        );
        assert!(
            c.map.contains_key("https://x.test/1"),
            "second-oldest survives"
        );
        assert!(
            c.map.contains_key(&format!("https://x.test/{MAX_ENTRIES}")),
            "newest survives"
        );
    }
}
