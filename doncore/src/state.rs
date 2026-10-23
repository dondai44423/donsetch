//! P3 · the state plane — THE CACHE, the warm jar, per-host learning.
//!
//! Concept: the wire remembers what it fetched and re-checks before trusting
//! it. A page is served from disk only when it is PROVABLY fresh (the server
//! granted max-age/Expires, window capped at 1h) or JUST revalidated (304);
//! anything else goes to the network. No heuristic serving, no stale-on-error
//! — a served body is either provably fresh or freshly verified, so "the site
//! updated today" can never hide behind a cache.
//!
//! Multi-session by construction: state lives on disk, so a new process (or a
//! rebooted machine) sees the same hit. Cross-platform: one root per OS.
//! Crash/corruption tolerance: entries are written temp+rename (meta last =
//! commit point) and sha256-verified on read; anything off is a miss that
//! self-heals. The cache is an accelerator — every failure inside it is
//! invisible to the fetch (DONCORE_DEBUG logs it).
//!
//! Layout: <root>/v1/c/<xx>/<key-hash>.{meta,body}; beside it the jar
//! (`v1/jar.json`) and the learned hosts (`v1/hosts.json`). Env: `DONCORE_CACHE=off`
//! disables; `DONCORE_CACHE_DIR` overrides the root; `DONCORE_CACHE_MAX_MB`
//! (default 256) and `DONCORE_CACHE_TTL_DAYS` (default 30) tune retention.
//! Retention policy: idle > TTL auto-prunes; over the cap the lowest-value
//! entries evict first (value = hits + recency) — what gets used more
//! survives longer.
//!
//! This is the module the swap re-points: donsetch passes its own state dir
//! and this plane becomes the replacement for route memory.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::wire::{Cookie, WireResp};

// ── constants ─────────────────────────────────────────────────────────────

/// The serve-fresh window never exceeds one hour, even when the server granted
/// longer: an agent fetches for CURRENT information — past an hour we re-verify.
const FRESH_CAP_SECS: u64 = 3600;
/// A single stored body is capped (8 MiB — the v4 cache's measured MAX_BODY).
const MAX_BODY: usize = 8 << 20;
const MAX_HEADER_FIELDS: usize = 256;
const MAX_HEADER_BYTES: usize = 64 << 10;
/// Retention defaults (env-tunable): unused this long → pruned; total cap.
const IDLE_DAYS_DEFAULT: u64 = 30;
const MAX_TOTAL_MB_DEFAULT: u64 = 256;
/// Evict down to this percentage of the cap when over it.
const EVICT_TO_PCT: u64 = 80;
/// The marker-gated sweep runs at most once per hour.
const PRUNE_INTERVAL_SECS: u64 = 3600;
/// Never let a prune walk an absurd tree.
const PRUNE_SCAN_CAP: usize = 50_000;

// ── types ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct Meta {
    v: u32,
    url: String,
    final_url: String,
    stored_at: u64,
    initial_age: u64,
    fresh_until: Option<u64>,
    etag: Option<String>,
    lm: Option<String>,
    /// no-cache / ambiguous freshness / Vary: Cookie → never fresh-served
    /// without the network; the entry may still revalidate.
    reval_only: bool,
    hits: u64,
    last_used: u64,
    body_len: u64,
    body_sha: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
}

pub struct FreshHit {
    pub body: String,
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    pub age: u64,
}

pub struct Snap {
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    /// The stored body as it was at check time. A concurrent replacement
    /// must not change what this snapshot's 304 validates (v4 semantics:
    /// the answer describes the exact bytes sent in its precondition).
    body: String,
    meta: Meta,
    meta_path: PathBuf,
    body_path: PathBuf,
}

pub enum Check {
    Fresh(FreshHit),
    Revalidate {
        conds: Vec<(String, String)>,
        snap: Snap,
    },
    Miss,
}

pub struct Cache {
    root: PathBuf, // <root>/v1
    max_total: u64,
    idle_secs: u64,
}

// ── the cache ─────────────────────────────────────────────────────────────

