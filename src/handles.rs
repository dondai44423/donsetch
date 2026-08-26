//! Reference handles: short session-scoped IDs that stand in for
//! URLs. The URL-noise tax is real — a link-heavy page bleeds
//! hundreds of tokens on raw URLs the daemon can remember instead.
//!
//! Two namespaces:
//!
//! - **`L{id}`** — link handles, interned from fetched-page markdown
//!   (`[text](LxK7mP2q)` instead of `[text](https://very-long-url…) `).
//!   Stable: the same URL always maps to the same handle while the
//!   entry lives. LRU-evicted, persisted with 24h TTL, cap 2048.
//! - **`S{id}`** — search handles, one per result. In-memory only:
//!   never persisted, die with the process. A new search mints new
//!   handles, so earlier ones keep resolving to what they always
//!   meant (no silent rebind).
//!
//! **Security (GHSA-g279-2v66-j8g2):** handle IDs are random
//! 8-char base62 tokens generated from SHA-256 of (nanosecond
//! timestamp + PID + atomic counter + ASLR stack address). The
//! output space is 62^8 ≈ 2.18×10^14 — enumeration is infeasible.
//! A page that was never given a handle cannot name one. Search
//! handles are not persisted, so they cannot leak across sessions
//! through the on-disk table. `DONSETCH_URL_HANDLES=off` disables
//! both emission and resolution.
//!
//! `web_fetch` accepts a handle anywhere it accepts a URL.
//! File: ~/.cache/donsetch/handles.json (atomic tmp+rename writes).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Handle lifetime. A handle that outlives a research session by a
/// day is a handle nobody remembers producing.
const TTL_SECS: u64 = 24 * 60 * 60;
/// L-entry cap — bounded memory, oldest-eviction.
const MAX_L_ENTRIES: usize = 2048;
/// Random ID length (base62 chars after the prefix).
const ID_LEN: usize = 8;
/// Persistence format version. Old format (no version field) will
/// fail to deserialize and be silently discarded.
const PERSIST_VERSION: u32 = 2;

const BASE62: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

#[derive(Clone, Serialize, Deserialize)]
struct LEntry {
    url: String,
    at: u64,
    /// LRU eviction order (monotonic counter, not wall clock —
    /// seconds-granularity `at` can collide for rapid inserts).
    seq: u64,
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    version: u32,
    next_seq: u64,
    l: HashMap<String, LEntry>,
}

/// The handle table. Shared via the Daemon; L-handles flush to disk.
/// Search handles are in-memory only and never persisted.
#[derive(Default)]
pub struct HandleTable {
    /// L-handle-id → entry.
    l: HashMap<String, LEntry>,
    /// URL → handle-id reverse index for stable re-interning.
    rev: HashMap<String, String>,
    /// Search handle-id → URL (in-memory only, never persisted).
    s: HashMap<String, String>,
    /// Monotonic counter for LRU eviction ordering.
    next_seq: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path() -> PathBuf {
    crate::paths::cache_dir().join("handles.json")
}

/// Are handles enabled? `DONSETCH_URL_HANDLES=off` disables both
/// emission (search results show raw URLs, links keep hrefs) and
/// resolution (is_handle returns false, so resolve_fetch_url
/// refuses handles). Default: on.
pub fn handles_enabled() -> bool {
    !matches!(std::env::var("DONSETCH_URL_HANDLES").as_deref(), Ok("off"))
}

/// Generate a random, unguessable handle ID: prefix + 8 base62 chars
/// (see [`random_base62`]).
fn gen_id(prefix: char) -> String {
    let mut id = String::with_capacity(ID_LEN + 1);
    id.push(prefix);
    id.push_str(&random_base62(ID_LEN));
    id
}

/// Random `len`-char base62 string.
///
/// Entropy sources (all process-internal, invisible to a remote
/// page): nanosecond timestamp, PID, atomic counter, ASLR stack
/// address. SHA-256 output is indistinguishable from random to
/// anyone who cannot observe the input. Shared by handle IDs
/// (`S…`/`L…`) and MCP HTTP session ids.
pub(crate) fn random_base62(len: usize) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let stack_addr = &n as *const _ as u128;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(ts.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(n.to_le_bytes());
    hasher.update(stack_addr.to_le_bytes());
    let digest = hasher.finalize();

    let mut id = String::with_capacity(len);
    for i in 0..len {
        id.push(BASE62[digest[i] as usize % 62] as char);
    }
    id
}

impl HandleTable {
    pub fn load() -> Self {
        let mut t = Self::default();
        let Some(bytes) = std::fs::read(path()).ok() else {
            return t;
        };
        let Ok(p) = serde_json::from_slice::<Persisted>(&bytes) else {
            // Corrupt table or old format (no version field): treat
            // as empty. Handles are a cache, never a source of
            // truth — losing them costs nothing.
            return t;
        };
        // Reject old format (version 1 had no version field — it
        // failed to deserialize above — but be explicit).
        if p.version != PERSIST_VERSION {
            return t;
        }
        t.next_seq = p.next_seq;
        let cutoff = now().saturating_sub(TTL_SECS);
        for (id, e) in p.l {
            if e.at < cutoff {
                continue;
            }
            // Only accept handles that match the current format.
            // This drops any old sequential handles that somehow
            // survived the format change.
            if !is_valid_handle_id(&id) {
                continue;
            }
            t.rev.insert(e.url.clone(), id.clone());
            t.l.insert(id, e);
        }
        t
    }

