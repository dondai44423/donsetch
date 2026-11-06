//! Disk-backed self-improving fetch intelligence.
//!
//! Every fetch is both an action AND an observation. This store
//! learns from each outcome and routes the next fetch more
//! efficiently: domains known to be walled fail fast, warm
//! clearance cookies ride when they can be trusted, and they are
//! kept alive by write-back from successful warm fetches.
//!
//! One persistent store:
//!
//! - **Domain profiles**: per-host routing intelligence + cookie
//!   vault. `route_for(host)` decides the route; `record_*`
//!   methods observe outcomes.
//!
//! File: ~/.cache/donsetch/ghost-state.json (atomic writes).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

// ────────────────────────── types ──────────────────────────

/// A cookie with its server-set expiry. `None` = session cookie
/// (no server-declared expiry; the adaptive-TTL layer fills in).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CookieRecord {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub expires_at: Option<u64>, // unix seconds; None = session
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub same_site: String,
}

/// A recent human-only refusal, scoped to one URL, route and cookie epoch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HumanWall {
    pub url_hash: String,
    pub route: String,
    pub cookie_stamp: String,
    pub observed_at: u64,
}

fn human_url_hash(url: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(url.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Per-domain intelligence. Evolves with every fetch outcome.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DomainProfile {
    #[serde(default)]
    pub human_wall: Option<HumanWall>,
    // === Cookie vault (clearance cookies only) ===
    #[serde(default)]
    pub cookies: Vec<CookieRecord>,
    /// Session vault: login/session cookies harvested from the
    /// browser after every tier-2 run and replayed into the next
    /// browser launch. Distinct from `cookies` (bot-wall clearance
    /// only): this holds the user's own authenticated state so a
    /// login survives daemon restarts, crashes and even the
    /// browser's own kill-without-flush.
    #[serde(default)]
    pub session_cookies: Vec<CookieRecord>,
    /// Explicit login material is never evicted by automatic harvest caps.
    #[serde(default)]
    pub session_explicit: bool,
    /// Last vault fence that deleted or replaced this host's session material
    /// (logout / clear). A render captured at or before this stamp
    /// must not survive a later merge, or an unsaved snapshot would
    /// resurrect the logged-out DOM.
    #[serde(default)]
    pub session_cleared_epoch: u64,
    /// When tier 2 last solved (unix seconds).
    #[serde(default)]
    pub last_solved: u64,
    /// When tier 1 last refreshed cookies via Set-Cookie write-back.
    #[serde(default)]
    pub last_refreshed: u64,

    // === Routing intelligence ===
    #[serde(default)]
    pub fetch_count: u32,
    #[serde(default)]
    pub walled_count: u32,
    #[serde(default)]
    pub warm_ok_count: u32,
    #[serde(default)]
    pub warm_fail_count: u32,

    // === Wall signature ===
    #[serde(default)]
    pub wall_vendor: Option<String>,
    /// Known to be walled (a challenge was seen here).
    #[serde(default)]
    pub needs_tier2: bool,
    /// Last time the host was tried cold (for wall-removal recheck).
    #[serde(default)]
    pub last_cold_check: u64,

    // === Adaptive TTL ===
    /// Shortest observed cookie lifetime (learned from warm-stale).
    /// When cookies die before their stated expiry, the system
    /// learns the real lifetime and re-solves proactively.
    #[serde(default)]
    pub observed_lifetime: Option<u64>,
    /// Consecutive warm fetches that hit a wall. A single warm
    /// failure is often transient (challenge rotation, vendor
    /// hiccup); two in a row is a real stale.
    #[serde(default)]
    pub warm_fail_streak: u32,
    /// Tier-1 replay of ghost-harvested cookies VERIFIED working
    /// (the post-solve tier-1 retry came back ContentOk). Warm
    /// routing is only offered after a verified replay : cookies
    /// that the vendor binds to the browser fingerprint never
    /// earn a doomed tier-1 roundtrip.
    #[serde(default)]
    pub replay_ok: bool,

    // === Solve-failure memory (v3.6: fail fast, honestly) ===
    /// Consecutive ghost passes where the wall persisted even in a
    /// REAL browser (challenge never cleared, content never came).
    /// Two in a row puts the domain into a cooldown: the odds the
    /// same host clears the same wall 20 minutes later are slim,
    /// so fetches fail fast with an honest answer instead of
    /// burning a full browser cycle per attempt.
    #[serde(default)]
    pub wall_fail_streak: u32,
    /// Unix seconds of the most recent wall-persisted ghost pass.
    #[serde(default)]
    pub last_wall_fail: u64,
    /// Network class on which the persistent wall was observed. Separate
    /// from the cookie vault's class: a failed solve must not relabel cookies.
    #[serde(default)]
    pub wall_egress_class: Option<String>,

    // === v4 route memory (phase 0) ===
    /// EWMA of cold (tier-1) success: 1.0 = clean history, 0.0 =
    /// every cold attempt walled. Lets the router act on a FLAKY wall
    /// the boolean flags miss (mixed history, no single verdict).
    #[serde(default = "default_ewma")]
    pub t1_ewma: f32,
    #[serde(default)]
    pub t1_samples: u32,
    /// EWMA of warm-clearance success.
    #[serde(default = "default_ewma")]
    pub warm_ewma: f32,
    #[serde(default)]
    pub warm_samples: u32,
    /// Recent classified failures (newest last, cap
    /// FAILURES_PER_HOST). Failure-class discipline: transport errors
    /// and auth/ratelimit land HERE, never in the wall flags, so a
    /// flaky network can never poison a route decision.
    #[serde(default)]
    pub failures: VecDeque<(u64, FailClass)>,
    /// The scheme this host was actually fetched over ("https" or
    /// "http"). The background prober (v4 phase 0.2) probes the real
    /// origin, not a guessed https upgrade.
    #[serde(default = "default_scheme")]
    pub origin_scheme: String,
    /// The port the origin was actually fetched on (0 = the
    /// scheme's default). Host keys strip ports; without this the
    /// prober would hit :443/:80 on dev servers and internal tools
    /// and learn nothing (live-caught on the wall rig: probes died
    /// on connection-refused against port 80).
    #[serde(default)]
    pub origin_port: u16,
    /// v4 B1: which egress class these cookies were learned on
    /// (`"direct"` or `"proxy"`). Cookies never cross egress
    /// classes: a Warm route is refused when the live lane's class
    /// differs. `None` = pre-B1 / unstamped (treat as match until
    /// the next success stamps the real class).
    #[serde(default)]
    pub egress_class: Option<String>,
}

fn default_scheme() -> String {
    "https".to_string()
}

fn default_ewma() -> f32 {
    1.0
}

fn state_version_v1() -> u32 {
    1
}

/// Current on-disk state format. Bump when a load-time migration
/// becomes historical; migrations gate on `version < N` so they
/// run exactly once per state file.
/// v3 (B1): DomainProfile.egress_class; no destructive migration.
const STATE_VERSION: u32 = 3;

impl DomainProfile {
    /// Most recent signal of any kind; the LRU eviction key.
    fn last_activity(&self) -> u64 {
        // LRU key. A future stamp (clock skew / corrupt file) is not
        // infinite activity: clamp to now so eviction still ages it.
        [
            self.last_solved,
            self.last_refreshed,
            self.last_cold_check,
            self.last_wall_fail,
            self.failures.back().map(|(t, _)| *t).unwrap_or(0),
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
        .min(now())
    }
}

/// Why a fetch attempt failed, at the granularity routing decisions
/// care about. Recorded for visibility and (phase 1) pacing; wall
/// flags only move on challenge verdicts.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum FailClass {
    /// Dial / timeout / reset: network, not the site.
    Network,
    /// TLS handshake / verify (incl. egress interception).
    Tls,
    /// Bot wall / challenge verdict.
    Block,
    /// 401/403 auth or cookie death.
    CookieAuth,
    /// 429 / explicit rate limiting.
    RateLimited,
    /// Anything else (5xx, paywall, dead link...).
    Other,
}

/// How to route a fetch to this host.
#[derive(Debug)]
pub enum RouteDecision {
    /// No profile (first visit) or easy domain. Tier 1 cold.
    Cold,
    /// Known to need tier 2, cookies still fresh : inject them.
    Warm(Vec<CookieRecord>),
    /// Known to need tier 2, cookies stale, cold-check recent.
    /// Skip the doomed tier-1 round-trip : go straight to solve.
    SkipToSolve,
    /// Known to need tier 2, but hasn't been cold-checked in a
    /// while : try tier 1 cold. The wall may have been removed.
    RecheckCold,
    /// The wall PERSISTED through real-browser solves recently
    /// (twice or more). Fail fast with an honest answer; retry in
    /// the carried number of seconds. No browser cycle wasted.
    SolveCooldown(u64),
}

/// Whole-jar bucket cap (v4 phase 1.4): 500 distinct (name,
/// domain, path) triples. Browsers hold far more (Chrome keeps
/// ~180/domain, thousands per store); 500 covers any real agent
/// working set and bounds the state file.
const TIER1_COOKIE_MAX: usize = 500;
/// Refuse to vault pathological cookie values: an oversized value
/// is either junk or an attack surface on state writes.
const TIER1_COOKIE_VALUE_MAX: usize = 4096;

#[derive(Default, Serialize, Deserialize)]
pub struct GhostState {
    /// State format version. 1 = pre-v4 (runs the one-time
    /// migrations in load()), 2 = v4+ (migrations never fire again).
    /// Persisted on every save; old files deserialize as 1.
    #[serde(default = "state_version_v1")]
    pub version: u32,
    /// Lifetime background-probe attempts (v4 phase 0.2), surfaced
    /// in `donsetch status`: the self-improvement loop must be
    /// observable, never magic.
    #[serde(default)]
    pub probes_total: u64,
    /// Lifetime shadow-fetched subresource count (v4 phase 1.3):
    /// page-load realism at work, surfaced in donsetch status.
    #[serde(default)]
    pub shadowed_assets_total: u64,
    /// Lifetime count of fetches served from the search->fetch
    /// prewarm cache (v4 phase 2.1 warm handoff), surfaced in
    /// donsetch status. Counter, not inventory: the RAM cache is
    /// one-shot and capped, so "served" proves the handoff saved a
    /// network fetch.
    #[serde(default)]
    pub prewarmed_served_total: u64,
    /// Tier-1 whole-jar persistence (v4 phase 1.4): the browser
    /// cookie-store view, so a returning agent replays device /
    /// analytics / cf_bm cookies like a returning browser instead
    /// of showing up fresh on every process. Login-worthy cookies
    /// ride their own vault; this bucket holds the plain
    /// navigation set (one store, like a browser: clearance
    /// duplicates at boot merge to the same key harmlessly).
    #[serde(default)]
    pub tier1_cookies: Vec<CookieRecord>,
    /// Per-domain stable identities (v4 phase 0.3): same key space
    /// as profiles, evicted together.
    #[serde(default)]
    pub personas: HashMap<String, crate::persona::Persona>,
    #[serde(default)]
    pub profiles: HashMap<String, DomainProfile>,
    /// Tier-1 echo stashed by sync_tier1_cookies for the save that
    /// follows to re-apply after disk reconciliation: (stamp of the
    /// snapshot, rows). Process-local; never serialized.
    #[serde(skip)]
    pub pending_tier1: Option<(u64, Vec<CookieRecord>)>,
    /// Vault epoch for cross-writer reconciliation: the session
    /// vault has TWO writer classes (the load-modify-save helpers
    /// store_session_cookies / clear_session_cookies_for, which
    /// never touch a daemon's live in-memory snapshot, and the
    /// snapshot's own sync_tier1_cookies). Every vault writer
    /// stamps a strictly larger epoch before saving; save() adopts
    /// the disk side's vault whenever disk carries a newer one, so
    /// a helper's harvest survives the daemon's next save and a
    /// logout's deletion is not resurrected by it.
    #[serde(default)]
    pub vault_epoch: u64,
    /// Explicit login/logout identity. Ordinary cookie refreshes preserve it.
    #[serde(default)]
    pub auth_revision: String,
    /// Atomic cookie change marker, last in canonical JSON for bounded reads.
    #[serde(default)]
    pub session_revision: Option<String>,
}

fn change_session_revision(state: &mut GhostState, consent: bool) {
    state.session_revision = Some(crate::handles::random_base62(24));
    if consent {
        state.auth_revision = crate::handles::random_base62(24);
    }
}

/// Give legacy vaults a stable cookie identity before a daemon freezes requests.
pub(crate) fn ensure_session_identity() {
    if crate::config::cfg().state.no_disk_state
        || !route_memory_enabled()
        || route_memory_readonly()
    {
        return;
    }
    let lock = StateWriteLock::acquire();
    if !lock.locked || validate_login_vault().is_err() {
        return;
    }
    let mut state = GhostState::load();
    if state.session_revision.is_none() {
        change_session_revision(&mut state, false);
        bump_vault_epoch(&mut state);
        if let Err(error) = state.persist() {
            crate::diag!("[state] session identity persist failed: {error}");
        }
    }
}

/// Cookie change stamp read from the same atomic snapshot as its data.
pub(crate) fn disk_session_stamp() -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path()).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::End(-(len.min(192) as i64))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    if let Some(body) = tail.trim_end().strip_suffix('}') {
        if let Some((_, value)) = body.rsplit_once(",\"session_revision\":")
            && let Ok(Some(revision)) = serde_json::from_str::<Option<String>>(value)
        {
            return Some(format!("session:{revision}"));
        }
        if let Some((_, value)) = body.rsplit_once(",\"vault_epoch\":")
            && let Ok(epoch) = value.split(',').next().unwrap_or(value).parse::<u64>()
        {
            return Some(format!("legacy:{epoch}"));
        }
    }
    // Noncanonical/legacy files still work; only our canonical hot path is bounded.
    #[derive(Deserialize)]
    struct Stamp {
        #[serde(default)]
        session_revision: Option<String>,
        #[serde(default)]
        vault_epoch: u64,
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    let stamp: Stamp = serde_json::from_reader(file).ok()?;
    Some(
        stamp
            .session_revision
            .map(|s| format!("session:{s}"))
            .unwrap_or_else(|| format!("legacy:{}", stamp.vault_epoch)),
    )
}

/// Stamp a vault write one past the state it was built from. Every
/// writer re-reads ghost-state.json under the write lock before
/// modifying it, so disk epochs are totally ordered across
/// processes; no process-local counter participates (a local chain
/// can outrun the disk and then mis-order against a sibling
/// process's logout). Saturating: u64::MAX stays u64::MAX.
fn bump_vault_epoch(state: &mut GhostState) {
    state.vault_epoch = state.vault_epoch.saturating_add(1);
}

/// Cross-process serialization for every ghost-state.json writer:
/// the cookie helpers, snapshot saves and any other donsetch process
/// sharing the cache dir. Unix takes a blocking flock, which the
/// kernel releases on crash; the Windows branch holds the file open
/// with no sharing, which makes competing opens wait and is equally
/// released on process death.
struct StateWriteLock {
    locked: bool,
    #[allow(dead_code)] // RAII: held for the lock, never read
    file: Option<std::fs::File>,
}

impl StateWriteLock {
    #[cfg(not(test))]
    fn acquire() -> Self {
        // Nested acquisition (save() reached from load()'s one-time
        // migrations while a helper already holds the lock): the
        // outer guard keeps the lock; re-locking would self-deadlock
        // (flock and the Windows share-mode open are both
        // non-reentrant).
        if STATE_LOCK_DEPTH.with(|d| d.get()) > 0 {
            return Self {
                file: None,
                locked: true,
            };
        }
        let path = crate::paths::cache_dir().join("ghost-state.lock");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let file = match std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) => {
                    // A faulted or displaced cache root must not take
                    // the process down here: locking is best effort
                    // (like the flock failure below), and the save
                    // reports its own persist error.
                    crate::diag!("[state] cookie vault lock failed: {error}");
                    return Self {
                        file: None,
                        locked: false,
                    };
                }
            };
            loop {
                let got = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if got == 0 {
                    STATE_LOCK_DEPTH.with(|d| d.set(d.get() + 1));
                    return Self {
                        file: Some(file),
                        locked: true,
                    };
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    // Best effort, like every other state write:
                    // report and proceed rather than wedge the caller.
                    crate::diag!("[state] cookie vault lock failed: {error}");
                    STATE_LOCK_DEPTH.with(|d| d.set(d.get() + 1));
                    return Self {
                        file: Some(file),
                        locked: false,
                    };
                }
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            loop {
                match std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .share_mode(0)
                    .open(&path)
                {
                    Ok(file) => {
                        STATE_LOCK_DEPTH.with(|d| d.set(d.get() + 1));
                        return Self {
                            file: Some(file),
                            locked: true,
                        };
                    }
                    // ERROR_SHARING_VIOLATION: another donsetch process
                    // holds the lock; wait for it. Anything else (a
                    // faulted or displaced cache root) is best effort:
                    // report and proceed without the lock so the save
                    // can surface its own persist error instead of
                    // spinning here.
                    Err(error) if error.raw_os_error() == Some(32) => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => {
                        crate::diag!("[state] cookie vault lock failed: {error}");
                        return Self {
                            file: None,
                            locked: false,
                        };
                    }
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self {
                file: None,
                locked: false,
            }
        }
    }

    #[cfg(test)]
    fn acquire() -> Self {
        Self {
            file: None,
            locked: true,
        }
    }
}

#[cfg(not(test))]
thread_local! {
    /// Re-entrancy depth for the state write lock (same thread):
    /// save() nested inside a helper's load-modify-save must not
    /// re-lock; see StateWriteLock::acquire.
    static STATE_LOCK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(not(test))]
impl Drop for StateWriteLock {
    fn drop(&mut self) {
        // Only the outer guard (the one that actually locked) counts
        // down; nested guards hold no file.
        if self.file.is_some() {
            STATE_LOCK_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }
}

/// Stamp the deletion of `key`'s session material across every
/// profile it touches (apex + subdomains) plus hosts that only ever
/// appeared as render URLs: a capture recorded at or before `stamp`
/// can never survive a later merge.
fn stamp_session_cleared(state: &mut GhostState, key: &str, extra_hosts: &[String], stamp: u64) {
    for (host, p) in state.profiles.iter_mut() {
        if crate::auth::cookie_belongs_to(key, host) {
            p.session_cleared_epoch = stamp;
        }
    }
    for host in extra_hosts {
        state
            .profiles
            .entry(host.clone())
            .or_default()
            .session_cleared_epoch = stamp;
    }
    state
        .profiles
        .entry(key.to_string())
        .or_default()
        .session_cleared_epoch = stamp;
}

// ────────────────────────── route memory (v4 phase 0) ──────────────────────────

/// Max remembered domains: LRU-evict the coldest beyond this
/// (route memory must never grow unbounded on a 24/7 daemon).
const PROFILE_CAP: usize = 10_000;
/// Max classified failures kept per domain.
const FAILURES_PER_HOST: usize = 16;
/// EWMA smoothing for route outcomes (recent weighs more).
const EWMA_ALPHA: f32 = 0.3;
/// Below this many samples the EWMA is noise; routing ignores it.
const EWMA_MIN_SAMPLES: u32 = 4;
/// Cold-EWMA below this (with enough samples) means a flaky wall:
/// route straight to solve even though no single verdict stuck.
const EWMA_BLOCK_THRESHOLD: f32 = 0.5;

fn ewma_update(cur: f32, samples: u32, ok: bool) -> f32 {
    let outcome = if ok { 1.0 } else { 0.0 };
    if samples == 0 {
        outcome
    } else {
        cur + EWMA_ALPHA * (outcome - cur)
    }
}

/// Route-memory kill switches (v4 phase 0). NO_ROUTE_MEMORY:
/// consult AND record both off (every fetch decides fresh, nothing
/// persists). ROUTE_MEMORY_READONLY: consult on, record off (debug
/// and A/B). Both fail closed through crate::config::env_flag.
fn route_memory_enabled() -> bool {
    crate::config::cfg().state.route_memory != crate::config::RouteMemory::Off
}

fn route_memory_readonly() -> bool {
    crate::config::cfg().state.route_memory == crate::config::RouteMemory::ReadOnly
}

/// Live egress class for `host` (v4 B1): `"proxy"` when a sticky
/// pool lane is a proxy exit, else `"direct"`. No pool / no sticky
/// = direct (the historical default).
pub fn current_egress_class(host: &str) -> &'static str {
    let Some(pool) = crate::search::egress::global() else {
        return "direct";
    };
    if !pool.has_proxies() || !crate::config::cfg().proxy.fetch_rotate {
        return "direct";
    }
    if pool.sticky_is_proxy(host) {
        "proxy"
    } else {
        "direct"
    }
}

// ────────────────────────── constants ──────────────────────────

/// Safety cap: never trust a solve older than this, even if the
/// cookie's stated expiry is further out. Covers server-side
/// invalidation (IP change, session revoke).
const TTL_CAP: u64 = 2 * 60 * 60; // 2 hours

/// If a domain is known to be walled, periodically try it cold
/// anyway : the wall may have been removed.
const RECHECK_INTERVAL: u64 = 24 * 60 * 60; // 24 hours

/// Solve-cooldown backoff: 15 min base, doubling per consecutive
/// wall-persisted failure, hard-capped at 2 hours. A permanently
/// walled domain costs days of sub-second honest errors instead of
/// a full browser cycle per fetch.
fn solve_cooldown_secs(streak: u32) -> u64 {
    let base: u64 = 15 * 60;
    let shifts = streak.saturating_sub(2).min(3);
    base << shifts
}

// ────────────────────────── cookie filtering ──────────────────────────

/// Cookie name prefixes that bot-wall vendors use for clearance/
/// verification cookies. We only persist these : tracking cookies
/// (_ga, TDID, demdex, etc.) bloat the state file with no benefit.
const CLEARANCE_PREFIXES: &[&str] = &[
    "cf_",         // Cloudflare: cf_clearance, cf_chl, cf_ob_setup
    "__cf",        // Cloudflare alt
    "__dd",        // DataDome: __dd_cookie, __dd_s
    "datadome",    // DataDome
    "_dd_s",       // DataDome session
    "bm_",         // Akamai: bm_sz, bm_mi, bm_sv, bm_lso
    "_abck",       // Akamai Bot Manager
    "ak_bmsb",     // Akamai bot sensor backup
    "bmsts",       // Akamai bot session token
    "senseguard",  // PerimeterX
    "_px",         // PerimeterX: _pxhd, _px2, _pxff
    "pxcts",       // PerimeterX session
    "incap_ses",   // Imperva Incapsula
    "visid_incap", // Imperva Incapsula
    "nlbi_",       // Imperva load balancer
    "__utmz",      // Sometimes used by walls for referrer tracking
];

/// Cookie names that are exact matches for clearance cookies.
const CLEARANCE_EXACT: &[&str] = &["cf_clearance"];

/// Is this a bot-wall clearance/verification cookie worth persisting?
/// Filters out tracking/analytics cookies that bloat the state file.
fn is_clearance_cookie(name: &str) -> bool {
    if CLEARANCE_EXACT.contains(&name) {
        return true;
    }
    CLEARANCE_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Filter a cookie list to only clearance cookies.
fn filter_clearance(cookies: &[CookieRecord]) -> Vec<CookieRecord> {
    cookies
        .iter()
        .filter(|c| is_clearance_cookie(&c.name))
        .cloned()
        .collect()
}

// ────────────────────────── session vault ──────────────────────────

/// Cap on vaulted session cookies: keeps ghost-state.json bounded
/// even after months of multi-site browsing.
const SESSION_MAX: usize = 500;

/// Max chars one vaulted cookie value may hold: jumbo tokens are
/// either junk or per-request blobs, never reusable login state.
const SESSION_VALUE_MAX: usize = 8_192;

/// Cookie names that are analytics/ad junk, never login state.
/// Prefix-matched, lowercase.
const SESSION_JUNK_PREFIXES: &[&str] = &[
    "_ga",
    "_gid",
    "_gat",
    "_gcl",
    "_gac",
    "__utma",
    "__utmb",
    "__utmc",
    "__utmz",
    "__utmv",
    "__utmt",
    "__utm",
    "_fbp",
    "_fbc",
    "tdid",
    "demdex",
    "dpm",
    "dextp",
    "adobemc",
    "mbox",
    "s_cc",
    "s_sq",
    "optimizely",
    "pardot",
    "visitor_id",
    "hubspotutk",
    "intercom-id-",
    "intercom-session-",
    "akaalb_",
    "akacd_",
    // Ad-mesh session junk from real harvests (DSP sync, exchange
    // seat ids, media sync timestamps): never login state.
    "_hj",
    "dsp2f_",
    "sspz",
    "sspr_",
    "stx_",
    "cnx_",
    "nm_",
    "st_usi",
    "ruds",
    "_cc_cc",
    "_twsid",
    "wp-wpml",
    "_cf",
    "wrv",
    "seedtag",
];

/// Exact-match junk names (lowercase) too risky to prefix-match.
const SESSION_JUNK_EXACT: &[&str] = &[
    "tz",
    "cpu_bucket",
    "preferred_color_mode",
    "countries_availability_flash_seen",
    "ingresscookie",
];

/// Long-lived cookies named like login state are vaulted even when
/// they carry an expiry (remember-me tokens). Lowercase substrings.
const SESSION_AUTH_HINTS: &[&str] = &[
    "session",
    "sess",
    "sid",
    "auth",
    "token",
    "jwt",
    "bearer",
    "login",
    "connect.sid",
    "phpsessid",
    "laravel_session",
    "cfid",
    "cftoken",
    "cookiesupport",
    "identity",
    "loggedin",
    "logged_in",
    "remember",
    "user_id",
    "userid",
    "guest",
    "member",
    "api_key",
    "apikey",
    "csrf",
    "sid",
];

/// Would replaying this cookie plausibly restore an authenticated
/// session? Session cookies (no expiry) are the login signature;
/// explicit auth-shaped names pass even with an expiry. Everything
/// else (trackers, preferences, A/B buckets) is dropped so the
/// vault stays small and meaningful.
pub fn is_session_worthy(c: &CookieRecord) -> bool {
    if !is_session_usable(c) {
        return false;
    }
    let n = c.name.to_ascii_lowercase();
    if c.expires_at.is_none() {
        return true;
    }
    SESSION_AUTH_HINTS.iter().any(|k| n.contains(k))
}

/// Cookies explicitly selected by a login may have opaque persistent names.
/// Apply value/scope/expiry hygiene without guessing which names authenticate.
pub fn is_session_usable(c: &CookieRecord) -> bool {
    if c.domain.is_empty()
        || c.name.is_empty()
        || c.value.is_empty()
        || c.value.len() > SESSION_VALUE_MAX
        || c.expires_at.is_some_and(|e| e <= now())
        || c.name
            .chars()
            .any(|ch| ch.is_control() || r#" ()<>@,;:\"/[]?={}"#.contains(ch))
        || c.value.chars().any(|ch| ch.is_control() || ch == ';')
        || (!c.path.is_empty() && !c.path.starts_with('/'))
    {
        return false;
    }
    let host = c.domain.trim_start_matches('.').trim_matches(['[', ']']);
    if host != "localhost"
        && host.parse::<std::net::IpAddr>().is_err()
        && (url::Host::parse(host).is_err() || psl::domain(host.as_bytes()).is_none())
    {
        return false;
    }
    let n = c.name.to_ascii_lowercase();
    !is_clearance_cookie(&n)
        && !SESSION_JUNK_PREFIXES.iter().any(|p| n.starts_with(p))
        && !SESSION_JUNK_EXACT.contains(&n.as_str())
}

/// Refuse to replace unreadable existing login material before capture starts.
pub fn validate_login_vault() -> Result<(), String> {
    match std::fs::read(path()) {
        Ok(bytes) => {
            serde_json::from_slice::<GhostState>(&bytes).map_err(|_| {
                "existing login vault is unreadable; preserve it and repair before importing"
                    .to_string()
            })?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("login vault was not saved: {e}")),
    }
    Ok(())
}

/// Save an explicit login without silently truncating or evicting its cookies.
/// Failure leaves the existing vault intact and is reported to the caller.
pub fn store_login_cookies(cookies: &[CookieRecord]) -> Result<(), String> {
    #[cfg(not(test))]
    if crate::config::cfg().state.no_disk_state {
        return Err("login requires disk state; state.no_disk_state is enabled".into());
    }
    let lock = StateWriteLock::acquire();
    if !lock.locked {
        return Err("login vault could not be locked; nothing stored".into());
    }
    validate_login_vault()?;
    let mut state = GhostState::load();
    for c in cookies {
        if !is_session_usable(c) {
            return Err("login contains an unusable cookie; nothing stored".into());
        }
        let p = state
            .profiles
            .entry(c.domain.trim_start_matches('.').to_ascii_lowercase())
            .or_default();
        p.session_explicit = true;
        if let Some(old) = p
            .session_cookies
            .iter_mut()
            .find(|old| old.name == c.name && old.domain == c.domain && old.path == c.path)
        {
            *old = c.clone();
        } else {
            p.session_cookies.push(c.clone());
        }
        if p.session_cookies.len() > 500 {
            return Err("login exceeds the 500-cookie domain safety limit; nothing stored".into());
        }
    }
    let total: usize = state
        .profiles
        .values()
        .map(|p| p.session_cookies.len())
        .sum();
    if total > 5_000 {
        return Err(
            "login vault exceeds the 5000-cookie safety limit; log out unused sites first".into(),
        );
    }
    if state.vault_epoch > u64::MAX - 2 {
        return Err("login vault ordering is exhausted; preserve and repair the state".into());
    }
    bump_vault_epoch(&mut state);
    let barrier = state.vault_epoch;
    let scopes: Vec<(String, bool)> = cookies
        .iter()
        .map(|c| {
            (
                c.domain
                    .trim_start_matches('.')
                    .trim_matches(['[', ']'])
                    .to_ascii_lowercase(),
                c.domain.starts_with('.'),
            )
        })
        .collect();
    let affected = |host: &str| {
        let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
        scopes.iter().any(|(domain, subdomains)| {
            host == *domain || (*subdomains && host.ends_with(&format!(".{domain}")))
        })
    };
    for (host, p) in &mut state.profiles {
        if affected(host) {
            p.session_cleared_epoch = barrier;
            p.wall_fail_streak = 0;
            p.last_wall_fail = 0;
            p.human_wall = None;
        }
    }
    change_session_revision(&mut state, true);
    bump_vault_epoch(&mut state);
    state
        .persist()
        .map_err(|e| format!("login vault was not saved: {e}"))?;
    #[cfg(not(test))]
    // Windows FlushFileBuffers requires a handle opened with write access.
    std::fs::OpenOptions::new()
        .write(true)
        .open(path())
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("login vault sync failed: {e}"))?;
    Ok(())
}

/// Merge one harvest into the session vault: dedupe by
/// (name, domain), per-domain cap for safety.
pub(crate) fn merge_session_cookies(vault: &mut Vec<CookieRecord>, harvested: &[CookieRecord]) {
    const PER_DOMAIN_MAX: usize = 50;
    for c in harvested {
        if !is_session_worthy(c) {
            continue;
        }
        // Same (name, domain): refresh in place (value/expiry
        // evolve as the site rotates its session).
        if let Some(existing) = vault
            .iter_mut()
            .find(|e| e.name == c.name && e.domain == c.domain && e.path == c.path)
        {
            *existing = c.clone();
            continue;
        }
        vault.insert(0, c.clone());
    }
    vault.truncate(PER_DOMAIN_MAX);
}

/// Persist a fresh cookie harvest into the session vault on disk.
/// Load-modify-save with the same atomic tmp+rename write as the
/// rest of the state file: a mid-write crash can never corrupt the
/// vault it updates.
pub fn store_session_cookies(cookies: &[CookieRecord]) {
    if cookies.is_empty() {
        return;
    }
    // Serialize the whole load-modify-save with every other state
    // writer (daemon saves, logouts, sibling processes): without it
    // an interleaved pair lost whichever write landed first.
    let _lock = StateWriteLock::acquire();
    let mut state = GhostState::load();
    let mut changed = false;
    for c in cookies {
        if !is_session_usable(c) {
            continue;
        }
        let p = state
            .profiles
            .entry(c.domain.trim_start_matches('.').to_ascii_lowercase())
            .or_default();
        if p.session_explicit {
            if let Some(old) = p
                .session_cookies
                .iter_mut()
                .find(|old| old.name == c.name && old.domain == c.domain && old.path == c.path)
            {
                changed |= *old != *c;
                *old = c.clone();
            }
        } else {
            changed |= is_session_worthy(c) && !p.session_cookies.iter().any(|old| old == c);
            merge_session_cookies(&mut p.session_cookies, std::slice::from_ref(c));
        }
    }
    // Global cap: if the vault exceeds SESSION_MAX, drop the
    // least-recently-solved domains' session vaults first
    // (deterministic: last_solved is the recency signal).
    let total: usize = state
        .profiles
        .values()
        .filter(|p| !p.session_explicit)
        .map(|p| p.session_cookies.len())
        .sum();
    if total > SESSION_MAX {
        let mut keys: Vec<(u64, String)> = state
            .profiles
            .iter()
            .filter(|(_, p)| !p.session_cookies.is_empty() && !p.session_explicit)
            .map(|(k, p)| (p.last_solved, k.clone()))
            .collect();
        keys.sort_by_key(|(last, _)| *last);
        let mut excess = total - SESSION_MAX;
        for (_, key) in keys {
            if excess == 0 {
                break;
            }
            let p = state.profiles.get_mut(&key).expect("key from iter");
            let drop = p.session_cookies.len().min(excess);
            p.session_cookies.truncate(p.session_cookies.len() - drop);
            excess -= drop;
            changed |= drop > 0;
        }
    }
    if changed {
        change_session_revision(&mut state, false);
    }
    bump_vault_epoch(&mut state);
    if let Err(error) = state.persist() {
        crate::diag!("[state] cookie vault persist failed: {error}");
    }
}

/// Everything currently vaulted, across all domains, for replay
/// into a fresh browser launch.
pub fn load_session_cookies() -> Vec<CookieRecord> {
    let state = GhostState::load();
    let mut out = Vec::new();
    for p in state.profiles.values() {
        for c in &p.session_cookies {
            if is_session_usable(c) {
                out.push(c.clone());
            }
        }
    }
    out
}

/// The cookie set a daemon replays from its persistent stores: the
/// tier-1 jar echo, minus every key the session vault owns, then the
/// vault's own cookies (#319). A login must outrank a jar entry for
/// the same name and domain: the echo is browser truth, but a
/// logged-out render can have written anonymous values over the
/// login cookie, and replaying those keeps every later fetch
/// anonymous until someone clears the jar. Jar-unique cookies
/// (device and analytics ids) still replay; the vault rides last so
/// any collision the filter cannot see still resolves its way
/// inside the jar store itself.
pub fn vault_over_jar(sessions: &[CookieRecord], jar: &[CookieRecord]) -> Vec<CookieRecord> {
    // Compare domains the way the jar normalizes them: a leading
    // dot and letter case must not hide a collision.
    fn domain_key(d: &str) -> String {
        d.trim_start_matches('.').to_ascii_lowercase()
    }
    let owned: std::collections::HashSet<(String, String, String)> = sessions
        .iter()
        .map(|c| (c.name.clone(), domain_key(&c.domain), c.path.clone()))
        .collect();
    let mut out: Vec<CookieRecord> = jar
        .iter()
        .filter(|c| !owned.contains(&(c.name.clone(), domain_key(&c.domain), c.path.clone())))
        .cloned()
        .collect();
    out.extend(sessions.iter().cloned());
    out
}

/// Logout helper: drop every vaulted cookie whose domain belongs
/// to `domain` (apex + subdomains + host-only subdomain cookies
/// that feed the same session). Returns true if anything was
/// removed. Load-modify-save, atomic, same as the harvest path.
pub fn clear_session_cookies_for(domain: &str) -> bool {
    match clear_session_cookies_for_checked(domain) {
        Ok(removed) => removed,
        Err(e) => {
            crate::diag!("[auth] logout failed: {e}");
            false
        }
    }
}

pub fn clear_session_cookies_for_checked(domain: &str) -> Result<bool, String> {
    #[cfg(not(test))]
    if crate::config::cfg().state.no_disk_state {
        return Err("logout requires disk state".into());
    }
    let key = domain.trim_start_matches('.').to_ascii_lowercase();
    if key.is_empty() {
        return Ok(false);
    }
    let lock = StateWriteLock::acquire();
    if !lock.locked {
        return Err("logout vault could not be locked; nothing removed".into());
    }
    validate_login_vault()?;
    let mut state = GhostState::load();
    let mut removed = false;
    // Issue #173: the tier-1 echo of the vault (synced by sync_tier1_cookies)
    // was a third copy of the session that logout missed. Same rule as
    // the profile jar: cookies are removed only when they belong to the
    // domain that is being logged out.
    let before_t1 = state.tier1_cookies.len();
    state.tier1_cookies.retain(|c| {
        let host = c.domain.trim_start_matches('.').to_ascii_lowercase();
        !crate::auth::cookie_belongs_to(&key, &host)
    });
    removed |= state.tier1_cookies.len() != before_t1;
    for p in state.profiles.values_mut() {
        let before = p.session_cookies.len();
        p.session_cookies.retain(|c| {
            let host = c.domain.trim_start_matches('.').to_ascii_lowercase();
            !crate::auth::cookie_belongs_to(&key, &host)
        });
        removed |= p.session_cookies.len() != before;
    }
    if removed {
        // Stamp the deletion so a live daemon snapshot (whose
        // tier1_cookies predate this logout) adopts the disk side
        // on its next save instead of resurrecting them.
        change_session_revision(&mut state, true);
        bump_vault_epoch(&mut state);
        let stamp = state.vault_epoch;
        stamp_session_cleared(&mut state, &key, &[], stamp);
        state
            .persist()
            .map_err(|e| format!("logout vault was not saved: {e}"))?;
    }
    Ok(removed)
}

// ────────────────────────── helpers ──────────────────────────

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path() -> PathBuf {
    crate::paths::cache_dir().join("ghost-state.json")
}

/// Are the stored cookies still fresh at time `now`?
/// Testable: takes `now` as a parameter.
pub fn cookies_fresh_at(profile: &DomainProfile, now: u64) -> bool {
    if profile.cookies.is_empty() {
        return false;
    }
    // Clock rollback or invalid persisted time is not fresh solve evidence.
    let Some(age) = now.checked_sub(profile.last_solved) else {
        return false;
    };

    // Server-set expiry: the earliest-expiring cookie is the
    // weakest link : if it's past, the batch is stale.
    if let Some(exp) = profile.cookies.iter().filter_map(|c| c.expires_at).min()
        && now >= exp
    {
        return false;
    }

    // Observed lifetime: if cookies died before their stated
    // expiry in the past, trust the observation over the server.
    if let Some(observed) = profile.observed_lifetime
        && age >= observed
    {
        return false;
    }

    // Safety cap.
    if age >= TTL_CAP {
        return false;
    }

    true
}

// ────────────────────────── GhostState impl ──────────────────────────

impl GhostState {
    /// The vault revision this snapshot carries (what `disk_session_stamp`
    /// reads from disk).
    #[cfg(test)]
    pub(crate) fn session_stamp(&self) -> String {
        self.session_revision
            .as_ref()
            .map(|s| format!("session:{s}"))
            .unwrap_or_else(|| format!("legacy:{}", self.vault_epoch))
    }

    pub fn load() -> Self {
        let p = path();
        if let Ok(s) = std::fs::read_to_string(&p) {
            // Try new format.
            if let Ok(mut state) = serde_json::from_str::<Self>(&s) {
                // One-time migration: prune tracking cookies from
                // existing profiles. Pre-filter versions stored ALL
                // cookies (_ga, TDID, demdex, etc.) : some domains
                // had 1000+ cookies, making the state file 100MB+.
                // After pruning, only clearance cookies remain.
                let mut changed = false;
                for profile in state.profiles.values_mut() {
                    let before = profile.cookies.len();
                    profile.cookies = filter_clearance(&profile.cookies);
                    if profile.cookies.len() != before {
                        changed = true;
                    }
                    // Corrupt or legacy floats must not steer routing:
                    // a NaN / out-of-range EWMA is not evidence.
                    for ewma in [&mut profile.t1_ewma, &mut profile.warm_ewma] {
                        let value = *ewma;
                        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                            *ewma = default_ewma();
                            changed = true;
                        }
                    }
                }
                // The session vault's own migration: junk from
                // earlier filter versions fades out of old state
                // files on load instead of lingering forever.
                for profile in state.profiles.values_mut() {
                    let before = profile.session_cookies.len();
                    profile.session_cookies.retain(is_session_usable);
                    if profile.session_cookies.len() != before {
                        changed = true;
                    }
                }
                // One-time un-poisoning (v2.2): pre-v2.2 code marked
                // domains walled on ANY non-content verdict :
                // a single 404 or rate-limit forced a walled route on
                // every later fetch. Profiles that carry no wall
                // knowledge (no cookies) are reset so they get a
                // fresh cold chance.
                // Gated on the state version (v4 phase 0): ungated,
                // this migration ran on EVERY load and wiped the
                // memory of legitimately-walled hosts between CLI
                // invocations and daemon restarts (live-caught on
                // the wall rig: needs_tier2 vanished between two CLI
                // runs). Route amnesia, now ended.
                if state.version < 2 {
                    for profile in state.profiles.values_mut() {
                        if profile.needs_tier2 && profile.cookies.is_empty() {
                            profile.needs_tier2 = false;
                            profile.wall_vendor = None;
                            changed = true;
                        }
                        // Pre-v2.2 warm-stale learning clamped lifetimes
                        // to as low as 1s (single-failure, unfloored).
                        // Those observations are garbage : drop them.
                        if profile.observed_lifetime.is_some_and(|o| o < 120) {
                            profile.observed_lifetime = None;
                            changed = true;
                        }
                    }
                }
                // Migrations consumed: stamp the current format so
                // they never fire again for this file.
                state.version = STATE_VERSION;

                if changed {
                    state.save();
                }
                return state;
            }
            // Try legacy format and migrate.
            if let Ok(old) = serde_json::from_str::<LegacyState>(&s) {
                let mut state = Self::default();
                for (host, solved) in old.solved {
                    state.profiles.insert(
                        host,
                        DomainProfile {
                            cookies: filter_clearance(
                                &solved
                                    .cookies
                                    .into_iter()
                                    .map(|(n, v, d)| CookieRecord {
                                        name: n,
                                        value: v,
                                        domain: d,
                                        path: "/".to_string(),
                                        expires_at: None,
                                        secure: false,
                                        http_only: false,
                                        same_site: "Lax".to_string(),
                                    })
                                    .collect::<Vec<_>>(),
                            ),
                            last_solved: solved.at,
                            needs_tier2: true,
                            ..Default::default()
                        },
                    );
                }
                state.save();
                return state;
            }
        }
        Self::default()
    }

    /// The recorded origin for a host: scheme and port, defaults
    /// https:443. The prober builds probe URLs from this.
    pub fn profile_origin(&self, host: &str) -> (String, u16) {
        self.profiles
            .get(host)
            .map(|p| {
                let port = if p.origin_port != 0 {
                    p.origin_port
                } else if p.origin_scheme == "http" {
                    80
                } else {
                    443
                };
                (p.origin_scheme.clone(), port)
            })
            .unwrap_or_else(|| ("https".to_string(), 443))
    }

    /// Remember the scheme a host is fetched over (v4 phase 0.2:
    /// the prober probes the real origin). Idempotent, cheap.
    pub fn note_origin(&mut self, host: &str, scheme: &str, port: u16) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        if scheme != "http" && scheme != "https" {
            return;
        }
        let p = self.profiles.entry(host.to_string()).or_default();
        if p.origin_scheme != scheme || p.origin_port != port {
            p.origin_scheme = scheme.to_string();
            p.origin_port = port;
            self.save();
        }
    }

    /// Stale-walled hosts due for a background probe (v4 phase
    /// 0.2), stalest first. Only hosts with tier-2 memory AND cold
    /// evidence older than `stale_secs` qualify. A host whose front
    /// page the local rules deny never qualifies, and never takes one
    /// of the `limit` slots.
    pub fn probe_candidates(&self, stale_secs: u64, limit: usize) -> Vec<String> {
        self.probe_candidates_under(stale_secs, limit, crate::rules::rules())
    }

    /// [`Self::probe_candidates`] against an explicit ruleset.
    fn probe_candidates_under(
        &self,
        stale_secs: u64,
        limit: usize,
        rules: &crate::rules::RuleSet,
    ) -> Vec<String> {
        let n = now();
        let mut stale: Vec<(&String, u64)> = self
            .profiles
            .iter()
            .filter(|(_, p)| {
                p.needs_tier2
                    && n.checked_sub(p.last_cold_check)
                        .is_none_or(|age| age > stale_secs)
            })
            // The prober cold-fetches the front page; the guard would
            // refuse a denied host on every pass and the host would
            // stay a candidate forever, re-armed as inconclusive each
            // time. The filter runs before `take(limit)` so a denied
            // host cannot starve an allowed one of a probe slot. Only
            // the scheme matters to a rule (a port never does), so the
            // URL is built from the recorded scheme alone.
            .filter(|(h, _)| {
                let scheme = self.profile_origin(h).0;
                let scheme = if scheme == "http" { "http" } else { "https" };
                rules.denial_for_str(&format!("{scheme}://{h}/")).is_none()
            })
            // Invalid future evidence gets a fresh probe before dated history.
            .map(|(h, p)| {
                (
                    h,
                    if p.last_cold_check > n {
                        0
                    } else {
                        p.last_cold_check
                    },
                )
            })
            .collect();
        stale.sort_by_key(|(_, t)| *t);
        stale
            .into_iter()
            .take(limit)
            .map(|(h, _)| h.clone())
            .collect()
    }

    /// A shadow burst fetched N subresources (v4 phase 1.3).
    pub fn note_shadow(&mut self, assets: usize) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        self.shadowed_assets_total = self.shadowed_assets_total.saturating_add(assets as u64);
        self.save();
    }

    /// v4 phase 2.1 visibility: count a fetch served from the
    /// search prewarm cache. DONSETCH_NO_PREWARM stops entries
    /// being parked in the first place, so this only grows while
    /// prewarm is on; no extra gate here.
    pub fn note_prewarm_served(&mut self) {
        self.prewarmed_served_total = self.prewarmed_served_total.saturating_add(1);
        self.save();
    }

    /// One background probe was attempted (any outcome).
    pub fn note_probe(&mut self) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        self.probes_total = self.probes_total.saturating_add(1);
    }

    /// A probe that says nothing about the wall (transport error,
    /// non-wall verdict on the front page). Re-arms the probe
    /// cadence so the host is not hammered, WITHOUT touching wall
    /// flags, EWMAs, or fetch counters: inconclusive is not
    /// evidence in either direction.
    pub fn record_probe_inconclusive(&mut self, host: &str) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.last_cold_check = n;
        self.save();
    }

    /// The persona for a host, minting or re-minting as needed
    /// (v4 phase 0.3). Longitudinal rule: an existing coherent
    /// persona is returned unchanged for weeks; a burned
    /// (quarantined) or drifted one is replaced with generation+1,
    /// never resurrected.
    pub fn ensure_persona(
        &mut self,
        host: &str,
        caps: &crate::persona::PersonaCaps,
    ) -> Option<&crate::persona::Persona> {
        if !route_memory_enabled() {
            return self.personas.get(host);
        }
        if route_memory_readonly() {
            return self.personas.get(host);
        }
        let n = now();
        let needs_mint = match self.personas.get(host) {
            None => true,
            Some(p) => !p.coherent(caps),
        };
        if needs_mint {
            let generation = self
                .personas
                .get(host)
                .map(|p| p.generation.saturating_add(1))
                .unwrap_or(1);
            self.personas.insert(
                host.to_string(),
                crate::persona::Persona::mint(host, caps, generation, n),
            );
            self.save();
        }
        self.personas.get(host)
    }

    /// Burn a persona (detection event, operator request). The next
    /// ensure_persona mints a fresh identity with a bumped
    /// generation; the burned one is never reused. Also frees any
    /// exclusive egress bind so the lane can serve a new mint.
    pub fn quarantine_persona(&mut self, host: &str, reason: &str) {
        if let Some(pool) = crate::search::egress::global() {
            pool.release_persona_lane(host);
        }
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        if let Some(p) = self.personas.get_mut(host) {
            p.quarantined_at = Some(now());
            p.quarantine_reason = Some(reason.to_string());
            self.save();
        }
    }

    /// Stamp the persona's bound exit (v4 A2). Returns true when
    /// the stamp changed. A burned or foreign-persona lane is never
    /// assigned: the pool's pick_persona_lane already excludes them.
    pub fn bind_persona_proxy(&mut self, host: &str, lane_id: Option<String>) -> bool {
        if !route_memory_enabled() || route_memory_readonly() {
            return false;
        }
        let Some(p) = self.personas.get_mut(host) else {
            return false;
        };
        if p.proxy_id == lane_id {
            return false;
        }
        p.proxy_id = lane_id;
        self.save();
        true
    }

    /// Ensure this host's persona is bound to a live exclusive
    /// egress lane (pool present). Remints via quarantine when the
    /// bound lane is burned or foreign. No-op when the pool is
    /// empty or route memory is off.
    pub fn ensure_persona_egress(&mut self, host: &str) {
        let Some(pool) = crate::search::egress::global() else {
            return;
        };
        if !pool.has_proxies() || !crate::config::cfg().proxy.fetch_rotate {
            return;
        }
        let bound = self.personas.get(host).and_then(|p| p.proxy_id.clone());
        if let Some(id) = bound {
            let ok = pool.persona_lane_ok(host, &id) && !pool.is_dead(&id);
            if ok {
                return;
            }
            // Foreign or burned: burn the identity, free the lane,
            // and let the next ensure mint generation+1.
            pool.release_persona_lane(host);
            self.quarantine_persona(host, "bound egress burned or foreign");
        }
        if let Some(eg) = pool.pick_persona_lane(host) {
            pool.bind_persona_lane(host, &eg.id);
            self.bind_persona_proxy(host, Some(eg.id));
        }
    }

    /// Aggregate route-memory stats for `donsetch status`
    /// (v4 phase 0: self-improvement must be observable).
    pub fn route_stats(&self) -> (usize, usize, usize, usize, usize) {
        let n = now();
        let hosts = self.profiles.len();
        let walled = self.profiles.values().filter(|p| p.needs_tier2).count();
        let warm = self
            .profiles
            .values()
            .filter(|p| cookies_fresh_at(p, n) && p.replay_ok)
            .count();
        let cooldowns = self
            .profiles
            .values()
            .filter(|p| {
                p.wall_fail_streak >= 2
                    && p.last_wall_fail > 0
                    && p.last_wall_fail <= n
                    && n.saturating_sub(p.last_wall_fail) < solve_cooldown_secs(p.wall_fail_streak)
            })
            .count();
        let flaky = self
            .profiles
            .values()
            .filter(|p| {
                !p.needs_tier2
                    && p.t1_samples >= EWMA_MIN_SAMPLES
                    && p.t1_ewma < EWMA_BLOCK_THRESHOLD
            })
            .count();
        (hosts, walled, warm, cooldowns, flaky)
    }

    /// Tier-1 jar flush (v4 phase 1.4): merge the whole jar into
    /// the persisted snapshot. Browser-true mechanics: dedupe
    /// (name, scope-folded domain, path), refresh in place at the front (cap
    /// evicts the coldest tail), drop expired and pathological
    /// values. Does NOT save: the caller is already inside the
    /// state lock and the outcome record after it saves once.
    pub fn sync_tier1_cookies(&mut self, all: &[CookieRecord]) {
        if !crate::config::cfg().state.cookie_vault
            || crate::config::cfg().state.route_memory == crate::config::RouteMemory::Off
        {
            return;
        }
        // Stash the snapshot for the save that follows: persist()
        // reconciles with disk first and re-applies the echo after,
        // so a helper harvest or logout landing in between is adopted
        // (never discarded by this writer, never discarded itself),
        // and a row a deletion removed cannot be re-raised.
        self.pending_tier1 = Some((self.vault_epoch, all.to_vec()));
        self.merge_tier1_jar(all);
    }

    /// The jar-merge mechanics for sync_tier1_cookies: dedupe by
    /// (name, scope-folded domain, path), refresh in place at the front, drop
    /// expired and pathological values, cap the tail.
    fn merge_tier1_jar(&mut self, all: &[CookieRecord]) {
        let now = now();
        for c in all {
            if c.domain.is_empty()
                || c.value.is_empty()
                || c.value.len() > TIER1_COOKIE_VALUE_MAX
                || c.expires_at.is_some_and(|e| e <= now)
            {
                continue;
            }
            // Scope-folded key: ".x.com" and "x.com" are one
            // cookie to the jar (`store_raw` normalizes both to
            // x.com), so a row re-synced under the scoped form must
            // replace its bare twin, not sit beside it and let the
            // older host-only flag win the next replay.
            let fold = |d: &str| d.trim_start_matches('.').to_ascii_lowercase();
            let key = (c.name.clone(), fold(&c.domain), c.path.clone());
            if let Some(pos) = self
                .tier1_cookies
                .iter()
                .position(|x| (x.name.clone(), fold(&x.domain), x.path.clone()) == key)
            {
                self.tier1_cookies.remove(pos);
            }
            self.tier1_cookies.insert(0, c.clone());
        }
        self.tier1_cookies.truncate(TIER1_COOKIE_MAX);
    }

    /// Cookie count for `donsetch status`: the vault must be
    /// observable, never magic.
    pub fn tier1_cookie_count(&self) -> usize {
        crate::config::cfg().state.cookie_vault as usize * self.tier1_cookies.len()
    }

    /// Write a complete snapshot to a private temporary file, then rename.
    /// Persistence errors are reported on stderr; in-memory learning survives.
    /// Unit-test builds and `state.no_disk_state` skip disk writes.
    pub fn save(&mut self) {
        // Serialize with every other ghost-state writer: the cookie
        // helpers and other processes write the same file, and a
        // nested load-modify-save interleave used to lose whichever
        // write landed first.
        let _lock = StateWriteLock::acquire();
        if let Err(error) = self.persist() {
            crate::diag!("[state] cookie vault persist failed: {error}");
        }
    }

    /// The write itself. Callers already holding the state write
    /// lock (the cookie helpers) must call this, never `save()`: the
    /// lock is not re-entrant.
    fn persist(&mut self) -> std::io::Result<()> {
        // Always persist the current format version: a fresh state
        // derives Default (version 0) and would otherwise keep
        // re-running one-time migrations on every load.
        self.version = STATE_VERSION;
        // LRU bound (v4 phase 0): route memory must never grow
        // unbounded on a 24/7 daemon. Evict the coldest domains
        // (oldest last-activity of any signal) past PROFILE_CAP.
        if self.profiles.len() > PROFILE_CAP {
            let mut by_activity: Vec<(String, u64)> = self
                .profiles
                .iter()
                .filter(|(_, p)| p.session_cookies.is_empty())
                .map(|(h, p)| (h.clone(), p.last_activity()))
                .collect();
            by_activity.sort_by_key(|(_, a)| *a);
            let evict = self.profiles.len() - PROFILE_CAP;
            for (host, _) in by_activity.into_iter().take(evict) {
                self.profiles.remove(&host);
                self.personas.remove(&host);
            }
        }
        #[cfg(not(test))]
        {
            // Allow users to disable disk persistence entirely.
            // In-memory state still works during the session.
            if crate::config::cfg().state.no_disk_state {
                return Ok(());
            }
            let p = path();
            let persist = (|| -> std::io::Result<()> {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Merge monotonic counters against the disk state as it
                // exists RIGHT NOW: independent load() snapshots race
                // (a stale in-memory copy saving later would otherwise
                // rewind lifetimes). Max-merge is correct for counters:
                // they only grow. Real case: the prewarm receipt.
                let disk = std::fs::read(&p)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<GhostState>(&bytes).ok());
                if let Some(disk) = &disk {
                    self.probes_total = self.probes_total.max(disk.probes_total);
                    self.shadowed_assets_total =
                        self.shadowed_assets_total.max(disk.shadowed_assets_total);
                    self.prewarmed_served_total =
                        self.prewarmed_served_total.max(disk.prewarmed_served_total);
                    self.merge_vault_from_disk(disk);
                }
                // A stashed tier-1 echo re-applies AFTER the merge:
                // adoption cannot drop it, and a row a session clear
                // that postdates the snapshot removed stays removed.
                if let Some((snapshot_epoch, rows)) = self.pending_tier1.take() {
                    let keep: Vec<CookieRecord> = rows
                        .into_iter()
                        .filter(|c| {
                            let host = c.domain.trim_start_matches('.').to_ascii_lowercase();
                            !disk.as_ref().is_some_and(|d| {
                                d.profiles
                                    .get(&host)
                                    .is_some_and(|p| p.session_cleared_epoch > snapshot_epoch)
                            })
                        })
                        .collect();
                    self.merge_tier1_jar(&keep);
                    bump_vault_epoch(self);
                }
                let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
                // Unique per process: a mixed-version writer that does
                // not share this advisory lock can never clobber this
                // write's temporary file.
                let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
                // 0600 BEFORE content lands on disk: the state file
                // carries harvested cookies (clearance / session
                // identifiers) and must not be world-readable, even
                // transiently on the tmp file.
                #[cfg(unix)]
                {
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&tmp)?;
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                    file.write_all(&bytes)?;
                }
                #[cfg(not(unix))]
                {
                    std::fs::write(&tmp, &bytes)?;
                }
                std::fs::rename(&tmp, &p)
            })();
            persist?;
        }
        Ok(())
    }

    /// Adopt the disk state's session vault into this snapshot.
    /// Pure reconciliation over one loaded disk copy; called from
    /// save() with the file as it exists RIGHT NOW.
    ///
    /// The vault (profiles[].session_cookies, tier1_cookies) has
    /// TWO writer classes: the load-modify-save
    /// helpers (store_session_cookies / clear_session_cookies_for),
    /// which never touch a live daemon snapshot, and the snapshot
    /// itself (sync_tier1_cookies). Both stamp a strictly larger
    /// vault epoch before saving, so the winner is the newer stamp.
    /// A TIE means no helper wrote since this snapshot loaded, so
    /// its copy is exactly what disk holds (load() read the same
    /// file) and keeping it is correct. Without this reconciliation
    /// a daemon save rewrote the file from its boot-era snapshot:
    /// every harvest since was erased, and every logout deletion
    /// came back (dead sessions replayed on the next launch).
    pub fn merge_vault_from_disk(&mut self, disk: &GhostState) {
        if disk.vault_epoch <= self.vault_epoch {
            return;
        }
        for (host, dp) in disk.profiles.iter() {
            match self.profiles.get_mut(host) {
                // Both sides know the host: disk's session vault wins,
                // and its deletion stamp rides along so a later merge
                // orders captures against it.
                Some(p) => {
                    p.session_cookies = dp.session_cookies.clone();
                    p.session_explicit = dp.session_explicit;
                    p.session_cleared_epoch = dp.session_cleared_epoch;
                }
                // Disk-only host: a helper harvested a host this
                // snapshot has never seen. Adopt the whole profile
                // (its wall-state rode along on disk; dropping it
                // here would strand the harvest).
                None => {
                    self.profiles.insert(host.clone(), dp.clone());
                }
            }
        }
        self.tier1_cookies = disk.tier1_cookies.clone();
        self.auth_revision = disk.auth_revision.clone();
        self.session_revision = disk.session_revision.clone();
        self.vault_epoch = disk.vault_epoch;
    }

    // ── Decision ──

    /// Route decision for the next fetch to this host.
    /// Is this host known to be walled (a challenge was seen here)?
    pub fn is_known_walled(&self, host: &str) -> bool {
        self.profiles.get(host).is_some_and(|p| p.needs_tier2)
    }

    /// Cached refusal age. Future/stale records, new cookies and new routes
    /// never suppress a fresh observation. Explicit tiers bypass this lookup.
    pub fn human_wall_age(&self, host: &str, url: &str, route: &str) -> Option<u64> {
        if !route_memory_enabled() {
            return None;
        }
        let wall = self.profiles.get(host)?.human_wall.as_ref()?;
        let age = now().checked_sub(wall.observed_at)?;
        (age < 300
            && wall.cookie_stamp == {
                #[cfg(test)]
                {
                    self.session_stamp()
                }
                #[cfg(not(test))]
                {
                    disk_session_stamp()?
                }
            }
            && wall.route == route
            && wall.url_hash == human_url_hash(url))
        .then_some(age)
    }

    /// Remember one observed human-only URL without penalizing other pages.
    pub fn record_human_wall(&mut self, host: &str, url: &str, route: &str, cookie_stamp: &str) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let wall = HumanWall {
            url_hash: human_url_hash(url),
            route: route.into(),
            cookie_stamp: cookie_stamp.into(),
            observed_at: now(),
        };
        self.profiles.entry(host.into()).or_default().human_wall = Some(wall);
        self.save();
    }

    pub fn route_for(&self, host: &str) -> RouteDecision {
        let class = current_egress_class(host);
        self.route_for_class(host, class)
    }

    /// Route with an explicit egress class (v4 B1). A Warm cookie
    /// vault learned on another class is never injected: clearance
    /// bound to a home IP must not ride a proxy exit (and vice
    /// versa). `None`/unstamped profiles stay usable until the next
    /// success stamps the real class.
    pub fn route_for_class(&self, host: &str, class: &str) -> RouteDecision {
        if !route_memory_enabled() {
            return RouteDecision::Cold;
        }
        let Some(profile) = self.profiles.get(host) else {
            return RouteDecision::Cold;
        };
        let n = now();
        // Solve-cooldown comes FIRST: a domain that kept walling
        // cold recently fails fast until the cooldown lapses.
        // Exponential backoff per failure streak, capped at 2 hours,
        // so a permanently-walled domain costs an honest sub-second
        // error instead of another doomed attempt per fetch.
        let wall_class = profile
            .wall_egress_class
            .as_ref()
            .or(profile.egress_class.as_ref());
        if profile.wall_fail_streak >= 2 && wall_class.is_none_or(|c| c == class) {
            let cooldown = solve_cooldown_secs(profile.wall_fail_streak);
            if profile.last_wall_fail > 0
                && profile.last_wall_fail <= n
                && n - profile.last_wall_fail < cooldown
            {
                return RouteDecision::SolveCooldown(cooldown - (n - profile.last_wall_fail));
            }
        }
        // Egress-class gate (B1): cookies never cross classes.
        let class_ok = match &profile.egress_class {
            None => true,
            Some(c) => c == class,
        };
        if profile.needs_tier2 {
            // Warm only when cookies are fresh AND cookie replay has
            // actually been verified to work for this domain. Vendors
            // that bind clearance to the browser fingerprint reject
            // cookie replay forever : serving Warm there just burns a
            // doomed roundtrip before every solve.
            if class_ok && cookies_fresh_at(profile, n) && profile.replay_ok {
                return RouteDecision::Warm(profile.cookies.clone());
            }
            // Cross-class or stale cookies. Should we recheck cold?
            if n.checked_sub(profile.last_cold_check)
                .is_none_or(|age| age > RECHECK_INTERVAL)
            {
                return RouteDecision::RecheckCold;
            }
            // Skip the doomed tier-1 attempt : go straight to solve.
            return RouteDecision::SkipToSolve;
        }
        // Flaky-wall detection (v4 phase 0): the boolean flags only
        // move on single verdicts, so a host whose wall appears
        // intermittently never gets needs_tier2 set and burns a
        // doomed tier-1 roundtrip plus an escalation on every fetch.
        // With enough cold samples, a failing EWMA routes straight
        // to solve instead.
        if profile.t1_samples >= EWMA_MIN_SAMPLES && profile.t1_ewma < EWMA_BLOCK_THRESHOLD {
            return RouteDecision::SkipToSolve;
        }
        // Easy domain : tier 1 cold.
        RouteDecision::Cold
    }

    // ── Observation ──

    /// A fetch completed with a non-challenge, non-content verdict
    /// (404, rate-limit, paywall, auth wall). These say nothing
    /// about walls or cookies : they must not poison the route.
    /// The counters move and the cold-check clock restarts (this
    /// WAS a tier-1 answer; the 24h recheck cadence follows it).
    pub fn record_fetch(&mut self, host: &str) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.fetch_count = p.fetch_count.saturating_add(1);
        p.last_cold_check = n;
        self.save();
    }

    /// Record a classified failure (v4 phase 0). Failure-class
    /// discipline: this updates ONLY the failure histogram and the
    /// counters; wall flags and EWMAs move exclusively on challenge
    /// verdicts, so a flaky network or a dead link can never poison
    /// a route decision.
    pub fn record_failure(&mut self, host: &str, class: FailClass) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.failures.push_back((n, class));
        while p.failures.len() > FAILURES_PER_HOST {
            p.failures.pop_front();
        }
        self.save();
    }

    /// Recent transport failures are diagnostic evidence, never a wall route.
    pub fn recent_network_failures(&self, host: &str) -> usize {
        let n = now();
        self.profiles.get(host).map_or(0, |p| {
            p.failures
                .iter()
                .filter(|(at, class)| {
                    *class == FailClass::Network && n.checked_sub(*at).is_some_and(|age| age < 600)
                })
                .count()
        })
    }

    /// The same intermittent wall signal counted by the status dashboard.
    pub fn is_flaky(&self, host: &str) -> bool {
        self.profiles.get(host).is_some_and(|p| {
            !p.needs_tier2 && p.t1_samples >= EWMA_MIN_SAMPLES && p.t1_ewma < EWMA_BLOCK_THRESHOLD
        })
    }

    /// A cold fetch succeeded. If the domain was previously known
    /// to be walled, the wall is gone : clear the flag.
    pub fn record_cold_ok(&mut self, host: &str) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.t1_ewma = ewma_update(p.t1_ewma, p.t1_samples, true);
        p.t1_samples = p.t1_samples.saturating_add(1);
        p.fetch_count = p.fetch_count.saturating_add(1);
        p.last_cold_check = n;
        p.warm_fail_streak = 0;
        // The cold fetch got real content : whatever wall state
        // existed is irrelevant now. Clear the wall + cooldown memory.
        p.wall_fail_streak = 0;
        p.last_wall_fail = 0;
        if p.needs_tier2 {
            p.needs_tier2 = false;
            p.wall_vendor = None;
            // Stale clearance cookies : clear them.
            p.cookies.clear();
            p.observed_lifetime = None;
            p.replay_ok = false;
        }
        p.egress_class = Some(current_egress_class(host).to_string());
        self.save();
    }

    /// A cold fetch was walled : the domain is defended.
    /// (Callers: ONLY on an actual Challenge verdict. A 404 or a
    /// rate-limit is not a wall : it must not burn route memory.)
    pub fn record_cold_walled(&mut self, host: &str, vendor: Option<&str>) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.t1_ewma = ewma_update(p.t1_ewma, p.t1_samples, false);
        p.t1_samples = p.t1_samples.saturating_add(1);
        p.fetch_count = p.fetch_count.saturating_add(1);
        p.walled_count = p.walled_count.saturating_add(1);
        p.needs_tier2 = true;
        p.last_cold_check = n;
        if let Some(v) = vendor {
            p.wall_vendor = Some(v.to_string());
        }
        // Consecutive cold walls arm the cooldown: the wall
        // persisted across attempts, so later fetches fail fast
        // until it is due for another honest try.
        let class = current_egress_class(host).to_string();
        if p.wall_egress_class.as_ref().is_some_and(|c| c != &class) {
            p.wall_fail_streak = 0;
        }
        p.wall_egress_class = Some(class);
        p.wall_fail_streak = p.wall_fail_streak.saturating_add(1);
        p.last_wall_fail = n;
        self.save();
    }

    /// Tier 1 warm succeeded : cookies are still valid. Refresh
    /// the cookie vault from the response's Set-Cookie headers
    /// so the on-disk cookies stay as fresh as the server's latest
    /// response. Only clearance cookies are merged : tracking
    /// cookies are filtered out to keep the state file compact.
    pub fn record_warm_ok(&mut self, host: &str, refreshed: &[CookieRecord]) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.warm_ewma = ewma_update(p.warm_ewma, p.warm_samples, true);
        p.warm_samples = p.warm_samples.saturating_add(1);
        p.fetch_count = p.fetch_count.saturating_add(1);
        p.warm_ok_count = p.warm_ok_count.saturating_add(1);
        p.warm_fail_streak = 0;
        // A verified warm replay proves the wall is passable:
        // clear the cooldown memory.
        p.wall_fail_streak = 0;
        p.last_wall_fail = 0;
        p.last_refreshed = n;
        // Merge: replace by (name, domain), add new ones : but only
        // clearance cookies.
        for new in filter_clearance(refreshed) {
            if let Some(existing) = p
                .cookies
                .iter_mut()
                .find(|c| c.name == new.name && c.domain == new.domain)
            {
                *existing = new.clone();
            } else {
                p.cookies.push(new.clone());
            }
        }
        p.egress_class = Some(current_egress_class(host).to_string());
        self.save();
    }

    /// A warm fetch was walled : cookies went stale. Learn the
    /// real lifetime: it's at most `now - last_solved`. Next
    /// time, trust the observation over the server's claim and
    /// stop serving the dead cookies.
    ///
    /// Dampened: the FIRST warm failure keeps the cookies (vendor
    /// challenges rotate; one wall is often transient : the next
    /// warm fetch gets to prove the cookies still live). Only a
    /// SECOND consecutive failure clears the vault and learns the
    /// lifetime. The learned lifetime is floored at 120s: a fluke
    /// wall one second after a fresh clearance must never clamp the
    /// domain to a permanent skip (the stackoverflow bug).
    pub fn record_warm_stale(&mut self, host: &str) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.warm_ewma = ewma_update(p.warm_ewma, p.warm_samples, false);
        p.warm_samples = p.warm_samples.saturating_add(1);
        p.fetch_count = p.fetch_count.saturating_add(1);
        p.warm_fail_count = p.warm_fail_count.saturating_add(1);
        p.warm_fail_streak = p.warm_fail_streak.saturating_add(1);
        if p.warm_fail_streak >= 2 {
            let elapsed = n.saturating_sub(p.last_solved).max(120);
            p.observed_lifetime = Some(match p.observed_lifetime {
                Some(prev) => prev.max(120).min(elapsed),
                None => elapsed,
            });
            // Cookies are dead : clear so route_for doesn't serve them.
            p.cookies.clear();
            p.last_refreshed = 0;
            p.replay_ok = false;
        }
        self.save();
    }
}

