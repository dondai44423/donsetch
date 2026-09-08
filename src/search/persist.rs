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
        if std::fs::write(&tmp, json).is_ok() {
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

/// Engine health persistence: trust EWMAs + failure streaks
/// survive restarts, so an engine benched for chronic failure
/// skips its fan-out slot immediately after a crash instead of
/// being re-paid three times from zero.
fn health_path() -> Option<std::path::PathBuf> {
    Some(crate::paths::cache_dir().join("search-trust.json"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HealthDisk {
    #[serde(default)]
    trust: HashMap<String, f64>,
    #[serde(default)]
    failures: HashMap<String, (u32, u64)>,
}

pub(crate) fn load_health_disk() -> (HashMap<String, f64>, HashMap<String, (u32, Instant)>) {
    let mut trust = HashMap::new();
    let mut failures = HashMap::new();
    let Some(path) = health_path() else {
        return (trust, failures);
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return (trust, failures);
    };
    let Ok(h) = serde_json::from_str::<HealthDisk>(&raw) else {
        return (trust, failures);
    };
    for (e, t) in h.trust {
        trust.insert(e, t.clamp(0.2, 2.0));
    }
    for (e, (n, age)) in h.failures {
        // Only a streak that WOULD still quarantine matters:
        // everything older expired while the process was down.
        if n >= 3 && Duration::from_secs(age) < super::QUARANTINE_TTL {
            failures.insert(e, (n, Instant::now() - Duration::from_secs(age.min(599))));
        }
    }
    (trust, failures)
}

pub(crate) fn save_health_disk(
    trust: &HashMap<String, f64>,
    failures: &HashMap<String, (u32, Instant)>,
) {
    let Some(path) = health_path() else { return };
    let now = Instant::now();
    let disk = HealthDisk {
        trust: trust.clone(),
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
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

/// Dirty-flag wrapper: skips the disk write entirely when no health
/// mutation happened since the last save (was: clone + serialize +
/// write on every uncached search, a few KB per query).
pub(crate) fn save_health_disk_if_dirty(
    searcher: &super::Searcher,
    trust: &HashMap<String, f64>,
    failures: &HashMap<String, (u32, Instant)>,
) {
    if !searcher
        .health_dirty
        .swap(false, std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    save_health_disk(trust, failures);
}
