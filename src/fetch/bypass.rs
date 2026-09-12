//! Anti-bot bypass fetch (v3.4): when the local stack hits a hard
//! wall, hand the URL to Bright Data's Web Unlocker API, which solves
//! the anti-bot challenge server-side and returns the rendered HTML.

//! This is the paid upgrade path for advanced users. It is strictly
//! opt-in: configure a key via `donsetch keys add unlocker <key>`,
//! optionally with a `::zone` suffix (default zone: `web_unlocker1`).
//! With no key, this module is inert: behavior is identical to
//! previous releases.

//! Billing: Bright Data bills only successful unlocks (standard zone
//! mode). Failures are free. Guardrails: daily cap, hard timeout,
//! and an explicit off switch : so no silent spend.

//! Env:
//!   DONSETCH_BYPASS=0                      disable bypass entirely
//!   DONSETCH_BYPASS_MAX_DAILY=<n>           max unlock calls per day (default 50)
//!   DONSETCH_BYPASS_TIMEOUT_SECS=<n>        per-request timeout (default 90)
//!   DONSETCH_BYPASS_RENDER=1               force JS render via unlocker browser
//!   DONSETCH_UNLOCKER_ZONE=<zone>           default zone when key has no ::zone
//!   DONSETCH_BYPASS_ENDPOINT=<url>          test hook: override API endpoint
//!   DONSETCH_BYPASS_CACHE_TTL_SECS=<n>      solve-cache TTL (default 21600, 0 disables)
//!   DONSETCH_BYPASS_CACHE_MAX_ENTRIES=<n>   solve-cache size cap (default 200)
//!   DONSETCH_BYPASS_CACHE=0                disable the solve-cache

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::search::byok::store::{ByokConfig, KeyState};
use base64::Engine;

pub const DEFAULT_ZONE: &str = "web_unlocker1";
const PROD_ENDPOINT: &str = "https://api.brightdata.com/request";

/// Parsed runtime config. All values come from env at call time.
pub struct BypassConfig {
    pub enabled: bool,
    pub max_daily: u32,
    pub timeout: Duration,
    pub render: bool,
    pub endpoint: String,
    pub cache_ttl: Duration,
    pub cache_max: u32,
}

impl Default for BypassConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_daily: 50,
            // Bright Data documents unlock wait times up to 150s
            // (expect_element etc.); 120s sits inside that window
            // without renting the request slot for the maximum.
            timeout: Duration::from_secs(120),
            render: false,
            endpoint: PROD_ENDPOINT.to_string(),
            cache_ttl: Duration::from_secs(21_600),
            cache_max: 200,
        }
    }
}

impl BypassConfig {
    pub fn from_env() -> Self {
        // Historically this read nine env vars directly; the knobs now
        // live in the layered config ([bypass] section). The clamps are
        // defense-in-depth: config validation already fails loudly on
        // out-of-range values.
        let b = &crate::config::cfg().bypass;
        Self {
            enabled: b.enabled,
            max_daily: b.max_daily.clamp(1, 10_000),
            timeout: Duration::from_secs(b.timeout_secs.clamp(5, 600)),
            render: b.render,
            endpoint: if b.endpoint.trim().is_empty() {
                PROD_ENDPOINT.to_string()
            } else {
                b.endpoint.trim().to_string()
            },
            cache_ttl: if !b.cache {
                Duration::ZERO
            } else {
                Duration::from_secs(b.cache_ttl_secs)
            },
            cache_max: b.cache_max_entries.clamp(1, 100_000),
        }
    }
}

