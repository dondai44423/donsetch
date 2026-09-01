//! Disk-backed self-improving fetch intelligence.
//!
//! Every fetch is both an action AND an observation. This store
//! learns from each outcome and routes the next fetch more
//! efficiently. The more you use DonSeTch, the less it escalates
//! to tier 2 — domains it has already solved get warm tier-1
//! with their clearance cookies injected, and cookies are kept
//! alive by write-back from successful warm fetches.
//!
//! Two persistent stores:
//!
//! - **Domain profiles**: per-host routing intelligence + cookie
//!   vault. `route_for(host)` decides cold / warm / skip-to-solve
//!   / recheck-cold. `record_*` methods observe outcomes.
//! - **Rendered pages**: SPA renders cached with a TTL.
//!
//! File: ~/.cache/donsetch/ghost-state.json (atomic writes).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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

/// Per-domain intelligence. Evolves with every fetch outcome.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DomainProfile {
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
    #[serde(default)]
    pub solve_count: u32,

    // === Wall signature ===
    #[serde(default)]
    pub wall_vendor: Option<String>,
    /// Known to need tier 2 (seen a challenge here).
    #[serde(default)]
    pub needs_tier2: bool,
    /// Last time we tried tier 1 cold here (for wall-removal recheck).
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
    /// routing is only offered after a verified replay — cookies
    /// that the vendor binds to the browser fingerprint never
    /// earn a doomed tier-1 roundtrip.
    #[serde(default)]
    pub replay_ok: bool,
}

/// How to route a fetch to this host.
#[derive(Debug)]
pub enum RouteDecision {
    /// No profile (first visit) or easy domain. Tier 1 cold.
    Cold,
    /// Known to need tier 2, cookies still fresh — inject them.
    Warm(Vec<CookieRecord>),
    /// Known to need tier 2, cookies stale, cold-check recent.
    /// Skip the doomed tier-1 round-trip — go straight to solve.
    SkipToSolve,
    /// Known to need tier 2, but hasn't been cold-checked in a
    /// while — try tier 1 cold. The wall may have been removed.
    RecheckCold,
}

