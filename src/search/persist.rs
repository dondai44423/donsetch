//! Disk persistence for search: the normalized-query result cache
//! and the learned engine health (trust EWMAs + failure streaks).
//! Both survive restarts so a daemon reboot never re-pays egress
//! budget or re-learns a walled engine from zero.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::EngineReport;
use super::Intent;
use super::cache_ttl;
use super::rank::Merged;

/// Query-cache map shape: key -> (written-at, up-to-12 results,
/// merge total at write time, engine reports). The reports are
/// cached with the results so a cache hit still carries engine
/// evidence (#164: cache hits used to return an empty report,
/// hiding whether the answer was fresh consensus or stale cache).
pub(crate) type CacheMap = HashMap<String, (Instant, Vec<Merged>, usize, Vec<EngineReport>)>;

/// On-disk cache entry: (key, age_secs, results, merge total,
/// engine reports). Owned form for load, borrowed form for save.
type DiskEntry = (String, u64, Vec<Merged>, usize, Vec<EngineReport>);
type DiskEntryRef<'a> = (String, u64, Vec<Merged>, usize, &'a [EngineReport]);
type DiskEntryLegacy = (String, u64, Vec<Merged>, usize);

/// Disk cache path (ghost-state pattern).
fn cache_path() -> Option<std::path::PathBuf> {
    let dir = dirs_cache()?;
    Some(dir.join("search-cache.json"))
}