impl Cache {
    /// Opens the state dir from the environment. `None` = cache off
    /// (explicitly disabled, or no usable root) — the engine then behaves
    /// exactly as it did before this module existed.
    pub fn open() -> Option<Cache> {
        if std::env::var("DONCORE_CACHE")
            .map(|v| v.eq_ignore_ascii_case("off"))
            .unwrap_or(false)
        {
            return None;
        }
        let root = state_root()?;
        let max_total = std::env::var("DONCORE_CACHE_MAX_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(MAX_TOTAL_MB_DEFAULT)
            .max(1)
            * (1 << 20);
        let idle_secs = std::env::var("DONCORE_CACHE_TTL_DAYS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(IDLE_DAYS_DEFAULT)
            * 86_400;
        let v1 = root.join("v1");
        if fs::create_dir_all(v1.join("c")).is_err() {
            dbg(&format!("root unusable: {}", v1.display()));
            return None;
        }
        Some(Cache {
            root: v1,
            max_total,
            idle_secs,
        })
    }

    /// Fresh if the server granted freshness and the (capped) window has not
    /// expired; otherwise revalidate when the entry carries validators;
    /// otherwise a miss. A fresh serve counts as use (hits + recency).
    pub fn check(&self, url: &str) -> Check {
        let (mp, bp) = self.entry_paths(url);
        let Some(meta) = read_meta(&mp) else {
            return Check::Miss;
        };
        let body = match fs::read_to_string(&bp) {
            Ok(b) => b,
            Err(_) => {
                self.drop_entry(&mp, &bp);
                return Check::Miss;
            }
        };
        if body.len() as u64 != meta.body_len || sha256_hex(body.as_bytes()) != meta.body_sha {
            dbg(&format!("corrupt entry, dropped: {url}"));
            self.drop_entry(&mp, &bp);
            return Check::Miss;
        }
        let now = now_secs();
        // A stored_at in the future means the clock moved backwards since the
        // write — freshness cannot be trusted; the entry only revalidates.
        let clock_ok = meta.stored_at <= now;
        if !meta.reval_only && clock_ok {
            if let Some(f) = meta.fresh_until {
                if now < f {
                    let mut m = meta.clone();
                    m.hits += 1;
                    m.last_used = now;
                    let _ = write_meta(&mp, &m);
                    return Check::Fresh(FreshHit {
                        body,
                        final_url: meta.final_url.clone(),
                        headers: meta.headers.clone(),
                        age: now.saturating_sub(meta.stored_at),
                    });
                }
            }
        }
        if meta.etag.is_some() || meta.lm.is_some() {
            let mut conds = Vec::new();
            if let Some(t) = &meta.etag {
                conds.push(("if-none-match".into(), t.clone()));
            }
            if let Some(d) = &meta.lm {
                conds.push(("if-modified-since".into(), d.clone()));
            }
            return Check::Revalidate {
                conds,
                snap: Snap {
                    final_url: meta.final_url.clone(),
                    headers: meta.headers.clone(),
                    body,
                    meta,
                    meta_path: mp,
                    body_path: bp,
                },
            };
        }
        self.drop_entry(&mp, &bp);
        Check::Miss
    }

    /// Decides cacheability from the measured rules and writes the entry.
    /// Returns true when an entry was stored. Any non-200 network outcome
    /// evicts a stale entry for this URL (the origin speaks now).
    pub fn store(&self, key: &str, url: &str, resp: &WireResp) -> bool {
        let (mp, bp) = self.entry_paths(key);
        if resp.status != 200 {
            self.drop_entry(&mp, &bp);
            return false;
        }
        if resp.body.is_empty() || resp.body.len() > MAX_BODY {
            self.drop_entry(&mp, &bp);
            return false;
        }
        let headers = stored_headers(&resp.headers);
        if headers.len() > MAX_HEADER_FIELDS
            || headers
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
                > MAX_HEADER_BYTES
        {
            self.drop_entry(&mp, &bp);
            return false;
        }
        // Port of the shipped v4 cache_policy, one deliberate deviation:
        // `private` is cacheable here (this is a single-user private cache;
        // v4's blanket treat-as-no-store guarded shared multi-profile dirs).
        let cc = parse_cache_control(&headers);
        if cc.no_store {
            self.drop_entry(&mp, &bp);
            return false;
        }
        let vk = vary_kind(&headers);
        if vk == VaryKind::Unevaluable {
            self.drop_entry(&mp, &bp);
            return false;
        }
        let etag = unique(&headers, "etag")
            .filter(|t| valid_etag(t))
            .map(str::to_owned);
        let lm = unique(&headers, "last-modified")
            .filter(|d| httpdate::parse_http_date(d).is_ok())
            .map(str::to_owned);
        let now = now_secs();
        let ia = initial_age(&headers, resp.ms, now);
        let lifetime = if let Some(ma) = cc.max_age {
            Some(ma)
        } else {
            expires_lifetime(&headers, now)
        };
        let fresh_until = if cc.validate {
            None
        } else {
            match (lifetime, ia) {
                (Some(l), Some(a)) => Some(now + l.saturating_sub(a).min(FRESH_CAP_SECS)),
                _ => None,
            }
        };
        let useful =
            fresh_until.map(|f| f > now).unwrap_or(false) || etag.is_some() || lm.is_some();
        if !useful {
            self.drop_entry(&mp, &bp);
            return false;
        }
        let meta = Meta {
            v: 1,
            url: url.to_string(),
            final_url: resp.final_url.clone(),
            stored_at: now,
            initial_age: ia.unwrap_or(0),
            fresh_until,
            etag,
            lm,
            reval_only: cc.validate,
            hits: 0,
            last_used: now,
            body_len: resp.body.len() as u64,
            body_sha: sha256_hex(resp.body.as_bytes()),
            headers,
        };
        if write_atomic(&bp, resp.body.as_bytes()).is_err() {
            return false;
        }
        if write_meta(&mp, &meta).is_err() {
            let _ = fs::remove_file(&bp);
            return false;
        }
        dbg(&format!(
            "stored {url} ({} KB, fresh_until={:?})",
            resp.body.len() / 1024,
            meta.fresh_until
        ));
        true
    }

    /// Applies a 304 to the stored snapshot with the strict CAS ported from
    /// the shipped v4 policy: opaque-tag equality ignoring `W/`, weak→strong
    /// upgrade rejected, ambiguous multi-tag rejected, LM-only answers must
    /// match the stored date, a fresh ETag over an LM-only snapshot is
    /// adopted. Returns the body to serve together with the merged headers,
    /// or None when the 304 is rejected (bounded recovery = one
    /// unconditional refetch by the caller). A concurrent replacement stays
    /// authoritative: the merge lands only while the stored entry still is
    /// this snapshot's.
    pub fn commit(
        &self,
        snap: &Snap,
        headers304: &[(String, String)],
        delay_ms: u128,
    ) -> Option<(String, Vec<(String, String)>)> {
        let body = snap.body.clone();
        if body.len() as u64 != snap.meta.body_len {
            self.drop_entry(&snap.meta_path, &snap.body_path);
            return None;
        }
        let h = stored_headers(headers304);
        let tags: Vec<_> = h
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("etag"))
            .collect();
        if let Some(old) = snap.meta.etag.as_deref() {
            if let Some((_, tag)) = tags.first() {
                if tags.len() != 1
                    || !valid_etag(tag)
                    || !valid_etag(old)
                    || opaque_tag(tag) != opaque_tag(old)
                    || (old.starts_with("W/") && !tag.trim().starts_with("W/"))
                {
                    dbg("304 rejected: etag does not identify the stored representation");
                    return None;
                }
            }
        } else if tags.is_empty() {
            let dates: Vec<_> = h
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("last-modified"))
                .collect();
            if let Some((_, d)) = dates.first() {
                let newd = httpdate::parse_http_date(d).ok();
                let oldd = snap
                    .meta
                    .lm
                    .as_deref()
                    .and_then(|o| httpdate::parse_http_date(o).ok());
                if dates.len() != 1 || newd.is_none() || oldd != newd {
                    dbg("304 rejected: last-modified does not identify the stored representation");
                    return None;
                }
            }
        }
        // RFC 9111 §4.3.4 merge: update stored headers from the 304, keep the
        // stored representation describing itself (no content-length/encoding).
        let mut merged = snap.meta.headers.clone();
        merged.retain(|(k, _)| {
            !k.eq_ignore_ascii_case("content-length")
                && !k.eq_ignore_ascii_case("content-encoding")
                && !k.eq_ignore_ascii_case("age")
                && !k.eq_ignore_ascii_case("date")
        });
        for (k, v) in &h {
            if k.eq_ignore_ascii_case("content-length")
                || k.eq_ignore_ascii_case("content-encoding")
            {
                continue;
            }
            merged.retain(|(mk, _)| !mk.eq_ignore_ascii_case(k));
            merged.push((k.clone(), v.clone()));
        }
        let now = now_secs();
        if !merged.iter().any(|(k, _)| k.eq_ignore_ascii_case("date")) {
            merged.push(("date".into(), httpdate::fmt_http_date(to_systime(now))));
        }
        let cc2 = parse_cache_control(&merged);
        if cc2.no_store {
            self.drop_entry(&snap.meta_path, &snap.body_path);
            return Some((body, merged));
        }
        let etag2 = unique(&merged, "etag")
            .filter(|t| valid_etag(t))
            .map(str::to_owned)
            .or_else(|| snap.meta.etag.clone());
        let lm2 = unique(&merged, "last-modified")
            .filter(|d| httpdate::parse_http_date(d).is_ok())
            .map(str::to_owned)
            .or_else(|| snap.meta.lm.clone());
        let ia2 = initial_age(&merged, delay_ms, now);
        let lifetime2 = if let Some(ma) = cc2.max_age {
            Some(ma)
        } else {
            expires_lifetime(&merged, now)
        };
        let fresh_until = if cc2.validate {
            None
        } else {
            match (lifetime2, ia2) {
                (Some(l), Some(a)) => Some(now + l.saturating_sub(a).min(FRESH_CAP_SECS)),
                _ => None,
            }
        };
        let useful =
            fresh_until.map(|f| f > now).unwrap_or(false) || etag2.is_some() || lm2.is_some();
        if !useful {
            self.drop_entry(&snap.meta_path, &snap.body_path);
            return Some((body, merged));
        }
        let m = Meta {
            v: 1,
            url: snap.meta.url.clone(),
            final_url: snap.meta.final_url.clone(),
            stored_at: now,
            initial_age: ia2.unwrap_or(0),
            fresh_until,
            etag: etag2,
            lm: lm2,
            reval_only: cc2.validate,
            hits: snap.meta.hits + 1,
            last_used: now,
            body_len: snap.meta.body_len,
            body_sha: snap.meta.body_sha.clone(),
            headers: merged.clone(),
        };
        // Concurrent replacement stays authoritative (v4 semantics): the
        // merge lands only while the stored entry still is this snapshot's.
        let unchanged = read_meta(&snap.meta_path).is_some_and(|cur| {
            cur.stored_at == snap.meta.stored_at && cur.body_sha == snap.meta.body_sha
        });
        if unchanged {
            let _ = write_meta(&snap.meta_path, &m);
        }
        Some((body, merged))
    }

