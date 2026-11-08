//! `donsetch search-bench`: the gold-set bench for the keyless search path.
//!
//! It runs `bench/search/queries.json` through the real [`Searcher`], one query
//! at a time at a human pace, and reports where the answer page landed. Two
//! numbers matter beside hit@k: `recall`, whether the answer arrived from *any*
//! source before the merge, and the gap between recall and hit@10, which is the
//! ordering the ranker lost. A miss with no recall is a retrieval problem; a miss
//! with recall is a ranking problem, and they have different fixes.
//!
//! The pace is part of the measurement. Back-to-back queries make every keyless
//! source rate-limit within a minute, so a fast bench measures the bench.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Searcher, egress::EgressPool, render};
use crate::fetch::client::Fetcher;
use crate::profile::BrowserProfile;

#[derive(Debug, Deserialize)]
struct QueryFile {
    queries: Vec<BenchQuery>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct BenchQuery {
    id: String,
    shape: String,
    /// `gold` is the tuning set; `held` is validation and is never tuned against.
    set: String,
    query: String,
    gold: Vec<String>,
}

struct Options {
    queries: PathBuf,
    set: String,
    shape: Option<String>,
    only: Vec<String>,
    smoke: bool,
    limit: Option<usize>,
    pace: Duration,
    max_results: usize,
    out: Option<PathBuf>,
    fresh: bool,
    /// Re-rank the candidates saved in these bench files with the current code,
    /// offline, instead of searching the web.
    replay: Vec<PathBuf>,
    verbose: bool,
}

const USAGE: &str = "usage: donsetch search-bench [--queries FILE] [--set gold|held|all] \
[--shape SHAPE] [--only ID,ID] [--smoke] [--limit N] [--pace-ms N] [--max-results N] \
[--out FILE] [--keep-state] [--replay FILE]...";

/// Ids the smoke subset runs: one per shape, plus a second code query. Six
/// queries, offline-cheap enough for a CI sanity run against the live web.
const SMOKE: &[&str] = &["c1", "e2", "p1", "n4", "b3", "l1"];

fn parse(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        queries: PathBuf::from("bench/search/queries.json"),
        set: "gold".into(),
        shape: None,
        only: Vec::new(),
        smoke: false,
        limit: None,
        pace: Duration::from_millis(4000),
        max_results: 10,
        out: None,
        fresh: true,
        replay: Vec::new(),
        verbose: false,
    };
    let mut i = 0;
    let value = |i: &mut usize| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("{} needs a value\n{USAGE}", args[*i - 1]))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--queries" => o.queries = PathBuf::from(value(&mut i)?),
            "--set" => o.set = value(&mut i)?,
            "--shape" => o.shape = Some(value(&mut i)?),
            "--only" => {
                o.only = value(&mut i)?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect()
            }
            "--smoke" => o.smoke = true,
            "--limit" => o.limit = value(&mut i)?.parse().ok(),
            "--pace-ms" => {
                o.pace = Duration::from_millis(
                    value(&mut i)?
                        .parse()
                        .map_err(|_| "bad --pace-ms".to_string())?,
                )
            }
            "--max-results" => {
                o.max_results = value(&mut i)?
                    .parse()
                    .map_err(|_| "bad --max-results".to_string())?
            }
            "--out" => o.out = Some(PathBuf::from(value(&mut i)?)),
            "--replay" => o.replay.push(PathBuf::from(value(&mut i)?)),
            "-v" | "--verbose" => o.verbose = true,
            "--keep-state" => o.fresh = false,
            "--fresh" => o.fresh = true,
            "-h" | "--help" => return Err(USAGE.to_string()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
        i += 1;
    }
    if !matches!(o.set.as_str(), "gold" | "held" | "all") {
        return Err(format!("--set is gold, held or all\n{USAGE}"));
    }
    Ok(o)
}

/// 1-based rank of the first URL any gold pattern matches.
fn first_match(gold: &[Regex], urls: &[String]) -> Option<usize> {
    urls.iter()
        .position(|u| gold.iter().any(|g| g.is_match(u)))
        .map(|p| p + 1)
}

#[derive(Default)]
struct Tally {
    n: usize,
    hits: Vec<Option<usize>>,
    recall: usize,
    ms: Vec<u64>,
}

impl Tally {
    fn add(&mut self, hit: Option<usize>, recalled: bool, ms: u64) {
        self.n += 1;
        self.hits.push(hit);
        self.recall += usize::from(recalled);
        self.ms.push(ms);
    }

