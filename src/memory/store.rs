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
use std::sync::atomic::{AtomicU64, Ordering};
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
    let tmp = stage_path(&path);
    std::fs::write(&tmp, &body).map_err(|e| format!("memory: write: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("memory: rename: {e}"))?;
    Ok(())
}

/// A staging path unique to this write. The tmp was keyed on the PID
/// alone, which was fine while ingest ran inline in the request task.
/// ingest_async (issue #178) now hands every ingest to the blocking
/// pool, and a single crawl fires it per 256-row chunk AND again on
/// completion, so persists overlap: two writers opening one shared
/// tmp with O_TRUNC truncate and interleave one inode, a rename moves
/// the torn bytes into place, and the next load() parse-fails to an
/// empty index (the whole recall cache silently wiped). A per-write
/// suffix gives each persist its own inode; the rename is atomic, so
/// index.json is always a complete file from some writer. PID stays
/// in the name so a second process still never collides either.
/// (PR #191, adopted; the companion test got the cache-dir isolation
/// the author's SAFETY note claimed but nextest.toml does not provide.)
fn stage_path(path: &std::path::Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("{}.{}.tmp", std::process::id(), n))
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

/// Batched ingest (issue #178): one lock acquisition, one eviction
/// pass, one disk write for a whole page or search batch. Rows that
/// fail the 80-char floor or the entry-key rules are dropped before
/// the model is ever touched. Returns how many rows reached the
/// index (including refreshes). Law 5 unchanged: an embed or
/// persistence failure returns Err as a receipt and nothing is
/// recorded.
pub fn ingest_batch(rows: &[(String, String, String)]) -> Result<usize, String> {
    if kill_switch() || rows.is_empty() {
        return Ok(0);
    }
    let now = now_ms();
    let mut prepped: Vec<(String, String, String)> = Vec::with_capacity(rows.len());
    for (url, title, body) in rows {
        let key = entry_key(url);
        if key.is_empty() {
            continue;
        }
        let body = snippetize(body, DOC_CHUNK);
        if body.chars().count() < 80 {
            continue;
        }
        prepped.push((key, title.clone(), body));
    }
    if prepped.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = prepped
        .iter()
        .map(|(_, t, b)| format!("{t}\n{b}"))
        .collect();
    let vecs = model::embed_batch(&texts)?;
    let lock = store();
    let mut acc = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut changed = 0usize;
    let mut added = 0usize;
    for (row, vec) in prepped.iter().zip(vecs.iter()) {
        let (key, title, body) = row;
        if let Some(pos) = acc.iter().position(|e| e.url == *key) {
            acc[pos].title = title.clone();
            acc[pos].body = body.clone();
            acc[pos].vec = vec.clone();
            acc[pos].ts = now;
        } else {
            acc.push(Entry {
                url: key.clone(),
                title: title.clone(),
                body: body.clone(),
                ts: now,
                vec: vec.clone(),
            });
            added += 1;
        }
        changed += 1;
    }
    if added > 0 {
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
    Ok(changed)
}

/// Fire-and-forget bookkeeping (issue #178): the embed + write run
/// on the blocking pool after the tool response is already on its
/// way, so web memory never holds a response past deadline_ms.
/// Law 5 unchanged: failures surface as a stderr receipt, never as
/// a changed result.
pub fn ingest_async(rows: Vec<(String, String, String)>) {
    if rows.is_empty() {
        return;
    }
    tokio::task::spawn_blocking(move || {
        if let Err(e) = ingest_batch(&rows) {
            eprintln!("[memory] ingest: {e}");
        }
    });
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

    // Two persists that overlap must not share one staging file.
    // The old per-PID-only tmp meant every write in a process opened
    // the SAME path with O_TRUNC; ingest_async (issue #178) made those
    // writes concurrent (per-chunk + on-completion within one crawl),
    // so a second writer could truncate the first mid-flight and the
    // rename would move torn bytes into index.json -> next load()
    // parse-fails to an empty index. Distinct staging paths per write
    // are the invariant that keeps each rename a complete file.
    #[test]
    fn stage_paths_are_unique_per_write() {
        let base = std::path::Path::new("/tmp/donsetch-idx/index.json");
        let a = stage_path(base);
        let b = stage_path(base);
        assert_ne!(a, b, "each persist must stage to its own tmp file");
        assert_ne!(a.file_name(), b.file_name());
        // Both still land beside the target (same dir) and read as tmp.
        assert_eq!(a.parent(), Some(std::path::Path::new("/tmp/donsetch-idx")));
        assert!(a.to_string_lossy().ends_with(".tmp"));
    }

    // A best-effort concurrency guard: many overlapping persists to
    // one path must always leave a parseable, complete index (never a
    // truncated/interleaved corpse). Deterministic reproduction of the
    // race needs a slow filesystem; this at least exercises the path
    // and locks the post-fix guarantee. SAFETY: an in-test
    // DONSETCH_CACHE_DIR isolates index_path(); the naive read of
    // persist() fires into the user's real dir and is forbidden.
    #[test]
    fn concurrent_persist_leaves_a_valid_index() {
        let snap = |tag: usize| -> Vec<Entry> {
            (0..64)
                .map(|i| Entry {
                    url: format!("https://ex{tag}.test/{i}"),
                    title: format!("t{tag}-{i}"),
                    body: "x".repeat(4096),
                    ts: i as u64,
                    vec: vec![0.1f32; 384],
                })
                .collect()
        };
        // Isolate the store dir under a temp DONSETCH_CACHE_DIR before
        // anything resolves cache_dir(); nextest runs one process per
        // test, so the env sticks for this test only. The naive read
        // of persist() (without this) fired 160 overwrite-renames into
        // the developer's real memory index on every test run: the
        // destructive pattern is explicitly forbidden.
        let dir =
            std::env::temp_dir().join(format!("donsetch-test-persist-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        unsafe {
            std::env::set_var("DONSETCH_CACHE_DIR", &dir);
        }

        let handles: Vec<_> = (0..8)
            .map(|tag| {
                let s = snap(tag);
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let _ = persist(&s);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let raw = std::fs::read(index_path()).expect("index present after persists");
        let parsed = serde_json::from_slice::<Index>(&raw);
        assert!(parsed.is_ok(), "index.json must stay valid JSON");
        assert_eq!(
            parsed.unwrap().entries.len(),
            64,
            "a full snapshot survived"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