    /// Evicts any entry for this URL (a revalidation answered with anything
    /// but 304/200 invalidates the stored representation).
    pub fn remove(&self, url: &str) {
        let (mp, bp) = self.entry_paths(url);
        self.drop_entry(&mp, &bp);
    }

    /// Marker-gated sweep: runs prune at most once per hour per cache dir.
    pub fn prune_if_due(&self) {
        let marker = self.root.join(".prune");
        let now = now_secs();
        if let Ok(s) = fs::read_to_string(&marker) {
            if let Ok(t) = s.trim().parse::<u64>() {
                if now.saturating_sub(t) < PRUNE_INTERVAL_SECS {
                    return;
                }
            }
        }
        self.prune();
        let _ = fs::write(&marker, now.to_string());
    }

    /// The sweep: drop idle/corrupt/orphan entries, then evict the
    /// lowest-value entries while over the cap (value = hits + recency).
    pub fn prune(&self) -> String {
        let now = now_secs();
        let cdir = self.root.join("c");
        let mut live: Vec<(PathBuf, Meta, u64)> = Vec::new();
        let mut scanned = 0u64;
        let mut removed = 0u64;
        let mut freed = 0u64;
        if let Ok(shards) = fs::read_dir(&cdir) {
            'outer: for shard in shards.flatten() {
                let sd = shard.path();
                let Ok(files) = fs::read_dir(&sd) else {
                    continue;
                };
                let mut bodies: Vec<(PathBuf, u64)> = Vec::new();
                let mut metas_here: Vec<String> = Vec::new();
                for f in files.flatten() {
                    scanned += 1;
                    if scanned > PRUNE_SCAN_CAP as u64 {
                        break 'outer;
                    }
                    let p = f.path();
                    let name = f.file_name().to_string_lossy().to_string();
                    if name.starts_with(".tmp-") {
                        let old = f
                            .metadata()
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.elapsed().ok())
                            .map(|e| e.as_secs() > 3600)
                            .unwrap_or(false);
                        if old {
                            let _ = fs::remove_file(&p);
                        }
                        continue;
                    }
                    if name.ends_with(".body") {
                        let sz = f.metadata().map(|md| md.len()).unwrap_or(0);
                        bodies.push((p, sz));
                        continue;
                    }
                    if !name.ends_with(".meta") {
                        continue;
                    }
                    let bp = p.with_extension("body");
                    match read_meta(&p) {
                        None => {
                            let _ = fs::remove_file(&p);
                            removed += 1;
                        }
                        Some(m) => {
                            let blen = fs::metadata(&bp).map(|md| md.len()).unwrap_or(u64::MAX);
                            if blen != m.body_len {
                                let _ = fs::remove_file(&p);
                                let _ = fs::remove_file(&bp);
                                removed += 1;
                                freed += m.body_len;
                                continue;
                            }
                            if self.idle_secs > 0
                                && now.saturating_sub(m.last_used) > self.idle_secs
                            {
                                let _ = fs::remove_file(&p);
                                let _ = fs::remove_file(&bp);
                                removed += 1;
                                freed += m.body_len;
                                continue;
                            }
                            metas_here.push(p.with_extension("").to_string_lossy().to_string());
                            live.push((p, m, blen));
                        }
                    }
                }
                // orphan bodies: no meta sibling, old enough that no writer
                // can still be mid-store (body-then-meta commits in ms)
                for (bp, sz) in bodies {
                    let stem = bp.with_extension("").to_string_lossy().to_string();
                    if metas_here.contains(&stem) {
                        continue;
                    }
                    let old = fs::metadata(&bp)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .map(|e| e.as_secs() > 3600)
                        .unwrap_or(false);
                    if old {
                        let _ = fs::remove_file(&bp);
                        removed += 1;
                        freed += sz;
                    }
                }
            }
        }
        let mut total: u64 = live.iter().map(|(_, _, sz)| *sz).sum();
        let mut evicted = 0u64;
        if total > self.max_total {
            let target = self.max_total * EVICT_TO_PCT / 100;
            live.sort_by(|a, b| {
                score(&a.1, now)
                    .partial_cmp(&score(&b.1, now))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for (mp, _, sz) in &live {
                if total <= target {
                    break;
                }
                let bp = mp.with_extension("body");
                let _ = fs::remove_file(mp);
                let _ = fs::remove_file(&bp);
                total = total.saturating_sub(*sz);
                evicted += 1;
            }
        }
        let live_n = live.len() as u64 - evicted;
        format!(
            "prune: scanned {scanned}, removed {removed} idle/corrupt/orphan (~{} KB), evicted {evicted} over-cap, {live_n} live (~{} KB)",
            freed / 1024,
            total / 1024
        )
    }

    /// A human summary of the whole store (scan only, no writes).
    pub fn stats(&self) -> String {
        let cdir = self.root.join("c");
        let mut count = 0u64;
        let mut bytes = 0u64;
        let mut hits = 0u64;
        let mut oldest = u64::MAX;
        let mut newest = 0u64;
        if let Ok(shards) = fs::read_dir(&cdir) {
            for shard in shards.flatten() {
                let Ok(files) = fs::read_dir(shard.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    let name = f.file_name().to_string_lossy().to_string();
                    if !name.ends_with(".meta") {
                        continue;
                    }
                    if let Some(m) = read_meta(&f.path()) {
                        count += 1;
                        bytes += m.body_len;
                        hits += m.hits;
                        oldest = oldest.min(m.last_used);
                        newest = newest.max(m.last_used);
                    }
                }
            }
        }
        if count == 0 {
            return format!("cache: 0 entries (root {})", self.root.display());
        }
        let now = now_secs();
        format!(
            "cache: {} entries, {} KB, {} hits total; oldest use {}s ago, newest {}s ago (root {})",
            count,
            bytes / 1024,
            hits,
            now.saturating_sub(oldest),
            now.saturating_sub(newest),
            self.root.display()
        )
    }

    /// Removes every entry (keeps the directory skeleton). Returns files removed.
    pub fn clear(&self) -> u64 {
        let mut n = 0u64;
        let cdir = self.root.join("c");
        if let Ok(shards) = fs::read_dir(&cdir) {
            for shard in shards.flatten() {
                let Ok(files) = fs::read_dir(shard.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    if fs::remove_file(f.path()).is_ok() {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    fn entry_paths(&self, url: &str) -> (PathBuf, PathBuf) {
        let h = key_hash(url);
        let shard = h.get(..2).unwrap_or("00");
        let dir = self.root.join("c").join(shard);
        (dir.join(format!("{h}.meta")), dir.join(format!("{h}.body")))
    }

    fn drop_entry(&self, mp: &Path, bp: &Path) {
        let _ = fs::remove_file(mp);
        let _ = fs::remove_file(bp);
    }
}

/// The state root shared by every state-plane tenant (the cache, the warm
/// jar, the learned hosts): DONCORE_STATE_DIR, the phase-1
/// DONCORE_CACHE_DIR alias, the product's own DONTSETCH_CACHE_DIR, then the
/// per-OS default below — so the swap re-points the engine at donsetch's
/// cache dir with no code change. Tests isolate with DONCORE_STATE_DIR.
pub fn state_root() -> Option<PathBuf> {
    for var in [
        "DONCORE_STATE_DIR",
        "DONCORE_CACHE_DIR",
        "DONSETCH_CACHE_DIR",
    ] {
        if let Ok(d) = std::env::var(var) {
            if !d.is_empty() {
                return Some(PathBuf::from(d));
            }
        }
    }
    default_root()
}

// ── per-OS roots (compile-checked by construction; mac/win runtime
//    qualification owed exactly like identity.rs) ─────────────────────────

#[cfg(target_os = "linux")]
fn default_root() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("doncore"));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".cache").join("doncore"))
}

#[cfg(target_os = "macos")]
fn default_root() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|h| {
        PathBuf::from(h)
            .join("Library")
            .join("Caches")
            .join("doncore")
    })
}

