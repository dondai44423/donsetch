//! Disk-backed self-improving fetch intelligence.
//!
//! Every fetch is both an action AND an observation. This store
//! learns from each outcome and routes the next fetch more
//! efficiently. The more you use DonSeTch, the less it escalates
//! to tier 2 : domains it has already solved get warm tier-1
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
    /// EWMA of ghost (tier-2/solve) success.
    #[serde(default = "default_ewma")]
    pub ghost_ewma: f32,
    #[serde(default)]
    pub ghost_samples: u32,
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
const STATE_VERSION: u32 = 2;

impl DomainProfile {
    /// Most recent signal of any kind; the LRU eviction key.
    fn last_activity(&self) -> u64 {
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
    /// Lifetime count of web_answer evidence packs served with at
    /// least one usable source (v4 phase 2.2), surfaced in
    /// donsetch status. Empty packs (nothing usable found) are not
    /// counted: a served pack means real evidence reached the agent.
    #[serde(default)]
    pub answered_packs_total: u64,
    /// v4 phase 3 ghost pool visibility: how many ghost acquires were
    /// served by an already-live pooled browser (thaw-and-go), as
    /// opposed to paying a browser launch. Sums across the daemon's
    /// lifetime; zero cold-starts are not counted. Surfaced in
    /// donsetch status as the pool's warm-reuse receipt.
    #[serde(default)]
    pub pool_served_total: u64,
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
    #[serde(default)]
    pub renders: HashMap<String, RenderCache>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RenderCache {
    pub html: String,
    pub at: u64,
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
    !crate::config::env_flag("DONSETCH_NO_ROUTE_MEMORY")
}

fn route_memory_readonly() -> bool {
    crate::config::env_flag("DONSETCH_ROUTE_MEMORY_READONLY")
}

// ────────────────────────── constants ──────────────────────────

/// Safety cap: never trust a solve older than this, even if the
/// cookie's stated expiry is further out. Covers server-side
/// invalidation (IP change, session revoke).
const TTL_CAP: u64 = 2 * 60 * 60; // 2 hours

/// If a domain is known to need tier 2, periodically try tier 1
/// cold anyway : the wall may have been removed.
const RECHECK_INTERVAL: u64 = 24 * 60 * 60; // 24 hours

/// SPA renders stale after 5 min.
const RENDER_TTL: u64 = 5 * 60;

/// Solve-cooldown backoff: 15 min base, doubling per consecutive
/// wall-persisted failure, hard-capped at 2 hours. A permanently
/// walled domain costs days of sub-second honest errors instead of
/// a full browser cycle per fetch.
fn solve_cooldown_secs(streak: u32) -> u64 {
    let base: u64 = 15 * 60;
    let shifts = streak.saturating_sub(2).min(3);
    base << shifts
}

/// Max render cache entries. Each stores full HTML : cap to
/// prevent unbounded growth. LRU eviction when full.
/// 20 entries × ~200KB avg = ~4MB max contribution to state file.
const RENDER_MAX: usize = 20;

/// Max HTML size to cache per render (200KB). Larger pages are
/// rendered but not persisted : they'd bloat the state file.
const RENDER_MAX_HTML: usize = 200_000;

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
pub fn store_session_cookies(cookies: &[CookieRecord]) {
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
pub fn load_session_cookies() -> Vec<CookieRecord> {
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

/// Logout helper: drop every vaulted cookie whose domain belongs
/// to `domain` (apex + subdomains + host-only subdomain cookies
/// that feed the same session). Returns true if anything was
/// removed. Load-modify-save, atomic, same as the harvest path.
pub fn clear_session_cookies_for(domain: &str) -> bool {
    let key = domain.trim_start_matches('.').to_ascii_lowercase();
    if key.is_empty() {
        return false;
    }
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
    // Issue #173 (deeper pass): the rendered-DOM cache is a fourth copy
    // of the session's fruit. A page fetched behind the session is
    // stored whole in `renders` (RENDER_TTL = 5 min) and served by the
    // tier-2 fetch shortcut, so a logged-out domain's dashboard would
    // still hand back its authenticated DOM for up to five minutes.
    // Drop every render whose host belongs to the logged-out domain.
    let before_r = state.renders.len();
    state.renders.retain(|url, _| {
        let host = crate::search::rank::host_of(url);
        !crate::auth::cookie_belongs_to(&key, &host)
    });
    removed |= state.renders.len() != before_r;
    if removed {
        state.save();
    }
    removed
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
    // weakest link : if it's past, the batch is stale.
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
                // domains needs_tier2 on ANY non-content verdict :
                // a single 404 or rate-limit forced ghost on every
                // later fetch. Profiles that never recorded an
                // actual solve carry no wall knowledge: reset them
                // so they get a fresh cold tier-1 chance.
                // Gated on the state version (v4 phase 0): ungated,
                // this migration ran on EVERY load and wiped the
                // memory of legitimately-walled-but-never-solved
                // hosts between CLI invocations and daemon restarts
                // (live-caught on the wall rig: needs_tier2 vanished
                // between two CLI runs). Route amnesia, now ended.
                if state.version < 2 {
                    for profile in state.profiles.values_mut() {
                        if profile.needs_tier2
                            && profile.solve_count == 0
                            && profile.cookies.is_empty()
                        {
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
    /// No-op in test builds : tests exercise the pure decision
    /// and freshness logic without disk side effects.
    /// No-op when DONSEEK_NO_DISK_STATE is set : keeps in-memory
    /// state for the session but doesn't persist to disk.
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
    /// evidence older than `stale_secs` qualify.
    pub fn probe_candidates(&self, stale_secs: u64, limit: usize) -> Vec<String> {
        let n = now();
        let mut stale: Vec<(&String, u64)> = self
            .profiles
            .iter()
            .filter(|(_, p)| p.needs_tier2 && n.saturating_sub(p.last_cold_check) > stale_secs)
            .map(|(h, p)| (h, p.last_cold_check))
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

    /// v4 phase 2.2 visibility: count an evidence pack served with
    /// at least one usable source.
    pub fn note_answer_served(&mut self) {
        self.answered_packs_total = self.answered_packs_total.saturating_add(1);
        self.save();
    }

    /// v4 phase 3 visibility: count a fetch served from an
    /// already-warm pool browser (no launch, no thaw failure). With
    /// DONSETCH_NO_GHOST_POOL set, the pool is one slot but warm
    /// reuse inside that slot still counts: the receipt lives with
    /// the behavior.
    pub fn note_pool_served(&mut self) {
        self.pool_served_total = self.pool_served_total.saturating_add(1);
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
    /// generation; the burned one is never reused.
    pub fn quarantine_persona(&mut self, host: &str, reason: &str) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        if let Some(p) = self.personas.get_mut(host) {
            p.quarantined_at = Some(now());
            p.quarantine_reason = Some(reason.to_string());
            self.save();
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
    /// (name, domain, path), refresh in place at the front (cap
    /// evicts the coldest tail), drop expired and pathological
    /// values. Does NOT save: the caller is already inside the
    /// state lock and the outcome record after it saves once.
    pub fn sync_tier1_cookies(&mut self, all: &[CookieRecord]) {
        if crate::config::env_flag("DONSETCH_NO_COOKIE_VAULT")
            || crate::config::env_flag("DONSETCH_NO_ROUTE_MEMORY")
        {
            return;
        }
        let now = now();
        for c in all {
            if c.domain.is_empty()
                || c.value.is_empty()
                || c.value.len() > TIER1_COOKIE_VALUE_MAX
                || c.expires_at.is_some_and(|e| e <= now)
            {
                continue;
            }
            let key = (c.name.clone(), c.domain.clone(), c.path.clone());
            if let Some(pos) = self
                .tier1_cookies
                .iter()
                .position(|x| (x.name.clone(), x.domain.clone(), x.path.clone()) == key)
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
        !crate::config::env_flag("DONSETCH_NO_COOKIE_VAULT") as usize * self.tier1_cookies.len()
    }

    pub fn save(&mut self) {
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
            if std::env::var_os("DONSEEK_NO_DISK_STATE").is_some() {
                return;
            }
            let p = path();
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Merge monotonic counters against the disk state as it
            // exists RIGHT NOW: independent load() snapshots race
            // (a stale in-memory copy saving later would otherwise
            // rewind lifetimes). Max-merge is correct for counters:
            // they only grow. Real case: the pool-warm receipt.
            if let Ok(bytes) = std::fs::read(&p)
                && let Ok(disk) = serde_json::from_slice::<GhostState>(&bytes)
            {
                self.probes_total = self.probes_total.max(disk.probes_total);
                self.shadowed_assets_total =
                    self.shadowed_assets_total.max(disk.shadowed_assets_total);
                self.prewarmed_served_total =
                    self.prewarmed_served_total.max(disk.prewarmed_served_total);
                self.answered_packs_total =
                    self.answered_packs_total.max(disk.answered_packs_total);
                self.pool_served_total = self.pool_served_total.max(disk.pool_served_total);
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
                    // clearance cookies are lost this session : say
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
        if !route_memory_enabled() {
            return RouteDecision::Cold;
        }
        let Some(profile) = self.profiles.get(host) else {
            return RouteDecision::Cold;
        };
        let n = now();
        // Solve-cooldown comes FIRST: a domain whose wall survived
        // real-browser passes recently fails fast until the cooldown
        // lapses. Exponential backoff per failure streak, capped at
        // 2 hours, so a permanently-walled domain costs an honest
        // sub-second error instead of a 20-40s browser cycle per
        // fetch.
        if profile.wall_fail_streak >= 2 {
            let cooldown = solve_cooldown_secs(profile.wall_fail_streak);
            if profile.last_wall_fail > 0 && n.saturating_sub(profile.last_wall_fail) < cooldown {
                return RouteDecision::SolveCooldown(
                    cooldown.saturating_sub(n.saturating_sub(profile.last_wall_fail)),
                );
            }
        }
        if profile.needs_tier2 {
            // Warm only when cookies are fresh AND tier-1 replay has
            // actually been verified to work for this domain. Vendors
            // that bind clearance to the browser fingerprint reject
            // tier-1 replay forever : serving Warm there just burns a
            // doomed roundtrip before every solve.
            if cookies_fresh_at(profile, n) && profile.replay_ok {
                return RouteDecision::Warm(profile.cookies.clone());
            }
            // Cookies stale. Should we recheck cold?
            if n - profile.last_cold_check > RECHECK_INTERVAL {
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
        p.fetch_count += 1;
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

    /// Tier 1 cold succeeded. If the domain was previously known
    /// to need tier 2, the wall is gone : clear the flag.
    pub fn record_cold_ok(&mut self, host: &str) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.t1_ewma = ewma_update(p.t1_ewma, p.t1_samples, true);
        p.t1_samples = p.t1_samples.saturating_add(1);
        p.fetch_count += 1;
        p.last_cold_check = n;
        p.warm_fail_streak = 0;
        // Tier 1 got real content : whatever wall state existed
        // is irrelevant now. Clear the wall + cooldown memory.
        p.wall_fail_streak = 0;
        p.last_wall_fail = 0;
        if p.needs_tier2 {
            p.needs_tier2 = false;
            p.wall_vendor = None;
            // Cookies from a previous solve are stale context : clear.
            p.cookies.clear();
            p.observed_lifetime = None;
            p.replay_ok = false;
        }
        self.save();
    }

    /// Tier 1 cold was walled : domain needs tier 2.
    /// (Callers: ONLY on an actual Challenge verdict. A 404 or a
    /// rate-limit is not a wall : it must not force ghost mode.)
    pub fn record_cold_walled(&mut self, host: &str, vendor: Option<&str>) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.t1_ewma = ewma_update(p.t1_ewma, p.t1_samples, false);
        p.t1_samples = p.t1_samples.saturating_add(1);
        p.fetch_count += 1;
        p.walled_count += 1;
        p.needs_tier2 = true;
        p.last_cold_check = n;
        if let Some(v) = vendor {
            p.wall_vendor = Some(v.to_string());
        }
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
        p.fetch_count += 1;
        p.warm_ok_count += 1;
        p.warm_fail_streak = 0;
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
        self.save();
    }

    /// Tier 1 warm was walled : cookies went stale. Learn the
    /// real lifetime: it's at most `now - last_solved`. Next
    /// time, trust the observation over the server's claim and
    /// re-solve before the cookies expire.
    ///
    /// Dampened: the FIRST warm failure keeps the cookies (vendor
    /// challenges rotate; one wall is often transient : the next
    /// warm fetch gets to prove the cookies still live). Only a
    /// SECOND consecutive failure clears the vault and learns the
    /// lifetime. The learned lifetime is floored at 120s: a fluke
    /// wall one second after a solve must never clamp the domain
    /// to permanent skip-to-solve (the stackoverflow bug).
    pub fn record_warm_stale(&mut self, host: &str) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.warm_ewma = ewma_update(p.warm_ewma, p.warm_samples, false);
        p.warm_samples = p.warm_samples.saturating_add(1);
        p.fetch_count += 1;
        p.warm_fail_count += 1;
        p.warm_fail_streak += 1;
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

    /// Tier 2 solved the wall : store fresh cookies with real
    /// expiry captured from CDP. Only clearance cookies are kept :
    /// tracking cookies bloat the state file with no benefit.
    ///
    /// `replay_ok` records whether tier-1 replay of these cookies
    /// was VERIFIED (post-solve tier-1 fetch came back with real
    /// content). Warm routing is gated on it : see route_for.
    pub fn record_solved(
        &mut self,
        host: &str,
        cookies: &[CookieRecord],
        vendor: Option<&str>,
        replay_ok: bool,
    ) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.ghost_ewma = ewma_update(p.ghost_ewma, p.ghost_samples, true);
        p.ghost_samples = p.ghost_samples.saturating_add(1);
        p.cookies = filter_clearance(cookies);
        p.last_solved = n;
        p.last_refreshed = n;
        p.solve_count += 1;
        p.needs_tier2 = true;
        p.warm_fail_streak = 0;
        p.replay_ok = replay_ok;
        // A SOLVED wall clears the failure memory: the browser got
        // through, so the environment can solve this domain today.
        p.wall_fail_streak = 0;
        p.last_wall_fail = 0;
        // A fresh solve restarts the lifetime observation window :
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

    /// Ghost pass ended walled: the challenge persisted even in a
    /// REAL browser. Consecutive failures drive the solve-cooldown.
    pub fn record_wall_failed(&mut self, host: &str) {
        if !route_memory_enabled() || route_memory_readonly() {
            return;
        }
        let n = now();
        let p = self.profiles.entry(host.to_string()).or_default();
        p.ghost_ewma = ewma_update(p.ghost_ewma, p.ghost_samples, false);
        p.ghost_samples = p.ghost_samples.saturating_add(1);
        p.wall_fail_streak = p.wall_fail_streak.saturating_add(1);
        p.last_wall_fail = n;
        // A wall that survives a real browser tells nothing about
        // cookies: keep whatever solve state existed. Only the
        // cooldown memory updates.
        self.save();
    }

    pub fn record_render(&mut self, url: &str, html: &str) {
        // Skip oversized pages : caching 1MB+ HTML bloats the state
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
        let p = state.profiles.get(host).unwrap();
        assert!(p.t1_ewma < EWMA_BLOCK_THRESHOLD, "ewma {}", p.t1_ewma);
        // Clear the boolean flag to prove the EWMA consult, not the
        // flag, drives the decision.
        state.profiles.get_mut(host).unwrap().needs_tier2 = false;
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
            solve_count: 0,
            ..Default::default()
        };
        state.profiles.insert("walled.example".into(), walled);
        // Simulate the v1 migration pass (the gated block in load):
        if state.version < 2 {
            for profile in state.profiles.values_mut() {
                if profile.needs_tier2 && profile.solve_count == 0 && profile.cookies.is_empty() {
                    profile.needs_tier2 = false;
                    profile.wall_vendor = None;
                }
            }
        }
        state.version = STATE_VERSION;
        assert!(!state.profiles["walled.example"].needs_tier2);
        assert_eq!(state.version, 2);

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
    fn note_probe_counts_and_respects_switches() {
        let mut state = GhostState::default();
        state.note_probe();
        state.note_probe();
        assert_eq!(state.probes_total, 2);
        unsafe { std::env::set_var("DONSETCH_ROUTE_MEMORY_READONLY", "1") };
        state.note_probe();
        assert_eq!(state.probes_total, 2);
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
        assert_eq!(p.ghost_ewma, 1.0);
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
        s.record_wall_failed("poison.com");
        s.record_wall_failed("poison.com");
        // Inside the cooldown: fail fast, no browser cycle.
        assert!(matches!(
            s.route_for("poison.com"),
            RouteDecision::SolveCooldown(_)
        ));
        // Cooldown lapsed: falls back to normal routing (cold here:
        // no other memory on this fresh profile).
        let p = s.profiles.get_mut("poison.com").unwrap();
        p.last_wall_fail = now() - 16 * 60;
        assert!(matches!(s.route_for("poison.com"), RouteDecision::Cold));
    }

    #[test]
    fn solved_wall_clears_cooldown_memory() {
        let mut s = GhostState::default();
        s.record_wall_failed("heals.com");
        s.record_wall_failed("heals.com");
        s.record_solved(
            "heals.com",
            &[cr("cf_clearance", "x", ".heals.com", Some(9_999_999_999))],
            Some("Cloudflare"),
            true,
        );
        let p = &s.profiles["heals.com"];
        assert_eq!(p.wall_fail_streak, 0);
        assert_eq!(p.last_wall_fail, 0);
        assert!(matches!(s.route_for("heals.com"), RouteDecision::Warm(_)));
    }

    #[test]
    fn cold_ok_clears_cooldown_memory() {
        let mut s = GhostState::default();
        s.record_wall_failed("eases.com");
        s.record_wall_failed("eases.com");
        s.record_cold_ok("eases.com");
        let p = &s.profiles["eases.com"];
        assert_eq!(p.wall_fail_streak, 0);
        assert!(!p.needs_tier2);
    }

    #[test]
    fn one_wall_fail_is_not_a_cooldown() {
        let mut s = GhostState::default();
        s.record_wall_failed("once.com");
        assert!(
            !matches!(s.route_for("once.com"), RouteDecision::SolveCooldown(_)),
            "a single failure must not gate the domain"
        );
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
        // Kill switch blocks the sync.
        unsafe { std::env::set_var("DONSETCH_NO_COOKIE_VAULT", "1") };
        st.sync_tier1_cookies(&[rec("post", "v", ".x.com", None)]);
        assert!(st.tier1_cookies.iter().all(|c| c.name != "post"));
        unsafe { std::env::remove_var("DONSETCH_NO_COOKIE_VAULT") };
        // Roundtrip through serde (persistence shape).
        let json = serde_json::to_string(&st).unwrap();
        let back: GhostState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tier1_cookies.len(), TIER1_COOKIE_MAX);
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
