//! Page history (v3): fingerprints and diff for re-fetches.
//!
//! Every successful fetch records a knowledge fingerprint : the
//! sha256 of the full pre-pagination markdown. Re-fetching a URL
//! the agent has seen before answers "did this change?" honestly:
//!
//! - `unchanged` : same fingerprint; with `since_last=true` the
//!   result collapses to a one-line verdict.
//! - `changed`/`rewritten` : section-level delta report (added /
//!   removed / changed sections) alongside (or, with since_last,
//!   instead of) the fresh content.
//!
//! The store keeps bounded previous markdown (up to 64KB per URL,
//! text budget 4MB total) so diffs are real, not just verdicts.
//! File: ~/.cache/donsetch/page-history.json (atomic writes).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_URLS: usize = 512;
/// Per-URL stored previous markdown cap.
const PER_TEXT_CAP: usize = 64 * 1024;
/// Total stored text budget across all entries.
const TEXT_BUDGET: usize = 4 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct PageRecord {
    pub fingerprint: String,
    pub at: u64,
    pub total_chars: usize,
    pub title: Option<String>,
    /// Previous markdown (capped) for real diffs. Entries evicted
    /// from the text budget keep fingerprints only.
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    entries: HashMap<String, PageRecord>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path() -> PathBuf {
    crate::paths::cache_dir().join("page-history.json")
}

/// The page-history store. Loaded once into the Daemon.
#[derive(Default)]
pub struct PageHistory {
    entries: HashMap<String, PageRecord>,
}

impl PageHistory {
    pub fn load() -> Self {
        let bytes = match std::fs::read(path()) {
            Ok(b) => b,
            Err(_) => return Self::default(),
        };
        let p: Persisted = serde_json::from_slice(&bytes).unwrap_or_default();
        Self { entries: p.entries }
    }

    /// Fingerprint of normalized full markdown: whitespace-tail
    /// trimmed (renderers differ on trailing newlines), sha256
    /// first 12 hex chars : enough to be collision-honest for
    /// change detection, short enough to show agents.
    pub fn fingerprint(markdown: &str) -> String {
        use sha2::Digest;
        let norm = markdown.trim_end();
        let h = sha2::Sha256::digest(norm.as_bytes());
        h.iter().take(6).map(|b| format!("{b:02x}")).collect()
    }

    /// Record a fetch. Returns the previous record if this URL was
    /// seen before (the caller uses it to compute change status).
    pub fn record(
        &mut self,
        url: &str,
        fingerprint: &str,
        total_chars: usize,
        title: Option<&str>,
        markdown: &str,
    ) -> Option<PageRecord> {
        let prev = self.entries.get(url).cloned();
        self.entries.insert(
            url.to_string(),
            PageRecord {
                fingerprint: fingerprint.to_string(),
                at: now(),
                total_chars,
                title: title.map(String::from),
                text: Some(markdown.chars().take(PER_TEXT_CAP).collect()),
            },
        );
        self.enforce_budget();
        prev
    }

    /// Cap URL count and total text. Oldest-first eviction.
    fn enforce_budget(&mut self) {
        while self.entries.len() > MAX_URLS {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, r)| r.at)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
        let mut total: usize = self
            .entries
            .values()
            .map(|r| r.text.as_ref().map_or(0, |t| t.len()))
            .sum();
        while total > TEXT_BUDGET {
            // Drop text from the oldest entries first.
            let Some(oldest) = self
                .entries
                .iter()
                .filter(|(_, r)| r.text.is_some())
                .min_by_key(|(_, r)| r.at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(r) = self.entries.get_mut(&oldest)
                && let Some(t) = r.text.take()
            {
                total = total.saturating_sub(t.len());
            }
        }
    }

    /// True when the URL is on file with the same content
    /// fingerprint. Powers delta crawl: only pages whose content
    /// changed since the last crawl are returned. No TTL here: an
    /// identical hash means identical content, however old it is.
    pub fn matches_fingerprint(&self, url: &str, fingerprint: &str) -> bool {
        self.entries
            .get(url)
            .is_some_and(|r| r.fingerprint == fingerprint)
    }

    /// Was this URL fetched (and recorded) recently? Powers delta
    /// crawls: `since_last` skips pages whose fingerprint is on
    /// file from the last 24h.
    pub fn has_recent(&self, url: &str) -> bool {
        match self.entries.get(url) {
            Some(r) => now().saturating_sub(r.at) < 24 * 60 * 60,
            None => false,
        }
    }

    pub fn flush(&self) {
        let p = Persisted {
            entries: self.entries.clone(),
        };
        let dir = crate::paths::cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        let tmp = dir.join(".page-history.json.tmp");
        if let Ok(bytes) = serde_json::to_vec(&p)
            && std::fs::write(&tmp, bytes).is_ok()
        {
            let _ = std::fs::rename(&tmp, path());
        }
    }
}

// ── Section diff ─────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub enum ChangeKind {
    Unchanged,
    Minor,
    Changed,
    Rewritten,
}