#[cfg(target_os = "windows")]
fn default_root() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|d| PathBuf::from(d).join("doncore").join("cache"))
}

// ── helpers ───────────────────────────────────────────────────────────────

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The request-context key for a cache entry (v4 semantics): a stored
/// representation belongs to the exact request identity — URL, egress
/// route (credentials included), and the header/cookie context actually
/// sent — never the URL alone and never "jar mode".
pub fn representation_key(
    url: &str,
    proxy: Option<&str>,
    no_cookies: bool,
    extra: &[(String, String)],
    cookie: Option<&str>,
) -> String {
    let mut hash = Sha256::new();
    hash.update([u8::from(no_cookies)]);
    for field in std::iter::once(url)
        .chain(std::iter::once(proxy.unwrap_or("direct")))
        .chain(
            extra
                .iter()
                .flat_map(|(name, value)| [name.as_str(), value.as_str()]),
        )
        .chain(cookie)
    {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn to_systime(s: u64) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_secs(s)
}

fn dbg(msg: &str) {
    if std::env::var("DONCORE_DEBUG").is_ok() {
        eprintln!("[cache] {msg}");
    }
}

fn key_hash(url: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"GET\n");
    h.update(url.as_bytes());
    let d = h.finalize();
    hex(&d)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    hex(&d)
}