    fn at(&self, k: usize) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        self.hits
            .iter()
            .filter(|h| h.is_some_and(|h| h <= k))
            .count() as f64
            / self.n as f64
    }

    fn mrr(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        self.hits
            .iter()
            .flatten()
            .map(|h| 1.0 / *h as f64)
            .sum::<f64>()
            / self.n as f64
    }

    fn pct(&self, p: f64) -> u64 {
        let mut v = self.ms.clone();
        v.sort_unstable();
        v.get(((v.len() as f64 * p) as usize).min(v.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    }

    fn line(&self, label: &str) -> String {
        format!(
            "{label:<8} n={:<3} hit@1 {:.2}  hit@3 {:.2}  hit@5 {:.2}  MRR {:.3}  recall {:.2}  p50 {:.2}s  p95 {:.2}s",
            self.n,
            self.at(1),
            self.at(3),
            self.at(5),
            self.mrr(),
            if self.n == 0 {
                0.0
            } else {
                self.recall as f64 / self.n as f64
            },
            self.pct(0.5) as f64 / 1000.0,
            self.pct(0.95) as f64 / 1000.0,
        )
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "n": self.n,
            "hit1": self.at(1), "hit3": self.at(3), "hit5": self.at(5),
            "mrr": self.mrr(),
            "recall": if self.n == 0 { 0.0 } else { self.recall as f64 / self.n as f64 },
            "p50_ms": self.pct(0.5), "p95_ms": self.pct(0.95),
        })
    }
}

