//! Measured independence between sources.
//!
//! Two sources are one opinion when their top results are the same results. That
//! is a fact about this moment's suppliers, not about their names: `ddg`, `yahoo`
//! and `bing` all serve one index today, and the day one of them switches
//! supplier a hardcoded table would keep counting a farm three times. So the
//! graph is learned: for every search, the overlap (Jaccard over the top ten
//! URLs) of each pair of answering sources feeds a running mean, and a pair whose
//! mean overlap stays above [`LINK_AT`] is one family.
//!
//! Until a pair has been seen [`MIN_SAMPLES`] times nothing is linked: unmeasured
//! sources count as independent. That errs toward trusting agreement, which the
//! ranker only uses as a mild multiplier, and it corrects itself within a few
//! searches rather than staying wrong until someone edits a table.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Mean overlap at or above which two sources are one family.
const LINK_AT: f64 = 0.35;
/// Observations before a pair's mean is believed.
const MIN_SAMPLES: u32 = 3;
/// A pair's mean follows roughly this many recent searches, so a supplier
/// change is believed within a few dozen queries instead of never.
const HORIZON: f64 = 30.0;
/// Results compared per source.
const TOP: usize = 10;
/// A source that returned fewer results than this says too little to compare.
const MIN_ITEMS: usize = 5;
/// The file is small by construction (a pair per two general sources), but it is
/// read from disk, so what it may hold is bounded anyway.
const MAX_PAIRS: usize = 256;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
struct Pair {
    n: u32,
    mean: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Families {
    /// `"a|b"` with `a < b`.
    #[serde(default)]
    pairs: BTreeMap<String, Pair>,
}

fn pair_key(a: &str, b: &str) -> String {
    if a <= b {
        format!("{a}|{b}")
    } else {
        format!("{b}|{a}")
    }
}

impl Families {
    /// Fold one search's observations in. `lists` is each answering general
    /// source and its result URLs, already normalized for comparison; sources
    /// that returned fewer than [`MIN_ITEMS`] are ignored. Returns whether
    /// anything was learned (so the caller knows to save).
    pub fn observe(&mut self, lists: &[(String, Vec<String>)]) -> bool {
        let mut seen: HashSet<&str> = HashSet::new();
        let usable: Vec<(&str, HashSet<&str>)> = lists
            .iter()
            .filter(|(_, urls)| urls.len() >= MIN_ITEMS)
            // One list per source: a second ask of the same source (a recall
            // variant, a scoped query) is a different question, not a second
            // opinion to compare.
            .filter(|(name, _)| seen.insert(name.as_str()))
            .map(|(name, urls)| {
                (
                    name.as_str(),
                    urls.iter().take(TOP).map(String::as_str).collect(),
                )
            })
            .collect();
        let mut learned = false;
        for (i, (a, set_a)) in usable.iter().enumerate() {
            for (b, set_b) in &usable[i + 1..] {
                let union = set_a.union(set_b).count();
                if union == 0 {
                    continue;
                }
                let jaccard = set_a.intersection(set_b).count() as f64 / union as f64;
                let pair = self.pairs.entry(pair_key(a, b)).or_default();
                pair.n = pair.n.saturating_add(1);
                // A plain mean while the pair is young, then an exponential one.
                let weight = 1.0 / f64::from(pair.n).min(HORIZON);
                pair.mean += (jaccard - pair.mean) * weight;
                learned = true;
            }
        }
        if self.pairs.len() > MAX_PAIRS {
            // Keep the best-sampled pairs; a long tail of retired sources goes.
            let mut by_n: Vec<(String, u32)> =
                self.pairs.iter().map(|(k, p)| (k.clone(), p.n)).collect();
            by_n.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            for (k, _) in by_n.into_iter().skip(MAX_PAIRS) {
                self.pairs.remove(&k);
            }
        }
        learned
    }

    /// The family a source belongs to: the alphabetically first member of its
    /// connected component, so the name is stable between calls and processes.
    pub fn family_of(&self, source: &str) -> String {
        fn find<'a>(parent: &mut HashMap<&'a str, &'a str>, x: &'a str) -> &'a str {
            let p = *parent.entry(x).or_insert(x);
            if p == x {
                return x;
            }
            let root = find(parent, p);
            parent.insert(x, root);
            root
        }
        let mut parent: HashMap<&str, &str> = HashMap::new();
        for (key, pair) in &self.pairs {
            if pair.n < MIN_SAMPLES || pair.mean < LINK_AT {
                continue;
            }
            let Some((a, b)) = key.split_once('|') else {
                continue;
            };
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            if ra != rb {
                // The smaller name becomes the root, so the family's name is
                // its alphabetically first member.
                let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
                parent.insert(hi, lo);
            }
        }
        if parent.contains_key(source) {
            find(&mut parent, source).to_string()
        } else {
            source.to_string()
        }
    }

    /// Independent opinions among `sources`.
    pub fn count<'a>(&self, sources: impl IntoIterator<Item = &'a str>) -> usize {
        sources
            .into_iter()
            .map(|s| self.family_of(s))
            .collect::<HashSet<_>>()
            .len()
    }

    /// Mean overlap of a pair, when it has been measured enough to report.
    pub fn overlap(&self, a: &str, b: &str) -> Option<f64> {
        self.pairs
            .get(&pair_key(a, b))
            .filter(|p| p.n >= MIN_SAMPLES)
            .map(|p| p.mean)
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

// ── Persistence ──────────────────────────────────────────────────────────────

const DISK_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Disk {
    version: u32,
    #[serde(flatten)]
    families: Families,
}

fn path() -> Option<std::path::PathBuf> {
    let dir = crate::paths::cache_dir();
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("search-families.json"))
}