fn hex(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(HEX[(x >> 4) as usize] as char);
        s.push(HEX[(x & 0x0f) as usize] as char);
    }
    s
}

fn read_meta(p: &Path) -> Option<Meta> {
    let s = fs::read_to_string(p).ok()?;
    let m: Meta = serde_json::from_str(&s).ok()?;
    (m.v == 1).then_some(m)
}

fn write_meta(p: &Path, m: &Meta) -> std::io::Result<()> {
    let js = serde_json::to_vec(m).map_err(std::io::Error::other)?;
    write_atomic(p, &js)
}

/// Temp+rename in the target's own directory: same filesystem, so the rename
/// is atomic; the meta file is written last = the commit point.
fn write_atomic(final_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = final_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), now_nanos()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, final_path)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn score(m: &Meta, now: u64) -> f64 {
    let age_days = now.saturating_sub(m.last_used) as f64 / 86_400.0;
    (m.hits as f64 + 1.0) / (1.0 + age_days)
}

// ── HTTP-lite rules (ported from the shipped v4 cache's measured lessons) ──

fn hop_header(name: &str) -> bool {
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ]
    .iter()
    .any(|hop| name.eq_ignore_ascii_case(hop))
}

fn stored_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let connection: Vec<String> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, value)| value.split(','))
        .map(|s| s.trim().to_string())
        .collect();
    headers
        .iter()
        .filter(|(name, _)| {
            !hop_header(name)
                && !connection
                    .iter()
                    .any(|field| name.eq_ignore_ascii_case(field))
        })
        .cloned()
        .collect()
}

/// The opaque entity-tag: the quoted string with any weak prefix removed
/// (RFC 9110 §8.8.3).
fn opaque_tag(tag: &str) -> &str {
    tag.trim().strip_prefix("W/").unwrap_or(tag.trim())
}

fn valid_etag(tag: &str) -> bool {
    let tag = opaque_tag(tag).as_bytes();
    tag.len() >= 2
        && tag[0] == b'"'
        && tag[tag.len() - 1] == b'"'
        && tag[1..tag.len() - 1]
            .iter()
            .all(|&byte| byte == 0x21 || (0x23..=0x7e).contains(&byte) || byte >= 0x80)
}

fn unique<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let mut it = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name));
    let v = it.next()?.1.trim();
    it.next().is_none().then_some(v)
}

fn delta_seconds(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(value.bytes().fold(0u64, |total, digit| {
        total
            .saturating_mul(10)
            .saturating_add(u64::from(digit.wrapping_sub(b'0')))
    }))
}

#[derive(Default)]
struct CcPolicy {
    no_store: bool,
    /// no-cache, or ambiguous/invalid freshness — the entry must never be
    /// served fresh; it may only revalidate.
    validate: bool,
    max_age: Option<u64>,
}

/// Cache-Control tokenizer: quoted strings are never directives; multiple or
/// invalid max-age is ambiguous → validate; `private` is deliberately NOT a
/// no-store for this single-user cache.
fn parse_cache_control(headers: &[(String, String)]) -> CcPolicy {
    let mut policy = CcPolicy::default();
    let mut max_age_seen = false;
    for (_, value) in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
    {
        let mut quoted = false;
        let mut escaped = false;
        let mut start = 0;
        for (end, byte) in value
            .bytes()
            .enumerate()
            .chain(std::iter::once((value.len(), b',')))
        {
            if escaped {
                escaped = false;
                continue;
            }
            if quoted && byte == b'\\' {
                escaped = true;
                continue;
            }
            if byte == b'"' {
                quoted = !quoted;
                continue;
            }
            if byte != b',' || quoted {
                continue;
            }
            let directive = value.get(start..end).unwrap_or("").trim();
            start = end + 1;
            let (name, argument) = directive
                .split_once('=')
                .map_or((directive, None), |(n, v)| (n.trim(), Some(v.trim())));
            if name.eq_ignore_ascii_case("no-store") {
                policy.no_store = true;
            } else if name.eq_ignore_ascii_case("no-cache") {
                policy.validate = true;
            } else if name.eq_ignore_ascii_case("max-age") {
                if max_age_seen {
                    policy.validate = true;
                }
                max_age_seen = true;
                policy.max_age = argument.and_then(|v| {
                    let v = if v.starts_with('"') {
                        v.strip_prefix('"')?.strip_suffix('"')?
                    } else {
                        v
                    };
                    delta_seconds(v)
                });
                if policy.max_age.is_none() {
                    policy.validate = true;
                }
            }
        }
        if quoted || escaped {
            policy.validate = true;
            let name = value
                .get(start..)
                .unwrap_or("")
                .split('=')
                .next()
                .unwrap_or_default()
                .trim();
            if name.eq_ignore_ascii_case("no-store") {
                policy.no_store = true;
            }
        }
    }
    policy
}

#[derive(PartialEq, Clone, Copy)]
enum VaryKind {
    Ok,
    Cookie,
    Unevaluable,
}

fn vary_kind(headers: &[(String, String)]) -> VaryKind {
    let mut cookie = false;
    for (_, value) in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("vary"))
    {
        for field in value.split(',') {
            let field = field.trim();
            if field.is_empty() {
                continue;
            }
            if field == "*"
                || field.eq_ignore_ascii_case("if-none-match")
                || field.eq_ignore_ascii_case("if-modified-since")
            {
                return VaryKind::Unevaluable;
            }
            if field.eq_ignore_ascii_case("cookie") {
                cookie = true;
                continue;
            }
            if !field
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            {
                return VaryKind::Unevaluable;
            }
        }
    }
    if cookie {
        VaryKind::Cookie
    } else {
        VaryKind::Ok
    }
}