#[derive(Default, Serialize, Deserialize)]
pub struct GhostState {
    #[serde(default)]
    pub profiles: HashMap<String, DomainProfile>,
    #[serde(default)]
    pub renders: HashMap<String, RenderCache>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RenderCache {
    pub html: String,
    pub at: u64,
}

// ────────────────────────── constants ──────────────────────────

/// Safety cap: never trust a solve older than this, even if the
/// cookie's stated expiry is further out. Covers server-side
/// invalidation (IP change, session revoke).
const TTL_CAP: u64 = 2 * 60 * 60; // 2 hours

/// If a domain is known to need tier 2, periodically try tier 1
/// cold anyway — the wall may have been removed.
const RECHECK_INTERVAL: u64 = 24 * 60 * 60; // 24 hours

/// SPA renders stale after 5 min.
const RENDER_TTL: u64 = 5 * 60;

/// Max render cache entries. Each stores full HTML — cap to
/// prevent unbounded growth. LRU eviction when full.
/// 20 entries × ~200KB avg = ~4MB max contribution to state file.
const RENDER_MAX: usize = 20;

/// Max HTML size to cache per render (200KB). Larger pages are
/// rendered but not persisted — they'd bloat the state file.
const RENDER_MAX_HTML: usize = 200_000;

// ────────────────────────── cookie filtering ──────────────────────────

/// Cookie name prefixes that bot-wall vendors use for clearance/
/// verification cookies. We only persist these — tracking cookies
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
pub(crate) fn is_session_worthy(c: &CookieRecord) -> bool {
    if c.domain.is_empty() || c.value.is_empty() {
        return false;
    }
    if c.value.len() > SESSION_VALUE_MAX {
        return false;
    }
    if is_clearance_cookie(&c.name) {
        return false;
    }
    let n = c.name.to_ascii_lowercase();
    if SESSION_JUNK_PREFIXES.iter().any(|p| n.starts_with(p))
        || SESSION_JUNK_EXACT.contains(&n.as_str())
    {
        return false;
    }
    if c.expires_at.is_none() {
        return true;
    }
    SESSION_AUTH_HINTS.iter().any(|k| n.contains(k))
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
            .find(|e| e.name == c.name && e.domain == c.domain)
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
pub(crate) fn store_session_cookies(cookies: &[CookieRecord]) {
    if cookies.is_empty() {
        return;
    }
    let mut state = GhostState::load();
    for c in cookies {
        if !is_session_worthy(c) {
            continue;
        }
        let p = state
            .profiles
            .entry(c.domain.trim_start_matches('.').to_string())
            .or_default();
        merge_session_cookies(&mut p.session_cookies, std::slice::from_ref(c));
    }
    // Global cap: if the vault exceeds SESSION_MAX, drop the
    // least-recently-solved domains' session vaults first
    // (deterministic: last_solved is the recency signal).
    let total: usize = state
        .profiles
        .values()
        .map(|p| p.session_cookies.len())
        .sum();
    if total > SESSION_MAX {
        let mut keys: Vec<(u64, String)> = state
            .profiles
            .iter()
            .filter(|(_, p)| !p.session_cookies.is_empty())
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
        }
    }
    state.save();
}

/// Everything currently vaulted, across all domains, for replay
/// into a fresh browser launch.
pub(crate) fn load_session_cookies() -> Vec<CookieRecord> {
    let state = GhostState::load();
    let mut out = Vec::new();
    for p in state.profiles.values() {
        for c in &p.session_cookies {
            if is_session_worthy(c) {
                out.push(c.clone());
            }
        }
    }
    out
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

    // Server-set expiry: the earliest-expiring cookie is the
    // weakest link — if it's past, the batch is stale.
    if let Some(exp) = profile.cookies.iter().filter_map(|c| c.expires_at).min()
        && now >= exp
    {
        return false;
    }

    // Observed lifetime: if cookies died before their stated
    // expiry in the past, trust the observation over the server.
    if let Some(observed) = profile.observed_lifetime
        && now - profile.last_solved >= observed
    {
        return false;
    }

    // Safety cap.
    if now - profile.last_solved >= TTL_CAP {
        return false;
    }

    true
}

// ────────────────────────── GhostState impl ──────────────────────────

impl GhostState {
    pub fn load() -> Self {
        let p = path();
        if let Ok(s) = std::fs::read_to_string(&p) {
            // Try new format.
            if let Ok(mut state) = serde_json::from_str::<Self>(&s) {
                // One-time migration: prune tracking cookies from
                // existing profiles. Pre-filter versions stored ALL
                // cookies (_ga, TDID, demdex, etc.) — some domains
                // had 1000+ cookies, making the state file 100MB+.
                // After pruning, only clearance cookies remain.
                let mut changed = false;
                for profile in state.profiles.values_mut() {
                    let before = profile.cookies.len();
                    profile.cookies = filter_clearance(&profile.cookies);
                    if profile.cookies.len() != before {
                        changed = true;
                    }
                }
                // The session vault's own migration: junk from
                // earlier filter versions fades out of old state
                // files on load instead of lingering forever.
                for profile in state.profiles.values_mut() {
                    let before = profile.session_cookies.len();
                    profile.session_cookies.retain(is_session_worthy);
                    if profile.session_cookies.len() != before {
                        changed = true;
                    }
                }
                // One-time un-poisoning (v2.2): pre-v2.2 code marked
                // domains needs_tier2 on ANY non-content verdict —
                // a single 404 or rate-limit forced ghost on every
                // later fetch. Profiles that never recorded an
                // actual solve carry no wall knowledge: reset them
                // so they get a fresh cold tier-1 chance.
                for profile in state.profiles.values_mut() {
                    if profile.needs_tier2 && profile.solve_count == 0 && profile.cookies.is_empty()
                    {
                        profile.needs_tier2 = false;
                        profile.wall_vendor = None;
                        changed = true;
                    }
                    // Pre-v2.2 warm-stale learning clamped lifetimes
                    // to as low as 1s (single-failure, unfloored).
                    // Those observations are garbage — drop them.
                    if profile.observed_lifetime.is_some_and(|o| o < 120) {
                        profile.observed_lifetime = None;
                        changed = true;
                    }
                }
                // Cap renders to RENDER_MAX (old state files may
                // have hundreds of cached renders).
                while state.renders.len() > RENDER_MAX {
                    if let Some(oldest_key) = state
                        .renders
                        .iter()
                        .min_by_key(|(_, r)| r.at)
                        .map(|(k, _)| k.clone())
                    {
                        state.renders.remove(&oldest_key);
                    }
                    changed = true;
                }
                // Prune oversized renders (old files may have
                // cached >200KB pages).
                let before_renders = state.renders.len();
                state.renders.retain(|_, r| r.html.len() <= RENDER_MAX_HTML);
                if state.renders.len() != before_renders {
                    changed = true;
                }
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
                state.renders = old.renders;
                state.save();
                return state;
            }
        }
        Self::default()
    }