/// Split a stored key into (api_token, zone). Zone may be
/// embedded as `token::zone`; otherwise the env default, then the
/// product default, applies. Empty token or zone is a config
/// error, not a network call : it would only cost the user a
/// confusing API rejection.
pub fn parse_key(raw: &str, default_zone: &str) -> Result<(String, String), BypassFail> {
    if raw.trim().is_empty() {
        return Err(BypassFail::Config(
            "unlocker key is empty : run `donsetch keys add unlocker <token>[::zone]`".to_string(),
        ));
    }
    if let Some((token, zone)) = raw.split_once("::") {
        if token.trim().is_empty() {
            return Err(BypassFail::Config(
                "unlocker key has an empty token before `::` : re-add the key".to_string(),
            ));
        }
        if zone.trim().is_empty() {
            return Err(BypassFail::Config(format!(
                "unlocker key `{token}::` has an empty zone : use `{token}::{default_zone}` or drop the `::` suffix"
            )));
        }
        return Ok((token.to_string(), zone.to_string()));
    }
    let zone = {
        let configured = crate::config::cfg().bypass.zone.trim().to_string();
        if configured.is_empty() {
            default_zone.to_string()
        } else {
            configured
        }
    };
    Ok((raw.trim().to_string(), zone))
}

/// UTC YYYYMMDD, civil-from-days (no date dep).
fn date_ymd() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y2 = if m <= 2 { y + 1 } else { y };
    format!("{y2:04}{m:02}{d:02}")
}

/// Path of the daily counter file (one per UTC day).
pub fn bypass_count_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(format!("bypass-{}.count", date_ymd()))
}

/// Check the daily cap and bump the counter. Returns false when
/// the cap is already exhausted. Counter files older than 31 days
/// are pruned: they are single integers, but a long-lived machine
/// needs no permanent litter.
pub fn check_and_bump_daily(path: &Path, max: u32) -> bool {
    let count = if path.exists() {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0)
    } else {
        0
    };
    if count >= max {
        return false;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let _ = std::fs::create_dir_all(dir);
    prune_stale_counters(dir);
    let _ = std::fs::write(path, (count + 1).to_string());
    true
}

/// Delete `bypass-*.count` files last modified over 31 days ago.
fn prune_stale_counters(dir: &Path) {
    const MAX_AGE_SECS: u64 = 31 * 86_400;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let now = now_ts();
    for e in rd.flatten() {
        let name = e.file_name();
        let Ok(name) = name.into_string() else {
            continue;
        };
        if !name.starts_with("bypass-") || !name.ends_with(".count") {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let ok = match meta.modified() {
            Ok(t) => match t.duration_since(UNIX_EPOCH) {
                Ok(d) => now.saturating_sub(d.as_secs()) > MAX_AGE_SECS,
                Err(_) => false,
            },
            Err(_) => false,
        };
        if ok {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Outcome of a successful unlock request.
#[derive(Debug, Clone)]
pub struct BypassOutcome {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// True when served from the local solve-cache (no API spend).
    pub cached: bool,
}

/// Failure classified for key-state feedback and call-site
/// messaging. Every variant carries actionable guidance so the
/// user never stares at a bare status code from a paid integrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BypassFail {
    /// The API itself rejected the call (auth, billing, rate).
    Api { status: u16, detail: String },
    /// Transport-level failure : nothing billed, likely local net.
    Network(String),
    /// Local configuration error : key shape, zone, caps. Free.
    Config(String),
    /// API accepted the call but the target did not unlock.
    Solve(String),
    /// Should-not-happen local failure (client build, dispatch).
    Internal(String),
}

impl std::fmt::Display for BypassFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api { status, detail } => {
                if detail.is_empty() {
                    write!(f, "bright data api returned HTTP {status}")
                } else {
                    write!(f, "bright data api returned HTTP {status}: {detail}")
                }
            }
            Self::Network(d) => write!(f, "{d}"),
            Self::Config(d) => write!(f, "{d}"),
            Self::Solve(d) => write!(f, "{d}"),
            Self::Internal(d) => write!(f, "{d}"),
        }
    }
}

/// Transient Solve-failure classes the current Bright Data docs
/// name as retry-friendly ("retrying can succeed"). Used twice:
/// at guidance time, and inside unlock() to decide the one retry.
fn is_superficial_solve_failure(detail: &str) -> bool {
    const RETRYABLE: &[&str] = &[
        "resolve_failed_ssl",
        "resolve_failed_timeout",
        "resolve_failed_transport",
        "resolve_failed_retryable",
        "failover_timeout",
        "max_requests_timeout",
        "blocked_requests_limit",
        "reject_block",
    ];
    RETRYABLE.iter().any(|c| detail.contains(c))
}

