//! The crawl dossier: one bounded record per host beside the v5 state.
//!
//! The map writes it; a repeat map inside its TTL serves it; the planner
//! and the delta read it later. It is not a second cache: pages live in the
//! fetch cache, contracts in the v5 state. This is the site model, the last
//! map's families and the flat URL view.

use super::ModelSummary;
use super::infer::Family;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const VERSION: u32 = 3;
/// Hosts kept before the oldest dossiers go. A bounded store, like the v5
/// state's origin cap.
const KEEP: usize = 256;
/// Families persisted per host; the top of the table is the planner's map.
const FAMILY_CAP: usize = 4096;
/// Fingerprint records kept per host. The dossier is the delta memory, not
/// a second page cache: no markdown, only the change signal.
pub const PAGE_CAP: usize = 4096;
/// Previous inventory URLs kept for removed-page reporting. A site over
/// this bound still crawls; its removed report is bounded and says so.
pub const INVENTORY_CAP: usize = 20_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeContract {
    pub inventory_hash: String,
    pub source: String,
    /// True when the site's sitemap gave lastmod values worth trusting.
    pub sitemap_lastmod_trust: bool,
    /// True when the same inventory source/hash is a stable page list.
    pub page_list_stable: bool,
    pub checked_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageMemory {
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub lastmod: Option<String>,
    #[serde(default)]
    pub chars: usize,
    #[serde(default)]
    pub fetched_at: u64,
    #[serde(default)]
    pub last_used: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SiteInfo {
    pub host: String,
    pub seed: String,
    pub urls: usize,
    pub families: usize,
    pub pages: usize,
    pub generated_at: u64,
    pub source: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Dossier {
    pub version: u32,
    pub seed: String,
    /// The focus the map was built with; a different focus is a fresh map.
    pub focus: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub map_cap: usize,
    pub same_host: bool,
    pub respect_robots: bool,
    pub summary: ModelSummary,
    pub families: Vec<Family>,
    /// The flat URL view, capped by `map_cap`.
    pub map: Vec<String>,
    /// The change contract learned by the last map.
    #[serde(default)]
    pub contract: Option<ChangeContract>,
    /// The last full in-scope inventory, bounded; removed-page reports use it.
    #[serde(default)]
    pub inventory: Vec<String>,
    /// Per-URL change memory, bounded by `PAGE_CAP`.
    #[serde(default)]
    pub pages: BTreeMap<String, PageMemory>,
}

/// Everything a map writes for the next one. A struct, not nine arguments.
pub struct Snapshot<'a> {
    pub summary: &'a ModelSummary,
    pub families: &'a [Family],
    pub map: &'a [String],
    /// Full current inventory for removed-page reporting.
    pub inventory: &'a [String],
    pub contract: Option<&'a ChangeContract>,
    pub focus: Option<&'a str>,
    pub include: &'a [String],
    pub exclude: &'a [String],
    pub map_cap: usize,
    pub same_host: bool,
    pub respect_robots: bool,
}

/// `cache/crawl/dossier`: beside the v5 state, under the same cache root.
pub fn dir() -> PathBuf {
    crate::paths::cache_dir().join("crawl").join("dossier")
}

fn file(host: &str) -> PathBuf {
    dir().join(format!("{}.json", safe_name(host)))
}

fn safe_name(host: &str) -> String {
    let mut s: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s.push_str("unknown");
    }
    s.truncate(120);
    s
}

pub fn load(host: &str) -> Option<Dossier> {
    let text = std::fs::read_to_string(file(host)).ok()?;
    let dossier: Dossier = serde_json::from_str(&text).ok()?;
    (dossier.version == VERSION).then_some(dossier)
}

/// Write the summary, the family table and the URL view atomically. A
/// dossier failure is not a map failure: the map still answers, and the
/// next map retries.
pub fn save(host: &str, snapshot: &Snapshot<'_>) -> bool {
    let dir = dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let previous = load(host);
    let mut dossier = Dossier {
        version: VERSION,
        seed: snapshot.summary.seed.clone(),
        focus: snapshot.focus.map(str::to_string),
        include: snapshot.include.to_vec(),
        exclude: snapshot.exclude.to_vec(),
        map_cap: snapshot.map_cap,
        same_host: snapshot.same_host,
        respect_robots: snapshot.respect_robots,
        summary: snapshot.summary.clone(),
        families: snapshot.families.iter().take(FAMILY_CAP).cloned().collect(),
        map: snapshot.map.to_vec(),
        contract: snapshot.contract.cloned(),
        inventory: snapshot
            .inventory
            .iter()
            .take(INVENTORY_CAP)
            .cloned()
            .collect(),
        pages: previous
            .as_ref()
            .map(|d| d.pages.clone())
            .unwrap_or_default(),
    };
    if dossier.contract.is_none()
        && let Some(old) = &previous
    {
        dossier.contract = old.contract.clone();
    }
    if dossier.inventory.is_empty()
        && let Some(old) = &previous
    {
        dossier.inventory = old.inventory.clone();
    }
    trim_pages(&mut dossier);
    write_dossier(&dossier)
}

/// Record page change memory after a run. The dossier owns the fingerprints;
/// the job owns the full page bytes.
#[derive(Debug, Clone)]
pub struct PageUpdate {
    pub url: String,
    pub fingerprint: Option<String>,
    pub lastmod: Option<String>,
    pub chars: usize,
    pub fetched_at: u64,
}

pub fn remember_pages(host: &str, updates: &[PageUpdate]) {
    let Some(mut dossier) = load(host) else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for u in updates {
        dossier.pages.insert(
            u.url.clone(),
            PageMemory {
                fingerprint: u.fingerprint.clone(),
                lastmod: u.lastmod.clone(),
                chars: u.chars,
                fetched_at: u.fetched_at,
                last_used: now,
            },
        );
    }
    trim_pages(&mut dossier);
    let _ = write_dossier(&dossier);
}

fn trim_pages(dossier: &mut Dossier) {
    trim_pages_to(dossier, PAGE_CAP);
}

fn trim_pages_to(dossier: &mut Dossier, cap: usize) {
    if dossier.pages.len() <= cap {
        return;
    }
    let mut keys: Vec<(u64, u64, String)> = dossier
        .pages
        .iter()
        .map(|(k, v)| (v.last_used, v.fetched_at, k.clone()))
        .collect();
    keys.sort_unstable();
    for (_, _, k) in keys.into_iter().take(dossier.pages.len() - cap) {
        dossier.pages.remove(&k);
    }
}

/// List dossiers newest first. `sites` uses this; it never reads a page.
pub fn list() -> Vec<SiteInfo> {
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return Vec::new();
    };
    let mut sites: Vec<SiteInfo> = entries
        .flatten()
        .filter_map(|e| {
            let host = e.path().file_stem()?.to_str()?.to_string();
            let d = load(&host)?;
            Some(SiteInfo {
                host: d.summary.host.clone(),
                seed: d.seed.clone(),
                urls: d.summary.urls,
                families: d.summary.families,
                pages: d.pages.len(),
                generated_at: d.summary.generated_at,
                source: d.summary.source.clone(),
            })
        })
        .collect();
    sites.sort_by_key(|s| std::cmp::Reverse(s.generated_at));
    sites
}