    /// Atomic save: write to temp, rename. Survives crashes.
    /// No-op in test builds — tests exercise the pure decision
    /// and freshness logic without disk side effects.
    /// No-op when DONSEEK_NO_DISK_STATE is set — keeps in-memory
    /// state for the session but doesn't persist to disk.
    pub fn save(&self) {
        #[cfg(not(test))]
        {
            // Allow users to disable disk persistence entirely.
            // In-memory state still works during the session.
            if std::env::var_os("DONSEEK_NO_DISK_STATE").is_some() {
                return;
            }
            let p = path();
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(s) = serde_json::to_string(self) {
                let tmp = p.with_extension("json.tmp");
                // 0600 BEFORE content lands on disk: the state file
                // carries harvested cookies (clearance / session
                // identifiers) and must not be world-readable, even
                // transiently on the tmp file.
                let write_ok = {
                    #[cfg(unix)]
                    {
                        use std::io::Write;
                        use std::os::unix::fs::OpenOptionsExt;
                        std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .mode(0o600)
                            .open(&tmp)
                            .and_then(|mut f| f.write_all(s.as_bytes()))
                            .is_ok()
                    }
                    #[cfg(not(unix))]
                    {
                        std::fs::write(&tmp, &s).is_ok()
                    }
                };
                if write_ok && let Err(e) = std::fs::rename(&tmp, &p) {
                    // e.g. antivirus lock on Windows: harvested
                    // clearance cookies are lost this session — say
                    // so instead of silently re-burning ghost solves.
                    eprintln!("[ghost] cookie vault persist failed: {e}");
                }
            }
        }
    }

    // ── Decision ──

    /// Route decision for the next fetch to this host.
    /// Is this host known to need tier 2 (seen a challenge)?
    /// Powers the v3 anti-cloak trigger: a walled domain that
    /// suddenly passes tier-1 cold is suspicious.
    pub fn is_known_walled(&self, host: &str) -> bool {
        self.profiles.get(host).is_some_and(|p| p.needs_tier2)
    }

    pub fn route_for(&self, host: &str) -> RouteDecision {
        let Some(profile) = self.profiles.get(host) else {
            return RouteDecision::Cold;
        };
        let n = now();
        if profile.needs_tier2 {
            // Warm only when cookies are fresh AND tier-1 replay has
            // actually been verified to work for this domain. Vendors
            // that bind clearance to the browser fingerprint reject
            // tier-1 replay forever — serving Warm there just burns a
            // doomed roundtrip before every solve.
            if cookies_fresh_at(profile, n) && profile.replay_ok {
                return RouteDecision::Warm(profile.cookies.clone());
            }
            // Cookies stale. Should we recheck cold?
            if n - profile.last_cold_check > RECHECK_INTERVAL {
                return RouteDecision::RecheckCold;
            }
            // Skip the doomed tier-1 attempt — go straight to solve.
            return RouteDecision::SkipToSolve;
        }
        // Easy domain — tier 1 cold.
        RouteDecision::Cold
    }

    // ── Observation ──