/// RFC 9111 §4.2.3 age computation, the v4 shape: max(Age + response delay,
/// now - Date). None when the fields exist but are unreadable → no fresh
/// window (a validator may still revalidate).
fn initial_age(headers: &[(String, String)], delay_ms: u128, now: u64) -> Option<u64> {
    let age = match unique(headers, "age") {
        Some(v) => Some(delta_seconds(v)?),
        None => Some(0),
    };
    let date = match unique(headers, "date") {
        Some(v) => Some(to_unix(httpdate::parse_http_date(v).ok()?)),
        None => Some(now),
    };
    match (age, date) {
        (Some(a), Some(d)) => Some((a + (delay_ms / 1000) as u64).max(now.saturating_sub(d))),
        _ => None,
    }
}

fn expires_lifetime(headers: &[(String, String)], now: u64) -> Option<u64> {
    let e = unique(headers, "expires")
        .and_then(|v| httpdate::parse_http_date(v).ok())
        .map(to_unix)?;
    let d = unique(headers, "date")
        .and_then(|v| httpdate::parse_http_date(v).ok())
        .map(to_unix)
        .unwrap_or(now);
    Some(e.saturating_sub(d))
}

// ── the warm store (phase 2 · WARM SESSIONS) ─────────────────────────────

/// The jar's disk home: `<state root>/v1/jar.json`. Corruption, an oversized
/// file, or an unreadable root never fail a fetch — the jar just starts
/// empty (or saves nothing). Atomic temp+rename; 0600 on unix (it holds
/// session credentials).
#[derive(Debug)]
pub struct JarStore {
    path: PathBuf,
    idle_secs: u64,
}

#[derive(Serialize, Deserialize)]
struct DiskCookie {
    name: String,
    value: String,
    domain: String,
    #[serde(default)]
    secure: bool,
    #[serde(default)]
    expires: Option<u64>,
    #[serde(default)]
    created: u64,
    #[serde(default)]
    updated: u64,
}

#[derive(Serialize, Deserialize)]
struct DiskJar {
    v: u32,
    cookies: Vec<DiskCookie>,
}

impl JarStore {
    /// Opens the jar file location from the environment. `None` = persistence
    /// off (DONCORE_JAR=off, or no usable root); the jar stays in-memory.
    pub fn open() -> Option<JarStore> {
        if std::env::var("DONCORE_JAR")
            .map(|v| v.eq_ignore_ascii_case("off"))
            .unwrap_or(false)
        {
            return None;
        }
        let root = state_root()?;
        let v1 = root.join("v1");
        if fs::create_dir_all(&v1).is_err() {
            dbg(&format!("jar root unusable: {}", v1.display()));
            return None;
        }
        let idle_secs = std::env::var("DONCORE_JAR_IDLE_DAYS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30)
            * 86_400;
        Some(JarStore {
            path: v1.join("jar.json"),
            idle_secs,
        })
    }

    /// The stored cookies that are still live: expired ones and session
    /// cookies idle past the retention window are dropped. Returns
    /// (live, dropped-count).
    pub fn read_cookies(&self) -> (Vec<Cookie>, usize) {
        let now = now_secs();
        let raw = self.read_raw();
        let before = raw.len();
        let live: Vec<Cookie> = raw
            .into_iter()
            .filter(|c| live_at(c, now, self.idle_secs))
            .map(|c| Cookie {
                name: c.name,
                value: c.value,
                domain: c.domain,
                secure: c.secure,
                expires: c.expires,
                created: c.created,
                updated: c.updated,
            })
            .collect();
        let dropped = before - live.len();
        (live, dropped)
    }

    fn read_raw(&self) -> Vec<DiskCookie> {
        let Ok(md) = fs::metadata(&self.path) else {
            return Vec::new();
        };
        if md.len() > 2 << 20 {
            dbg("jar: file over 2MiB, ignored");
            return Vec::new();
        }
        let Ok(s) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        match serde_json::from_str::<DiskJar>(&s) {
            Ok(j) if j.v == 1 => j.cookies,
            Ok(_) => Vec::new(),
            Err(_) => {
                dbg("jar: unreadable, starting empty");
                Vec::new()
            }
        }
    }

    /// Merge-and-write: the disk file's live cookies that memory does not
    /// carry (another process's additions) ∪ this process's cookies (memory
    /// wins on identity), minus tombstones, dead entries dropped, capped to
    /// the newest 1000.
    pub fn write_cookies(
        &self,
        cookies: &[Cookie],
        deleted: &[(String, String)],
    ) -> std::io::Result<()> {
        let now = now_secs();
        let mut out: Vec<DiskCookie> = Vec::new();
        for c in self.read_raw() {
            if !live_at(&c, now, self.idle_secs) {
                continue;
            }
            if deleted.iter().any(|(n, d)| c.name == *n && c.domain == *d) {
                continue;
            }
            if cookies
                .iter()
                .any(|m| m.name == c.name && m.domain == c.domain)
            {
                continue;
            }
            out.push(c);
        }
        for c in cookies {
            out.push(DiskCookie {
                name: c.name.clone(),
                value: c.value.clone(),
                domain: c.domain.clone(),
                secure: c.secure,
                expires: c.expires,
                created: c.created,
                updated: c.updated,
            });
        }
        if out.len() > 1000 {
            out.sort_by(|a, b| b.updated.cmp(&a.updated));
            out.truncate(1000);
        }
        let j = DiskJar { v: 1, cookies: out };
        let js = serde_json::to_vec(&j).map_err(std::io::Error::other)?;
        write_atomic(&self.path, &js)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        }
        dbg(&format!("jar: saved {} cookies", j.cookies.len()));
        Ok(())
    }

    /// Human summary; scan only, no writes.
    pub fn stats(&self) -> String {
        let (live, dropped) = self.read_cookies();
        if live.is_empty() {
            return format!(
                "jar: 0 cookies ({} dead on disk) (file {})",
                dropped,
                self.path.display()
            );
        }
        let now = now_secs();
        let expiring = live.iter().filter(|c| c.expires.is_some()).count();
        let oldest = live.iter().map(|c| c.updated).min().unwrap_or(now);
        format!(
            "jar: {} cookies ({} with expiry, {} session, {} dead on disk); oldest update {}s ago (file {})",
            live.len(),
            expiring,
            live.len() - expiring,
            dropped,
            now.saturating_sub(oldest),
            self.path.display()
        )
    }

