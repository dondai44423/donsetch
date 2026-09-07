//! Disk persistence for search: the normalized-query result cache
//! and the learned engine health (trust EWMAs + failure streaks).
//! Both survive restarts so a daemon reboot never re-pays egress
//! budget or re-learns a walled engine from zero.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::Intent;
use super::cache_ttl;
use super::rank::Merged;

/// Query-cache map shape: key -> (written-at, up-to-12 results,
/// merge total at write time).
pub(crate) type CacheMap = HashMap<String, (Instant, Vec<Merged>, usize)>;

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

/// On disk: (key, age_secs, results, total) : age lets us
/// re-base Instant across process restarts.
pub(crate) fn save_cache_disk(cache: &CacheMap) {
    let Some(path) = cache_path() else { return };
    let now = Instant::now();
    let entries: Vec<(String, u64, Vec<Merged>, usize)> = cache
        .iter()
        .map(|(k, (at, r, t))| {
            (
                k.clone(),
                now.saturating_duration_since(*at).as_secs(),
                r.clone(),
                *t,
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
    let Ok(entries) = serde_json::from_str::<Vec<(String, u64, Vec<Merged>, usize)>>(&raw) else {
        return map;
    };
    for (key, age, results, total) in entries {
        // TTL is intent + recency keyed (the query text
        // is the key's first segment).
        let (qpart, ipart) = key.rsplit_once('|').unwrap_or((key.as_str(), ""));
        let intent = match ipart {
            "News" => Intent::News,
            "Code" => Intent::Code,
            "Paper" => Intent::Paper,
            "Entity" => Intent::Entity,
            _ => Intent::Web,
        };
        let ttl = cache_ttl(intent, qpart);
        if Duration::from_secs(age) < ttl {
            map.insert(
                key,
                (Instant::now() - Duration::from_secs(age), results, total),
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