    /// A fetch completed with a non-challenge, non-content verdict
    /// (404, rate-limit, paywall, auth wall). These say nothing
    /// about walls or cookies — they must not poison the route.
    /// The counters move and the cold-check clock restarts (this
    /// WAS a tier-1 answer; the 24h recheck cadence follows it).
    pub fn record_fetch(&mut self, host: &str) {
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.fetch_count += 1;
        p.last_cold_check = n;
        self.save();
    }

    /// Tier 1 cold succeeded. If the domain was previously known
    /// to need tier 2, the wall is gone — clear the flag.
    pub fn record_cold_ok(&mut self, host: &str) {
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.fetch_count += 1;
        p.last_cold_check = n;
        p.warm_fail_streak = 0;
        if p.needs_tier2 {
            p.needs_tier2 = false;
            p.wall_vendor = None;
            // Cookies from a previous solve are stale context — clear.
            p.cookies.clear();
            p.observed_lifetime = None;
            p.replay_ok = false;
        }
        self.save();
    }

    /// Tier 1 cold was walled — domain needs tier 2.
    /// (Callers: ONLY on an actual Challenge verdict. A 404 or a
    /// rate-limit is not a wall — it must not force ghost mode.)
    pub fn record_cold_walled(&mut self, host: &str, vendor: Option<&str>) {
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.fetch_count += 1;
        p.walled_count += 1;
        p.needs_tier2 = true;
        p.last_cold_check = n;
        if let Some(v) = vendor {
            p.wall_vendor = Some(v.to_string());
        }
        self.save();
    }

    /// Tier 1 warm succeeded — cookies are still valid. Refresh
    /// the cookie vault from the response's Set-Cookie headers
    /// so the on-disk cookies stay as fresh as the server's latest
    /// response. Only clearance cookies are merged — tracking
    /// cookies are filtered out to keep the state file compact.
    pub fn record_warm_ok(&mut self, host: &str, refreshed: &[CookieRecord]) {
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.fetch_count += 1;
        p.warm_ok_count += 1;
        p.warm_fail_streak = 0;
        p.last_refreshed = n;
        // Merge: replace by (name, domain), add new ones — but only
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
        self.save();
    }

    /// Tier 1 warm was walled — cookies went stale. Learn the
    /// real lifetime: it's at most `now - last_solved`. Next
    /// time, trust the observation over the server's claim and
    /// re-solve before the cookies expire.
    ///
    /// Dampened: the FIRST warm failure keeps the cookies (vendor
    /// challenges rotate; one wall is often transient — the next
    /// warm fetch gets to prove the cookies still live). Only a
    /// SECOND consecutive failure clears the vault and learns the
    /// lifetime. The learned lifetime is floored at 120s: a fluke
    /// wall one second after a solve must never clamp the domain
    /// to permanent skip-to-solve (the stackoverflow bug).
    pub fn record_warm_stale(&mut self, host: &str) {
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.fetch_count += 1;
        p.warm_fail_count += 1;
        p.warm_fail_streak += 1;
        if p.warm_fail_streak >= 2 {
            let elapsed = n.saturating_sub(p.last_solved).max(120);
            p.observed_lifetime = Some(match p.observed_lifetime {
                Some(prev) => prev.max(120).min(elapsed),
                None => elapsed,
            });
            // Cookies are dead — clear so route_for doesn't serve them.
            p.cookies.clear();
            p.last_refreshed = 0;
            p.replay_ok = false;
        }
        self.save();
    }

    /// Tier 2 solved the wall — store fresh cookies with real
    /// expiry captured from CDP. Only clearance cookies are kept —
    /// tracking cookies bloat the state file with no benefit.
    ///
    /// `replay_ok` records whether tier-1 replay of these cookies
    /// was VERIFIED (post-solve tier-1 fetch came back with real
    /// content). Warm routing is gated on it — see route_for.
    pub fn record_solved(
        &mut self,
        host: &str,
        cookies: &[CookieRecord],
        vendor: Option<&str>,
        replay_ok: bool,
    ) {
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.cookies = filter_clearance(cookies);
        p.last_solved = n;
        p.last_refreshed = n;
        p.solve_count += 1;
        p.needs_tier2 = true;
        p.warm_fail_streak = 0;
        p.replay_ok = replay_ok;
        // A fresh solve restarts the lifetime observation window —
        // stale pre-fix lifetimes (the 1-second clamp bug) must
        // not outlive the solve that invalidated them.
        if p.observed_lifetime.is_some_and(|o| o < 120) {
            p.observed_lifetime = None;
        }
        if let Some(v) = vendor {
            p.wall_vendor = Some(v.to_string());
        }
        self.save();
    }