// ────────────────────────── legacy migration ──────────────────────────

#[derive(Deserialize)]
struct LegacyState {
    #[serde(default)]
    solved: HashMap<String, LegacySolved>,
}

#[derive(Deserialize)]
struct LegacySolved {
    cookies: Vec<(String, String, String)>,
    at: u64,
}

// ────────────────────────── tests ──────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cr(name: &str, value: &str, domain: &str, exp: Option<u64>) -> CookieRecord {
        CookieRecord {
            name: name.into(),
            value: value.into(),
            domain: domain.into(),
            path: "/".into(),
            expires_at: exp,
            secure: false,
            http_only: false,
            same_site: "Lax".into(),
        }
    }

    // == vault epoch reconciliation ==

    // A helper's fresh harvest must survive a live snapshot's next
    // save: the helper stamped a newer epoch, so save() must NOT
    // adopt disk here (memory IS the newer writer) and the harvest
    // stands.
    #[test]
    fn wave450_network_diagnostics_never_become_wall_routes() {
        let mut state = GhostState::default();
        let n = now();
        let profile = state.profiles.entry("offline.example".into()).or_default();
        profile.failures = [
            (n - 601, FailClass::Network),
            (n, FailClass::Block),
            (n, FailClass::Network),
            (n, FailClass::Network),
            (n, FailClass::Network),
        ]
        .into();
        assert_eq!(state.recent_network_failures("offline.example"), 3);
        assert_eq!(state.recent_network_failures("other.example"), 0);
        assert!(matches!(
            state.route_for_class("offline.example", "direct"),
            RouteDecision::Cold
        ));
        assert!(!state.is_flaky("offline.example"));
        let p = state.profiles.get_mut("offline.example").unwrap();
        p.t1_samples = EWMA_MIN_SAMPLES;
        p.t1_ewma = 0.1;
        assert!(state.is_flaky("offline.example"));
        state
            .profiles
            .get_mut("offline.example")
            .unwrap()
            .needs_tier2 = true;
        assert!(!state.is_flaky("offline.example"));
    }

    #[test]
    fn newer_snapshot_keeps_its_vault() {
        let mut mine = GhostState::default();
        mine.profiles
            .entry("ex.com".into())
            .or_default()
            .session_cookies = vec![cr("sid", "fresh", ".ex.com", None)];
        bump_vault_epoch(&mut mine);
        // Disk carries an OLDER epoch with different cookies.
        let mut disk = GhostState {
            vault_epoch: 0,
            ..Default::default()
        };
        disk.profiles
            .entry("ex.com".into())
            .or_default()
            .session_cookies = vec![cr("sid", "stale", ".ex.com", None)];
        mine.merge_vault_from_disk(&disk);
        assert_eq!(mine.profiles["ex.com"].session_cookies[0].value, "fresh");
    }

    // The core resurrection bug: a live daemon snapshot (boot-era
    // vault) saving AFTER a helper deleted a domain's session must
    // adopt the deletion, not rewrite the dead session back.
    #[test]
    fn newer_disk_vault_wins_so_logout_is_not_resurrected() {
        // The daemon snapshot, loaded at boot (epoch 7).
        let mut mine = GhostState {
            vault_epoch: 7,
            ..Default::default()
        };
        mine.profiles
            .entry("ex.com".into())
            .or_default()
            .session_cookies = vec![cr("sid", "dead-session", ".ex.com", None)];
        mine.tier1_cookies = vec![cr("sid", "dead-session", ".ex.com", None)];
        // The logout helper wrote disk: deletion, newer epoch.
        // clear_session_cookies_for keeps the profile entry and
        // empties its cookies, so disk carries ex.com with none.
        let mut disk = GhostState {
            vault_epoch: 8,
            ..Default::default()
        };
        disk.profiles.entry("ex.com".into()).or_default();
        mine.merge_vault_from_disk(&disk);
        assert!(
            mine.profiles["ex.com"].session_cookies.is_empty(),
            "the logged-out session must stay deleted"
        );
        assert!(
            mine.tier1_cookies.is_empty(),
            "the logged-out tier-1 echo must stay deleted"
        );
    }

    // A helper's harvest lands on disk with a newer epoch; the live
    // snapshot's next save must ADOPT it, not erase it.
    #[test]
    fn newer_disk_harvest_is_adopted_by_the_snapshot() {
        let mut mine = GhostState {
            vault_epoch: 3,
            ..Default::default()
        };
        // Boot-era: ex.com had nothing, other.com still does.
        mine.profiles
            .entry("other.com".into())
            .or_default()
            .session_cookies = vec![cr("o", "kept", ".other.com", None)];
        let mut disk = GhostState {
            vault_epoch: 4,
            ..Default::default()
        };
        disk.profiles
            .entry("ex.com".into())
            .or_default()
            .session_cookies = vec![cr("sid", "harvested", ".ex.com", None)];
        mine.merge_vault_from_disk(&disk);
        assert_eq!(
            mine.profiles["ex.com"].session_cookies[0].value, "harvested",
            "the harvest must survive the snapshot's next save"
        );
        assert_eq!(mine.vault_epoch, 4, "the adopted epoch must ride along");
    }

    // A tie means no helper wrote since this snapshot loaded, so its
    // copy is exactly what disk holds: keep it (this is what lets a
    // helper's own save keep its in-memory mutation).
    #[test]
    fn epoch_tie_keeps_the_snapshot() {
        let mut mine = GhostState {
            vault_epoch: 5,
            ..Default::default()
        };
        mine.tier1_cookies = vec![cr("t1", "mine", ".ex.com", None)];
        let mut disk = GhostState {
            vault_epoch: 5,
            ..Default::default()
        };
        disk.tier1_cookies = vec![];
        mine.merge_vault_from_disk(&disk);
        assert_eq!(mine.tier1_cookies.len(), 1, "tie = snapshot stands");
    }

    // Deletion stamps ride onto every profile the logout touches,
    // including hosts listed as extras by the caller.
    #[test]
    fn session_clear_stamp_reaches_subdomains_and_listed_extras() {
        let mut state = GhostState::default();
        state.profiles.entry("ex.com".into()).or_default();
        state.profiles.entry("app.ex.com".into()).or_default();
        state.profiles.entry("other.com".into()).or_default();
        let extras = vec!["deep.app.ex.com".to_string()];
        stamp_session_cleared(&mut state, "ex.com", &extras, 42);
        assert_eq!(state.profiles["ex.com"].session_cleared_epoch, 42);
        assert_eq!(state.profiles["app.ex.com"].session_cleared_epoch, 42);
        assert_eq!(state.profiles["deep.app.ex.com"].session_cleared_epoch, 42);
        assert_eq!(state.profiles["other.com"].session_cleared_epoch, 0);
    }

    // Vault writes stamp one past the state they were built from
    // and saturate instead of wrapping into the past.
    #[test]
    fn vault_writes_stamp_one_past_their_base_and_saturate() {
        let mut base = GhostState {
            vault_epoch: 40,
            ..Default::default()
        };
        bump_vault_epoch(&mut base);
        assert_eq!(base.vault_epoch, 41);
        let mut top = GhostState {
            vault_epoch: u64::MAX,
            ..Default::default()
        };
        bump_vault_epoch(&mut top);
        assert_eq!(top.vault_epoch, u64::MAX);
    }

    // == v4 phase 0: route memory ==

    #[test]
    fn ewma_math() {
        // First sample takes the outcome exactly.
        assert_eq!(ewma_update(1.0, 0, false), 0.0);
        assert_eq!(ewma_update(0.0, 0, true), 1.0);
        // Then smooths with alpha.
        let v = ewma_update(1.0, 3, false);
        assert!((v - 0.7).abs() < 1e-6, "got {v}");
        let v = ewma_update(0.0, 3, true);
        assert!((v - 0.3).abs() < 1e-6, "got {v}");
    }

    #[test]
    fn flaky_wall_routes_to_solve_via_ewma() {
        // Discriminating: pre-fix route_for had no EWMA consult, so a
        // host whose wall appears intermittently (never two verdicts
        // in a row, needs_tier2 never sticks... it does stick, but a
        // host that keeps clearing needs_tier2 via cold-ok would loop
        // Cold forever) routes Cold and burns a doomed tier-1 attempt
        // every fetch. Post-fix: enough failing samples = solve.
        let mut state = GhostState::default();
        let host = "flaky.example";
        // Alternating wall/ok: the EWMA settles under the block
        // threshold with enough samples either way; make it clearly
        // failing: 4 walled, 1 ok.
        for _ in 0..4 {
            state.record_cold_walled(host, Some("cloudflare"));
            state.record_cold_ok(host);
        }
        // The alternating history keeps needs_tier2 flipping off via
        // record_cold_ok (that is the pre-fix loop). EWMA sees the
        // truth.
        let p = state.profiles.get(host).unwrap();
        assert!(p.t1_samples >= EWMA_MIN_SAMPLES);
        // 4 fails + 4 oks alternating ending on ok: ewma < 0.5?
        // Sequence f,o,f,o,f,o,f,o = 0,0.3,0.21,0.447,0.313,0.519,
        // 0.363,0.554 -> NOT below threshold. Force clearly-failing:
        for _ in 0..3 {
            state.record_cold_walled(host, Some("cloudflare"));
        }
        let p = state.profiles.get_mut(host).unwrap();
        assert!(p.t1_ewma < EWMA_BLOCK_THRESHOLD, "ewma {}", p.t1_ewma);
        // The three straight walls also armed the cooldown, which
        // sits above this consult. Clear it and the boolean flag so
        // only the EWMA consult can drive the decision under test.
        p.wall_fail_streak = 0;
        p.last_wall_fail = 0;
        p.needs_tier2 = false;
        assert!(matches!(state.route_for(host), RouteDecision::SkipToSolve));
    }

    #[test]
    fn clean_ewma_stays_cold() {
        let mut state = GhostState::default();
        let host = "easy.example";
        for _ in 0..6 {
            state.record_cold_ok(host);
        }
        assert!(matches!(state.route_for(host), RouteDecision::Cold));
    }

    #[test]
    fn failures_cap_and_never_poison_routing() {
        let mut state = GhostState::default();
        let host = "flaky-net.example";
        for _ in 0..40 {
            state.record_failure(host, FailClass::Network);
        }
        let p = state.profiles.get(host).unwrap();
        assert_eq!(p.failures.len(), FAILURES_PER_HOST);
        // 40 network failures and routing still says cold: a flaky
        // network can never poison a route decision.
        assert!(matches!(state.route_for(host), RouteDecision::Cold));
        assert!(!p.needs_tier2);
    }

    #[test]
    fn lru_evicts_coldest_past_cap() {
        let mut state = GhostState::default();
        // Oldest host first (all timestamps 0), then fresh ones.
        state
            .profiles
            .insert("ancient.example".into(), DomainProfile::default());
        for i in 0..PROFILE_CAP {
            let p = DomainProfile {
                last_cold_check: 1_700_000_000 + i as u64,
                ..Default::default()
            };
            state.profiles.insert(format!("h{i}.example"), p);
        }
        assert_eq!(state.profiles.len(), PROFILE_CAP + 1);
        state.save();
        assert_eq!(state.profiles.len(), PROFILE_CAP);
        assert!(!state.profiles.contains_key("ancient.example"));
    }

    #[test]
    fn kill_switch_disables_consult_and_record() {
        unsafe { std::env::set_var("DONSETCH_NO_ROUTE_MEMORY", "1") };
        let mut state = GhostState::default();
        let host = "walled.example";
        // Consult off: even a known-walled host routes Cold.
        {
            let p = DomainProfile {
                needs_tier2: true,
                ..Default::default()
            };
            state.profiles.insert(host.into(), p);
        }
        assert!(matches!(state.route_for(host), RouteDecision::Cold));
        // Record off: nothing mutates.
        state.record_cold_ok(host);
        assert_eq!(state.profiles.get(host).unwrap().fetch_count, 0);
        unsafe { std::env::remove_var("DONSETCH_NO_ROUTE_MEMORY") };
    }

    #[test]
    fn readonly_consults_but_never_writes() {
        unsafe { std::env::set_var("DONSETCH_ROUTE_MEMORY_READONLY", "1") };
        let mut state = GhostState::default();
        let host = "walled2.example";
        {
            // Fresh cold check: otherwise RecheckCold (correctly)
            // takes precedence over SkipToSolve.
            let p = DomainProfile {
                needs_tier2: true,
                wall_fail_streak: 0,
                last_cold_check: now(),
                ..Default::default()
            };
            state.profiles.insert(host.into(), p);
        }
        // Consult on: known-walled host skips to solve.
        assert!(matches!(state.route_for(host), RouteDecision::SkipToSolve));
        // Record off.
        state.record_cold_ok(host);
        assert_eq!(state.profiles.get(host).unwrap().fetch_count, 0);
        state.record_failure(host, FailClass::Network);
        assert!(state.profiles.get(host).unwrap().failures.is_empty());
        unsafe { std::env::remove_var("DONSETCH_ROUTE_MEMORY_READONLY") };
    }

    #[test]
    fn migration_gate_runs_once_for_v1_never_for_v2() {
        // Discriminating (live-caught bug class): the un-poisoning
        // migration used to fire on EVERY load, wiping needs_tier2
        // of legitimately-walled-but-never-solved hosts between
        // runs. Post-fix: version 1 files migrate once (the walled
        // flag is correctly cleared one last time for possibly-
        // poisoned old data) and stamp to version 2; version 2
        // files NEVER migrate.
        let mut state = GhostState {
            version: 1,
            ..Default::default()
        };
        let walled = DomainProfile {
            needs_tier2: true,
            ..Default::default()
        };
        state.profiles.insert("walled.example".into(), walled);
        // Simulate the v1 migration pass (the gated block in load):
        if state.version < 2 {
            for profile in state.profiles.values_mut() {
                if profile.needs_tier2 && profile.cookies.is_empty() {
                    profile.needs_tier2 = false;
                    profile.wall_vendor = None;
                }
            }
        }
        state.version = STATE_VERSION;
        assert!(!state.profiles["walled.example"].needs_tier2);
        assert_eq!(state.version, STATE_VERSION);

        // Now the host legitimately re-earns its wall (a real
        // challenge verdict), gets saved at version 2, and the next
        // load must NOT clear it.
        let mut state2 = state;
        {
            let p = state2.profiles.get_mut("walled.example").unwrap();
            p.needs_tier2 = true;
            p.wall_vendor = Some("cloudflare".into());
        }
        // Simulated second load: version 2, gate closed.
        if state2.version < 2 {
            panic!("v2 file must never enter the migration");
        }
        assert!(state2.profiles["walled.example"].needs_tier2);
    }

    /// B1 anti-amnesia: a pre-B1 (v2) state file loads without
    /// wiping wall knowledge; egress_class starts unstamped (None).
    #[test]
    fn b1_egress_class_defaults_none_and_preserves_walls() {
        let mut state = GhostState {
            version: 2,
            ..Default::default()
        };
        state.profiles.insert(
            "cf.example".into(),
            DomainProfile {
                needs_tier2: true,
                replay_ok: true,
                wall_vendor: Some("cloudflare".into()),
                last_solved: now(),
                ..Default::default()
            },
        );
        // v2 file: no migration that touches walls (only the <2 gate).
        assert!(state.version >= 2);
        assert!(state.profiles["cf.example"].needs_tier2);
        assert!(state.profiles["cf.example"].egress_class.is_none());
    }

    /// B1: Warm cookies learned on `direct` are never injected when
    /// the live lane is `proxy` (and vice versa).
    #[test]
    fn warm_cookies_never_cross_egress_classes() {
        let mut state = GhostState::default();
        let n = now();
        state.profiles.insert(
            "bank.example".into(),
            DomainProfile {
                needs_tier2: true,
                replay_ok: true,
                last_solved: n,
                cookies: vec![CookieRecord {
                    name: "cf_clearance".into(),
                    value: "tok".into(),
                    domain: "bank.example".into(),
                    path: "/".into(),
                    expires_at: Some(n + 3600),
                    secure: true,
                    http_only: true,
                    same_site: "None".into(),
                }],
                egress_class: Some("direct".into()),
                ..Default::default()
            },
        );
        match state.route_for_class("bank.example", "direct") {
            RouteDecision::Warm(c) => assert_eq!(c.len(), 1, "same class must warm"),
            other => panic!("expected Warm on matching class, got {other:?}"),
        }
        match state.route_for_class("bank.example", "proxy") {
            RouteDecision::Warm(_) => {
                panic!("cross-class warm is a cookie/IP link; must not serve")
            }
            RouteDecision::SkipToSolve | RouteDecision::RecheckCold | RouteDecision::Cold => {}
            other => panic!("unexpected cross-class route: {other:?}"),
        }
    }

    #[test]
    fn probe_candidates_stalest_first_and_filtered() {
        let mut state = GhostState::default();
        let n = now();
        // Stale walled host: candidate.
        state.profiles.insert(
            "stale.example".into(),
            DomainProfile {
                needs_tier2: true,
                last_cold_check: n - 30_000,
                ..Default::default()
            },
        );
        // Staler walled host: candidate, first.
        state.profiles.insert(
            "staler.example".into(),
            DomainProfile {
                needs_tier2: true,
                last_cold_check: n - 40_000,
                ..Default::default()
            },
        );
        // Fresh walled host: not yet.
        state.profiles.insert(
            "fresh.example".into(),
            DomainProfile {
                needs_tier2: true,
                last_cold_check: n - 10,
                ..Default::default()
            },
        );
        // Stale but NOT walled: never probed (nothing to heal).
        state.profiles.insert(
            "easy.example".into(),
            DomainProfile {
                last_cold_check: n - 99_999,
                ..Default::default()
            },
        );
        let c = state.probe_candidates(6 * 3600, 3);
        assert_eq!(c, vec!["staler.example", "stale.example"]);
        let one = state.probe_candidates(6 * 3600, 1);
        assert_eq!(one, vec!["staler.example"]);
    }

    #[test]
    fn probe_candidates_skip_hosts_the_rules_deny() {
        use crate::rules::{RuleAction, RuleSet, RulesSection, UrlRule};
        let mut state = GhostState::default();
        let n = now();
        for (host, age) in [
            ("www.walled.example", 90_000),
            ("stale.example", 40_000),
            ("other.example", 30_000),
        ] {
            state.profiles.insert(
                host.into(),
                DomainProfile {
                    needs_tier2: true,
                    last_cold_check: n - age,
                    ..Default::default()
                },
            );
        }
        let mut section = RulesSection::default();
        section.url.insert(
            "walled.example".into(),
            UrlRule {
                action: RuleAction::Deny,
                message: Some("use a real browser".into()),
                ..Default::default()
            },
        );
        let rules = RuleSet::compile(&section).unwrap();
        // The denied host is the stalest, so it would take the single
        // slot if the filter ran after the limit.
        assert_eq!(
            state.probe_candidates_under(6 * 3600, 1, &rules),
            vec!["stale.example"]
        );
        assert_eq!(
            state.probe_candidates_under(6 * 3600, 3, &rules),
            vec!["stale.example", "other.example"]
        );
        // The negative case: without the rule the stalest host leads.
        assert_eq!(
            state.probe_candidates_under(6 * 3600, 1, &RuleSet::empty()),
            vec!["www.walled.example"]
        );
    }

    #[test]
    fn probe_inconclusive_rearms_without_touching_evidence() {
        // Discriminating: inconclusive must change ONLY the probe
        // cadence field. Wall flags, EWMAs, fetch counters stay put;
        // an inconclusive probe is not evidence in either direction.
        let mut state = GhostState::default();
        let host = "walled.example";
        state.profiles.insert(
            host.into(),
            DomainProfile {
                needs_tier2: true,
                fetch_count: 5,
                last_cold_check: 1,
                ..Default::default()
            },
        );
        state.record_cold_walled(host, Some("cloudflare"));
        // Simulate staleness (record_cold_walled stamps now(); the
        // probe must re-arm from an OLD value to be observable).
        state.profiles.get_mut(host).unwrap().last_cold_check = 1;
        let before = state.profiles.get(host).unwrap().clone();
        state.record_probe_inconclusive(host);
        let after = state.profiles.get(host).unwrap();
        assert!(after.last_cold_check > before.last_cold_check);
        assert_eq!(after.needs_tier2, before.needs_tier2);
        assert_eq!(after.fetch_count, before.fetch_count);
        assert_eq!(after.t1_ewma, before.t1_ewma);
        assert_eq!(after.t1_samples, before.t1_samples);
        assert_eq!(after.walled_count, before.walled_count);
    }

    #[test]
    fn note_probe_counts() {
        let mut state = GhostState::default();
        state.note_probe();
        state.note_probe();
        assert_eq!(state.probes_total, 2);
    }

    // The readonly kill switch maps through the config layer
    // (DONSETCH_ROUTE_MEMORY_READONLY); set before the first cfg() touch
    // so the process-wide config sees it.
    #[test]
    fn note_probe_skips_when_readonly() {
        unsafe { std::env::set_var("DONSETCH_ROUTE_MEMORY_READONLY", "1") };
        let mut state = GhostState::default();
        state.note_probe();
        assert_eq!(state.probes_total, 0);
        unsafe { std::env::remove_var("DONSETCH_ROUTE_MEMORY_READONLY") };
    }

    // == v4 phase 0.3: personas ==

    fn test_caps() -> crate::persona::PersonaCaps {
        crate::persona::PersonaCaps {
            chrome_major: 150,
            platform: crate::profile::Platform::Linux,
            branded: true,
        }
    }

    #[test]
    fn persona_mints_once_and_stays_stable() {
        let mut state = GhostState::default();
        let caps = test_caps();
        let first = state.ensure_persona("example.com", &caps).unwrap().clone();
        let second = state.ensure_persona("example.com", &caps).unwrap().clone();
        // Longitudinal rule: same persona, unchanged, forever.
        assert_eq!(first, second);
        assert_eq!(first.generation, 1);
        assert!(first.quarantined_at.is_none());
    }

    #[test]
    fn quarantined_persona_is_replaced_never_resurrected() {
        let mut state = GhostState::default();
        let caps = test_caps();
        let burned = state.ensure_persona("example.com", &caps).unwrap().clone();
        state.quarantine_persona("example.com", "detected by scorecard");
        let fresh = state.ensure_persona("example.com", &caps).unwrap().clone();
        assert_eq!(fresh.generation, burned.generation + 1);
        assert_ne!(fresh.entropy_seed, burned.entropy_seed);
        assert!(fresh.quarantined_at.is_none());
        assert!(fresh.coherent(&caps));
    }

    #[test]
    fn drifted_persona_remints_on_ensure() {
        // The wire profile bumped (Chrome 150 -> 151): the old
        // persona would claim a build the TLS layer no longer
        // emits. ensure_persona must re-mint, not ship the tell.
        let mut state = GhostState::default();
        let old_caps = test_caps();
        state.ensure_persona("example.com", &old_caps).unwrap();
        let new_caps = crate::persona::PersonaCaps {
            chrome_major: 151,
            ..test_caps()
        };
        let p = state.ensure_persona("example.com", &new_caps).unwrap();
        assert_eq!(p.chrome_major, 151);
        assert_eq!(p.generation, 2);
    }

    #[test]
    fn personas_respect_route_memory_switches() {
        let mut state = GhostState::default();
        let caps = test_caps();
        unsafe { std::env::set_var("DONSETCH_NO_ROUTE_MEMORY", "1") };
        assert!(state.ensure_persona("x.example", &caps).is_none());
        assert!(state.personas.is_empty());
        state.quarantine_persona("x.example", "should be a no-op");
        unsafe { std::env::remove_var("DONSETCH_NO_ROUTE_MEMORY") };
    }

    #[test]
    fn personas_survive_serde_roundtrip_and_evict_with_profiles() {
        let mut state = GhostState::default();
        let caps = test_caps();
        state.ensure_persona("a.example", &caps).unwrap();
        let json = serde_json::to_string(&state).unwrap();
        let back: GhostState = serde_json::from_str(&json).unwrap();
        assert!(back.personas.contains_key("a.example"));
        // LRU eviction removes the persona alongside the profile.
        let mut state = GhostState::default();
        state.personas.insert(
            "ancient.example".into(),
            crate::persona::Persona::mint("ancient.example", &caps, 1, 1),
        );
        state
            .profiles
            .insert("ancient.example".into(), DomainProfile::default());
        for i in 0..PROFILE_CAP {
            let p = DomainProfile {
                last_cold_check: 1_700_000_000 + i as u64,
                ..Default::default()
            };
            state.profiles.insert(format!("h{i}.example"), p);
        }
        state.save();
        assert_eq!(state.profiles.len(), PROFILE_CAP);
        assert!(!state.personas.contains_key("ancient.example"));
    }

    #[test]
    fn old_state_json_migrates_with_defaults() {
        // A 3.6.x state file (no v4 fields) must load with sane
        // defaults: ewma 1.0 (clean prior), empty histogram.
        let old = r#"{"fetch_count":3,"walled_count":1,"needs_tier2":true,
            "warm_ok_count":0,"warm_fail_streak":0,"warm_fail_count":0,
            "solve_fail_streak":0,"solve_fail_cooldown_until":0,
            "wall_fail_streak":0,"cookies":[],"cookies_updated_at":0,
            "wall_vendor":null,"last_cold_check":0,"last_wall_fail":0}"#;
        let p: DomainProfile = serde_json::from_str(old).unwrap();
        assert_eq!(p.t1_ewma, 1.0);
        assert_eq!(p.warm_ewma, 1.0);
        assert_eq!(p.t1_samples, 0);
        assert!(p.failures.is_empty());
        assert!(p.needs_tier2);
        assert_eq!(p.fetch_count, 3);
    }

    // ── session vault ──

    #[test]
    fn session_worthy_keeps_and_drops() {
        // Session cookie (no expiry) = the login signature.
        assert!(is_session_worthy(&cr("probe", "v", "a.com", None)));
        // Auth-named with expiry = remember-me.
        assert!(is_session_worthy(&cr(
            "sessionid",
            "v",
            "a.com",
            Some(9_999_999_999)
        )));
        assert!(is_session_worthy(&cr(
            "connect.sid",
            "v",
            "a.com",
            Some(9_999_999_999)
        )));
        // Trackers never vault regardless of expiry.
        assert!(!is_session_worthy(&cr("_ga", "v", "a.com", None)));
        assert!(!is_session_worthy(&cr("TDID", "v", "a.com", None)));
        // Clearance cookies belong to the wall vault, not here.
        assert!(!is_session_worthy(&cr("cf_clearance", "v", "a.com", None)));
        // A preference cookie with expiry: junk.
        assert!(!is_session_worthy(&cr(
            "theme",
            "dark",
            "a.com",
            Some(9_999_999_999)
        )));
        // Degenerate shapes.
        assert!(!is_session_worthy(&cr("x", "", "a.com", None)));
        assert!(!is_session_worthy(&cr("x", "v", "", None)));
        // Oversized value: never a reusable session token.
        assert!(!is_session_worthy(&cr(
            "tok",
            &"a".repeat(8193),
            "a.com",
            None
        )));
    }

    #[test]
    fn session_merge_dedupes_and_caps_per_domain() {
        let mut vault = vec![cr("a", "1", "site.com", None)];
        // Same (name, domain) refreshes in place.
        merge_session_cookies(
            &mut vault,
            std::slice::from_ref(&cr("a", "2", "site.com", None)),
        );
        assert_eq!(vault.len(), 1);
        assert_eq!(vault[0].value, "2");
        // Fill past the per-domain cap: oldest entries drop.
        let many: Vec<CookieRecord> = (0..60)
            .map(|i| cr(&format!("c{i}"), "v", "site.com", None))
            .collect();
        merge_session_cookies(&mut vault, &many);
        assert!(
            vault.len() <= 50,
            "per-domain cap breached: {}",
            vault.len()
        );
        // Newest survive: c59 must be present, c0 must not.
        assert!(vault.iter().any(|c| c.name == "c59"));
        assert!(vault.iter().all(|c| c.name != "c0"));
    }

    // ── cookies_fresh_at ──

    #[test]
    fn fresh_no_cookies() {
        let p = DomainProfile::default();
        assert!(!cookies_fresh_at(&p, 1000));
    }

    #[test]
    fn fresh_server_expiry_ok() {
        let p = DomainProfile {
            cookies: vec![cr("cf", "x", ".a.com", Some(2000))],
            last_solved: 1000,
            ..Default::default()
        };
        assert!(cookies_fresh_at(&p, 1500));
    }

    #[test]
    fn fresh_server_expiry_past() {
        let p = DomainProfile {
            cookies: vec![cr("cf", "x", ".a.com", Some(1500))],
            last_solved: 1000,
            ..Default::default()
        };
        assert!(!cookies_fresh_at(&p, 1500));
    }

    #[test]
    fn fresh_earliest_expires_wins() {
        let p = DomainProfile {
            cookies: vec![
                cr("a", "x", ".c.com", Some(5000)),
                cr("b", "y", ".c.com", Some(1200)),
            ],
            last_solved: 1000,
            ..Default::default()
        };
        assert!(!cookies_fresh_at(&p, 1200));
        assert!(cookies_fresh_at(&p, 1199));
    }

    #[test]
    fn fresh_observed_lifetime_shorter() {
        // Server says 1h, but we learned cookies die at 300s.
        let p = DomainProfile {
            cookies: vec![cr("cf", "x", ".a.com", Some(100000))],
            last_solved: 1000,
            observed_lifetime: Some(300),
            ..Default::default()
        };
        assert!(cookies_fresh_at(&p, 1299));
        assert!(!cookies_fresh_at(&p, 1300));
    }

    #[test]
    fn fresh_ttl_cap() {
        // No server expiry, no observed lifetime : cap at 2h.
        let p = DomainProfile {
            cookies: vec![cr("s", "x", ".a.com", None)],
            last_solved: 1000,
            ..Default::default()
        };
        assert!(cookies_fresh_at(&p, 1000 + 2 * 3600 - 1));
        assert!(!cookies_fresh_at(&p, 1000 + 2 * 3600));
    }

    #[test]
    fn fresh_session_cookie_no_expiry() {
        // Session cookie (None): relies on observed_lifetime or TTL_CAP.
        let p = DomainProfile {
            cookies: vec![cr("s", "x", ".a.com", None)],
            last_solved: 1000,
            ..Default::default()
        };
        assert!(cookies_fresh_at(&p, 1000));
    }

    // ── route_for ──

    #[test]
    fn route_unknown_domain_is_cold() {
        let s = GhostState::default();
        assert!(matches!(s.route_for("new.com"), RouteDecision::Cold));
    }

    #[test]
    fn route_easy_domain_is_cold() {
        let mut s = GhostState::default();
        s.profiles.insert(
            "easy.com".into(),
            DomainProfile {
                needs_tier2: false,
                ..Default::default()
            },
        );
        assert!(matches!(s.route_for("easy.com"), RouteDecision::Cold));
    }

    #[test]
    fn route_hard_fresh_is_warm() {
        let mut s = GhostState::default();
        s.profiles.insert(
            "hard.com".into(),
            DomainProfile {
                needs_tier2: true,
                replay_ok: true,
                cookies: vec![cr("cf", "x", ".hard.com", Some(now() + 3600))],
                last_solved: now(),
                ..Default::default()
            },
        );
        assert!(matches!(s.route_for("hard.com"), RouteDecision::Warm(_)));
    }

    #[test]
    fn route_hard_stale_recent_is_skip() {
        let mut s = GhostState::default();
        s.profiles.insert(
            "hard.com".into(),
            DomainProfile {
                needs_tier2: true,
                cookies: vec![cr("cf", "x", ".hard.com", Some(100))],
                last_solved: 100,
                last_cold_check: now() - 100, // recent
                ..Default::default()
            },
        );
        assert!(matches!(
            s.route_for("hard.com"),
            RouteDecision::SkipToSolve
        ));
    }

    #[test]
    fn route_hard_stale_old_cold_check_is_recheck() {
        let mut s = GhostState::default();
        s.profiles.insert(
            "hard.com".into(),
            DomainProfile {
                needs_tier2: true,
                cookies: vec![cr("cf", "x", ".hard.com", Some(100))],
                last_solved: 100,
                last_cold_check: 1, // very old : > RECHECK_INTERVAL
                ..Default::default()
            },
        );
        assert!(matches!(
            s.route_for("hard.com"),
            RouteDecision::RecheckCold
        ));
    }

    // ── observation: convergence ──

    #[test]
    fn cold_ok_clears_needs_tier2() {
        let mut s = GhostState::default();
        s.profiles.insert(
            "walled.com".into(),
            DomainProfile {
                needs_tier2: true,
                cookies: vec![cr("cf", "x", ".walled.com", Some(9999))],
                last_solved: 100,
                ..Default::default()
            },
        );
        s.record_cold_ok("walled.com");
        let p = &s.profiles["walled.com"];
        assert!(!p.needs_tier2);
        assert!(p.cookies.is_empty()); // stale context cleared
        assert!(p.observed_lifetime.is_none());
    }

    #[test]
    fn warm_stale_learns_lifetime() {
        let mut s = GhostState::default();
        s.profiles.insert(
            "hard.com".into(),
            DomainProfile {
                needs_tier2: true,
                replay_ok: true,
                cookies: vec![cr("cf", "x", ".hard.com", Some(9999))],
                last_solved: 1000,
                ..Default::default()
            },
        );
        // Simulate: warm fetch at t=1300 was walled. Dampened
        // learning: the first failure is tolerated (transient
        // vendor rotation), the second confirms and learns.
        s.record_warm_stale("hard.com");
        let p = &s.profiles["hard.com"];
        assert!(!p.cookies.is_empty(), "first failure tolerated");
        s.record_warm_stale("hard.com");
        let p = &s.profiles["hard.com"];
        assert!(p.cookies.is_empty());
        assert!(p.observed_lifetime.is_some());
        assert_eq!(p.warm_fail_count, 2);
    }

    #[test]
    fn warm_ok_refreshes_cookies() {
        let mut s = GhostState::default();
        s.profiles.insert(
            "hard.com".into(),
            DomainProfile {
                needs_tier2: true,
                cookies: vec![cr("cf_clearance", "old_val", ".hard.com", Some(9999))],
                last_solved: 1000,
                ..Default::default()
            },
        );
        // Simulate: server sent a refreshed cookie + a new clearance cookie.
        let refreshed = vec![
            cr("cf_clearance", "new_val", ".hard.com", Some(9999)),
            cr("datadome", "new_cookie", ".hard.com", Some(9999)),
        ];
        s.record_warm_ok("hard.com", &refreshed);
        let p = &s.profiles["hard.com"];
        assert_eq!(p.cookies.len(), 2);
        assert_eq!(p.warm_ok_count, 1);
        // Old cookie value was replaced.
        assert_eq!(p.cookies[0].value, "new_val");
        // New clearance cookie was added.
        assert!(p.cookies.iter().any(|c| c.name == "datadome"));
    }

    #[test]
    fn cold_walled_marks_the_wall_and_stores_the_vendor() {
        let mut s = GhostState::default();
        s.record_cold_walled("hard.com", Some("cloudflare"));
        let p = &s.profiles["hard.com"];
        assert!(p.needs_tier2);
        assert_eq!(p.walled_count, 1);
        assert_eq!(p.wall_vendor.as_deref(), Some("cloudflare"));
        assert_eq!(p.wall_fail_streak, 1);
        assert!(p.cookies.is_empty());
    }

    // ── replay gating + warm-stale dampening (v2.2) ──

    #[test]
    fn warm_requires_verified_replay() {
        // Cookies fresh but replay never verified → SkipToSolve,
        // not a doomed Warm roundtrip.
        let mut s = GhostState::default();
        s.profiles.insert(
            "strict.com".into(),
            DomainProfile {
                needs_tier2: true,
                replay_ok: false,
                cookies: vec![cr("cf_clearance", "x", ".strict.com", Some(now() + 3600))],
                last_solved: now(),
                last_cold_check: now(),
                ..Default::default()
            },
        );
        assert!(matches!(
            s.route_for("strict.com"),
            RouteDecision::SkipToSolve
        ));
        // Verified replay flips it to Warm.
        s.profiles.get_mut("strict.com").unwrap().replay_ok = true;
        assert!(matches!(s.route_for("strict.com"), RouteDecision::Warm(_)));
    }

    #[test]
    fn single_warm_failure_keeps_cookies() {
        // Transient tolerance: one walled warm fetch must not kill
        // the vault or learn a bogus lifetime.
        let mut s = GhostState::default();
        s.profiles.insert(
            "hard.com".into(),
            DomainProfile {
                needs_tier2: true,
                replay_ok: true,
                cookies: vec![cr("cf_clearance", "x", ".hard.com", Some(now() + 3600))],
                last_solved: now() - 5, // solved 5s ago
                ..Default::default()
            },
        );
        s.record_warm_stale("hard.com");
        let p = &s.profiles["hard.com"];
        assert!(!p.cookies.is_empty(), "first failure keeps cookies");
        assert!(
            p.observed_lifetime.is_none(),
            "no learning on first failure"
        );
        assert!(p.replay_ok);
        // Still Warm-routable (cookies alive).
        assert!(matches!(s.route_for("hard.com"), RouteDecision::Warm(_)));
    }

    #[test]
    fn solve_cooldown_backoff_caps_at_two_hours() {
        assert_eq!(solve_cooldown_secs(2), 15 * 60);
        assert_eq!(solve_cooldown_secs(3), 30 * 60);
        assert_eq!(solve_cooldown_secs(4), 60 * 60);
        assert_eq!(solve_cooldown_secs(5), 120 * 60);
        assert_eq!(solve_cooldown_secs(9), 120 * 60, "hard cap");
    }

    #[test]
    fn solve_cooldown_routes_fast_fail_then_heals() {
        let mut s = GhostState::default();
        s.record_cold_walled("poison.com", Some("cf"));
        s.record_cold_walled("poison.com", Some("cf"));
        // Inside the cooldown: fail fast, no wasted attempt.
        assert!(matches!(
            s.route_for("poison.com"),
            RouteDecision::SolveCooldown(_)
        ));
        // Cooldown lapsed: routable again (an honest attempt; the
        // wall memory stays).
        let p = s.profiles.get_mut("poison.com").unwrap();
        p.last_wall_fail = now() - 16 * 60;
        assert!(matches!(
            s.route_for("poison.com"),
            RouteDecision::SkipToSolve
        ));
    }

    #[test]
    fn warm_ok_clears_cooldown_memory() {
        let mut s = GhostState::default();
        s.record_cold_walled("heals.com", Some("Cloudflare"));
        s.record_cold_walled("heals.com", Some("Cloudflare"));
        let p = s.profiles.get_mut("heals.com").unwrap();
        p.replay_ok = true;
        // Fresh solve evidence: cookies_fresh_at needs last_solved
        // inside its cap (the field rides legacy state otherwise).
        p.last_solved = now();
        s.record_warm_ok(
            "heals.com",
            &[cr("cf_clearance", "x", ".heals.com", Some(9_999_999_999))],
        );
        let p = &s.profiles["heals.com"];
        assert_eq!(p.wall_fail_streak, 0);
        assert_eq!(p.last_wall_fail, 0);
        assert!(matches!(s.route_for("heals.com"), RouteDecision::Warm(_)));
    }

    #[test]
    fn cold_ok_clears_cooldown_memory() {
        let mut s = GhostState::default();
        s.record_cold_walled("eases.com", Some("cf"));
        s.record_cold_walled("eases.com", Some("cf"));
        s.record_cold_ok("eases.com");
        let p = &s.profiles["eases.com"];
        assert_eq!(p.wall_fail_streak, 0);
        assert!(!p.needs_tier2);
    }

    #[test]
    fn one_wall_fail_is_not_a_cooldown() {
        let mut s = GhostState::default();
        s.record_cold_walled("once.com", Some("cf"));
        assert!(
            !matches!(s.route_for("once.com"), RouteDecision::SolveCooldown(_)),
            "a single failure must not gate the domain"
        );
    }

    #[test]
    fn wave450_cooldown_does_not_cross_egress_classes() {
        let mut s = GhostState::default();
        s.profiles.insert(
            "wall.example".into(),
            DomainProfile {
                egress_class: Some("direct".into()),
                wall_fail_streak: 2,
                last_wall_fail: now(),
                ..Default::default()
            },
        );
        assert!(matches!(
            s.route_for_class("wall.example", "direct"),
            RouteDecision::SolveCooldown(_)
        ));
        assert!(matches!(
            s.route_for_class("wall.example", "proxy"),
            RouteDecision::Cold
        ));
    }

    #[test]
    fn second_consecutive_warm_failure_learns() {
        let mut s = GhostState::default();
        s.profiles.insert(
            "hard.com".into(),
            DomainProfile {
                needs_tier2: true,
                replay_ok: true,
                cookies: vec![cr("cf_clearance", "x", ".hard.com", Some(9999))],
                last_solved: now() - 3_600, // solved 1h ago
                ..Default::default()
            },
        );
        s.record_warm_stale("hard.com");
        s.record_warm_stale("hard.com");
        let p = &s.profiles["hard.com"];
        assert!(p.cookies.is_empty(), "second failure clears cookies");
        let learned = p.observed_lifetime.expect("learned after 2nd failure");
        assert!(learned >= 120, "lifetime floored at 120s, got {learned}");
        assert!(!p.replay_ok);
        // Warm success resets the streak.
        s.record_warm_ok("hard.com", &[]);
        assert_eq!(s.profiles["hard.com"].warm_fail_streak, 0);
    }

    #[test]
    fn one_second_fluke_never_poisons_lifetime() {
        // The live stackoverflow bug: a wall 1s after solve clamped
        // observed_lifetime to 1s → warm forever dead. The floor
        // plus dampening make that impossible.
        let mut s = GhostState::default();
        s.profiles.insert(
            "so.com".into(),
            DomainProfile {
                needs_tier2: true,
                replay_ok: true,
                cookies: vec![cr("cf_clearance", "x", ".so.com", Some(9999))],
                last_solved: now() - 1,
                ..Default::default()
            },
        );
        s.record_warm_stale("so.com");
        s.record_warm_stale("so.com");
        let p = &s.profiles["so.com"];
        let learned = p.observed_lifetime.expect("learned");
        assert!(learned >= 120, "got {learned} : must be floored");
    }

    #[test]
    fn non_challenge_outcomes_do_not_poison() {
        // record_fetch moves counters only: a 404/429/paywall must
        // never force a domain into ghost mode.
        let mut s = GhostState::default();
        s.record_fetch("newsite.com");
        s.record_fetch("newsite.com");
        let p = &s.profiles["newsite.com"];
        assert_eq!(p.fetch_count, 2);
        assert!(!p.needs_tier2);
        assert!(matches!(s.route_for("newsite.com"), RouteDecision::Cold));
    }

    // ── convergence simulation ──

    #[test]
    fn convergence_lifecycle() {
        // Simulate the full lifecycle of a domain through the loop.
        let mut s = GhostState::default();
        let host = "cf-protected.com";
        let now = now();

        // Visit 1: unknown → cold → walled; the vault carries
        // verified clearance (a login session or legacy state).
        assert!(matches!(s.route_for(host), RouteDecision::Cold));
        s.record_cold_walled(host, Some("cloudflare"));
        {
            let p = s.profiles.get_mut(host).unwrap();
            p.cookies = vec![cr(
                "cf_clearance",
                "tok1",
                ".cf-protected.com",
                Some(now + 3600),
            )];
            p.replay_ok = true;
            p.last_solved = now;
        }

        // Visit 2: hard + fresh + replay-verified → warm
        match s.route_for(host) {
            RouteDecision::Warm(c) => assert_eq!(c.len(), 1),
            other => panic!("expected Warm, got {other:?}"),
        }

        // Visit 2 outcome: warm ok → cookies refreshed
        let refreshed = vec![cr(
            "cf_clearance",
            "tok2",
            ".cf-protected.com",
            Some(now + 7200),
        )];
        s.record_warm_ok(host, &refreshed);

        // Visit 3: still warm (cookies refreshed, still fresh)
        match s.route_for(host) {
            RouteDecision::Warm(c) => {
                assert_eq!(c[0].value, "tok2"); // write-back worked
            }
            other => panic!("expected Warm after refresh, got {other:?}"),
        }

        // Verify the domain profile converged
        let p = &s.profiles[host];
        assert_eq!(p.warm_ok_count, 1);
        assert_eq!(p.walled_count, 1);
        assert!(p.needs_tier2);
    }

    // ── legacy migration ──

    #[test]
    fn legacy_migration() {
        // Verify LegacyState deserialization : the production
        // load() uses this to migrate old state files.
        let legacy_json = serde_json::json!({
            "solved": {
                "old.com": {
                    "cookies": [["cf", "val", ".old.com"]],
                    "at": 1000u64
                }
            },
            "renders": {}
        });
        let old: LegacyState = serde_json::from_str(&legacy_json.to_string()).unwrap();
        assert_eq!(old.solved.len(), 1);
        assert_eq!(old.solved["old.com"].cookies[0].0, "cf");
    }

    // ── save is a no-op in test builds (see save() impl) ──

    // ── cookie filtering ──

    #[test]
    fn clearance_filter_keeps_cf() {
        let cookies = vec![
            cr("cf_clearance", "tok", ".a.com", None),
            cr("_ga", "GA1.2.xxx", ".a.com", None),
        ];
        let filtered = filter_clearance(&cookies);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "cf_clearance");
    }

    #[test]
    fn clearance_filter_keeps_datadome() {
        let cookies = vec![
            cr("datadome", "val", ".a.com", None),
            cr("__dd_cookie", "val", ".a.com", None),
            cr("TDID", "tracking", ".a.com", None),
        ];
        let filtered = filter_clearance(&cookies);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn clearance_filter_keeps_akamai() {
        let cookies = vec![
            cr("_abck", "val", ".a.com", None),
            cr("bm_sz", "val", ".a.com", None),
            cr("ak_bmsb", "val", ".a.com", None),
            cr("bcookie", "tracking", ".a.com", None),
        ];
        let filtered = filter_clearance(&cookies);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn clearance_filter_keeps_perimeterx() {
        let cookies = vec![
            cr("_pxhd", "val", ".a.com", None),
            cr("_px2", "val", ".a.com", None),
            cr("demdex", "tracking", ".a.com", None),
        ];
        let filtered = filter_clearance(&cookies);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn clearance_filter_empty() {
        let cookies = vec![
            cr("_ga", "val", ".a.com", None),
            cr("TDID", "val", ".a.com", None),
            cr("demdex", "val", ".a.com", None),
        ];
        let filtered = filter_clearance(&cookies);
        assert!(filtered.is_empty());
    }

    #[test]
    fn warm_ok_stores_only_clearance() {
        let mut s = GhostState::default();
        let cookies = vec![
            cr("cf_clearance", "tok", ".a.com", Some(now() + 3600)),
            cr("_ga", "tracking", ".a.com", None),
            cr("datadome", "val", ".a.com", Some(now() + 3600)),
        ];
        s.record_warm_ok("a.com", &cookies);
        let p = &s.profiles["a.com"];
        assert_eq!(p.cookies.len(), 2); // cf_clearance + datadome only
    }

    #[test]
    fn tier1_jar_sync_dedupes_refreshes_and_caps() {
        let mut st = GhostState::default();
        let rec = |n: &str, v: &str, d: &str, exp: Option<u64>| CookieRecord {
            name: n.into(),
            value: v.into(),
            domain: d.into(),
            path: "/".into(),
            expires_at: exp,
            secure: false,
            http_only: false,
            same_site: "Lax".into(),
        };
        st.sync_tier1_cookies(&[rec("uid", "a", ".x.com", Some(now() + 999))]);
        assert_eq!(st.tier1_cookies.len(), 1);
        // Same key, new value: refresh in place, never duplicated.
        st.sync_tier1_cookies(&[rec("uid", "b", ".x.com", Some(now() + 999))]);
        assert_eq!(st.tier1_cookies.len(), 1);
        assert_eq!(st.tier1_cookies[0].value, "b");
        // Expired + empty-value + giant-value refused.
        st.sync_tier1_cookies(&[
            rec("dead", "v", ".x.com", Some(now().saturating_sub(10))),
            rec("none", "", ".x.com", None),
            rec(
                "big",
                ("x".repeat(TIER1_COOKIE_VALUE_MAX + 1)).as_str(),
                ".x.com",
                None,
            ),
        ]);
        assert_eq!(st.tier1_cookies.len(), 1);
        // Cap: flood past 500 distinct, cap holds, refreshed survives.
        for i in 0..600 {
            st.sync_tier1_cookies(&[rec(&format!("k{i}"), "v", ".x.com", None)]);
        }
        assert_eq!(st.tier1_cookies.len(), TIER1_COOKIE_MAX);
        // Honest cap semantics: the freshest flood entries survived,
        // the oldest (never-refreshed) tail left. uid was last
        // touched before the flood: it is the first evicted.
        assert!(st.tier1_cookies.iter().any(|c| c.name == "k599"));
        assert!(st.tier1_cookies.iter().all(|c| c.name != "k0"));
        // Roundtrip through serde (persistence shape).
        let json = serde_json::to_string(&st).unwrap();
        let back: GhostState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tier1_cookies.len(), TIER1_COOKIE_MAX);
    }

    // V02: a cookie re-synced under its scoped form (".x.com") must
    // replace a bare twin ("x.com"), because `store_raw` folds both
    // to one jar cookie : leaving the bare row in place let the
    // older host-only form win the next replay.
    #[test]
    fn tier1_jar_sync_folds_scope_into_the_dedupe_key() {
        let mut st = GhostState::default();
        let rec = |n: &str, v: &str, d: &str| CookieRecord {
            name: n.into(),
            value: v.into(),
            domain: d.into(),
            path: "/".into(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".into(),
        };
        st.sync_tier1_cookies(&[rec("loid", "old", "reddit.com")]);
        assert_eq!(st.tier1_cookies.len(), 1);
        st.sync_tier1_cookies(&[rec("loid", "new", ".reddit.com")]);
        assert_eq!(
            st.tier1_cookies.len(),
            1,
            "the scoped form must replace the bare row, not sit beside it"
        );
        assert_eq!(st.tier1_cookies[0].value, "new");
        // And the bare form folds back the same way.
        st.sync_tier1_cookies(&[rec("loid", "bare", "reddit.com")]);
        assert_eq!(st.tier1_cookies.len(), 1);
        assert_eq!(st.tier1_cookies[0].value, "bare");
    }

    // The vault kill switch maps through the config layer
    // (DONSETCH_NO_COOKIE_VAULT); set before the first cfg() touch.
    #[test]
    fn tier1_sync_honors_vault_kill_switch() {
        unsafe { std::env::set_var("DONSETCH_NO_COOKIE_VAULT", "1") };
        let mut st = GhostState::default();
        let rec = CookieRecord {
            name: "post".into(),
            value: "v".into(),
            domain: ".x.com".into(),
            path: "/".into(),
            expires_at: None,
            secure: false,
            http_only: false,
            same_site: "Lax".into(),
        };
        st.sync_tier1_cookies(&[rec]);
        assert!(st.tier1_cookies.iter().all(|c| c.name != "post"));
        unsafe { std::env::remove_var("DONSETCH_NO_COOKIE_VAULT") };
    }

    // The vault outranks the tier-1 echo (#319): the replay set a
    // daemon builds from (vault sessions, jar) must never let a jar
    // entry shadow a login cookie for the same name and domain, and
    // jar-unique cookies must still replay.
    #[test]
    fn vault_outranks_the_tier1_echo_on_the_same_key() {
        let sessions = vec![
            cr("token_v2", "LOGIN", ".reddit.com", None),
            cr("reddit_session", "LOGIN2", ".reddit.com", None),
        ];
        // The echo: an anonymous value under a login key (dot and
        // case differences must not hide it), a cookie of its own,
        // and one from another domain.
        let jar = vec![
            cr("token_v2", "ANON", "reddit.com", Some(now() + 999)),
            cr("loid", "anon-id", ".reddit.com", Some(now() + 999)),
            cr("sid", "other", ".example.com", Some(now() + 999)),
        ];
        let replay = vault_over_jar(&sessions, &jar);
        assert!(
            replay
                .iter()
                .any(|c| c.name == "token_v2" && c.value == "LOGIN"),
            "the login value must survive"
        );
        assert!(
            replay.iter().all(|c| c.value != "ANON"),
            "the anonymous echo must not shadow the login"
        );
        assert!(replay.iter().any(|c| c.name == "loid"), "jar-unique stays");
        assert!(
            replay
                .iter()
                .any(|c| c.name == "sid" && c.domain == ".example.com"),
            "other domains are untouched"
        );
        // The vault rides last: a collision the filter cannot see
        // still resolves in its favor inside the jar store itself.
        assert_eq!(
            replay.last().map(|c| c.name.as_str()),
            Some("reddit_session")
        );
        assert_eq!(replay.len(), 4);
    }

    #[test]
    fn warm_ok_filters_tracking_cookies() {
        let mut s = GhostState::default();
        s.profiles.insert(
            "hard.com".into(),
            DomainProfile {
                needs_tier2: true,
                cookies: vec![cr("cf_clearance", "old", ".hard.com", Some(9999))],
                last_solved: 1000,
                ..Default::default()
            },
        );
        let refreshed = vec![
            cr("cf_clearance", "new", ".hard.com", Some(9999)),
            cr("_ga", "tracking", ".hard.com", None),
            cr("TDID", "tracking", ".hard.com", None),
        ];
        s.record_warm_ok("hard.com", &refreshed);
        let p = &s.profiles["hard.com"];
        assert_eq!(p.cookies.len(), 1); // only cf_clearance updated
        assert_eq!(p.cookies[0].value, "new");
    }
    #[test]
    fn reliability_human_refusal_is_scoped_and_never_stores_url_tokens() {
        let mut state = GhostState::default();
        let url = "https://example.com/blocked?token=OWNED-DO-NOT-STORE";
        state.record_human_wall("example.com", url, "direct", &state.session_stamp());
        assert!(state.human_wall_age("example.com", url, "direct").is_some());
        assert!(
            state
                .human_wall_age("example.com", "https://example.com/other", "direct")
                .is_none()
        );
        assert!(
            state
                .human_wall_age("example.com", url, "other-route")
                .is_none()
        );
        assert!(
            !serde_json::to_string(&state)
                .unwrap()
                .contains("OWNED-DO-NOT-STORE")
        );
        state.session_revision = Some("changed-login-cookie-identity".into());
        assert!(state.human_wall_age("example.com", url, "direct").is_none());
        state.record_human_wall("example.com", url, "direct", &state.session_stamp());
        state
            .profiles
            .get_mut("example.com")
            .unwrap()
            .human_wall
            .as_mut()
            .unwrap()
            .observed_at = now() + 100;
        assert!(state.human_wall_age("example.com", url, "direct").is_none());
        state
            .profiles
            .get_mut("example.com")
            .unwrap()
            .human_wall
            .as_mut()
            .unwrap()
            .observed_at = now() - 301;
        assert!(state.human_wall_age("example.com", url, "direct").is_none());
    }
}