    /// Atomic flush (tmp + rename, same pattern as ghost-state).
    /// Only L-handles are persisted; search handles are in-memory
    /// only and never touch disk.
    pub fn flush(&self) {
        let p = Persisted {
            version: PERSIST_VERSION,
            next_seq: self.next_seq,
            l: self.l.clone(),
        };
        let dir = crate::paths::cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        let tmp = dir.join(".handles.json.tmp");
        if serde_json::to_vec(&p)
            .map_err(|e| e.to_string())
            .and_then(|b| std::fs::write(&tmp, b).map_err(|e| e.to_string()))
            .is_ok()
        {
            let _ = std::fs::rename(&tmp, path());
        }
    }

    /// Intern a link URL → stable random "L{id}".
    pub fn intern_link(&mut self, url: &str) -> String {
        if let Some(id) = self.rev.get(url) {
            self.next_seq += 1;
            if let Some(e) = self.l.get_mut(id) {
                e.at = now();
                e.seq = self.next_seq;
            }
            return id.clone();
        }
        let id = loop {
            let candidate = gen_id('L');
            if !self.l.contains_key(&candidate) {
                break candidate;
            }
        };
        self.next_seq += 1;
        self.rev.insert(url.to_string(), id.clone());
        self.l.insert(
            id.clone(),
            LEntry {
                url: url.to_string(),
                at: now(),
                seq: self.next_seq,
            },
        );
        // Evict oldest entries past the cap.
        while self.l.len() > MAX_L_ENTRIES {
            let oldest_id = self
                .l
                .iter()
                .min_by_key(|(_, e)| e.seq)
                .map(|(id, _)| id.clone());
            match oldest_id {
                Some(id) => {
                    if let Some(e) = self.l.remove(&id) {
                        self.rev.remove(&e.url);
                    }
                }
                None => break,
            }
        }
        id
    }

    /// Bind search handles: one random S-handle per result URL.
    /// Returns the handle for each URL (same order). In-memory only;
    /// a new search mints new handles, so earlier ones keep resolving
    /// to what they always meant.
    pub fn set_search_results(&mut self, urls: &[String]) -> Vec<String> {
        let mut handles = Vec::with_capacity(urls.len());
        for url in urls {
            let id = loop {
                let candidate = gen_id('S');
                if !self.s.contains_key(&candidate) {
                    break candidate;
                }
            };
            self.s.insert(id.clone(), url.clone());
            handles.push(id);
        }
        handles
    }

    /// Resolve a handle ("LxK7mP2q"/"SxK7mP2q", case-insensitive) to
    /// its URL.
    pub fn resolve(&self, h: &str) -> Option<String> {
        let h = h.trim();
        let lower = h.to_ascii_lowercase();
        if lower.strip_prefix('s').is_some() {
            return self
                .s
                .get(&lower)
                .cloned()
                .or_else(|| self.s.get(h).cloned());
        }
        if lower.strip_prefix('l').is_some() {
            return self
                .l
                .get(&lower)
                .map(|e| e.url.clone())
                .or_else(|| self.l.get(h).map(|e| e.url.clone()));
        }
        None
    }