impl BypassFail {
    pub fn key_state(&self) -> Option<KeyState> {
        match self {
            Self::Api { status: 401, .. } => Some(KeyState::Invalid),
            Self::Api { status: 402, .. } => Some(KeyState::CreditDepleted),
            Self::Api { status: 429, .. } => Some(KeyState::RateLimited),
            _ => None,
        }
    }

    /// One-sentence recovery hint per failure class, attached to
    /// the fetch escalation trace so agents can act on it.
    pub fn guidance(&self) -> &'static str {
        match self {
            Self::Api { status: 401, .. } => {
                "the token was rejected: re-add it (`donsetch keys add unlocker <token>[::zone]`) and make sure it is an API token, not a password"
            }
            Self::Api { status: 403, .. } => {
                "Bright Data policy blocked this request or the zone type does not match the Web Unlocker API: verify the zone is a Web Unlocker zone and the target is not reserved/private"
            }
            Self::Api { status: 400, .. } => {
                "the API rejected the request shape: the zone name or URL is wrong (a zone-not-found detail means the ::zone suffix has no match in the dashboard)"
            }
            Self::Api { status: 402, .. } => {
                "this zone has no balance left: top up the Bright Data account or point the key at another zone (`::zone` suffix)"
            }
            Self::Api { status: 429, .. } => {
                "Bright Data rate limit: wait a minute and retry, or lower the number of concurrent locked fetches"
            }
            Self::Api { .. } => "check the status code in the Bright Data dashboard and retry",
            Self::Network(_) => {
                "network could not reach the Bright Data API: check connectivity and any local proxy, then retry (nothing was billed)"
            }
            Self::Config(_) => {
                "fix the unlocker key configuration: `donsetch keys add unlocker <token>[::zone]` and match the zone name in the Bright Data dashboard"
            }
            Self::Solve(d) => {
                if is_superficial_solve_failure(d) {
                    "a transient unlock failure class fired twice in a row: retry the fetch in a few seconds (neither attempt was billed)"
                } else {
                    "the unlocker ran but the target still returned a wall: try again later or confirm this site is solvable in the Bright Data dashboard"
                }
            }
            Self::Internal(_) => "retry; if it persists, report it with the trace",
        }
    }
}