    /// Removes the file. True when something was removed.
    pub fn clear(&self) -> bool {
        fs::remove_file(&self.path).is_ok()
    }
}

fn live_at(c: &DiskCookie, now: u64, idle_secs: u64) -> bool {
    match c.expires {
        Some(e) => e > now,
        // session cookie: browser-restore semantics — kept until it has been
        // idle past the retention window.
        None => now.saturating_sub(c.updated) <= idle_secs,
    }
}

// ── the learned store (phase 3 · PER-HOST LEARNING) ───────────────────────

/// Per-host evidence that routes the fast path, in ONE file with a v1
/// schema: `<state root>/v1/hosts.json`. It records what the wire learned
/// about a host — outcome history, challenge/clearance observations, and the
/// runtime-need samples the skip consults — so a page shape whose runtime
/// runs never gained content stops paying for the runtime while the evidence
/// stays fresh, and a challenge is never skipped. This is the file the swap
/// points donsetch's route memory at. Corruption, an oversized file or an
/// unreadable root never fail a fetch — the store just learns nothing.
/// Atomic temp+rename; 0600 on unix.
#[derive(Debug)]
pub struct HostStore {
    path: PathBuf,
    sample_secs: u64,
    ttl_secs: u64,
    inner: Mutex<HostMem>,
}

#[derive(Debug, Default)]
struct HostMem {
    hosts: BTreeMap<String, HostRec>,
    dirty: bool,
    last_persist: Option<Instant>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct HostRec {
    #[serde(default)]
    updated: u64,
    #[serde(default)]
    fetches: u64,
    #[serde(default)]
    last_status: u16,
    #[serde(default)]
    last_label: String,
    #[serde(default)]
    challenges: u64,
    #[serde(default)]
    walls: u64,
    #[serde(default)]
    cleared_at: u64,
    #[serde(default)]
    clearance_ttl: Option<u64>,
    #[serde(default)]
    keys: BTreeMap<String, KeyRec>,
}

/// One learned page-shape bucket ("b<len><scripts>") for a host: `runs` =
/// runtime runs that gained no content, `gains` = runs that did (a permanent
/// veto for the shape), `skips` = fast-path skips served, `last` = secs of
/// the last run, `budget` = the largest caller budget (ms) under which the
/// shape produced no gain — the skip engages only when the current budget
/// is ≤ this, so a page is never skipped while being given more time than
/// any sample ever had.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct KeyRec {
    #[serde(default)]
    runs: u64,
    #[serde(default)]
    gains: u64,
    #[serde(default)]
    skips: u64,
    #[serde(default)]
    last: u64,
    #[serde(default)]
    budget: u64,
}

#[derive(Serialize, Deserialize)]
struct DiskHosts {
    v: u32,
    hosts: BTreeMap<String, HostRec>,
}

/// A runtime run on a thin page counts as futile unless it gained at least
/// this many crude-text chars (the no-content-gain class the skip targets).
pub(crate) const RUNTIME_GAIN_MIN: u64 = 200;
/// The skip needs this many futile samples and zero productive ones, all
/// within the sample window, before it engages.
const HOSTS_MIN_RUNS: u64 = 3;
const HOSTS_MAX_FILE: u64 = 2 << 20;
const HOSTS_CAP: usize = 2000;
const HOSTS_KEYS_CAP: usize = 8;
const HOSTS_TTL_DAYS_DEFAULT: u64 = 90;
const HOSTS_SAMPLE_DAYS_DEFAULT: u64 = 3;