impl ChangeKind {
    pub fn label(&self) -> &'static str {
        match self {
            ChangeKind::Unchanged => "unchanged",
            ChangeKind::Minor => "minor",
            ChangeKind::Changed => "changed",
            ChangeKind::Rewritten => "rewritten",
        }
    }
}

/// A named section of markdown (split on heading lines).
struct Section {
    title: String,
    body: String,
}

fn split_sections(md: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut cur = Section {
        title: "(intro)".to_string(),
        body: String::new(),
    };
    for line in md.lines() {
        let t = line.trim_start();
        if t.starts_with('#') && t.len() > 1 {
            out.push(std::mem::replace(
                &mut cur,
                Section {
                    title: t.trim_start_matches('#').trim().to_string(),
                    body: String::new(),
                },
            ));
        } else {
            cur.body.push_str(line);
            cur.body.push('\n');
        }
    }
    out.push(cur);
    out
}

fn norm_body(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Classify the overall change between old and new markdown.
pub fn classify_change(old: &str, new: &str) -> ChangeKind {
    let old_sec = split_sections(old);
    let new_sec = split_sections(new);
    let old_n: Vec<String> = old_sec.iter().map(|s| norm_body(&s.body)).collect();
    let new_n: Vec<String> = new_sec.iter().map(|s| norm_body(&s.body)).collect();
    let old_all = old_n.concat();
    let new_all = new_n.concat();
    if old_all == new_all {
        return ChangeKind::Unchanged;
    }
    // Section-level similarity via word-set Jaccard.
    fn words(s: &str) -> std::collections::HashSet<&str> {
        s.split_whitespace().collect()
    }
    let (a, b) = (words(&old_all), words(&new_all));
    let inter = a.intersection(&b).count();
    let union = a.union(&b).count();
    let sim = if union == 0 {
        1.0
    } else {
        inter as f64 / union as f64
    };
    if sim > 0.90 {
        ChangeKind::Minor
    } else if sim > 0.55 {
        ChangeKind::Changed
    } else {
        ChangeKind::Rewritten
    }
}

/// Human/agent-readable section-level delta: which sections were
/// added, removed, or materially changed. Capped : the report must
/// never outweigh the news it delivers.
pub fn section_delta_report(old: &str, new: &str) -> String {
    let old_sec = split_sections(old);
    let new_sec = split_sections(new);
    let norm = |s: &Section| norm_body(&s.body);
    let mut report = Vec::new();

    let mut new_used = vec![false; new_sec.len()];
    for o in &old_sec {
        let on = norm(o);
        // Exact-section match?
        let mut matched = false;
        for (j, n) in new_sec.iter().enumerate() {
            if new_used[j] {
                continue;
            }
            if norm(n) == on {
                new_used[j] = true;
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        // Fuzzy match (≥60% word overlap) → changed; else removed.
        let o_words: std::collections::HashSet<_> = on.split_whitespace().collect();
        let mut best: Option<(f64, usize)> = None;
        for (j, n) in new_sec.iter().enumerate() {
            if new_used[j] {
                continue;
            }
            let nn = norm(n);
            let n_words: std::collections::HashSet<_> = nn.split_whitespace().collect();
            let inter = o_words.intersection(&n_words).count();
            let union = o_words.union(&n_words).count();
            let sim = if union == 0 {
                0.0
            } else {
                inter as f64 / union as f64
            };
            if sim >= 0.6 && best.is_none_or(|(b, _)| sim > b) {
                best = Some((sim, j));
            }
        }
        match best {
            Some((_, j)) => {
                new_used[j] = true;
                report.push(format!("changed: {}", o.title));
            }
            None => report.push(format!("removed: {}", o.title)),
        }
    }
    for (j, used) in new_used.iter().enumerate() {
        if !used && !new_sec[j].body.trim().is_empty() {
            report.push(format!("added: {}", new_sec[j].title));
        }
    }
    let capped: Vec<String> = report.into_iter().take(8).collect();
    if capped.is_empty() {
        "no section-level changes detected".to_string()
    } else {
        capped.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_stable_and_sensitive() {
        let a = PageHistory::fingerprint("# Title\n\nBody text here.");
        let b = PageHistory::fingerprint("# Title\n\nBody text here.");
        let c = PageHistory::fingerprint("# Title\n\nBody text CHANGED.");
        let d = PageHistory::fingerprint("# Title\n\nBody text here.\n\n\n");
        assert_eq!(a, b, "same content = same fingerprint");
        assert_ne!(a, c, "different content = different fingerprint");
        assert_eq!(a, d, "trailing whitespace does not matter");
        assert_eq!(a.len(), 12);
    }

    #[test]
    fn classify_levels() {
        let body: Vec<String> = (0..40).map(|i| format!("w{i}")).collect();
        let body: Vec<&str> = body.iter().map(String::as_str).collect();
        let old = format!("# Page\n\n{}", body.join(" "));
        assert_eq!(classify_change(&old, &old), ChangeKind::Unchanged);
        // A cosmetic edit: two words different out of forty.
        let minor = old.replace("w1 ", "w1fixed ");
        assert_eq!(classify_change(&old, &minor), ChangeKind::Minor);
        // A real edit: a third of the body replaced.
        let mut changed_words = body.clone();
        for w in changed_words.iter_mut().take(14) {
            *w = "fresh";
        }
        let changed = format!("# Page\n\n{}", changed_words.join(" "));
        assert_eq!(classify_change(&old, &changed), ChangeKind::Changed);
        // A rewrite: nothing survives.
        let rewritten = format!(
            "# Page\n\n{}",
            (0..40).map(|_| "entirely").collect::<Vec<_>>().join(" ")
        );
        assert_eq!(classify_change(&old, &rewritten), ChangeKind::Rewritten);
    }

    #[test]
    fn section_delta_names_sections() {
        let old = "# Doc\n\nintro\n\n## Install\n\nrun the installer\n\n## Usage\n\ncall the api";
        let new =
            "# Doc\n\nintro\n\n## Install\n\nrun the new installer script\n\n## FAQ\n\nis it free?";
        let report = section_delta_report(old, new);
        assert!(report.contains("changed: Install"), "{report}");
        assert!(report.contains("removed: Usage"), "{report}");
        assert!(report.contains("added: FAQ"), "{report}");
    }

    #[test]
    fn record_returns_previous_and_evicts() {
        let mut h = PageHistory::default();
        assert!(
            h.record("https://x.example/a", "aaaa", 10, Some("A"), "content a")
                .is_none()
        );
        let prev = h.record("https://x.example/a", "bbbb", 12, Some("A"), "content b");
        assert_eq!(prev.unwrap().fingerprint, "aaaa");
        // Budget eviction: fill past the text budget with fat entries.
        let fat = "x".repeat(64 * 1024);
        for i in 0..80 {
            h.record(
                &format!("https://x.example/{i}"),
                "ffff",
                fat.len(),
                None,
                &fat,
            );
        }
        let total: usize = h
            .entries
            .values()
            .map(|r| r.text.as_ref().map_or(0, |t| t.len()))
            .sum();
        assert!(
            total <= TEXT_BUDGET + PER_TEXT_CAP,
            "text budget blown: {total}"
        );
        assert!(h.entries.len() <= MAX_URLS);
    }
}