/// Parse the unlocker wrapper (format: "json") against the
/// current Bright Data contract. Unlock failures come in two
/// shapes:
/// - OUTER status != 200 : the API rejected the call before any
///   unlock work (401 invalid token, 400 unknown zone / bad
///   payload, 403 policy block, and so on).
/// - OUTER 200 with a failing x-brd-status-code header : the
///   request reached the unlocker but the target was not served;
///   details ride x-brd-error / x-brd-error-code (or the legacy
///   JSON `status`/`status_code` wrapper fields).
///
/// Returns (target_status, content_type, body) on success.
pub fn parse_response(
    api_status: u16,
    headers: &reqwest::header::HeaderMap,
    bytes: &[u8],
) -> Result<(u16, String, Vec<u8>), BypassFail> {
    if api_status != 200 {
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(400)])
            .trim()
            .to_string();
        // 400 with the documented zone-not-found body is a local
        // config problem, not an API fault: the user typed a zone
        // name that does not exist in their account.
        if api_status == 400 {
            let lower = text.to_lowercase();
            if (lower.contains("zone") && lower.contains("not found"))
                || lower.contains("zone is not")
            {
                return Err(BypassFail::Config(format!(
                    "bright data rejected the zone: {text}"
                )));
            }
        }
        let detail = serde_json::from_slice::<Value>(bytes)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| (!text.is_empty()).then_some(text));
        return Err(BypassFail::Api {
            status: api_status,
            detail: detail.unwrap_or_default(),
        });
    }
    let header = |name: &str| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    // Bright Data's current docs put the real target status in the
    // x-brd-status-code response header (older releases used
    // status/status_code fields in the JSON wrapper). Accept both.
    let v: Option<Value> = serde_json::from_slice(bytes).ok();
    let legacy_status = v
        .as_ref()
        .and_then(|j| j.get("status").or_else(|| j.get("status_code")))
        .and_then(|x| x.as_u64());
    let target_status: Option<u64> =
        legacy_status.or_else(|| header("x-brd-status-code").and_then(|h| h.parse::<u64>().ok()));
    let status: u16 = match target_status.and_then(|n| u16::try_from(n).ok()) {
        Some(s) => s,
        None => {
            let code = header("x-brd-error-code").or_else(|| header("x-brd-err-code"));
            let msg = header("x-brd-error");
            return Err(BypassFail::Solve(match (code, msg) {
                (Some(c), Some(m)) => format!("{c}: {m}"),
                (Some(c), None) => format!("{c}: no target status in response"),
                (None, Some(m)) => format!("no target status: {m}"),
                (None, None) => "unlocker response missing status".to_string(),
            }));
        }
    };
    let ct: String = v
        .as_ref()
        .and_then(|j| j.get("headers"))
        .and_then(|h| h.as_object())
        .and_then(|h| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map(|(_, val)| val.as_str().unwrap_or("").to_string())
        })
        .unwrap_or_else(|| "text/html".to_string());
    if !(200..300).contains(&status) {
        let code = header("x-brd-error-code").or_else(|| header("x-brd-err-code"));
        let msg = header("x-brd-error").unwrap_or_default();
        return Err(BypassFail::Solve(match code {
            Some(c) if !msg.is_empty() => format!("target returned status {status} ({c}: {msg})"),
            Some(c) => format!("target returned status {status} ({c})"),
            None if !msg.is_empty() => format!("target returned status {status}: {msg}"),
            None => format!("target returned status {status}"),
        }));
    }
    let body: Vec<u8> = match v
        .as_ref()
        .and_then(|j| j.get("body").and_then(|b| b.as_str()))
    {
        Some(s) => s.as_bytes().to_vec(),
        None => {
            return Err(BypassFail::Solve(
                "unlocker response missing body".to_string(),
            ));
        }
    };
    if body.is_empty() {
        return Err(BypassFail::Solve(
            "unlocker returned an empty body".to_string(),
        ));
    }
    Ok((status, ct, body))
}

/// Find the first active `unlocker` key from the BYOK store.
pub fn active_unlocker_key(cfg: &ByokConfig) -> Option<String> {
    cfg.providers
        .iter()
        .find(|p| p.name == "unlocker")
        .and_then(|p| {
            p.keys
                .iter()
                .find(|k| k.state == KeyState::Active)
                .map(|k| k.key.clone())
        })
}

/// Update the stored key state on API-level failures (billing, auth, rate).
pub fn apply_key_state(provider: &str, key: &str, fail: &BypassFail) {
    let Some(state) = fail.key_state() else {
        return;
    };
    let mut cfg = ByokConfig::load();
    cfg.update_key_state(provider, key, state);
    cfg.save();
}

// ── Solve-cache: never pay twice for the same page ───────────
//
// A successful unlock is stored under `bypass-cache/<sha256(url)>.json`.
// Repeated fetches of the same URL are served from cache for the TTL
// window at zero API cost. Sliding TTL: every hit rewrites the
// timestamp, so hot URLs stay alive and cold ones expire. Oldest-entry
// pruning caps disk growth. In-flight locking coalesces parallel
// requests for the same URL into one paid call.

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_key(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn bypass_cache_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("bypass-cache")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    v: u32,
    url: String,
    ts: u64,
    status: u16,
    content_type: String,
    /// Base64 of the raw body. v1 stored lossy UTF-8 text, which
    /// corrupts binary bodies (PDF images etc.): v1 entries are
    /// deliberately treated as a miss and expire on their own.
    body: String,
}

const CACHE_VERSION: u32 = 2;