    // ── Render cache (unchanged from v1) ──

    pub fn record_render(&mut self, url: &str, html: &str) {
        // Skip oversized pages — caching 1MB+ HTML bloats the state
        // file with no benefit (large pages are usually not SPAs
        // that need render caching).
        if html.len() > RENDER_MAX_HTML {
            return;
        }
        // LRU cap: evict oldest renders when at capacity.
        if self.renders.len() >= RENDER_MAX
            && let Some(oldest_key) = self
                .renders
                .iter()
                .min_by_key(|(_, r)| r.at)
                .map(|(k, _)| k.clone())
        {
            self.renders.remove(&oldest_key);
        }
        self.renders.insert(
            url.to_string(),
            RenderCache {
                html: html.to_string(),
                at: now(),
            },
        );
        self.save();
    }

    pub fn render_for(&self, url: &str) -> Option<&RenderCache> {
        self.renders.get(url).filter(|r| now() - r.at < RENDER_TTL)
    }
}

// ────────────────────────── legacy migration ──────────────────────────

#[derive(Deserialize)]
struct LegacyState {
    #[serde(default)]
    solved: HashMap<String, LegacySolved>,
    #[serde(default)]
    renders: HashMap<String, RenderCache>,
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
        // No server expiry, no observed lifetime — cap at 2h.
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
                last_cold_check: 1, // very old — > RECHECK_INTERVAL
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
    fn solved_stores_cookies_and_vendor() {
        let mut s = GhostState::default();
        let cookies = vec![cr("cf_clearance", "tok", ".hard.com", Some(now() + 3600))];
        s.record_solved("hard.com", &cookies, Some("cloudflare"), true);
        let p = &s.profiles["hard.com"];
        assert!(p.needs_tier2);
        assert_eq!(p.solve_count, 1);
        assert_eq!(p.wall_vendor.as_deref(), Some("cloudflare"));
        assert_eq!(p.cookies.len(), 1);
        assert!(!p.cookies.is_empty());
        assert!(p.replay_ok);
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
        let cookies = vec![cr("cf_clearance", "x", ".strict.com", Some(now() + 3600))];
        s.record_solved("strict.com", &cookies, Some("cloudflare"), true);
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
        s.record_solved(
            "hard.com",
            &[cr("cf_clearance", "y", ".hard.com", Some(9999))],
            None,
            true,
        );
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
        assert!(learned >= 120, "got {learned} — must be floored");
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

        // Visit 1: unknown → cold → walled → solve (replay verified)
        assert!(matches!(s.route_for(host), RouteDecision::Cold));
        s.record_cold_walled(host, Some("cloudflare"));
        let cookies = vec![cr(
            "cf_clearance",
            "tok1",
            ".cf-protected.com",
            Some(now + 3600),
        )];
        s.record_solved(host, &cookies, Some("cloudflare"), true);

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
        assert_eq!(p.solve_count, 1);
        assert_eq!(p.walled_count, 1);
        assert!(p.needs_tier2);
    }

    // ── legacy migration ──

    #[test]
    fn legacy_migration() {
        // Verify LegacyState deserialization — the production
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
    fn solved_stores_only_clearance() {
        let mut s = GhostState::default();
        let cookies = vec![
            cr("cf_clearance", "tok", ".a.com", Some(now() + 3600)),
            cr("_ga", "tracking", ".a.com", None),
            cr("datadome", "val", ".a.com", Some(now() + 3600)),
        ];
        s.record_solved("a.com", &cookies, Some("cloudflare"), true);
        let p = &s.profiles["a.com"];
        assert_eq!(p.cookies.len(), 2); // cf_clearance + datadome only
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
}
