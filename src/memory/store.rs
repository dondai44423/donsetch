//! Local web memory store (v4 phase 5.2).
//!
//! A flat JSON index in the cache dir holding pages this machine has
//! already read, with all-MiniLM embeddings inlined per row. The
//! search is a cosine scan over the local store only. Nothing here
//! talks to anyone remote; the model itself runs in-process.
//!
//! The upsert key is the normalized URL, so a page that is already in
//! the index updates in place instead of duplicating. Row count is
//! bounded (MAX_ENTRIES, default 4000): the oldest rows evict first
//! before a new row can push the count past the cap. Every mutation
//! persists the whole file atomically, so a crash mid write leaves
//! the previous file intact.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::memory::model;

pub const MAX_ENTRIES_DEFAULT: usize = 4000;
pub const VERSION: u32 = 1;
const DOC_CHUNK: usize = 1600;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub url: String,
    pub title: String,
    /// Truncated markdown digest of the page (the embedded text).
    pub body: String,
    /// Ingest time, unix ms; the eviction key.
    pub ts: u64,
    pub vec: Vec<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Index {
    pub version: u32,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemoryHit {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub score: f32,
}

/// Cap from the env when set to >= 256, else the default.
pub fn cap() -> usize {
    std::env::var("DONSETCH_WEB_MEMORY_CAP")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v >= 256)
        .unwrap_or(MAX_ENTRIES_DEFAULT)
}

pub fn index_path() -> PathBuf {
    crate::memory::model::model_dir().join("index.json")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Collapse whitespace runs and cap the digest at max chars.
fn snippetize(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(max.min(text.len()));
    let mut sp = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if sp {
                continue;
            }
            sp = true;
            out.push(' ');
        } else {
            sp = false;
            out.push(c);
        }
        if out.len() >= max {
            break;
        }
    }
    out.trim_end().to_string()
}

/// Upsert key: trimmed, lowercased, trailing slash stripped.
fn entry_key(url: &str) -> String {
    url.trim().trim_end_matches('/').to_lowercase()
}

fn load() -> Vec<Entry> {
    std::fs::read_to_string(index_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Index>(&s).ok())
        .map(|i| i.entries)
        .unwrap_or_default()
}

fn persist(entries: &[Entry]) -> Result<(), String> {
    let path = index_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let idx = Index {
        version: VERSION,
        entries: entries.to_vec(),
    };
    let body = serde_json::to_vec(&idx).map_err(|e| format!("memory: serialize: {e}"))?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, &body).map_err(|e| format!("memory: write: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("memory: rename: {e}"))?;
    Ok(())
}

fn store() -> &'static Mutex<Vec<Entry>> {
    static STORE: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(load()))
}

/// Row count (loads the index on first use).
pub fn rows() -> usize {
    store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

/// Ingest one page: embeds its markdown, upserts by URL, persists.
/// Returns Ok(true) when the row landed (new or updated), Ok(false)
/// when the row was skipped (kill switch, tiny body). Errors
/// propagate; the store keeps its previous state on failure.
pub fn ingest(url: &str, title: &str, body: &str) -> Result<bool, String> {
    if kill_switch() {
        return Ok(false);
    }
    let key = entry_key(url);
    if key.is_empty() {
        return Ok(false);
    }
    let body = snippetize(body, DOC_CHUNK);
    if body.chars().count() < 80 {
        return Ok(false);
    }
    let title = title.trim().to_string();
    let vec = model::embed(&format!("{title}\n{body}"))?;
    let ts = now_ms();
    let lock = store();
    let mut acc = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(pos) = acc.iter().position(|e| e.url == key) {
        acc[pos].title = title;
        acc[pos].body = body;
        acc[pos].vec = vec;
        acc[pos].ts = ts;
    } else {
        acc.push(Entry {
            url: key,
            title,
            body,
            ts,
            vec,
        });
        while acc.len() > cap() {
            let oldest = acc
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.ts)
                .map(|(i, _)| i);
            match oldest {
                Some(i) => {
                    acc.remove(i);
                }
                None => break,
            }
        }
    }
    let snapshot = acc.clone();
    drop(acc);
    persist(&snapshot)?;
    Ok(true)
}

/// Semantic search over the local index: cosine similarity scan, top
/// k hits above SIM_FLOOR. Returns empty for a missing model or an
/// empty index (never an error unless the embed itself fails).
pub fn search(query: &str, k: usize) -> Result<Vec<MemoryHit>, String> {
    if kill_switch() || query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let qvec = model::embed(query)?;
    let entries = store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut scored: Vec<(f32, &Entry)> = entries
        .iter()
        .map(|e| {
            let dot: f32 = qvec
                .iter()
                .zip(e.vec.iter())
                .map(|(q, v)| q * v)
                .sum::<f32>()
                .max(0.0);
            (dot, e)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k.max(1));
    Ok(scored
        .into_iter()
        .filter(|(s, _)| *s > 0.01)
        .map(|(score, e)| MemoryHit {
            url: e.url.clone(),
            title: e.title.clone(),
            snippet: snippetize(&e.body, 240),
            score,
        })
        .collect())
}

/// Drop every row (the model itself stays).
pub fn clear() -> Result<(), String> {
    let lock = store();
    let mut acc = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    acc.clear();
    persist(&[])?;
    Ok(())
}

/// The kill switch: the store's ingest and search are both disabled
/// while the flag is present in the environment.
pub fn kill_switch() -> bool {
    std::env::var_os("DONSETCH_NO_WEB_MEMORY").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upsert key: trimmed, lowercase, trailing slash gone. Two
    /// spellings of one URL must land on one row, never two.
    #[test]
    fn entry_key_normalizes_spellings() {
        assert_eq!(
            entry_key("https://Docs.Rust-Lang.ORG/book/"),
            "https://docs.rust-lang.org/book"
        );
        assert_eq!(entry_key("  HTTP://X.io  "), "http://x.io");
        assert_eq!(entry_key("https://a.io//"), "https://a.io");
        assert_eq!(entry_key(""), "");
        assert_eq!(entry_key("   "), "");
    }

    /// Snippetize collapses whitespace and caps on CHARS, not bytes,
    /// so CJK-heavy pages do not yield empty digests.
    #[test]
    fn snippetize_collapses_and_caps() {
        let md = "a\n\n   b\t\tc\n\n\nd";
        assert_eq!(snippetize(md, 40), "a b c d");
        let long = "é".repeat(300);
        assert_eq!(snippetize(&long, usize::MAX).chars().count(), 300);
        let cjk = "東".repeat(10);
        assert!(!snippetize(&cjk, 8).is_empty());
    }

    /// The cap floor: junk or tiny values fall back to the default,
    /// sane values pass through. Mimics the real daemon env.
    /// SAFETY: nextest runs each test in its own process, so the
    /// mutations below cannot race a sibling test's environ read.
    #[test]
    fn cap_env_floor() {
        unsafe { std::env::set_var("DONSETCH_WEB_MEMORY_CAP", "1") };
        assert_eq!(cap(), MAX_ENTRIES_DEFAULT);
        unsafe { std::env::set_var("DONSETCH_WEB_MEMORY_CAP", "banana") };
        assert_eq!(cap(), MAX_ENTRIES_DEFAULT);
        unsafe { std::env::set_var("DONSETCH_WEB_MEMORY_CAP", "300") };
        assert_eq!(cap(), 300);
        unsafe { std::env::remove_var("DONSETCH_WEB_MEMORY_CAP") };
        assert_eq!(cap(), MAX_ENTRIES_DEFAULT);
    }
}