fn cache_get(cache_dir: &Path, url: &str, ttl_secs: u64) -> Option<BypassOutcome> {
    if ttl_secs == 0 {
        return None;
    }
    let path = bypass_cache_dir(cache_dir).join(format!("{}.json", cache_key(url)));
    let raw = std::fs::read_to_string(&path).ok()?;
    let entry: CacheEntry = serde_json::from_str(&raw).ok()?;
    if entry.v != CACHE_VERSION || entry.url != url {
        return None;
    }
    if now_ts().saturating_sub(entry.ts) > ttl_secs {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let body = base64::engine::general_purpose::STANDARD
        .decode(&entry.body)
        .ok()?;
    Some(BypassOutcome {
        status: entry.status,
        content_type: entry.content_type,
        body,
        cached: true,
    })
}

/// Refresh the timestamp on a hit (sliding TTL: hot entries survive).
fn cache_touch(cache_dir: &Path, url: &str) {
    let path = bypass_cache_dir(cache_dir).join(format!("{}.json", cache_key(url)));
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut entry) = serde_json::from_str::<CacheEntry>(&raw) else {
        return;
    };
    entry.ts = now_ts();
    if let Ok(json) = serde_json::to_string(&entry) {
        let _ = std::fs::write(&path, json);
    }
}

fn cache_put(cache_dir: &Path, url: &str, outcome: &BypassOutcome, max_entries: u32) {
    let dir = bypass_cache_dir(cache_dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let key = cache_key(url);
    let entry = CacheEntry {
        v: CACHE_VERSION,
        url: url.to_string(),
        ts: now_ts(),
        status: outcome.status,
        content_type: outcome.content_type.clone(),
        body: base64::engine::general_purpose::STANDARD.encode(&outcome.body),
    };
    let path = dir.join(format!("{key}.json"));
    let tmp = dir.join(format!("{key}.tmp"));
    if std::fs::write(&tmp, serde_json::to_string(&entry).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    cache_prune(&dir, max_entries);
}

/// Oldest-first eviction until the entry count is within max.
fn cache_prune(dir: &Path, max_entries: u32) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut metas: Vec<(u64, PathBuf)> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let ts = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str::<CacheEntry>(&s).ok())
            .map(|c| c.ts)
            .unwrap_or(0);
        metas.push((ts, p));
    }
    if metas.len() <= max_entries as usize {
        return;
    }
    metas.sort_by_key(|(ts, _)| *ts);
    let drop = metas.len() - max_entries as usize;
    for (_, p) in metas.iter().take(drop) {
        let _ = std::fs::remove_file(p);
    }
}