/// Re-run this binary with a clean cache directory, so one bench run starts from
/// no learned state and shares nothing with the user's own searches.
fn reexec_fresh(args: &[String]) -> i32 {
    let dir = std::env::temp_dir().join(format!("donsetch-search-bench-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    crate::diag!("[search-bench] fresh state in {}", dir.display());
    let Ok(exe) = std::env::current_exe() else {
        crate::diag!("search-bench: cannot find this executable");
        return 1;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("search-bench")
        .args(args.iter().filter(|a| *a != "--fresh"))
        .arg("--keep-state")
        .env("DONSETCH_CACHE_DIR", &dir);
    let code = match cmd.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            crate::diag!("search-bench: {e}");
            1
        }
    };
    let _ = std::fs::remove_dir_all(&dir);
    code
}

#[derive(Deserialize)]
struct SavedRun {
    queries: Vec<SavedQuery>,
}

#[derive(Deserialize)]
struct SavedQuery {
    id: String,
    #[serde(default)]
    raw: Vec<SavedSource>,
}

#[derive(Deserialize)]
struct SavedSource {
    source: String,
    #[serde(default)]
    hits: Vec<SavedHit>,
}

#[derive(Deserialize)]
struct SavedHit {
    title: String,
    url: String,
    #[serde(default)]
    snippet: String,
    rank: usize,
    #[serde(default)]
    published: Option<String>,
}

/// Re-rank saved candidates with the current ranking code. The web supplied the
/// candidates once; every change to the ranker is then measured on identical
/// input, in seconds, with no network noise. Only queries still in the selected
/// set are scored, so tuning on `gold` never reads a `held` row.
fn replay(opts: &Options, queries: &[BenchQuery]) -> i32 {
    use super::{intent, rank};
    let trust = std::collections::HashMap::new();
    let mut all = Tally::default();
    let mut by_shape: std::collections::BTreeMap<String, Tally> = Default::default();
    for path in &opts.replay {
        let saved: SavedRun = match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        {
            Ok(r) => r,
            Err(e) => {
                crate::diag!("search-bench: {}: {e}", path.display());
                return 2;
            }
        };
        let mut run = Tally::default();
        let mut families = super::families::Families::default();
        for sq in saved.queries {
            let Some(q) = queries.iter().find(|q| q.id == sq.id) else {
                continue;
            };
            let gold: Vec<Regex> = q.gold.iter().filter_map(|g| Regex::new(g).ok()).collect();
            let per_engine: Vec<(String, Vec<super::engines::Hit>)> = sq
                .raw
                .into_iter()
                .map(|s| {
                    (
                        s.source,
                        s.hits
                            .into_iter()
                            .map(|h| super::engines::Hit {
                                title: h.title,
                                url: h.url,
                                snippet: h.snippet,
                                rank: h.rank,
                                published: h.published,
                            })
                            .collect(),
                    )
                })
                .collect();
            let recalled = per_engine
                .iter()
                .any(|(_, hits)| hits.iter().any(|h| gold.iter().any(|g| g.is_match(&h.url))));
            // The product learns the independence graph as it goes and ranks
            // each query with what it has learned so far; so does the replay.
            let lists: Vec<(String, Vec<String>)> = per_engine
                .iter()
                .filter(|(name, _)| !rank::is_vertical(name))
                .map(|(name, hits)| {
                    (
                        super::egress::health_key(name).to_string(),
                        hits.iter().map(|h| rank::norm_key(&h.url)).collect(),
                    )
                })
                .collect();
            families.observe(&lists);
            let merged = rank::merge(
                &per_engine,
                &q.query,
                intent::detect(&q.query),
                &trust,
                &families,
                12,
            );
            let urls: Vec<String> = merged
                .iter()
                .take(opts.max_results)
                .map(|m| m.url.clone())
                .collect();
            let hit = first_match(&gold, &urls);
            if opts.verbose {
                crate::diag!(
                    "{:<6} {:<10} {}",
                    q.id,
                    hit.map_or("MISS".to_string(), |h| format!("HIT@{h}")),
                    q.query
                );
            }
            run.add(hit, recalled, 0);
            all.add(hit, recalled, 0);
            by_shape
                .entry(q.shape.clone())
                .or_default()
                .add(hit, recalled, 0);
        }
        crate::diag!(
            "{}",
            run.line(&path.file_name().map_or(String::new(), |f| {
                f.to_string_lossy().chars().take(8).collect()
            }))
        );
    }
    for (shape, t) in &by_shape {
        crate::diag!("{}", t.line(shape));
    }
    crate::diag!("{}", all.line("ALL"));
    0
}

pub async fn run(args: &[String]) -> i32 {
    let opts = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            crate::diag!("{e}");
            return 2;
        }
    };
    if opts.fresh && opts.replay.is_empty() && std::env::var_os("DONSETCH_CACHE_DIR").is_none() {
        return reexec_fresh(args);
    }
    let raw = match std::fs::read_to_string(&opts.queries) {
        Ok(r) => r,
        Err(e) => {
            crate::diag!("search-bench: cannot read {}: {e}", opts.queries.display());
            return 2;
        }
    };
    let file: QueryFile = match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(e) => {
            crate::diag!("search-bench: {}: {e}", opts.queries.display());
            return 2;
        }
    };
    let mut queries: Vec<BenchQuery> = file
        .queries
        .into_iter()
        .filter(|q| opts.set == "all" || q.set == opts.set)
        .filter(|q| opts.shape.as_deref().is_none_or(|s| q.shape == s))
        .filter(|q| opts.only.is_empty() || opts.only.contains(&q.id))
        .collect();
    if opts.smoke {
        queries.retain(|q| SMOKE.contains(&q.id.as_str()));
    }
    if let Some(n) = opts.limit {
        queries.truncate(n);
    }
    if queries.is_empty() {
        crate::diag!("search-bench: no queries selected");
        return 2;
    }

    if !opts.replay.is_empty() {
        return replay(&opts, &queries);
    }

    let profile = BrowserProfile::host_default();
    let egress = std::sync::Arc::new(EgressPool::from_env());
    super::egress::install_global(std::sync::Arc::clone(&egress));
    let fetcher = match Fetcher::new(profile) {
        Ok(f) => f.with_egress(std::sync::Arc::clone(&egress)),
        Err(e) => {
            crate::diag!("search-bench: {e}");
            return 1;
        }
    };
    let searcher = Searcher::new_shared(fetcher, egress);

    let mut all = Tally::default();
    let mut by_shape: std::collections::BTreeMap<String, Tally> = Default::default();
    let mut rows = Vec::new();
    for (n, q) in queries.iter().enumerate() {
        if n > 0 {
            tokio::time::sleep(opts.pace).await;
        }
        let gold: Vec<Regex> = q.gold.iter().filter_map(|g| Regex::new(g).ok()).collect();
        let t0 = Instant::now();
        let outcome = searcher.search(&q.query, opts.max_results, None).await;
        let ms = t0.elapsed().as_millis() as u64;
        let raw = searcher.last_raw();
        let (urls, census): (Vec<String>, String) = match &outcome {
            Ok(o) => (
                o.results.iter().map(|r| r.url.clone()).collect(),
                render::census(&o.report),
            ),
            Err(e) => (Vec::new(), format!("error: {e}")),
        };
        let hit = first_match(&gold, &urls);
        let raw_best: Vec<(String, Option<usize>)> = raw
            .iter()
            .map(|(name, hits)| {
                let u: Vec<String> = hits.iter().map(|h| h.url.clone()).collect();
                (name.clone(), first_match(&gold, &u))
            })
            .collect();
        let recalled = raw_best.iter().any(|(_, r)| r.is_some());
        all.add(hit, recalled, ms);
        by_shape
            .entry(q.shape.clone())
            .or_default()
            .add(hit, recalled, ms);
        let verdict = match (hit, recalled) {
            (Some(h), _) => format!("HIT@{h}"),
            (None, true) => "RANK-LOST".to_string(),
            (None, false) => "NO-RECALL".to_string(),
        };
        crate::diag!(
            "{:<6} {:<10} {:>5.1}s  {}  [{}]",
            q.id,
            verdict,
            ms as f64 / 1000.0,
            q.query,
            census
        );
        rows.push(json!({
            "id": q.id, "set": q.set, "shape": q.shape, "query": q.query,
            "hit": hit, "recalled": recalled, "ms": ms, "census": census,
            "urls": urls,
            "recall_by_source": raw_best.iter()
                .map(|(s, r)| json!({"source": s, "rank": r})).collect::<Vec<_>>(),
            "raw": raw.iter().map(|(s, hits)| json!({
                "source": s,
                "urls": hits.iter().map(|h| h.url.clone()).collect::<Vec<_>>(),
                "hits": hits.iter().map(|h| json!({
                    "title": h.title,
                    "url": h.url,
                    "snippet": h.snippet.chars().take(400).collect::<String>(),
                    "rank": h.rank,
                    "published": h.published,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }));
    }

    crate::diag!("");
    for (shape, t) in &by_shape {
        crate::diag!("{}", t.line(shape));
    }
    crate::diag!("{}", all.line("ALL"));

    if let Some(path) = &opts.out {
        let doc = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "set": opts.set,
            "pace_ms": opts.pace.as_millis() as u64,
            "all": all.json(),
            "by_shape": by_shape.iter().map(|(k, t)| (k.clone(), t.json()))
                .collect::<serde_json::Map<_, _>>(),
            "queries": rows,
        });
        match serde_json::to_string_pretty(&doc) {
            Ok(s) => {
                if let Err(e) = std::fs::write(path, s) {
                    crate::diag!("search-bench: cannot write {}: {e}", path.display());
                    return 1;
                }
            }
            Err(e) => {
                crate::diag!("search-bench: {e}");
                return 1;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_first_gold_match_sets_the_rank() {
        let gold = vec![Regex::new(r"docs\.rs/tokio/").unwrap()];
        let urls = strings(&[
            "https://a.com",
            "https://docs.rs/tokio/latest",
            "https://docs.rs/tokio/x",
        ]);
        assert_eq!(first_match(&gold, &urls), Some(2));
        assert_eq!(first_match(&gold, &strings(&["https://a.com"])), None);
    }

    #[test]
    fn a_tally_reports_rates_and_reciprocal_rank() {
        let mut t = Tally::default();
        t.add(Some(1), true, 1000);
        t.add(Some(4), true, 2000);
        t.add(None, true, 3000);
        t.add(None, false, 4000);
        assert_eq!(t.at(1), 0.25);
        assert_eq!(t.at(5), 0.5);
        assert!((t.mrr() - (1.0 + 0.25) / 4.0).abs() < 1e-9);
        assert_eq!(t.recall, 3);
    }

    #[test]
    fn arguments_are_validated_before_any_search_runs() {
        assert!(parse(&strings(&["--set", "bogus"])).is_err());
        assert!(parse(&strings(&["--nope"])).is_err());
        assert!(parse(&strings(&["--pace-ms"])).is_err());
        let o = parse(&strings(&["--set", "held", "--smoke", "--pace-ms", "10"])).unwrap();
        assert_eq!(o.set, "held");
        assert!(o.smoke);
        assert_eq!(o.pace, Duration::from_millis(10));
    }

    #[test]
    fn every_bundled_query_compiles_and_ids_are_unique() {
        let raw = include_str!("../../bench/search/queries.json");
        let file: QueryFile = serde_json::from_str(raw).unwrap();
        let mut ids = std::collections::HashSet::new();
        for q in &file.queries {
            assert!(ids.insert(q.id.clone()), "duplicate id {}", q.id);
            assert!(matches!(q.set.as_str(), "gold" | "held"), "{}", q.id);
            assert!(!q.gold.is_empty(), "{} has no gold", q.id);
            for g in &q.gold {
                Regex::new(g).unwrap_or_else(|e| panic!("{}: {g}: {e}", q.id));
            }
        }
        for id in SMOKE {
            assert!(ids.contains(*id), "smoke id {id} is not in the file");
        }
    }
}