impl HostStore {
    /// Opens the learned store from the environment. `None` = learning off
    /// (explicitly disabled, or no usable root) — the engine then behaves
    /// exactly as it did before this store existed.
    pub fn open() -> Option<HostStore> {
        if std::env::var("DONCORE_HOSTS")
            .map(|v| v.eq_ignore_ascii_case("off"))
            .unwrap_or(false)
        {
            return None;
        }
        let root = state_root()?;
        let v1 = root.join("v1");
        if fs::create_dir_all(&v1).is_err() {
            dbg_h(&format!("root unusable: {}", v1.display()));
            return None;
        }
        let sample_secs = std::env::var("DONCORE_HOSTS_SAMPLE_DAYS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(HOSTS_SAMPLE_DAYS_DEFAULT)
            * 86_400;
        let ttl_secs = std::env::var("DONCORE_HOSTS_TTL_DAYS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(HOSTS_TTL_DAYS_DEFAULT)
            * 86_400;
        let path = v1.join("hosts.json");
        let now = now_secs();
        let mut hosts = read_raw_hosts(&path);
        if ttl_secs > 0 {
            hosts.retain(|_, r| now.saturating_sub(r.updated) <= ttl_secs);
        }
        Some(HostStore {
            path,
            sample_secs,
            ttl_secs,
            inner: Mutex::new(HostMem {
                hosts: cap_hosts(hosts),
                dirty: false,
                last_persist: None,
            }),
        })
    }

    /// Some(futile-run count) when the learned skip is active for this page
    /// shape: ≥3 futile runtime runs, zero productive ones, the last sample
    /// inside the freshness window, and the caller's budget no larger than
    /// the largest budget any futile sample ran under. A future timestamp
    /// never activates (the safe direction is to sample again).
    pub fn skip_ok(&self, host: &str, key: &str, budget_ms: u64) -> Option<u64> {
        if self.sample_secs == 0 {
            return None;
        }
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let k = g.hosts.get(host)?.keys.get(key)?;
        let now = now_secs();
        (k.runs >= HOSTS_MIN_RUNS
            && k.gains == 0
            && k.last <= now
            && now.saturating_sub(k.last) <= self.sample_secs
            && budget_ms <= k.budget)
            .then_some(k.runs)
    }

    /// Records one fetch outcome for the host.
    pub fn note_fetch(&self, host: &str, label: &str, status: u16) {
        self.update(host, |r, now| {
            r.fetches += 1;
            r.last_status = status;
            r.last_label = label.to_string();
            if label == "walled" {
                r.walls += 1;
            }
            r.updated = now;
        });
    }

    /// Records that a challenge was served to this host.
    pub fn note_challenge(&self, host: &str) {
        self.update(host, |r, now| {
            r.challenges += 1;
            r.updated = now;
        });
    }

    /// Records one runtime run on a thin page: a gain ≥ RUNTIME_GAIN_MIN is
    /// a productive sample (permanent veto for the shape), anything less is
    /// a futile one — the flowing samples the skip later stands on. Budget-
    /// cut runs are samples too: the gain is the measure, and the largest
    /// futile budget seen is what the skip must respect.
    pub fn note_runtime(&self, host: &str, key: &str, gain: u64, budget_ms: u64) {
        self.update(host, |r, now| {
            let k = r.keys.entry(key.to_string()).or_default();
            if gain >= RUNTIME_GAIN_MIN {
                k.gains += 1;
            } else {
                k.runs += 1;
                k.budget = k.budget.max(budget_ms);
            }
            k.last = now;
            r.updated = now;
            while r.keys.len() > HOSTS_KEYS_CAP {
                let victim = r
                    .keys
                    .iter()
                    .filter(|(n, _)| n.as_str() != key)
                    .min_by_key(|(_, v)| v.last)
                    .map(|(n, _)| n.clone());
                match victim {
                    Some(v) => {
                        r.keys.remove(&v);
                    }
                    None => break,
                }
            }
        });
    }

    /// Records a skip served on a learned shape.
    pub fn note_skipped(&self, host: &str, key: &str) {
        self.update(host, |r, now| {
            r.keys.entry(key.to_string()).or_default().skips += 1;
            r.updated = now;
        });
    }

    /// Records a cleared challenge: when, and the observed lifetime of the
    /// cookies the solve minted (the clearance-TTL observation).
    pub fn note_cleared(&self, host: &str, ttl: Option<u64>) {
        self.update(host, |r, now| {
            r.cleared_at = now;
            if let Some(t) = ttl {
                r.clearance_ttl = Some(t);
            }
            r.updated = now;
        });
    }

    fn update(&self, host: &str, f: impl FnOnce(&mut HostRec, u64)) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let now = now_secs();
        let r = g.hosts.entry(host.to_string()).or_default();
        f(r, now);
        g.dirty = true;
        drop(g);
        self.persist_if_dirty(false);
    }

    /// Writes when dirty; `force` bypasses the debounce (process exit).
    /// Merge-on-write: hosts the disk holds that memory does not carry are
    /// kept (another process's work); memory wins for every host it has.
    /// Dirty clears only on a successful write.
    pub fn persist_if_dirty(&self, force: bool) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if !g.dirty {
            return;
        }
        if !force {
            if let Some(t) = g.last_persist {
                if t.elapsed().as_millis() < 300 {
                    return;
                }
            }
        }
        let now = now_secs();
        let mut out = read_raw_hosts(&self.path);
        for (h, r) in &g.hosts {
            out.insert(h.clone(), r.clone());
        }
        if self.ttl_secs > 0 {
            out.retain(|_, r| now.saturating_sub(r.updated) <= self.ttl_secs);
        }
        let out = cap_hosts(out);
        let j = DiskHosts { v: 1, hosts: out };
        let written = serde_json::to_vec(&j)
            .map_err(std::io::Error::other)
            .and_then(|js| write_atomic(&self.path, &js))
            .is_ok();
        if written {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
            }
            g.dirty = false;
            dbg_h(&format!("saved {} hosts", j.hosts.len()));
        } else {
            dbg_h("hosts: save failed, will retry");
        }
        g.last_persist = Some(Instant::now());
    }

    /// A human summary (scan only, no writes).
    pub fn stats(&self) -> String {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.hosts.is_empty() {
            return format!("hosts: 0 learned hosts (file {})", self.path.display());
        }
        let now = now_secs();
        let mut runs = 0u64;
        let mut gains = 0u64;
        let mut skips = 0u64;
        let mut eligible = 0u64;
        let mut last = 0u64;
        for r in g.hosts.values() {
            last = last.max(r.updated);
            for k in r.keys.values() {
                runs += k.runs;
                gains += k.gains;
                skips += k.skips;
                if self.sample_secs > 0
                    && k.runs >= HOSTS_MIN_RUNS
                    && k.gains == 0
                    && k.last <= now
                    && now.saturating_sub(k.last) <= self.sample_secs
                {
                    eligible += 1;
                }
            }
        }
        format!(
            "hosts: {} hosts, {} futile + {} productive runtime samples, {} skip-eligible shapes, {} skips served; last activity {}s ago (file {})",
            g.hosts.len(),
            runs,
            gains,
            eligible,
            skips,
            now.saturating_sub(last),
            self.path.display()
        )
    }

    /// Removes the file. True when something was removed.
    pub fn clear(&self) -> bool {
        fs::remove_file(&self.path).is_ok()
    }
}

fn read_raw_hosts(path: &Path) -> BTreeMap<String, HostRec> {
    let Ok(md) = fs::metadata(path) else {
        return BTreeMap::new();
    };
    if md.len() > HOSTS_MAX_FILE {
        dbg_h("hosts: file over 2MiB, ignored");
        return BTreeMap::new();
    }
    let Ok(s) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    match serde_json::from_str::<DiskHosts>(&s) {
        Ok(j) if j.v == 1 => j.hosts,
        Ok(_) => BTreeMap::new(),
        Err(_) => {
            dbg_h("hosts: unreadable, starting empty");
            BTreeMap::new()
        }
    }
}

/// Keeps the newest HOSTS_CAP hosts (by last update).
fn cap_hosts(hosts: BTreeMap<String, HostRec>) -> BTreeMap<String, HostRec> {
    if hosts.len() <= HOSTS_CAP {
        return hosts;
    }
    let mut v: Vec<(String, HostRec)> = hosts.into_iter().collect();
    v.sort_by(|a, b| b.1.updated.cmp(&a.1.updated));
    v.truncate(HOSTS_CAP);
    v.into_iter().collect()
}

fn dbg_h(msg: &str) {
    if std::env::var("DONCORE_DEBUG").is_ok() {
        eprintln!("[hosts] {msg}");
    }
}