fn dirs_cache() -> Option<std::path::PathBuf> {
    let dir = crate::paths::cache_dir();
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// On disk: (key, age_secs, results, total, reports) : age lets us
/// re-base Instant across process restarts. Reports were added for
/// #164; entries written before that carry a 4-tuple and load with
/// an empty report list rather than being discarded.
pub(crate) fn save_cache_disk(cache: &CacheMap) {
    let Some(path) = cache_path() else { return };
    let now = Instant::now();
    let entries: Vec<DiskEntryRef> = cache
        .iter()
        .map(|(k, (at, r, t, rep))| {
            (
                k.clone(),
                now.saturating_duration_since(*at).as_secs(),
                r.clone(),
                *t,
                rep.as_slice(),
            )
        })
        .collect();
    if let Ok(json) = serde_json::to_string(&entries) {
        let tmp = path.with_extension("tmp");
        if crate::config::write_private(&tmp, json.as_bytes()).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

pub(crate) fn load_cache_disk() -> CacheMap {
    let mut map = CacheMap::new();
    let Some(path) = cache_path() else { return map };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return map;
    };
    // Current 5-tuple format first; fall back to the pre-#164
    // 4-tuple format so an existing cache survives an upgrade.
    let entries: Vec<DiskEntry> = match serde_json::from_str(&raw) {
        Ok(e) => e,
        Err(_) => match serde_json::from_str::<Vec<DiskEntryLegacy>>(&raw) {
            Ok(old) => old
                .into_iter()
                .map(|(k, age, results, total)| (k, age, results, total, Vec::new()))
                .collect(),
            Err(_) => return map,
        },
    };
    for (key, age, results, total, reports) in entries {
        // TTL is intent + recency keyed (the query text
        // is the key's first segment). Keys carry a stable u8
        // intent code; pre-code entries carry the Debug string and
        // remap by name for one TTL generation.
        let (qpart, ipart) = key.rsplit_once('|').unwrap_or((key.as_str(), ""));
        let intent = if let Ok(code) = ipart.parse::<u8>() {
            Intent::from_code(code)
        } else {
            match ipart {
                "News" => Intent::News,
                "Code" => Intent::Code,
                "Paper" => Intent::Paper,
                "Entity" => Intent::Entity,
                _ => Intent::Web,
            }
        };
        let ttl = cache_ttl(intent, qpart);
        if Duration::from_secs(age) < ttl {
            map.insert(
                key,
                (
                    Instant::now() - Duration::from_secs(age),
                    results,
                    total,
                    reports,
                ),
            );
        }
    }
    map
}

/// Trust map: engine or `engine|intent_code` -> EWMA.
pub(crate) type TrustMap = HashMap<String, f64>;
/// Failure streak map: engine -> (consecutive fails, last Instant).
pub(crate) type FailureMap = HashMap<String, (u32, Instant)>;
/// Loaded health store (B2): global trust, per-intent trust, quarantines.
pub type HealthSnapshot = (TrustMap, TrustMap, FailureMap);

/// Status/doctor receipt reader: trust maps + failure streaks
/// without constructing a Searcher (no fetcher, no pool).
pub fn load_for_status() -> HealthSnapshot {
    load_health_disk()
}

/// Engine health persistence: trust EWMAs + failure streaks
/// survive restarts, so an engine benched for chronic failure
/// skips its fan-out slot immediately after a crash instead of
/// being re-paid three times from zero.
fn health_path() -> Option<std::path::PathBuf> {
    Some(crate::paths::cache_dir().join("search-trust.json"))
}

/// B2: versioned trust store. v1 was engine-global only; v2 adds
/// per-intent EWMAs. Old files migrate (anti-amnesia: every intent
/// inherits the engine-global trust) instead of being discarded.
const HEALTH_DISK_VERSION: u32 = 2;

/// Stable key for a (engine, intent) trust sample.
pub(crate) fn trust_intent_key(engine: &str, intent: Intent) -> String {
    format!("{}|{}", engine, intent.code())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HealthDisk {
    #[serde(default = "health_disk_version")]
    version: u32,
    /// Engine-global trust (legacy shape; still the fallback).
    #[serde(default)]
    trust: HashMap<String, f64>,
    /// Per-intent trust: `engine|intent_code` -> EWMA.
    #[serde(default)]
    trust_intent: HashMap<String, f64>,
    #[serde(default)]
    failures: HashMap<String, (u32, u64)>,
}

fn health_disk_version() -> u32 {
    1
}

pub(crate) fn load_health_disk() -> HealthSnapshot {
    let mut trust = HashMap::new();
    let mut trust_intent = HashMap::new();
    let mut failures = HashMap::new();
    let Some(path) = health_path() else {
        return (trust, trust_intent, failures);
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return (trust, trust_intent, failures);
    };
    let Ok(h) = serde_json::from_str::<HealthDisk>(&raw) else {
        return (trust, trust_intent, failures);
    };
    for (e, t) in h.trust {
        trust.insert(e, t.clamp(0.2, 2.0));
    }
    // Anti-amnesia: a store that has engine-global trust but no
    // per-intent samples (v1 files, or a fresh v2 write) seeds every
    // intent from the engine-global EWMA so a restart never re-learns
    // a walled engine from zero.
    if trust_intent.is_empty() && !trust.is_empty() {
        for (engine, t) in &trust {
            for intent in [
                Intent::Web,
                Intent::Code,
                Intent::Paper,
                Intent::News,
                Intent::Entity,
            ] {
                trust_intent
                    .entry(trust_intent_key(engine, intent))
                    .or_insert(*t);
            }
        }
    }
    for (k, t) in h.trust_intent {
        trust_intent.insert(k, t.clamp(0.2, 2.0));
    }
    for (e, (n, age)) in h.failures {
        // Only a streak that WOULD still quarantine matters:
        // everything older expired while the process was down.
        if n >= 3 && Duration::from_secs(age) < super::QUARANTINE_TTL {
            failures.insert(e, (n, Instant::now() - Duration::from_secs(age.min(599))));
        }
    }
    (trust, trust_intent, failures)
}

pub(crate) fn save_health_disk(
    trust: &HashMap<String, f64>,
    trust_intent: &HashMap<String, f64>,
    failures: &HashMap<String, (u32, Instant)>,
) {
    let Some(path) = health_path() else { return };
    let now = Instant::now();
    let disk = HealthDisk {
        version: HEALTH_DISK_VERSION,
        trust: trust.clone(),
        trust_intent: trust_intent.clone(),
        failures: failures
            .iter()
            .map(|(e, (n, at))| {
                (
                    e.clone(),
                    (*n, now.saturating_duration_since(*at).as_secs()),
                )
            })
            .collect(),
    };
    let Ok(json) = serde_json::to_string(&disk) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if crate::config::write_private(&tmp, json.as_bytes()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

/// Dirty-flag wrapper: skips the disk write entirely when no health
/// mutation happened since the last save (was: clone + serialize +
/// write on every uncached search, a few KB per query).
pub(crate) fn save_health_disk_if_dirty(
    searcher: &super::Searcher,
    trust: &HashMap<String, f64>,
    trust_intent: &HashMap<String, f64>,
    failures: &HashMap<String, (u32, Instant)>,
) {
    if !searcher
        .health_dirty
        .swap(false, std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    save_health_disk(trust, trust_intent, failures);
}

// ────────────────────────── B3 domain quality prior ──────────────────────────

/// Learned host quality from enrich/prefetch outcomes.
/// `ewma` is success density in 0.0..=1.0 (0.5 = neutral seed).
/// Rank only reads hosts with enough samples so one lucky clean
/// fetch cannot promote a junk domain.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct HostQuality {
    pub ewma: f32,
    pub samples: u32,
    /// Unix seconds of the last observation (LRU / TTL).
    pub last: u64,
}

impl Default for HostQuality {
    fn default() -> Self {
        Self {
            ewma: 0.5,
            samples: 0,
            last: 0,
        }
    }
}

/// host (www-stripped) -> learned quality.
pub type QualityMap = HashMap<String, HostQuality>;

/// Status/doctor receipt: (tracked hosts, high-quality, low-quality).
pub type QualitySnapshot = (usize, usize, usize);

const QUALITY_DISK_VERSION: u32 = 1;
const QUALITY_CAP: usize = 2_000;
const QUALITY_TTL_SECS: u64 = 30 * 86_400;
const QUALITY_MIN_SAMPLES: u32 = 3;
/// Capped small: consensus / BM25 / static prior stay dominant.
const QUALITY_WEIGHT: f64 = 0.08;

fn quality_path() -> Option<std::path::PathBuf> {
    Some(crate::paths::cache_dir().join("search-quality.json"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct QualityDisk {
    #[serde(default = "quality_disk_version")]
    version: u32,
    #[serde(default)]
    hosts: QualityMap,
}

fn quality_disk_version() -> u32 {
    1
}

/// Strip `www.` so quality keys match `rank::host_of` + domain_prior.
pub(crate) fn quality_host_key(host: &str) -> String {
    host.trim_start_matches("www.")
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Status/doctor reader without constructing a Searcher.
pub fn load_quality_for_status() -> QualitySnapshot {
    let map = load_quality_disk();
    let high = map
        .values()
        .filter(|q| q.samples >= QUALITY_MIN_SAMPLES && q.ewma >= 0.65)
        .count();
    let low = map
        .values()
        .filter(|q| q.samples >= QUALITY_MIN_SAMPLES && q.ewma <= 0.35)
        .count();
    (map.len(), high, low)
}

pub(crate) fn load_quality_disk() -> QualityMap {
    let mut map = QualityMap::new();
    let Some(path) = quality_path() else {
        return map;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return map;
    };
    let Ok(d) = serde_json::from_str::<QualityDisk>(&raw) else {
        return map;
    };
    let now = now_unix();
    for (host, q) in d.hosts {
        if now.saturating_sub(q.last) > QUALITY_TTL_SECS {
            continue;
        }
        map.insert(host, q);
    }
    map
}

pub(crate) fn save_quality_disk(map: &QualityMap) {
    let Some(path) = quality_path() else { return };
    let disk = QualityDisk {
        version: QUALITY_DISK_VERSION,
        hosts: map.clone(),
    };
    let Ok(json) = serde_json::to_string(&disk) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if crate::config::write_private(&tmp, json.as_bytes()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

pub(crate) fn save_quality_disk_if_dirty(searcher: &super::Searcher, map: &QualityMap) {
    if !searcher
        .quality_dirty
        .swap(false, std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    save_quality_disk(map);
}

/// One enrich/prefetch observation. Clean content raises density;
/// dead/soft-404 lowers it. Walls/timeouts never call this (routing
/// fact, not a quality fact).
pub(crate) fn observe_quality(map: &mut QualityMap, host: &str, ok: bool) {
    let key = quality_host_key(host);
    let target = if ok { 0.9 } else { 0.1 };
    let entry = map.entry(key).or_default();
    entry.ewma = entry.ewma * 0.7 + (target as f32) * 0.3;
    entry.samples = entry.samples.saturating_add(1);
    entry.last = now_unix();
    // Bound: evict coldest LRU beyond cap.
    if map.len() > QUALITY_CAP {
        let Some(coldest) = map
            .iter()
            .min_by_key(|(_, q)| q.last)
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        map.remove(&coldest);
    }
}

/// B3 rank nudge. Applied with static domain_prior, before the
/// vertical-only penalty so CE can still rescue semantically-
/// relevant results. Kill switch: `search.quality_prior=false`.
pub(crate) fn apply_quality_prior(results: &mut [Merged], quality: &QualityMap) {
    if !crate::config::cfg().search.quality_prior {
        return;
    }
    for r in results.iter_mut() {
        let host = super::rank::host_of(&r.url);
        let key = quality_host_key(&host);
        let Some(q) = quality.get(&key) else {
            continue;
        };
        if q.samples < QUALITY_MIN_SAMPLES {
            continue;
        }
        let learned = ((q.ewma as f64 - 0.5) * 2.0).clamp(-1.0, 1.0);
        r.score += QUALITY_WEIGHT * learned;
    }
}

// ────────────────────────── B4 agent-outcome feedback ──────────────────────────

/// Coarse class of the agent's fetch intent. Keys the outcome
/// store so a must_contain probe miss does not poison full-page
/// ranking for the same host, and vice versa.
pub fn outcome_class(must_contain: bool, targeted_read: bool) -> &'static str {
    if must_contain {
        "probe"
    } else if targeted_read {
        "read"
    } else {
        "page"
    }
}

/// `class|host` -> soft demote EWMA (0.5 seed; misses pull toward 0.1).
pub type OutcomeMap = HashMap<String, HostQuality>;

const OUTCOME_DISK_VERSION: u32 = 1;
const OUTCOME_CAP: usize = 2_000;
const OUTCOME_TTL_SECS: u64 = 30 * 86_400;
const OUTCOME_MIN_SAMPLES: u32 = 2;
/// Smaller than the quality prior: outcome feedback is a tie-break,
/// not a verdict. Default-off until soak.
const OUTCOME_WEIGHT: f64 = 0.05;

fn outcome_path() -> Option<std::path::PathBuf> {
    Some(crate::paths::cache_dir().join("outcome-feedback.json"))
}

fn outcome_key(class: &str, host: &str) -> String {
    format!("{}|{}", class, quality_host_key(host))
}

/// Status/doctor: (tracked keys, demoted keys).
pub fn load_outcome_for_status() -> (usize, usize) {
    let map = load_outcome_disk();
    let demoted = map
        .values()
        .filter(|q| q.samples >= OUTCOME_MIN_SAMPLES && q.ewma <= 0.40)
        .count();
    (map.len(), demoted)
}

pub(crate) fn load_outcome_disk() -> OutcomeMap {
    let mut map = OutcomeMap::new();
    let Some(path) = outcome_path() else {
        return map;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return map;
    };
    #[derive(serde::Deserialize)]
    struct Disk {
        #[serde(default)]
        hosts: OutcomeMap,
    }
    let Ok(d) = serde_json::from_str::<Disk>(&raw) else {
        return map;
    };
    let now = now_unix();
    for (k, q) in d.hosts {
        if now.saturating_sub(q.last) > OUTCOME_TTL_SECS {
            continue;
        }
        map.insert(k, q);
    }
    map
}

pub(crate) fn save_outcome_disk(map: &OutcomeMap) {
    let Some(path) = outcome_path() else { return };
    #[derive(serde::Serialize)]
    struct Disk {
        version: u32,
        hosts: OutcomeMap,
    }
    let disk = Disk {
        version: OUTCOME_DISK_VERSION,
        hosts: map.clone(),
    };
    let Ok(json) = serde_json::to_string(&disk) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if crate::config::write_private(&tmp, json.as_bytes()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

pub(crate) fn save_outcome_disk_if_dirty(searcher: &super::Searcher, map: &OutcomeMap) {
    if !searcher
        .outcome_dirty
        .swap(false, std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    save_outcome_disk(map);
}

/// One agent-outcome miss. Demote-only: successes are not recorded
/// (the agent only sets must_contain when it is hunting; a MATCH is
/// the expected path, not a rare quality signal).
pub(crate) fn observe_outcome_miss(map: &mut OutcomeMap, class: &str, host: &str) {
    let key = outcome_key(class, host);
    let target = 0.1f32;
    let entry = map.entry(key).or_default();
    entry.ewma = entry.ewma * 0.7 + target * 0.3;
    entry.samples = entry.samples.saturating_add(1);
    entry.last = now_unix();
    if map.len() > OUTCOME_CAP {
        let Some(coldest) = map
            .iter()
            .min_by_key(|(_, q)| q.last)
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        map.remove(&coldest);
    }
}

/// Soft demote hosts that repeatedly failed agent outcomes under any
/// class. Search does not know the agent's next must_contain; a host
/// that chronically misses probes or yields thin pages is a weak
/// source for the next click too.
pub(crate) fn apply_outcome_demote(results: &mut [Merged], outcomes: &OutcomeMap) {
    if !crate::config::cfg().search.outcome_feedback {
        return;
    }
    for r in results.iter_mut() {
        let host = super::rank::host_of(&r.url);
        let key = quality_host_key(&host);
        let worst = outcomes
            .iter()
            .filter(|(k, q)| {
                k.ends_with(&format!("|{key}"))
                    && q.samples >= OUTCOME_MIN_SAMPLES
                    && q.ewma <= 0.40
            })
            .map(|(_, q)| q.ewma)
            .fold(None::<f32>, |acc, e| Some(acc.map_or(e, |a: f32| a.min(e))));
        if let Some(ewma) = worst {
            let demote = ((0.5 - ewma as f64) * 2.0).clamp(0.0, 1.0);
            r.score -= OUTCOME_WEIGHT * demote;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolate_cache(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "donsetch-quality-b3-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }
        dir
    }

    fn clean_isolate(dir: std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("DONSETCH_CACHE_DIR");
        }
    }

    fn merged(url: &str, score: f64) -> Merged {
        Merged {
            title: "t".into(),
            url: url.into(),
            snippet: "s".into(),
            sources: vec![("brave".into(), 1)],
            score,
            published: None,
        }
    }

    fn quality_for(ewma: f32, samples: u32) -> HostQuality {
        HostQuality {
            ewma,
            samples,
            last: now_unix(),
        }
    }

    #[test]
    fn observe_quality_moves_ewma_toward_target() {
        let mut map = QualityMap::new();
        observe_quality(&mut map, "www.good.example", true);
        let q = &map["good.example"];
        assert!(q.ewma > 0.5, "clean must raise density, got {}", q.ewma);
        assert_eq!(q.samples, 1);
        // A few more cleans should push well above the apply gate.
        for _ in 0..5 {
            observe_quality(&mut map, "good.example", true);
        }
        let q = &map["good.example"];
        assert!(
            q.ewma > 0.75,
            "sustained clean must stay high, got {}",
            q.ewma
        );
        assert_eq!(q.samples, 6);

        observe_quality(&mut map, "bad.example", false);
        let b = &map["bad.example"];
        assert!(b.ewma < 0.5, "dead must lower density, got {}", b.ewma);
    }

    #[test]
    fn quality_prior_nudges_only_after_min_samples() {
        let dir = isolate_cache("nudge");
        // Config is process-global; defaults have quality_prior=true.
        let mut map = QualityMap::new();
        map.insert("hot.example".into(), quality_for(0.95, 3));
        map.insert("cold.example".into(), quality_for(0.05, 3));
        map.insert("young.example".into(), quality_for(0.99, 1));

        let mut results = vec![
            merged("https://hot.example/a", 1.0),
            merged("https://cold.example/b", 1.0),
            merged("https://young.example/c", 1.0),
            merged("https://unknown.example/d", 1.0),
        ];
        apply_quality_prior(&mut results, &map);
        assert!(
            results[0].score > 1.0,
            "proven-good host must gain, got {}",
            results[0].score
        );
        assert!(
            results[1].score < 1.0,
            "proven-dead host must lose, got {}",
            results[1].score
        );
        assert!(
            (results[2].score - 1.0).abs() < 1e-9,
            "single-sample host must stay neutral, got {}",
            results[2].score
        );
        assert!(
            (results[3].score - 1.0).abs() < 1e-9,
            "unknown host must stay neutral, got {}",
            results[3].score
        );
        clean_isolate(dir);
    }

    #[test]
    fn quality_prior_kill_switch_skips_nudge() {
        let dir = isolate_cache("kill");
        unsafe {
            std::env::set_var("DONSETCH_NO_CONFIG_FILE", "1");
            std::env::set_var("DONSETCH_NO_QUALITY_PRIOR", "1");
        }
        // Force a fresh config load under the kill switch.
        // cfg() is frozen at first use; nextest = process-per-test
        // so this test's first cfg() sees the env.
        let mut map = QualityMap::new();
        map.insert("hot.example".into(), quality_for(0.95, 10));
        let mut results = vec![merged("https://hot.example/a", 1.0)];
        apply_quality_prior(&mut results, &map);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "kill switch must leave scores untouched, got {}",
            results[0].score
        );
        unsafe {
            std::env::remove_var("DONSETCH_NO_QUALITY_PRIOR");
        }
        clean_isolate(dir);
    }

    #[test]
    fn quality_survives_restart() {
        let dir = isolate_cache("persist");
        let mut map = QualityMap::new();
        for _ in 0..8 {
            observe_quality(&mut map, "keep.example", true);
        }
        save_quality_disk(&map);
        let reloaded = load_quality_disk();
        let q = reloaded
            .get("keep.example")
            .expect("quality must survive restart");
        assert_eq!(q.samples, 8);
        assert!(
            q.ewma > 0.7,
            "learned high density must load, got {}",
            q.ewma
        );
        let (n, high, _low) = load_quality_for_status();
        assert!(n >= 1);
        assert!(high >= 1);
        clean_isolate(dir);
    }

    #[test]
    fn quality_cap_evicts_coldest_host() {
        let mut map = QualityMap::new();
        // Fill to cap with an old host, then one more observe forces LRU out.
        for i in 0..QUALITY_CAP {
            let key = format!("h{i}.example");
            map.insert(
                key,
                HostQuality {
                    ewma: 0.5,
                    samples: 1,
                    last: i as u64,
                },
            );
        }
        assert_eq!(map.len(), QUALITY_CAP);
        observe_quality(&mut map, "new.example", true);
        assert_eq!(map.len(), QUALITY_CAP, "cap must hold");
        assert!(map.contains_key("new.example"));
        assert!(!map.contains_key("h0.example"), "coldest LRU must evict");
    }

    // ── B4 agent-outcome ──

    #[test]
    fn outcome_class_separates_probe_read_page() {
        assert_eq!(outcome_class(true, false), "probe");
        assert_eq!(outcome_class(false, true), "read");
        assert_eq!(outcome_class(false, false), "page");
    }

    #[test]
    fn outcome_demote_needs_two_misses_and_default_off_blocks_apply() {
        let dir = isolate_cache("outcome");
        // Default: outcome_feedback=false → apply is a no-op even
        // with a demoted map (the store is the soak-proven layer).
        let mut map = OutcomeMap::new();
        observe_outcome_miss(&mut map, "probe", "weak.example");
        observe_outcome_miss(&mut map, "probe", "weak.example");
        let key = outcome_key("probe", "weak.example");
        let q = &map[&key];
        assert_eq!(q.samples, 2);
        assert!(q.ewma <= 0.40, "two misses must demote, got {}", q.ewma);

        let mut results = vec![merged("https://weak.example/a", 1.0)];
        apply_outcome_demote(&mut results, &map);
        assert!(
            (results[0].score - 1.0).abs() < 1e-9,
            "default-off must leave scores untouched, got {}",
            results[0].score
        );
        clean_isolate(dir);
    }

    #[test]
    fn outcome_demote_applies_when_enabled() {
        let dir = isolate_cache("outcome-on");
        unsafe {
            std::env::set_var("DONSETCH_NO_CONFIG_FILE", "1");
            std::env::set_var("DONSETCH_OUTCOME_FEEDBACK", "1");
        }
        let mut map = OutcomeMap::new();
        observe_outcome_miss(&mut map, "probe", "weak.example");
        observe_outcome_miss(&mut map, "probe", "weak.example");
        let mut results = vec![
            merged("https://weak.example/a", 1.0),
            merged("https://fine.example/b", 1.0),
        ];
        apply_outcome_demote(&mut results, &map);
        assert!(
            results[0].score < 1.0,
            "enabled outcome feedback must soft-demote, got {}",
            results[0].score
        );
        assert!(
            (results[1].score - 1.0).abs() < 1e-9,
            "untracked host must stay neutral, got {}",
            results[1].score
        );
        unsafe {
            std::env::remove_var("DONSETCH_OUTCOME_FEEDBACK");
        }
        clean_isolate(dir);
    }

    #[test]
    fn outcome_probe_miss_does_not_poison_other_hosts_via_class() {
        // Key is class|host: a probe miss on a.example never touches b.example.
        let mut map = OutcomeMap::new();
        observe_outcome_miss(&mut map, "probe", "a.example");
        observe_outcome_miss(&mut map, "probe", "a.example");
        assert!(map.contains_key("probe|a.example"));
        assert!(!map.contains_key("probe|b.example"));
        assert!(!map.contains_key("page|a.example"), "class stays scoped");
    }

    #[test]
    fn outcome_survives_restart() {
        let dir = isolate_cache("outcome-persist");
        let mut map = OutcomeMap::new();
        observe_outcome_miss(&mut map, "page", "keep.example");
        observe_outcome_miss(&mut map, "page", "keep.example");
        save_outcome_disk(&map);
        let reloaded = load_outcome_disk();
        assert_eq!(reloaded["page|keep.example"].samples, 2);
        let (n, demoted) = load_outcome_for_status();
        assert!(n >= 1);
        assert!(demoted >= 1);
        clean_isolate(dir);
    }
}