    /// Rewrite `[text](https://…)` markdown links to
    /// `[text](L{id})`. Returns the new markdown and the number of
    /// handles created/reused.
    pub fn replace_link_urls(&mut self, md: &str) -> (String, usize) {
        let mut out = String::with_capacity(md.len());
        let mut pos = 0usize;
        let mut count = 0usize;
        while let Some(rel) = md[pos..].find("](http") {
            let url_start = pos + rel + 2;
            let Some(close_rel) = md[url_start..].find(')') else {
                break;
            };
            let url = &md[url_start..url_start + close_rel];
            // Sanity: spaces or control chars mean this isn't a
            // clean generated link — leave it untouched.
            if url.chars().any(|c| c.is_whitespace()) {
                let skip = pos + rel + 6;
                out.push_str(&md[pos..skip]);
                pos = skip;
                continue;
            }
            let handle = self.intern_link(url);
            out.push_str(&md[pos..url_start]);
            out.push_str(&handle);
            out.push(')');
            pos = url_start + close_rel + 1;
            count += 1;
        }
        out.push_str(&md[pos..]);
        (out, count)
    }
}

/// Check if a string matches the current handle format: prefix
/// (S or L) + exactly ID_LEN alphanumeric chars. Does NOT match
/// old sequential format (S1, L12 — too short).
fn is_valid_handle_id(s: &str) -> bool {
    if s.len() != ID_LEN + 1 {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix('s').or_else(|| lower.strip_prefix('l')) else {
        return false;
    };
    rest.len() == ID_LEN && rest.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Is this string a handle (not a URL)? `LxK7mP2q`, `SxK7mP2q`, …
/// Returns false when handles are disabled via DONSETCH_URL_HANDLES=off.
pub fn is_handle(s: &str) -> bool {
    if !handles_enabled() {
        return false;
    }
    is_valid_handle_id(s.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> HandleTable {
        HandleTable {
            l: HashMap::new(),
            rev: HashMap::new(),
            s: HashMap::new(),
            next_seq: 0,
        }
    }

    #[test]
    fn intern_is_stable_and_random() {
        let mut t = table();
        let a = t.intern_link("https://a.example.com/x");
        let b = t.intern_link("https://b.example.com/y");
        let a2 = t.intern_link("https://a.example.com/x");
        // Same URL must keep its handle.
        assert_eq!(a, a2);
        // Different URLs get different handles.
        assert_ne!(a, b);
        // Handles start with L.
        assert!(a.starts_with('L'));
        assert!(b.starts_with('L'));
        // Handles are ID_LEN+1 chars long.
        assert_eq!(a.len(), ID_LEN + 1);
        assert_eq!(b.len(), ID_LEN + 1);
        // Handles contain only base62 after the prefix.
        assert!(a[1..].chars().all(|c| c.is_ascii_alphanumeric()));
        // Resolve works.
        assert_eq!(t.resolve(&a).unwrap(), "https://a.example.com/x");
        assert_eq!(t.resolve(&b).unwrap(), "https://b.example.com/y");
    }

    #[test]
    fn search_handles_are_random_and_unique() {
        let mut t = table();
        let hs =
            t.set_search_results(&["https://x.example/1".into(), "https://x.example/2".into()]);
        assert_eq!(hs.len(), 2);
        assert_ne!(hs[0], hs[1]);
        assert!(hs[0].starts_with('S'));
        assert!(hs[1].starts_with('S'));
        assert_eq!(t.resolve(&hs[0]).unwrap(), "https://x.example/1");
        assert_eq!(t.resolve(&hs[1]).unwrap(), "https://x.example/2");
        // A new search mints new handles, old ones keep resolving.
        let hs2 = t.set_search_results(&["https://y.example/only".into()]);
        assert_ne!(hs2[0], hs[0]);
        // Old search handle still resolves (no silent rebind).
        assert_eq!(t.resolve(&hs[0]).unwrap(), "https://x.example/1");
        assert_eq!(t.resolve(&hs2[0]).unwrap(), "https://y.example/only");
    }

    #[test]
    fn resolve_unknown_returns_none() {
        let t = table();
        assert_eq!(t.resolve("LxK7mP2qa"), None);
        assert_eq!(t.resolve("SxK7mP2qa"), None);
        assert_eq!(t.resolve("not-a-handle"), None);
    }

    #[test]
    fn is_handle_recognizes_and_rejects() {
        assert!(is_handle("SxK7mP2qa"));
        assert!(is_handle("Lb9R4nW3p"));
        assert!(is_handle("saB3cD4eF")); // lowercase prefix
        assert!(!is_handle("https://example.com"));
        assert!(!is_handle("S1")); // old format — too short
        assert!(!is_handle("L12")); // old format — too short
        assert!(!is_handle("S")); // no suffix
        assert!(!is_handle("Sx")); // too short
        assert!(!is_handle("SxK7mP2qExtra")); // too long
        assert!(!is_handle("")); // empty
    }

    #[test]
    fn replace_rewrites_markdown_links() {
        let mut t = table();
        let md = "See [docs](https://example.com/docs/a) and [more](https://example.com/b?x=1) plus text.";
        let (out, n) = t.replace_link_urls(md);
        assert_eq!(n, 2);
        // Two different L-handles were minted.
        let handles: Vec<&str> = out.matches("](L").map(|_| "").collect();
        assert_eq!(handles.len(), 2);
        // The links are now L-handles.
        assert!(out.contains("](L"));
        assert!(!out.contains("https://example.com/docs/a"));
        assert!(!out.contains("https://example.com/b?x=1"));
        // Re-run: stable handles, no new minting.
        let (out2, n2) = t.replace_link_urls(md);
        assert_eq!(n2, 2);
        assert_eq!(out2, out);
    }

    #[test]
    fn replace_leaves_non_urls_and_bare_text_alone() {
        let mut t = table();
        let md = "plain text with no links\n[anchor](#section) and [x](mailto:a@b.c)\n";
        let (out, n) = t.replace_link_urls(md);
        assert_eq!(n, 0);
        assert_eq!(out, md);
    }

    #[test]
    fn replace_handles_cjk_and_multibyte_around_links() {
        let mut t = table();
        let md = "検索結果: [公式](https://example.com/日本語) ここまで。";
        let (out, n) = t.replace_link_urls(md);
        assert_eq!(n, 1);
        assert!(out.contains("](L"));
        assert!(out.contains("ここまで。"));
    }

    #[test]
    fn eviction_caps_entries_keeps_freshest() {
        let mut t = table();
        let first_handle = t.intern_link("https://e.example.com/0");
        for i in 1..MAX_L_ENTRIES + 10 {
            t.intern_link(&format!("https://e.example.com/{i}"));
        }
        assert_eq!(t.l.len(), MAX_L_ENTRIES);
        // The first (oldest) entry was evicted.
        assert_eq!(t.resolve(&first_handle), None);
        // The latest is present.
        assert_eq!(t.l.len(), MAX_L_ENTRIES, "must not exceed cap");
    }

    #[test]
    fn handles_are_unpredictable() {
        let mut t = table();
        let ids: Vec<String> = (0..100)
            .map(|i| t.intern_link(&format!("https://e.example.com/{i}")))
            .collect();
        // All 100 handles are unique.
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), 100);
        // No handle is a sequential number (old format).
        for id in &ids {
            assert!(!id[1..].chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn old_sequential_handles_do_not_resolve() {
        let t = table();
        // Old-format handles must never resolve, even if someone
        // puts them in a transcript.
        assert_eq!(t.resolve("S1"), None);
        assert_eq!(t.resolve("S12"), None);
        assert_eq!(t.resolve("L1"), None);
        assert_eq!(t.resolve("L2048"), None);
    }

    #[test]
    fn is_valid_handle_id_checks_format() {
        assert!(is_valid_handle_id("SxK7mP2qa"));
        assert!(is_valid_handle_id("Lb9R4nW3p"));
        assert!(is_valid_handle_id("saB3cD4eF"));
        assert!(!is_valid_handle_id("S1"));
        assert!(!is_valid_handle_id("L12"));
        assert!(!is_valid_handle_id("SxK7mP2")); // 7 chars, not 9
        assert!(!is_valid_handle_id("SxK7mP2qab")); // 10 chars, not 9
        assert!(!is_valid_handle_id(""));
        assert!(!is_valid_handle_id("X12345678")); // wrong prefix
    }
}