/// Remove one host's dossier. The fetch cache is deliberately untouched.
pub fn forget(host: &str) -> bool {
    std::fs::remove_file(file(host)).is_ok()
}

fn write_dossier(dossier: &Dossier) -> bool {
    let Ok(bytes) = serde_json::to_vec_pretty(dossier) else {
        return false;
    };
    let path = file(&dossier.summary.host);
    let dir_path = dir();
    let tmp = dir_path.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let Ok(mut f) = options.open(&tmp) else {
        return false;
    };
    if f.write_all(&bytes).is_err() || f.flush().is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    drop(f);
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&path);
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
    }
    prune(&dir_path);
    true
}

/// Keep the newest KEEP dossiers; the rest are old maps of sites nobody
/// asked about again.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                return None;
            }
            let modified = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            Some((modified, path))
        })
        .collect();
    if files.len() <= KEEP {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    for (_, path) in files.iter().take(files.len() - KEEP) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_memory_evicts_the_least_recently_used() {
        let mut d = Dossier::default();
        for (i, key) in ["a", "b", "c"].iter().enumerate() {
            d.pages.insert(
                (*key).to_string(),
                PageMemory {
                    fingerprint: Some(key.to_string()),
                    last_used: i as u64,
                    fetched_at: i as u64,
                    ..Default::default()
                },
            );
        }
        trim_pages_to(&mut d, 2);
        assert!(!d.pages.contains_key("a"));
        assert!(d.pages.contains_key("b"));
        assert!(d.pages.contains_key("c"));
    }
}