/// One gate per URL: parallel fetches of the same URL share one
/// paid unlock. The gate map is pruned past a cap so a daemon
/// that sees a stream of unique walled URLs does not leak one
/// mutex per URL for its whole lifetime.
fn in_flight_lock(url: &str) -> Arc<tokio::sync::Mutex<()>> {
    const GATE_CAP: usize = 512;
    static MAP: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let map = MAP.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
    if guard.len() > GATE_CAP {
        // Keep only gates somebody is still waiting on (strong
        // count > 1 : the map itself holds the remaining clone).
        guard.retain(|_, v| Arc::strong_count(v) > 1);
    }
    guard
        .entry(cache_key(url))
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Perform one unlock request. Cache-first: a fresh solve-cache hit
/// costs nothing and does not consume the daily cap. Parallel
/// requests for the same URL coalesce into one paid call. Pure
/// network + IO; the MCP layer composes the value (extraction,
/// envelopes) from the outcome.
pub async fn unlock(
    key: &str,
    url: &str,
    cfg: &BypassConfig,
    cache_dir: &Path,
) -> Result<BypassOutcome, BypassFail> {
    let ttl = cfg.cache_ttl.as_secs();
    if let Some(outcome) = cache_get(cache_dir, url, ttl) {
        cache_touch(cache_dir, url);
        return Ok(outcome);
    }
    let gate = in_flight_lock(url);
    let _guard = gate.lock().await;
    if let Some(outcome) = cache_get(cache_dir, url, ttl) {
        cache_touch(cache_dir, url);
        return Ok(outcome);
    }
    let count_path = bypass_count_path(cache_dir);
    if !check_and_bump_daily(&count_path, cfg.max_daily) {
        return Err(BypassFail::Config(format!(
            "daily unlock cap of {} reached : raise DONSETCH_BYPASS_MAX_DAILY or wait for the UTC-day reset",
            cfg.max_daily
        )));
    }
    let (token, zone) = parse_key(key, DEFAULT_ZONE)?;
    let client = reqwest::Client::builder()
        .timeout(cfg.timeout)
        .no_gzip()
        .no_deflate()
        .no_brotli()
        .build()
        .map_err(|e| BypassFail::Internal(format!("bypass client init failed ({e})")))?;
    let mut payload = serde_json::json!({
        "zone": zone,
        "url": url,
        "format": "json",
    });
    if cfg.render {
        payload["render"] = serde_json::json!(true);
    }
    let request = || {
        let client = &client;
        let payload = &payload;
        let endpoint = &cfg.endpoint;
        let token = &token;
        async move {
            client
                .post(endpoint)
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .json(payload)
                .send()
                .await
        }
    };
    let resp = match request().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() || e.is_connect() => {
            // One retry on transient transport failures: a paid
            // tier deserves it, and a timeout/connect reset costs
            // nothing (the API never saw the request, or the
            // request never completed billing).
            tokio::time::sleep(Duration::from_millis(800)).await;
            request()
                .await
                .map_err(|e| BypassFail::Network(format!("bypass request failed twice: {e}")))?
        }
        Err(e) => return Err(BypassFail::Network(format!("bypass request failed ({e})"))),
    };
    let api_status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| BypassFail::Network(format!("bypass response truncated ({e})")))?;
    let mut result = parse_response(api_status, &headers, &bytes);
    // One retry for the transient solve classes Bright Data names
    // as retry-friendly: a different unlocker peer frequently
    // succeeds where the first one failed, and a failed unlock is
    // not billed, so no double spend is possible.
    if let Err(e) = &result
        && let BypassFail::Solve(d) = e
        && is_superficial_solve_failure(d)
    {
        tokio::time::sleep(Duration::from_millis(800)).await;
        let resp = match request().await {
            Ok(r) => r,
            Err(_) => {
                return Err(BypassFail::Internal(
                    "retry request failed after transient unlock failure".to_string(),
                ));
            }
        };
        let api_status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BypassFail::Network(format!("bypass response truncated ({e})")))?;
        result = parse_response(api_status, &headers, &bytes);
    }
    let outcome = result.map(|(status, content_type, body)| BypassOutcome {
        status,
        content_type,
        body,
        cached: false,
    })?;
    if ttl > 0 {
        cache_put(cache_dir, url, &outcome, cfg.cache_max);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::byok::store::{KeyEntry, ProviderConfig};

    #[test]
    fn parse_key_token_only() {
        let (token, zone) = parse_key("abc123", "web_unlocker1").unwrap();
        assert_eq!(token, "abc123");
        assert_eq!(zone, "web_unlocker1");
    }

    #[test]
    fn parse_key_embedded_zone() {
        let (token, zone) = parse_key("abc123::custom_zone", "web_unlocker1").unwrap();
        assert_eq!(token, "abc123");
        assert_eq!(zone, "custom_zone");
    }

    #[test]
    fn parse_key_rejects_empty_token() {
        assert!(matches!(
            parse_key("", "web_unlocker1"),
            Err(BypassFail::Config(_))
        ));
        assert!(matches!(
            parse_key("  ", "web_unlocker1"),
            Err(BypassFail::Config(_))
        ));
    }

    #[test]
    fn parse_key_rejects_empty_zone() {
        assert!(matches!(
            parse_key("abc123::", "web_unlocker1"),
            Err(BypassFail::Config(_))
        ));
        assert!(matches!(
            parse_key("::zone", "web_unlocker1"),
            Err(BypassFail::Config(_))
        ));
    }

    #[test]
    fn parse_key_env_zone_fallback() {
        unsafe { std::env::set_var("DONSETCH_UNLOCKER_ZONE", "env_zone") };
        let (_, zone) = parse_key("abc123", "web_unlocker1").unwrap();
        assert_eq!(zone, "env_zone");
        // `::zone` still wins over the env var.
        let (_, zone2) = parse_key("abc123::explicit", "web_unlocker1").unwrap();
        assert_eq!(zone2, "explicit");
        unsafe { std::env::remove_var("DONSETCH_UNLOCKER_ZONE") };
    }

    #[test]
    fn daily_cap_allows_until_exhausted() {
        let dir = std::env::temp_dir().join(format!("donsetch-bypass-test-{}", std::process::id()));
        let path = dir.join("bypass-test.count");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(check_and_bump_daily(&path, 3));
        assert!(check_and_bump_daily(&path, 3));
        assert!(check_and_bump_daily(&path, 3));
        assert!(!check_and_bump_daily(&path, 3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_response_ok_shape() {
        let resp = br#"{"status":200,"headers":{"content-type":"text/html; charset=utf-8"},"body":"<html>hi</html>"}"#;
        let (status, ct, body) =
            parse_response(200, &reqwest::header::HeaderMap::new(), resp).unwrap();
        assert_eq!(status, 200);
        assert_eq!(ct, "text/html; charset=utf-8");
        assert_eq!(body, b"<html>hi</html>");
    }

    #[test]
    fn parse_response_accepts_status_code_field() {
        let resp = br#"{"status_code":202,"headers":{},"body":"ok"}"#;
        let (status, _, body) =
            parse_response(200, &reqwest::header::HeaderMap::new(), resp).unwrap();
        assert_eq!(status, 202);
        assert_eq!(body, b"ok");
    }

    #[test]
    fn parse_response_header_status_contract() {
        // The current docs put the target status in the
        // x-brd-status-code response header; the JSON wrapper may
        // carry no status at all. Must work without it.
        let resp = br#"{"headers":{},"body":"ok"}"#;
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-brd-status-code", "201".parse().unwrap());
        let (status, _, body) = parse_response(200, &h, resp).unwrap();
        assert_eq!(status, 201);
        assert_eq!(body, b"ok");
    }

    #[test]
    fn parse_response_header_status_failure_with_codes() {
        let resp = br#"not json"#;
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-brd-status-code", "502".parse().unwrap());
        h.insert("x-brd-error-code", "reject_block".parse().unwrap());
        h.insert("x-brd-error", "challenge blocked".parse().unwrap());
        let err = parse_response(200, &h, resp).unwrap_err();
        assert_eq!(
            err,
            BypassFail::Solve(
                "target returned status 502 (reject_block: challenge blocked)".to_string()
            )
        );
        assert!(is_superficial_solve_failure(&err.to_string()));
    }

    #[test]
    fn parse_response_missing_status() {
        let err = parse_response(200, &reqwest::header::HeaderMap::new(), br#"{"body":"x"}"#)
            .unwrap_err();
        assert_eq!(
            err,
            BypassFail::Solve("unlocker response missing status".to_string())
        );
    }

    #[test]
    fn parse_response_zone_not_found_is_config() {
        let err = parse_response(
            400,
            &reqwest::header::HeaderMap::new(),
            b"zone \"nope\" not found",
        )
        .unwrap_err();
        assert!(matches!(err, BypassFail::Config(_)), "got {err:?}");
        assert_eq!(err.key_state(), None);
    }

    #[test]
    fn parse_response_api_error_maps_state() {
        let err =
            parse_response(401, &reqwest::header::HeaderMap::new(), b"unauthorized").unwrap_err();
        assert_eq!(err.key_state(), Some(KeyState::Invalid));
        let err =
            parse_response(402, &reqwest::header::HeaderMap::new(), b"no credit").unwrap_err();
        assert_eq!(err.key_state(), Some(KeyState::CreditDepleted));
        let err =
            parse_response(429, &reqwest::header::HeaderMap::new(), b"slow down").unwrap_err();
        assert_eq!(err.key_state(), Some(KeyState::RateLimited));
        // 403 is policy/zone-type, not a key problem.
        let err = parse_response(403, &reqwest::header::HeaderMap::new(), b"policy").unwrap_err();
        assert_eq!(err.key_state(), None);
    }

    #[test]
    fn parse_response_api_error_extracts_json_error_text() {
        let err = parse_response(
            401,
            &reqwest::header::HeaderMap::new(),
            br#"{"error":"user is not authorized"}"#,
        )
        .unwrap_err();
        assert_eq!(
            err,
            BypassFail::Api {
                status: 401,
                detail: "user is not authorized".to_string()
            }
        );
    }

    #[test]
    fn parse_response_target_error() {
        let resp = br#"{"status":403,"headers":{},"body":"forbidden"}"#;
        let err = parse_response(200, &reqwest::header::HeaderMap::new(), resp).unwrap_err();
        assert_eq!(
            err,
            BypassFail::Solve("target returned status 403".to_string())
        );
        assert_eq!(err.key_state(), None);
    }

    #[test]
    fn bypass_fail_guidance_covers_every_variant() {
        let fails = [
            BypassFail::Api {
                status: 401,
                detail: String::new(),
            },
            BypassFail::Api {
                status: 403,
                detail: String::new(),
            },
            BypassFail::Api {
                status: 402,
                detail: String::new(),
            },
            BypassFail::Api {
                status: 429,
                detail: String::new(),
            },
            BypassFail::Api {
                status: 500,
                detail: String::new(),
            },
            BypassFail::Network("x".into()),
            BypassFail::Config("x".into()),
            BypassFail::Solve("x".into()),
            BypassFail::Internal("x".into()),
        ];
        for f in &fails {
            assert!(!f.guidance().is_empty(), "{f:?} must carry guidance");
        }
    }

    #[test]
    fn active_unlocker_key_picks_active_only() {
        let cfg = ByokConfig {
            default: String::new(),
            providers: vec![ProviderConfig {
                name: "unlocker".into(),
                keys: vec![
                    KeyEntry {
                        key: "bad".into(),
                        state: KeyState::Invalid,
                        ts: 0,
                    },
                    KeyEntry {
                        key: "good".into(),
                        state: KeyState::Active,
                        ts: 0,
                    },
                ],
            }],
        };
        assert_eq!(active_unlocker_key(&cfg), Some("good".to_string()));
    }

    #[test]
    fn cache_roundtrip_hit_ttl_and_miss() {
        let dir =
            std::env::temp_dir().join(format!("donsetch-bypass-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let url = "https://example.com/x";
        let outcome = BypassOutcome {
            status: 200,
            content_type: "text/html".into(),
            body: b"<html>solved</html>".to_vec(),
            cached: false,
        };
        cache_put(&dir, url, &outcome, 10);
        let got = cache_get(&dir, url, 3600).unwrap();
        assert_eq!(got.body, outcome.body);
        assert_eq!(got.status, 200);
        assert!(got.cached);
        // ttl 0 = cache disabled, never hits
        assert!(cache_get(&dir, url, 0).is_none());
        // different URL misses
        assert!(cache_get(&dir, "https://example.com/y", 3600).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_roundtrip_preserves_binary_body() {
        // Binary bodies (PDFs, images) must survive the cache
        // byte-for-byte: the v1 lossy UTF-8 round-trip corrupted
        // them, so v2 stores base64.
        let dir =
            std::env::temp_dir().join(format!("donsetch-bypass-cache-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let url = "https://example.com/doc.pdf";
        let mut body = b"%PDF-1.4".to_vec();
        body.extend_from_slice(&[0x00, 0xff, 0x80, 0xfe]);
        let outcome = BypassOutcome {
            status: 200,
            content_type: "application/pdf".into(),
            body: body.clone(),
            cached: false,
        };
        cache_put(&dir, url, &outcome, 10);
        let got = cache_get(&dir, url, 3600).unwrap();
        assert_eq!(got.body, body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_prune_evicts_oldest() {
        let dir =
            std::env::temp_dir().join(format!("donsetch-bypass-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for i in 0..4 {
            let url = format!("https://example.com/p{i}");
            let outcome = BypassOutcome {
                status: 200,
                content_type: "text/html".into(),
                body: format!("body{i}").into_bytes(),
                cached: false,
            };
            cache_put(&dir, &url, &outcome, 2);
        }
        let dir2 = bypass_cache_dir(&dir);
        let n = std::fs::read_dir(&dir2)
            .unwrap()
            .flatten()
            .filter(|e| {
                let p = e.path();
                p.extension().and_then(|x| x.to_str()) == Some("json")
            })
            .count();
        assert_eq!(n, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inflight_lock_shares_gate_per_url() {
        let a = in_flight_lock("https://example.com/z");
        let b = in_flight_lock("https://example.com/z");
        let c = in_flight_lock("https://example.com/w");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
    }
}