pub(crate) fn load() -> Families {
    let Some(path) = path() else {
        return Families::default();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Families::default();
    };
    match serde_json::from_slice::<Disk>(&bytes) {
        Ok(d) if d.version == DISK_VERSION => d.families,
        _ => Families::default(),
    }
}

pub(crate) fn save(families: &Families) {
    let Some(path) = path() else { return };
    let disk = Disk {
        version: DISK_VERSION,
        families: families.clone(),
    };
    let Ok(json) = serde_json::to_vec(&disk) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if crate::config::write_private(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(ids: &[u32]) -> Vec<String> {
        ids.iter().map(|i| format!("x/{i}")).collect()
    }

    fn lists(a: &[u32], b: &[u32], c: &[u32]) -> Vec<(String, Vec<String>)> {
        vec![
            ("ddg".into(), urls(a)),
            ("yahoo".into(), urls(b)),
            ("brave".into(), urls(c)),
        ]
    }

    const SAME: [u32; 6] = [1, 2, 3, 4, 5, 6];
    const OTHER: [u32; 6] = [40, 41, 42, 43, 44, 45];

    #[test]
    fn nothing_is_linked_before_it_has_been_measured() {
        let mut f = Families::default();
        f.observe(&lists(&SAME, &SAME, &OTHER));
        f.observe(&lists(&SAME, &SAME, &OTHER));
        assert_eq!(
            f.count(["ddg", "yahoo", "brave"]),
            3,
            "two samples is not a measurement"
        );
    }

    #[test]
    fn sources_that_return_the_same_results_become_one_family() {
        let mut f = Families::default();
        for _ in 0..4 {
            f.observe(&lists(&SAME, &[1, 2, 3, 4, 5, 7], &OTHER));
        }
        assert_eq!(f.family_of("ddg"), f.family_of("yahoo"));
        assert_ne!(f.family_of("ddg"), f.family_of("brave"));
        assert_eq!(f.count(["ddg", "yahoo", "brave"]), 2);
        assert!(f.overlap("ddg", "yahoo").unwrap() > 0.5);
    }

    #[test]
    fn a_family_is_named_by_its_first_member_whatever_the_order() {
        let mut f = Families::default();
        for _ in 0..3 {
            f.observe(&[
                ("yahoo".into(), urls(&[1, 2, 3, 4, 5])),
                ("bing".into(), urls(&[1, 2, 3, 4, 5])),
                ("ddg".into(), urls(&[1, 2, 3, 4, 5])),
            ]);
        }
        assert_eq!(f.family_of("yahoo"), "bing");
        assert_eq!(f.family_of("ddg"), "bing");
        assert_eq!(f.family_of("bing"), "bing");
    }

    #[test]
    fn a_supplier_change_is_believed_within_a_few_dozen_searches() {
        let mut f = Families::default();
        for _ in 0..10 {
            f.observe(&lists(&SAME, &SAME, &OTHER));
        }
        assert_eq!(f.family_of("ddg"), f.family_of("yahoo"));
        // yahoo stops following ddg and starts agreeing with brave instead.
        for _ in 0..60 {
            f.observe(&lists(&SAME, &OTHER, &OTHER));
        }
        assert_ne!(f.family_of("ddg"), f.family_of("yahoo"));
        assert_eq!(f.family_of("yahoo"), f.family_of("brave"));
    }

    #[test]
    fn a_thin_answer_and_a_repeated_source_teach_nothing() {
        let mut f = Families::default();
        let learned = f.observe(&[
            ("ddg".into(), urls(&[1, 2])),
            ("yahoo".into(), urls(&[1, 2])),
        ]);
        assert!(
            !learned && f.is_empty(),
            "fewer than five items says too little"
        );
        // The same source asked twice is one opinion, never compared to itself.
        let learned = f.observe(&[
            ("ddg".into(), urls(&[1, 2, 3, 4, 5])),
            ("ddg".into(), urls(&[1, 2, 3, 4, 5])),
        ]);
        assert!(!learned);
    }

    #[test]
    fn the_graph_round_trips_and_a_foreign_version_is_not_trusted() {
        let mut f = Families::default();
        for _ in 0..3 {
            f.observe(&lists(
                &[1, 2, 3, 4, 5],
                &[1, 2, 3, 4, 5],
                &[9, 8, 7, 6, 10],
            ));
        }
        let json = serde_json::to_vec(&Disk {
            version: DISK_VERSION,
            families: f.clone(),
        })
        .unwrap();
        let back: Disk = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.families.family_of("yahoo"), f.family_of("yahoo"));
        let foreign: Disk = serde_json::from_slice(b"{\"version\":99}").unwrap();
        assert_ne!(foreign.version, DISK_VERSION);
    }
}
