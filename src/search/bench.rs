//! `donsetch search-bench`: the live bench for the keyless search path.
//!
//! It runs `bench/search/queries.json` through the real [`Searcher`] (or, with
//! `--bin`, through another build's `search --json`) and reports what an agent
//! would have got: where the answering page landed, whether the answer's fact was
//! in the evidence, how often the answer was empty, how often a `must_not` page
//! won, and what the call cost.
//!
//! **The bench must be able to lose.** A row carries `gold` (URL patterns that
//! prove the answering page), `answer_key` (the answering fact, as phrases a
//! returned title or snippet must carry), `must_not` (the false answer that must
//! never appear), `expect` (`answer`: a real answer exists; `refuse`: none does,
//! so nothing may be invented), `lang` (the answer must be written in this
//! script) and `window` (a dated answer, inside this span). A corpus that cannot
//! fail measures nothing; 5.2.1's 109 rows scored 0.97 while live queries
//! returned zero, and the shapes below exist so that cannot repeat: identifier,
//! operator, long topical, how-to, shopping, CJK, mixed-script, ambiguous and
//! adversarial were all absent.
//!
//! **The pace is part of the measurement.** Back-to-back queries make every
//! keyless source rate-limit within a minute, so a fast bench measures the bench.
//! `--runs N` repeats the whole corpus N times, each from an empty cache
//! (`--keep-state` to reuse one), because egress and challenge state are part of
//! what is measured; the release protocol spaces those runs across hours.
//!
//! `recall` is whether the answer arrived from *any* source before the merge; the
//! gap between recall and hit@10 is ordering the ranker lost. A miss with no
//! recall is a retrieval problem, a miss with recall is a ranking problem, and
//! they have different fixes. `--bin` cannot see pre-merge candidates, so it
//! reports recall as absent instead of guessing.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Searcher, egress::EgressPool, render};
use crate::fetch::client::Fetcher;
use crate::profile::BrowserProfile;

#[path = "../../bench/live.rs"]
pub mod live;

/// The shapes the corpus measures. The first five are the 5.2.1 corpus's own;
/// the rest are the shapes it never had, and each names a different failure:
/// a name that is not a word (`identifier`), a filter the source must honour
/// (`operator`), a question long enough that per-term coverage says nothing
/// (`long`), a procedure (`howto`), a product page (`shopping`), a query whose
/// answer must be in its own script (`cjk`, `mixed`), a word with several senses
/// (`ambiguous`), and a query that must be refused rather than answered
/// (`adversarial`).
pub const SHAPES: &[&str] = &[
    "code",
    "web",
    "paper",
    "entity",
    "news",
    "identifier",
    "operator",
    "long",
    "howto",
    "shopping",
    "cjk",
    "mixed",
    "ambiguous",
    "adversarial",
];

/// Languages `locale::native` can judge a title against. A row that says `jp`
/// would silently test nothing, so the schema refuses it.
const LANGS: &[&str] = &["ja", "zh", "ko", "ru", "ar", "he", "th", "hi", "el"];

/// Rows the smoke subset takes per shape: enough to reach every shape in a few
/// minutes, never enough for a release number.
const SMOKE_PER_SHAPE: usize = 3;
const SMOKE_MAX: usize = 60;

#[derive(Debug, Deserialize)]
pub struct QueryFile {
    pub queries: Vec<BenchQuery>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BenchQuery {
    pub id: String,
    pub shape: String,
    /// `gold` is the tuning set; `held` is validation and is never tuned against.
    pub set: String,
    pub query: String,
    /// URL patterns that prove the answering page arrived.
    #[serde(default)]
    pub gold: Vec<String>,
    /// The answering fact, as phrases one returned title/snippet must carry.
    /// An `answer` row's key is what makes "accurate" mean *the answer is
    /// extractable from what was returned*, not "a URL matched".
    #[serde(default)]
    pub answer_key: Vec<String>,
    /// Patterns that must not appear in any returned URL or title. A row with a
    /// `must_not` is a gate: the run fails when one matches.
    #[serde(default)]
    pub must_not: Vec<String>,
    /// `answer` (default): a real answer exists and must be found. `refuse`: no
    /// honest answer exists; the right outcome is empty or thin, and any URL
    /// matching `must_not` is a confident wrong answer.
    #[serde(default = "expect_answer")]
    pub expect: String,
    /// Every returned URL must be on this host or one of its subdomains. A
    /// `site:` query's whole contract, which a `must_not` cannot express: the
    /// regex engine has no look-around, so "a URL that is not on docs.rs" is not
    /// a pattern — it is a scope.
    #[serde(default)]
    pub only_host: Option<String>,
    /// A dated query's freshness (`7d`, `365d`): every item a news lane returns
    /// must be dated and inside it, or the row fails.
    #[serde(default)]
    pub window: Option<String>,
    /// The query's language (`ja`, `ko`, `ru`): the top three results must be
    /// written in its script, or the row fails.
    #[serde(default)]
    pub lang: Option<String>,
}

fn expect_answer() -> String {
    "answer".into()
}

impl BenchQuery {
    pub fn is_answer(&self) -> bool {
        self.expect == "answer"
    }

    pub fn gold_re(&self) -> Vec<Regex> {
        compile_all(&self.gold)
    }

    pub fn must_not_re(&self) -> Vec<Regex> {
        compile_all(&self.must_not)
    }

    pub fn window(&self) -> Option<super::contract::Window> {
        self.window
            .as_deref()
            .and_then(super::contract::window_from_str)
    }
}

fn compile_all(patterns: &[String]) -> Vec<Regex> {
    patterns.iter().filter_map(|p| Regex::new(p).ok()).collect()
}

/// One row's answer, reduced to what the metrics read. Both the in-process and
/// the external mode produce this, so their numbers are the same numbers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnswerRow {
    pub url: String,
    pub title: String,
    pub snippet: String,
}

/// What one answer was worth.
#[derive(Debug, Clone, Default)]
pub struct Judged {
    /// 1-based rank of the first URL any gold pattern matches.
    pub hit: Option<usize>,
    /// 1-based rank of the first result whose title+snippet carries the whole
    /// answer-key phrases. This is lexical coverage, not factual accuracy;
    /// factual extractability requires the separate blinded evidence review.
    pub key_at: Option<usize>,
    /// URLs/titles that matched a `must_not`, in rank order.
    pub violations: Vec<String>,
    /// How many of the top five are a `must_not` match: the junk@5 metric.
    pub junk5: usize,
}

/// Judge one answer against its row. Never panics on a bad pattern: the corpus
/// is validated at load, and a pattern that will not compile was rejected then.
pub fn judge(q: &BenchQuery, rows: &[AnswerRow]) -> Judged {
    let gold = q.gold_re();
    let negative = q.must_not_re();
    let mut out = Judged::default();
    if let Some(pos) = rows
        .iter()
        .position(|r| gold.iter().any(|g| g.is_match(&r.url)))
    {
        out.hit = Some(pos + 1);
    }
    if !q.answer_key.is_empty() {
        let keys: Vec<String> = q.answer_key.iter().map(|k| fold(k)).collect();
        if let Some(pos) = rows.iter().position(|r| {
            let text = fold(&format!("{} {}", r.title, r.snippet));
            keys.iter().all(|k| text.contains(k.as_str()))
        }) {
            out.key_at = Some(pos + 1);
        }
    }
    for (i, r) in rows.iter().enumerate() {
        let off_scope = q.only_host.as_ref().is_some_and(|host| {
            let want = host.trim_start_matches("www.").to_ascii_lowercase();
            !url::Url::parse(&r.url).ok().is_some_and(|u| {
                u.host_str()
                    .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
                    .is_some_and(|h| h == want || h.ends_with(&format!(".{want}")))
            })
        });
        if off_scope
            || negative
                .iter()
                .any(|n| n.is_match(&r.url) || n.is_match(&r.title))
        {
            out.violations.push(r.url.clone());
            if i < 5 {
                out.junk5 += 1;
            }
        }
    }
    out
}

/// Case- and whitespace-insensitive form of a phrase, so an answer key is not a
/// test of the engine's punctuation.
fn fold(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// How many distinct hosts the answer used. A list of ten pages from one host is
/// one result wearing ten hats.
pub fn domains(rows: &[AnswerRow]) -> usize {
    let mut hosts: Vec<String> = rows
        .iter()
        .filter_map(|r| url::Url::parse(&r.url).ok())
        .filter_map(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_string())
        })
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts.len()
}

/// One row of one run, as the artifact stores it and the tables read it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowRecord {
    pub id: String,
    pub shape: String,
    pub set: String,
    pub query: String,
    pub expect: String,
    pub hit: Option<usize>,
    pub key_at: Option<usize>,
    pub violations: Vec<String>,
    pub junk5: usize,
    pub redirects: usize,
    pub ms: u64,
    pub urls: Vec<String>,
    /// Exactly the bounded title/snippet evidence used by the judge.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<AnswerRow>,
    /// The actual trust and independence state used for this merge. Old
    /// raw-only artifacts can be replayed approximately, never as parity proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ranking: Option<super::rank::MergeInput>,
    /// Per-source status, latency and request counts for diagnosing a miss.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<super::EngineReport>,
    pub domains: usize,
    /// Absent in `--bin` mode: recall is pre-merge knowledge of the searcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<usize>,
    pub census: String,
    /// Window and language contracts, checked on what the run returned.
    #[serde(default)]
    pub stale: usize,
    #[serde(default)]
    pub undated: usize,
    #[serde(default)]
    pub foreign: usize,
    /// Per-lane detail (in-process only): what every source answered before the
    /// merge, so a later unit can re-rank identical input offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl RowRecord {
    fn zero(&self) -> bool {
        self.urls.is_empty()
    }

    /// A `refuse` row passes when nothing was invented: no `must_not` matched and
    /// the answer did not present itself as full.
    fn refused_ok(&self) -> bool {
        self.expect != "refuse" || (self.junk5 == 0 && self.urls.len() < 3)
    }
}

/// The columns the design asks for, per shape and overall.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    /// Rows where a real answer exists.
    pub n: usize,
    pub hits: Vec<Option<usize>>,
    pub keys: Vec<Option<usize>>,
    pub recalls: Vec<bool>,
    pub ms: Vec<u64>,
    pub zeros: usize,
    pub rows: usize,
    pub junk5_rows: usize,
    pub domains: Vec<usize>,
    pub reqs: Vec<usize>,
    pub refuse: usize,
    pub refuse_ok: usize,
}

impl Tally {
    pub fn push(&mut self, r: &RowRecord) {
        self.rows += 1;
        self.ms.push(r.ms);
        if r.junk5 > 0 {
            self.junk5_rows += 1;
        }
        if r.expect == "refuse" {
            self.refuse += 1;
            self.refuse_ok += usize::from(r.refused_ok());
            return;
        }
        self.n += 1;
        self.hits.push(r.hit);
        self.keys.push(r.key_at);
        if let Some(recall) = r.recall {
            self.recalls.push(recall);
        }
        if r.zero() {
            self.zeros += 1;
        }
        self.domains.push(r.domains);
        if let Some(req) = r.requests {
            self.reqs.push(req);
        }
    }

    fn rate(&self, count: usize, of: usize) -> f64 {
        if of == 0 {
            0.0
        } else {
            count as f64 / of as f64
        }
    }

    pub fn at(&self, k: usize) -> f64 {
        self.rate(
            self.hits
                .iter()
                .filter(|h| h.is_some_and(|h| h <= k))
                .count(),
            self.n,
        )
    }

    pub fn mrr(&self) -> f64 {
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

    /// Legacy metric: expected answer-key phrases occur in the top-five evidence.
    pub fn accuracy(&self) -> f64 {
        self.rate(
            self.keys
                .iter()
                .filter(|k| k.is_some_and(|k| k <= 5))
                .count(),
            self.n,
        )
    }

    pub fn recall(&self) -> f64 {
        self.rate(
            self.recalls.iter().filter(|r| **r).count(),
            self.recalls.len(),
        )
    }

    pub fn zero_rate(&self) -> f64 {
        self.rate(self.zeros, self.n)
    }

    pub fn junk5(&self) -> f64 {
        self.rate(self.junk5_rows, self.rows)
    }

    pub fn refuse_rate(&self) -> f64 {
        self.rate(self.refuse_ok, self.refuse)
    }

    pub fn mean_domains(&self) -> f64 {
        if self.domains.is_empty() {
            return 0.0;
        }
        self.domains.iter().sum::<usize>() as f64 / self.domains.len() as f64
    }

    pub fn req_median(&self) -> Option<usize> {
        let mut v = self.reqs.clone();
        v.sort_unstable();
        v.get(v.len() / 2).copied()
    }

    pub fn pct(&self, p: f64) -> u64 {
        let mut v = self.ms.clone();
        v.sort_unstable();
        v.get(((v.len() as f64 * p) as usize).min(v.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    }

    pub fn line(&self, label: &str) -> String {
        format!(
            "{:<11} {:>3}  {:>5.2} {:>5.2} {:>5.2}  {:>5.3}  {:>5.2} {:>5.2} {:>5.2} {:>5.2}  {:>5.1}s {:>5.1}s {:>4} {:>5.1}",
            label,
            self.n,
            self.at(1),
            self.at(3),
            self.at(5),
            self.mrr(),
            self.recall(),
            self.zero_rate(),
            self.junk5(),
            self.accuracy(),
            self.pct(0.5) as f64 / 1000.0,
            self.pct(0.95) as f64 / 1000.0,
            self.req_median().map_or("-".to_string(), |r| r.to_string()),
            self.mean_domains(),
        )
    }

    pub fn json(&self) -> serde_json::Value {
        json!({
            "rows": self.rows,
            "n": self.n,
            "hit1": self.at(1), "hit3": self.at(3), "hit5": self.at(5),
            "mrr": self.mrr(),
            "recall": self.recall(), "recall_rows": self.recalls.len(),
            "zero_rate": self.zero_rate(), "zeros": self.zeros,
            "junk5": self.junk5(), "junk5_rows": self.junk5_rows,
            "accuracy": self.accuracy(),
            "key_coverage": self.accuracy(),
            "refuse": self.refuse, "refuse_ok": self.refuse_ok,
            "refuse_rate": self.refuse_rate(),
            "mean_domains": self.mean_domains(),
            "requests_median": self.req_median(),
            "p50_ms": self.pct(0.5), "p95_ms": self.pct(0.95),
        })
    }
}

/// The table header, printed once above the per-shape lines.
pub const TABLE_HEAD: &str =
    "shape         n  hit@1 hit@3 hit@5    MRR recall  zero junk5   key    p50    p95  req   dom";

/// Aggregate a set of rows into the overall and per-shape tables.
pub fn summarise(rows: &[RowRecord]) -> (Tally, std::collections::BTreeMap<String, Tally>) {
    let mut all = Tally::default();
    let mut by_shape: std::collections::BTreeMap<String, Tally> = Default::default();
    for r in rows {
        all.push(r);
        by_shape.entry(r.shape.clone()).or_default().push(r);
    }
    (all, by_shape)
}

#[derive(Clone)]
pub struct Options {
    pub queries: PathBuf,
    pub set: String,
    pub shape: Option<String>,
    pub only: Vec<String>,
    pub smoke: bool,
    pub limit: Option<usize>,
    pub pace: Duration,
    pub max_results: usize,
    pub out: Option<PathBuf>,
    /// Start every run from an empty cache directory. On by default: a bench run
    /// that inherits the user's learned state measures the user's history.
    pub cold: bool,
    /// Run every shape of a cold run from one state instead of one fresh state
    /// per shape. Off by default: a long single-state run exhausts the general
    /// lanes part-way (every bing/ddg/brave lane `rested` from row ~111 of 214),
    /// and the rows after that measure the roster's rest, not search.
    pub one_ledger: bool,
    /// How many times the corpus is run, each its own measurement.
    pub runs: usize,
    /// Minutes to wait between runs, so a multi-run measurement can land in
    /// different hours without a second command.
    pub gap: Duration,
    /// Measure another build through its `search --json` instead of this one.
    pub bin: Option<PathBuf>,
    /// Re-rank the candidates saved in these bench files with the current code,
    /// offline, instead of searching the web.
    pub replay: Vec<PathBuf>,
    /// With `--replay`: write one JSON line per row — the whole candidate pool
    /// with its labels and ranking features. What `scripts/train_rank.py`
    /// trains on.
    pub features_out: Option<PathBuf>,
    pub verbose: bool,
}

pub const USAGE: &str = "usage: donsetch search-bench [--queries FILE] [--set gold|held|all] \
[--shape SHAPE] [--only ID,ID] [--smoke] [--limit N] [--pace-ms N] [--max-results N] \
[--runs N] [--gap-min N] [--cold] [--keep-state] [--one-ledger] [--bin PATH] [--out FILE] \
[--replay FILE]... [--features-out FILE]";

pub fn parse(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        queries: PathBuf::from("bench/search/queries.json"),
        set: "gold".into(),
        shape: None,
        only: Vec::new(),
        smoke: false,
        limit: None,
        pace: Duration::from_millis(2500),
        max_results: 10,
        out: None,
        cold: true,
        one_ledger: false,
        runs: 1,
        gap: Duration::ZERO,
        bin: None,
        replay: Vec::new(),
        features_out: None,
        verbose: false,
    };
    let mut i = 0;
    let value = |i: &mut usize| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("{} needs a value\n{USAGE}", args[*i - 1]))
    };
    let number = |i: &mut usize, what: &str| -> Result<u64, String> {
        value(i)?
            .parse()
            .map_err(|_| format!("bad {what}\n{USAGE}"))
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
            "--pace-ms" => o.pace = Duration::from_millis(number(&mut i, "--pace-ms")?),
            "--max-results" => {
                o.max_results = value(&mut i)?
                    .parse()
                    .map_err(|_| format!("bad --max-results\n{USAGE}"))?
            }
            "--runs" => {
                let n = number(&mut i, "--runs")?;
                if n == 0 || n > 20 {
                    return Err(format!("--runs is 1..=20\n{USAGE}"));
                }
                o.runs = n as usize;
            }
            "--gap-min" => o.gap = Duration::from_secs(number(&mut i, "--gap-min")? * 60),
            "--cold" => o.cold = true,
            "--keep-state" => o.cold = false,
            "--one-ledger" => o.one_ledger = true,
            "--bin" => o.bin = Some(PathBuf::from(value(&mut i)?)),
            "--out" => o.out = Some(PathBuf::from(value(&mut i)?)),
            "--replay" => o.replay.push(PathBuf::from(value(&mut i)?)),
            "--features-out" => o.features_out = Some(PathBuf::from(value(&mut i)?)),
            "-v" | "--verbose" => o.verbose = true,
            "-h" | "--help" => return Err(USAGE.to_string()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
        i += 1;
    }
    if o.features_out.is_some() && o.replay.is_empty() {
        return Err(format!(
            "--features-out reads saved candidate pools: pass --replay FILE\n{USAGE}"
        ));
    }
    if !matches!(o.set.as_str(), "gold" | "held" | "all") {
        return Err(format!("--set is gold, held or all\n{USAGE}"));
    }
    if let Some(shape) = &o.shape
        && !SHAPES.contains(&shape.as_str())
    {
        return Err(format!(
            "--shape must be one of {}\n{USAGE}",
            SHAPES.join(", ")
        ));
    }
    Ok(o)
}

/// Read and validate the corpus. A bench that silently accepts a broken row
/// reports a number that means nothing, so every row is checked here: unique id,
/// known shape, known set and expectation, compilable patterns, a question, and
/// the fields its `expect` needs.
pub fn load(path: &std::path::Path) -> Result<Vec<BenchQuery>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let file: QueryFile =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut ids = std::collections::HashSet::new();
    for q in &file.queries {
        if !ids.insert(q.id.clone()) {
            return Err(format!("{}: duplicate id {}", path.display(), q.id));
        }
        if q.id.trim().is_empty() || q.query.trim().is_empty() {
            return Err(format!(
                "{}: row {} has no id or no query",
                path.display(),
                q.id
            ));
        }
        if !SHAPES.contains(&q.shape.as_str()) {
            return Err(format!(
                "{}: row {} has unknown shape {:?} (known: {})",
                path.display(),
                q.id,
                q.shape,
                SHAPES.join(", ")
            ));
        }
        if !matches!(q.set.as_str(), "gold" | "held") {
            return Err(format!(
                "{}: row {} has set {:?}",
                path.display(),
                q.id,
                q.set
            ));
        }
        if !matches!(q.expect.as_str(), "answer" | "refuse") {
            return Err(format!(
                "{}: row {} has expect {:?} (answer or refuse)",
                path.display(),
                q.id,
                q.expect
            ));
        }
        if q.is_answer() {
            if q.gold.is_empty() {
                return Err(format!(
                    "{}: answer row {} has no gold",
                    path.display(),
                    q.id
                ));
            }
            if q.answer_key.is_empty() {
                return Err(format!(
                    "{}: answer row {} has no answer_key",
                    path.display(),
                    q.id
                ));
            }
        } else if q.must_not.is_empty() && q.only_host.is_none() {
            return Err(format!(
                "{}: refuse row {} has no must_not or only_host to fail on",
                path.display(),
                q.id
            ));
        }
        if let Some(host) = &q.only_host
            && (host.trim().is_empty()
                || !host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        {
            return Err(format!(
                "{}: row {} has only_host {:?} (a bare host name)",
                path.display(),
                q.id,
                host
            ));
        }
        for (kind, patterns) in [("gold", &q.gold), ("must_not", &q.must_not)] {
            for p in patterns {
                Regex::new(p)
                    .map_err(|e| format!("{}: row {} {kind} {p:?}: {e}", path.display(), q.id))?;
            }
        }
        if let Some(lang) = &q.lang
            && !LANGS.contains(&lang.as_str())
        {
            return Err(format!(
                "{}: row {} has lang {:?} (known: {})",
                path.display(),
                q.id,
                lang,
                LANGS.join(", ")
            ));
        }
        if let Some(window) = &q.window
            && super::contract::window_from_str(window).is_none()
        {
            return Err(format!(
                "{}: row {} has window {:?} (7d, 2w, 3m, 1y)",
                path.display(),
                q.id,
                window
            ));
        }
    }
    if file.queries.is_empty() {
        return Err(format!("{}: no queries", path.display()));
    }
    Ok(file.queries)
}

/// Select the rows this invocation runs.
pub fn select(opts: &Options, mut queries: Vec<BenchQuery>) -> Vec<BenchQuery> {
    queries.retain(|q| opts.set == "all" || q.set == opts.set);
    if let Some(shape) = &opts.shape {
        queries.retain(|q| &q.shape == shape);
    }
    if !opts.only.is_empty() {
        queries.retain(|q| opts.only.contains(&q.id));
    }
    if opts.smoke {
        queries = smoke(queries);
    }
    if let Some(n) = opts.limit {
        queries.truncate(n);
    }
    queries
}

/// One row per shape, up to `SMOKE_PER_SHAPE`, so a smoke run reaches every
/// shape instead of only the corpus's first block.
fn smoke(queries: Vec<BenchQuery>) -> Vec<BenchQuery> {
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    let mut out = Vec::new();
    for q in queries {
        let c = counts.entry(q.shape.clone()).or_insert(0);
        if *c < SMOKE_PER_SHAPE {
            *c += 1;
            out.push(q);
        }
        if out.len() >= SMOKE_MAX {
            break;
        }
    }
    out
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One execution of the corpus, in this process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub started_unix: u64,
    pub finished_unix: u64,
    pub rows: Vec<RowRecord>,
}

/// Run the corpus against this process's searcher. `run_index` only names the
/// artifact; every run is its own measurement.
pub async fn one_run(
    opts: &Options,
    queries: &[BenchQuery],
    _run_index: usize,
) -> Result<Run, String> {
    let profile = BrowserProfile::host_default();
    let egress = std::sync::Arc::new(EgressPool::from_env());
    super::egress::install_global(std::sync::Arc::clone(&egress));
    let fetcher = match Fetcher::new(profile) {
        Ok(f) => f.with_egress(std::sync::Arc::clone(&egress)),
        Err(e) => return Err(e.to_string()),
    };
    let searcher = Searcher::new_shared(fetcher, egress);
    let started_unix = now_unix();
    let mut rows = Vec::new();
    for (n, q) in queries.iter().enumerate() {
        if n > 0 {
            tokio::time::sleep(opts.pace).await;
        }
        rows.push(in_process_row(&searcher, opts, q).await);
    }
    Ok(Run {
        started_unix,
        finished_unix: now_unix(),
        rows,
    })
}

/// The single-row body of an in-process run: search, then judge what came back.
async fn in_process_row(searcher: &Searcher, opts: &Options, q: &BenchQuery) -> RowRecord {
    let t0 = Instant::now();
    let outcome = searcher.search(&q.query, opts.max_results, None).await;
    let ms = t0.elapsed().as_millis() as u64;
    let requests = outcome
        .as_ref()
        .ok()
        .and_then(|out| out.requests)
        .unwrap_or_else(|| searcher.last_request_count());
    let raw = searcher.last_raw();
    let visible = outcome
        .as_ref()
        .map(|out| render::evidence_snippets(&out.results))
        .unwrap_or_default();
    let (answers, census): (Vec<AnswerRow>, String) = match &outcome {
        Ok(o) => (
            o.results
                .iter()
                .enumerate()
                .map(|(position, r)| AnswerRow {
                    url: render::evidence_url(&r.url),
                    title: r.title.clone(),
                    snippet: visible[position].clone(),
                })
                .collect(),
            render::census(&o.report),
        ),
        Err(e) => (Vec::new(), format!("error: {e}")),
    };
    let judged = judge(q, &answers);
    let redirects = answers
        .iter()
        .filter(|r| super::contract::url_class(&r.url) != super::contract::UrlClass::Page)
        .count();

    // The date contract, measured on what the news program offered: every item a
    // news lane returns is dated, and dated inside the query's window. The
    // general web is not held to a date it does not carry.
    let (mut stale, mut undated) = (0usize, 0usize);
    if let Some(window) = q.window() {
        for (lane, hits) in &raw {
            if !matches!(lane.as_str(), "bing-news" | "feed" | "gdelt") {
                continue;
            }
            for h in hits {
                match h.published.as_deref().and_then(super::contract::day_of) {
                    None => undated += 1,
                    Some(day) if !window.contains(day) => stale += 1,
                    _ => {}
                }
            }
        }
    }
    // Native results: the top three answer in the query's own script.
    let mut foreign = 0usize;
    if let Some(lang) = q.lang.as_deref() {
        for row in answers.iter().take(3) {
            if !super::locale::native(lang, &row.title, &row.snippet) {
                foreign += 1;
            }
        }
    }
    let gold = q.gold_re();
    let recalled = raw
        .iter()
        .any(|(_, hits)| hits.iter().any(|h| gold.iter().any(|g| g.is_match(&h.url))));

    let record = RowRecord {
        id: q.id.clone(),
        shape: q.shape.clone(),
        set: q.set.clone(),
        query: q.query.clone(),
        expect: q.expect.clone(),
        hit: judged.hit,
        key_at: judged.key_at,
        violations: judged.violations,
        junk5: judged.junk5,
        redirects,
        ms,
        urls: answers.iter().map(|r| r.url.clone()).collect(),
        domains: domains(&answers),
        recall: outcome
            .as_ref()
            .ok()
            .filter(|o| !o.cached)
            .map(|_| recalled),
        requests: Some(requests),
        census: census.clone(),
        stale,
        undated,
        foreign,
        raw: Some(raw_json(&raw)),
        evidence: answers,
        ranking: searcher.last_merge(),
        reports: outcome
            .as_ref()
            .map(|o| o.report.clone())
            .unwrap_or_default(),
    };
    if opts.verbose {
        crate::diag!(
            "{:<6} {:<10} {:>5.1}s  {}  [{}]",
            q.id,
            verdict(&record),
            ms as f64 / 1000.0,
            q.query,
            census
        );
    }
    record
}

fn raw_json(raw: &[(String, Vec<super::engines::Hit>)]) -> serde_json::Value {
    raw.iter()
        .map(|(source, hits)| {
            json!({
                "source": source,
                "hits": hits.iter().map(|h| json!({
                    "title": h.title,
                    "url": h.url,
                    "snippet": h.snippet.chars().take(400).collect::<String>(),
                    "rank": h.rank,
                    "published": h.published,
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// The one-word verdict printed beside a row in verbose runs.
pub fn verdict(r: &RowRecord) -> String {
    if !r.violations.is_empty() {
        "VIOLATION".to_string()
    } else if r.stale > 0 {
        "STALE".to_string()
    } else if r.undated > 0 {
        "UNDATED".to_string()
    } else if r.foreign > 0 {
        "FOREIGN".to_string()
    } else if r.expect == "refuse" {
        if r.refused_ok() {
            "REFUSED".to_string()
        } else {
            "INVENTED".to_string()
        }
    } else {
        match (r.hit, r.key_at) {
            (Some(h), _) => format!("HIT@{h}"),
            (None, Some(k)) => format!("KEY@{k}"),
            (None, None) if r.urls.is_empty() => "ZERO".to_string(),
            (None, None) if r.recall == Some(true) => "RANK-LOST".to_string(),
            _ => "NO-RECALL".to_string(),
        }
    }
}

/// Print the tables for a set of runs, merged: every run counts, so the numbers
/// are over all runs' rows rather than one hour's.
pub fn report(label: &str, runs: &[Run]) {
    let rows: Vec<RowRecord> = runs.iter().flat_map(|r| r.rows.iter().cloned()).collect();
    let (all, by_shape) = summarise(&rows);
    crate::diag!("");
    crate::diag!("{label}");
    crate::diag!("{TABLE_HEAD}");
    for (shape, t) in &by_shape {
        crate::diag!("{}", t.line(shape));
    }
    crate::diag!("{}", t_line_all(&all));
    crate::diag!("key = expected-phrase coverage, not factual accuracy (scored separately).");
    let violations: Vec<String> = rows
        .iter()
        .flat_map(|r| r.violations.iter().cloned())
        .collect();
    let redirects: usize = rows.iter().map(|r| r.redirects).sum();
    let stale: usize = rows.iter().map(|r| r.stale).sum();
    let undated: usize = rows.iter().map(|r| r.undated).sum();
    let foreign: usize = rows.iter().map(|r| r.foreign).sum();
    crate::diag!(
        "runs {}  rows {}  redirects {redirects}  must-not violations {}  stale {stale}  undated {undated}  foreign {foreign}",
        runs.len(),
        rows.len(),
        violations.len(),
    );
    if all.refuse > 0 {
        crate::diag!(
            "refuse rows {}  passed {} ({:.2})",
            all.refuse,
            all.refuse_ok,
            all.refuse_rate()
        );
    }
    for v in violations.iter().take(10) {
        crate::diag!("  violation: {v}");
    }
}

fn t_line_all(t: &Tally) -> String {
    t.line("ALL")
}

/// The artifact: the numbers, the rows they came from, and what produced them.
pub fn artifact(
    opts: &Options,
    mode: &str,
    bin_version: Option<&str>,
    runs: &[Run],
) -> serde_json::Value {
    let rows: Vec<RowRecord> = runs.iter().flat_map(|r| r.rows.iter().cloned()).collect();
    let (all, by_shape) = summarise(&rows);
    let mut answer = Tally::default();
    let mut refuse = Tally::default();
    for r in &rows {
        if r.expect == "refuse" {
            refuse.push(r);
        } else {
            answer.push(r);
        }
    }
    json!({
        "tool": "donsetch-search-bench",
        "schema": 2,
        "accuracy_definition": "legacy-accuracy-field-is-expected-phrase-coverage-not-fact-accuracy",
        "version": env!("CARGO_PKG_VERSION"),
        "mode": mode,
        "git": option_env!("DONSETCH_GIT_HASH"),
        "binary_sha256": opts.bin.as_deref().map_or_else(
            || std::env::current_exe().ok().and_then(|p| corpus_hash(&p)), corpus_hash),
        "request_accounting": if mode == "in-process" { "http-exchanges-v1" } else { "not-observed" },
        "cold": opts.cold,
        "refusal_policy": "preserve-active-source-refusals-v1",
        "default_pipeline": if mode == "in-process" { "native-search-and-default-repair-v1" } else { "not-observed" },
        "file_config": if opts.cold || std::env::var("DONSETCH_NO_CONFIG_FILE").as_deref() == Ok("1") { "disabled" } else { "ambient" },
        "search_env_overrides": std::env::vars_os().filter_map(|(name, _)| {
            let name = name.to_string_lossy();
            ((name.starts_with("DONSETCH_") || name.starts_with("DONSEEK_")) && !matches!(name.as_ref(),
                "DONSETCH_CACHE_DIR" | "DONSETCH_CONFIG" | "DONSETCH_NO_CONFIG_FILE" | "DONSETCH_NO_UPDATE_CHECK" | "DONSETCH_NO_SPONSOR"))
                .then(|| name.to_string())
        }).collect::<Vec<_>>(),
        "gap_s": opts.gap.as_secs(),
        "bin_version": bin_version,
        "queries_file": opts.queries.display().to_string(),
        "queries_sha256": corpus_hash(&opts.queries),
        "set": opts.set,
        "shape": opts.shape,
        "max_results": opts.max_results,
        "pace_ms": opts.pace.as_millis() as u64,
        "runs": runs.len(),
        "run_started_unix": runs.iter().map(|r| r.started_unix).collect::<Vec<_>>(),
        "started_unix": runs.iter().map(|r| r.started_unix).min(),
        "all": all.json(),
        "answer": answer.json(),
        "refuse_table": refuse.json(),
        "by_shape": by_shape.iter().map(|(k, t)| (k.clone(), t.json()))
            .collect::<serde_json::Map<_, _>>(),
        "queries": rows,
    })
}

/// The corpus an artifact came from, by content: two runs of "the same" bench
/// are only comparable if the rows were the same bytes.
pub fn corpus_hash(path: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    let digest = Sha256::digest(&bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    Some(out)
}

pub fn write_artifact(path: &std::path::Path, doc: &serde_json::Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

pub async fn run(args: &[String]) -> i32 {
    let opts = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            crate::diag!("{e}");
            return 2;
        }
    };
    let queries = match load(&opts.queries) {
        Ok(q) => q,
        Err(e) => {
            crate::diag!("search-bench: {e}");
            return 2;
        }
    };
    let selected = select(&opts, queries);
    if selected.is_empty() {
        crate::diag!("search-bench: no queries selected");
        return 2;
    }
    if !opts.replay.is_empty() {
        return replay(&opts, &selected);
    }
    live::protocol(&opts, &selected).await
}

/// Re-run saved candidates through the current ranking code, offline. The web
/// supplied the candidates once; every change to the ranker is then measured on
/// identical input, in seconds, with no network noise. Only queries still in the
/// selected set are scored, so tuning on `gold` never reads a `held` row.
fn replay(opts: &Options, queries: &[BenchQuery]) -> i32 {
    use super::{evidence, intent, rank};
    let mut shown = SnippetTally::default();
    let mut all = Tally::default();
    let mut by_shape: std::collections::BTreeMap<String, Tally> = Default::default();
    for path in &opts.replay {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                crate::diag!("search-bench: {}: {e}", path.display());
                return 2;
            }
        };
        // A saved artifact is either a run (`{"queries": [...]}`) or a bare list.
        let saved: Vec<SavedQuery> = match serde_json::from_str::<SavedRun>(&text) {
            Ok(run) => run
                .queries
                .into_iter()
                .filter_map(|q| q.into_saved())
                .collect(),
            Err(_) => match serde_json::from_str::<Vec<SavedQueryAny>>(&text) {
                Ok(v) => v.into_iter().filter_map(|q| q.into_saved()).collect(),
                Err(e) => {
                    crate::diag!("search-bench: {}: {e}", path.display());
                    return 2;
                }
            },
        };
        let mut run = Tally::default();
        let mut families = super::families::Families::default();
        for sq in saved {
            let Some(q) = queries.iter().find(|q| q.id == sq.id) else {
                continue;
            };
            let captured = sq.ranking;
            if captured
                .as_ref()
                .is_some_and(|input| input.query != super::query::normalize(&q.query))
            {
                crate::diag!("search-bench: saved ranking query differs for {}", q.id);
                return 2;
            }
            let gold = q.gold_re();
            let per_engine: Vec<(String, Vec<super::engines::Hit>)> = sq
                .raw
                .into_iter()
                .map(|s| (s.source, s.hits.into_iter().map(Into::into).collect()))
                .collect();
            let recalled = per_engine
                .iter()
                .any(|(_, hits)| hits.iter().any(|h| gold.iter().any(|g| g.is_match(&h.url))));
            let per_engine = captured
                .as_ref()
                .map_or(per_engine, |input| input.per_engine.clone());
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
            if captured.is_none() {
                families.observe(&lists);
            }
            let families = captured
                .as_ref()
                .map_or_else(|| families.clone(), |input| input.families.clone());
            let trust = captured
                .as_ref()
                .map(|input| input.trust.clone())
                .unwrap_or_default();
            let intent = captured
                .as_ref()
                .map_or_else(|| intent::detect(&q.query), |input| input.intent);
            if let Some(path) = &opts.features_out {
                let mut pool = rank::pool(&per_engine, &q.query, intent, &trust, &families);
                rank::order(&mut pool, None);
                // What the operator filters remove after the merge is never shown,
                // so it is never a training example either.
                super::site_filter(&q.query, &mut pool.items);
                super::query::intitle_filter(&q.query, &mut pool.items);
                super::query::filetype_filter(&q.query, &mut pool.items);
                write_features(path, q, &pool);
            }
            // The live path, step for step: an operator query merges deeper
            // because its own filters cut the list after the merge.
            let depth = if super::query::compile(&q.query).has_operators() {
                40
            } else {
                12
            };
            let mut merged = rank::merge(&per_engine, &q.query, intent, &trust, &families, depth);
            if let Some(input) = &captured {
                input.apply_post_adjustments(&mut merged);
            }
            super::site_filter(&q.query, &mut merged);
            super::query::intitle_filter(&q.query, &mut merged);
            super::query::filetype_filter(&q.query, &mut merged);
            for m in merged.iter().take(7) {
                let key = rank::norm_key(&m.url);
                let longest = per_engine
                    .iter()
                    .flat_map(|(_, hits)| hits.iter())
                    .filter(|h| rank::norm_key(&h.url) == key)
                    .map(|h| h.snippet.split_whitespace().collect::<Vec<_>>().join(" "))
                    .max_by_key(|s| s.len())
                    .unwrap_or_default();
                let old = render::clip_snippet(&longest, evidence::EVIDENCE_CHARS);
                shown.add(&q.query, &old, &m.snippet);
            }
            let answers: Vec<AnswerRow> = merged
                .iter()
                .take(opts.max_results)
                .map(|m| AnswerRow {
                    url: m.url.clone(),
                    title: m.title.clone(),
                    snippet: m.snippet.clone(),
                })
                .collect();
            let judged = judge(q, &answers);
            let record = RowRecord {
                id: q.id.clone(),
                shape: q.shape.clone(),
                set: q.set.clone(),
                query: q.query.clone(),
                expect: q.expect.clone(),
                hit: judged.hit,
                key_at: judged.key_at,
                violations: judged.violations,
                junk5: judged.junk5,
                redirects: 0,
                ms: 0,
                urls: answers.iter().map(|a| a.url.clone()).collect(),
                domains: domains(&answers),
                recall: Some(recalled),
                requests: None,
                census: String::new(),
                stale: 0,
                undated: 0,
                foreign: 0,
                raw: None,
                evidence: answers,
                ranking: None,
                reports: Vec::new(),
            };
            if opts.verbose {
                crate::diag!("{:<6} {:<10} {}", q.id, verdict(&record), q.query);
            }
            run.push(&record);
            all.push(&record);
            by_shape.entry(q.shape.clone()).or_default().push(&record);
        }
        crate::diag!(
            "{}",
            run.line(&path.file_name().map_or(String::new(), |f| {
                f.to_string_lossy().chars().take(8).collect()
            }))
        );
    }
    crate::diag!("{TABLE_HEAD}");
    for (shape, t) in &by_shape {
        crate::diag!("{}", t.line(shape));
    }
    crate::diag!("{}", t_line_all(&all));
    crate::diag!("{}", shown.line());
    0
}

#[derive(Deserialize)]
struct SavedRun {
    queries: Vec<SavedQueryAny>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SavedQueryAny {
    /// An artifact row: lane detail under `raw` as `[{source, hits}]`.
    Artifact {
        id: String,
        #[serde(default)]
        raw: Option<Vec<SavedSource>>,
        #[serde(default)]
        ranking: Option<super::rank::MergeInput>,
    },
    /// An older bench run: lane detail under `raw` as a map.
    Legacy {
        id: String,
        #[serde(default)]
        raw: Vec<SavedSource>,
    },
}

impl SavedQueryAny {
    fn into_saved(self) -> Option<SavedQuery> {
        match self {
            Self::Artifact { id, raw, ranking } => raw.map(|raw| SavedQuery { id, raw, ranking }),
            Self::Legacy { id, raw } => (!raw.is_empty()).then_some(SavedQuery {
                id,
                raw,
                ranking: None,
            }),
        }
    }
}

struct SavedQuery {
    id: String,
    raw: Vec<SavedSource>,
    ranking: Option<super::rank::MergeInput>,
}

#[derive(Deserialize)]
struct SavedSource {
    source: String,
    #[serde(default)]
    hits: Vec<SavedHit>,
}

/// A saved candidate. Defined here rather than reusing [`super::engines::Hit`]:
/// the artifact is read back from JSON years of runs may span, and the wire form
/// is this one, not whatever the live struct gains later.
#[derive(Deserialize)]
struct SavedHit {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    rank: usize,
    #[serde(default)]
    published: Option<String>,
}

impl From<SavedHit> for super::engines::Hit {
    fn from(h: SavedHit) -> Self {
        Self {
            title: h.title,
            url: h.url,
            snippet: h.snippet,
            rank: h.rank,
            published: h.published,
        }
    }
}

/// Snippet cost and fit, old against new, over the same results.
#[derive(Default)]
struct SnippetTally {
    n: usize,
    old_chars: usize,
    new_chars: usize,
    old_cover: f64,
    new_cover: f64,
    old_clean_end: usize,
    new_clean_end: usize,
}

impl SnippetTally {
    fn add(&mut self, query: &str, old: &str, new: &str) {
        let clean = |s: &str| s.ends_with(['.', '!', '?']);
        self.n += 1;
        self.old_chars += old.chars().count();
        self.new_chars += new.chars().count();
        self.old_cover += super::evidence::coverage(query, old);
        self.new_cover += super::evidence::coverage(query, new);
        self.old_clean_end += usize::from(clean(old));
        self.new_clean_end += usize::from(clean(new));
    }

    fn line(&self) -> String {
        let n = self.n.max(1) as f64;
        format!(
            "snippets n={}  chars {:.0} -> {:.0}  covers query {:.2} -> {:.2}  ends at a sentence {:.2} -> {:.2}",
            self.n,
            self.old_chars as f64 / n,
            self.new_chars as f64 / n,
            self.old_cover / n,
            self.new_cover / n,
            self.old_clean_end as f64 / n,
            self.new_clean_end as f64 / n,
        )
    }
}

/// The page a candidate is, for training: `2` when its URL matches one of the
/// row's gold patterns, `1` when it is on the same host as one, `0` otherwise.
/// A `must_not` candidate is not a training example at all — the ranker is never
/// asked to choose it.
fn label_of(q: &BenchQuery, url: &str) -> Option<u8> {
    if q.must_not_re().iter().any(|re| re.is_match(url)) {
        return None;
    }
    if q.gold_re().iter().any(|re| re.is_match(url)) {
        return Some(2);
    }
    let host = super::rank::host_of(url);
    let same = q
        .gold
        .iter()
        .filter_map(|pattern| gold_host(pattern))
        .any(|gold| gold == host);
    Some(u8::from(same))
}

/// The host a gold pattern is anchored on: `docs\.rs/tokio[^ ]*` → `docs.rs`.
/// A pattern that names no host (`^arxiv`) yields none, and is just not used for
/// the same-host label.
fn gold_host(pattern: &str) -> Option<String> {
    let head = pattern.split('/').next()?;
    let cleaned: String = head
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .collect();
    let cleaned = cleaned.trim_start_matches("www.").to_string();
    let looks_like_host = cleaned.contains('.') && !cleaned.starts_with('.');
    looks_like_host.then_some(cleaned)
}

/// Append one row's training example: every candidate in the pool the ranker
/// ordered, in the fusion's order, with its features and the label the row's
/// gold patterns give it.
fn write_features(path: &std::path::Path, q: &BenchQuery, pool: &super::rank::Pool) {
    use std::io::Write;
    let features = super::ltr::features(&pool.facts, &pool.items);
    let candidates: Vec<serde_json::Value> = pool
        .items
        .iter()
        .zip(&features)
        .filter_map(|(r, x)| {
            let label = label_of(q, &r.url)?;
            Some(json!({ "url": r.url, "tier": r.tier, "label": label, "x": x.to_vec() }))
        })
        .collect();
    if candidates.is_empty() {
        return;
    }
    let line = json!({
        "id": q.id,
        "set": q.set,
        "shape": q.shape,
        "query_shape": pool.facts.shape().label(),
        "query": q.query,
        "candidates": candidates,
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn row(id: &str, gold: &[&str], key: &[&str], must_not: &[&str]) -> BenchQuery {
        BenchQuery {
            id: id.into(),
            shape: "code".into(),
            set: "gold".into(),
            query: "q".into(),
            gold: strings(gold),
            answer_key: strings(key),
            must_not: strings(must_not),
            expect: "answer".into(),
            only_host: None,
            window: None,
            lang: None,
        }
    }

    fn answers(pairs: &[(&str, &str, &str)]) -> Vec<AnswerRow> {
        pairs
            .iter()
            .map(|(url, title, snippet)| AnswerRow {
                url: (*url).into(),
                title: (*title).into(),
                snippet: (*snippet).into(),
            })
            .collect()
    }

    #[test]
    fn the_first_gold_match_sets_the_rank() {
        let q = row("t", &[r"docs\.rs/tokio/"], &["spawn"], &[]);
        let rows = answers(&[
            ("https://a.com", "a", ""),
            ("https://docs.rs/tokio/latest", "tokio", ""),
            ("https://docs.rs/tokio/x", "x", ""),
        ]);
        assert_eq!(judge(&q, &rows).hit, Some(2));
        assert_eq!(judge(&q, &answers(&[("https://a.com", "a", "")])).hit, None);
    }

    #[test]
    fn the_answer_key_is_a_fact_in_the_evidence_not_a_url_match() {
        let q = row(
            "t",
            &[r"docs\.rs/tokio/"],
            &["spawn_blocking", "blocking thread"],
            &[],
        );
        let ok = answers(&[(
            "https://docs.rs/tokio/x",
            "spawn_blocking",
            "runs the closure on a blocking thread pool",
        )]);
        assert_eq!(judge(&q, &ok).key_at, Some(1));
        // The right URL without the fact is not an accurate answer.
        let thin = answers(&[(
            "https://docs.rs/tokio/x",
            "tokio::task",
            "functions for task management",
        )]);
        assert_eq!(judge(&q, &thin).hit, Some(1));
        assert_eq!(judge(&q, &thin).key_at, None);
    }

    #[test]
    fn a_must_not_counts_inside_the_top_five_only_as_junk() {
        let q = row("t", &[r"docs\.rs/tokio/"], &["x"], &[r"youtube"]);
        let rows = answers(&[
            ("https://a.example/1", "one", ""),
            ("https://a.example/2", "two", ""),
            ("https://a.example/3", "three", ""),
            ("https://a.example/4", "four", ""),
            ("https://a.example/5", "five", ""),
            ("https://youtube.com/watch", "a video", ""),
        ]);
        let j = judge(&q, &rows);
        assert_eq!(j.violations.len(), 1);
        assert_eq!(j.junk5, 0, "rank 6 is outside junk@5 but still a violation");
    }

    #[test]
    fn a_folded_key_ignores_case_and_whitespace() {
        assert_eq!(fold("  Tokio   Spawn_Blocking "), "tokio spawn_blocking");
    }

    #[test]
    fn a_tally_reports_rates_and_reciprocal_rank() {
        let mut t = Tally::default();
        for (hit, key, ms, urls) in [
            (Some(1), Some(1), 1000u64, 3usize),
            (Some(4), None, 2000, 2),
            (None, Some(3), 3000, 0),
            (None, None, 4000, 1),
        ] {
            t.push(&RowRecord {
                id: "x".into(),
                shape: "code".into(),
                set: "gold".into(),
                query: "q".into(),
                expect: "answer".into(),
                hit,
                key_at: key,
                violations: Vec::new(),
                junk5: 0,
                redirects: 0,
                ms,
                urls: vec!["u".into(); urls],
                domains: urls,
                recall: Some(true),
                requests: Some(1),
                census: String::new(),
                stale: 0,
                undated: 0,
                foreign: 0,
                raw: None,
                evidence: Vec::new(),
                ranking: None,
                reports: Vec::new(),
            });
        }
        assert_eq!(t.n, 4);
        assert_eq!(t.at(1), 0.25);
        assert_eq!(t.at(5), 0.5);
        assert!((t.mrr() - (1.0 + 0.25) / 4.0).abs() < 1e-9);
        assert_eq!(t.accuracy(), 0.5, "rank 1 and rank 3 carry the key");
        assert_eq!(t.zero_rate(), 0.25);
        assert_eq!(t.recall(), 1.0);
    }

    #[test]
    fn a_refuse_row_fails_when_it_is_answered_confidently() {
        let base = RowRecord {
            id: "x".into(),
            shape: "adversarial".into(),
            set: "gold".into(),
            query: "q".into(),
            expect: "refuse".into(),
            hit: None,
            key_at: None,
            violations: Vec::new(),
            junk5: 0,
            redirects: 0,
            ms: 10,
            urls: Vec::new(),
            domains: 0,
            recall: Some(false),
            requests: Some(1),
            census: String::new(),
            stale: 0,
            undated: 0,
            foreign: 0,
            raw: None,
            evidence: Vec::new(),
            ranking: None,
            reports: Vec::new(),
        };
        assert!(base.refused_ok(), "an empty answer is the right answer");
        let mut invented = base.clone();
        invented.urls = vec!["https://a".into(), "https://b".into(), "https://c".into()];
        assert!(
            !invented.refused_ok(),
            "three confident results are an invention"
        );
        let mut leaked = base.clone();
        leaked.junk5 = 1;
        assert!(!leaked.refused_ok(), "a must_not match fails the row");
    }

    #[test]
    fn saved_replay_keeps_the_actual_trust_and_independence_state() {
        let mut input = super::super::rank::MergeInput {
            per_engine: Vec::new(),
            query: "rust ownership".into(),
            intent: super::super::intent::Intent::Code,
            trust: [("bing".into(), 0.4)].into(),
            families: Default::default(),
            depth: 12,
            post_adjustments: Default::default(),
        };
        let urls: Vec<_> = (0..5).map(|i| format!("https://example.com/{i}")).collect();
        for _ in 0..3 {
            input.families.observe(&[
                ("bing".into(), urls.clone()),
                ("yahoo".into(), urls.clone()),
            ]);
        }
        let saved: SavedQueryAny =
            serde_json::from_value(json!({"id":"x", "raw":[], "ranking":input})).unwrap();
        let restored = saved.into_saved().unwrap().ranking.unwrap();
        assert_eq!(restored.trust["bing"], 0.4);
        assert_eq!(
            restored.families.family_of("bing"),
            restored.families.family_of("yahoo")
        );
        let legacy: SavedQueryAny = serde_json::from_value(json!({"id":"x", "raw":[]})).unwrap();
        assert!(legacy.into_saved().unwrap().ranking.is_none());
    }

    #[test]
    fn arguments_are_validated_before_any_search_runs() {
        assert!(parse(&strings(&["--set", "bogus"])).is_err());
        assert!(parse(&strings(&["--nope"])).is_err());
        assert!(parse(&strings(&["--pace-ms"])).is_err());
        assert!(parse(&strings(&["--shape", "nonsense"])).is_err());
        assert!(parse(&strings(&["--runs", "0"])).is_err());
        assert!(parse(&strings(&["--runs", "99"])).is_err());
        let o = parse(&strings(&[
            "--set",
            "held",
            "--smoke",
            "--pace-ms",
            "10",
            "--runs",
            "3",
        ]))
        .unwrap();
        assert_eq!(o.set, "held");
        assert!(o.smoke);
        assert_eq!(o.pace, Duration::from_millis(10));
        assert_eq!(o.runs, 3);
        // Cold is the default; --keep-state is the opt-out.
        assert!(o.cold);
        assert!(!parse(&strings(&["--keep-state"])).unwrap().cold);
    }

    #[test]
    fn the_bundled_corpus_validates_and_covers_every_shape() {
        let rows = load(std::path::Path::new("bench/search/queries.json")).unwrap();
        assert!(
            rows.len() >= 150,
            "the gate floor is 150 rows, got {}",
            rows.len()
        );
        for shape in SHAPES {
            assert!(
                rows.iter().any(|r| r.shape == *shape),
                "shape {shape} has no row"
            );
        }
        let held = rows.iter().filter(|r| r.set == "held").count();
        assert!(
            held * 2 >= rows.len(),
            "held-out rows must be at least half the corpus: {held}/{}",
            rows.len()
        );
        assert!(
            rows.iter().any(|r| r.expect == "refuse"),
            "the corpus needs adversarial rows that must be refused"
        );
        // Every answer row's key must be expressible in a title or snippet: an
        // empty phrase would make accuracy true of anything.
        for r in rows.iter().filter(|r| r.is_answer()) {
            assert!(!r.answer_key.is_empty(), "{} has no key", r.id);
            assert!(
                r.answer_key.iter().all(|k| !k.trim().is_empty()),
                "{} has an empty key phrase",
                r.id
            );
        }
    }

    #[test]
    fn a_broken_row_is_refused_with_its_id() {
        let dir = std::env::temp_dir().join(format!("bench-schema-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        let cases: Vec<(&str, &str, &str)> = vec![
            // (name, row json, expected fragment)
            (
                "shape",
                r#"{"id":"a","shape":"nope","set":"gold","query":"q","gold":[".*"],"answer_key":["k"]}"#,
                "unknown shape",
            ),
            (
                "set",
                r#"{"id":"a","shape":"code","set":"both","query":"q","gold":[".*"],"answer_key":["k"]}"#,
                "set",
            ),
            (
                "expect",
                r#"{"id":"a","shape":"code","set":"gold","query":"q","expect":"maybe","gold":[".*"],"answer_key":["k"]}"#,
                "expect",
            ),
            (
                "no key",
                r#"{"id":"a","shape":"code","set":"gold","query":"q","gold":[".*"]}"#,
                "answer_key",
            ),
            (
                "no gold",
                r#"{"id":"a","shape":"code","set":"gold","query":"q","answer_key":["k"]}"#,
                "gold",
            ),
            (
                "refuse without must_not",
                r#"{"id":"a","shape":"adversarial","set":"gold","query":"q","expect":"refuse"}"#,
                "must_not",
            ),
            (
                "bad regex",
                r#"{"id":"a","shape":"code","set":"gold","query":"q","gold":["("],"answer_key":["k"]}"#,
                "gold",
            ),
            (
                "bad lang",
                r#"{"id":"a","shape":"cjk","set":"gold","query":"q","gold":[".*"],"answer_key":["k"],"lang":"jp"}"#,
                "lang",
            ),
            (
                "bad window",
                r#"{"id":"a","shape":"news","set":"gold","query":"q","gold":[".*"],"answer_key":["k"],"window":"3x"}"#,
                "window",
            ),
        ];
        for (name, row, fragment) in cases {
            std::fs::write(&path, format!("{{\"queries\":[{row}]}}")).unwrap();
            let err = load(&path).expect_err(name);
            assert!(err.contains(fragment), "{name}: {err}");
        }
        // Duplicate ids are named, not silently merged.
        std::fs::write(
            &path,
            r#"{"queries":[{"id":"a","shape":"code","set":"gold","query":"q","gold":[".*"],"answer_key":["k"]},{"id":"a","shape":"code","set":"gold","query":"q2","gold":[".*"],"answer_key":["k"]}]}"#,
        )
        .unwrap();
        assert!(load(&path).unwrap_err().contains("duplicate"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_smoke_subset_reaches_every_shape_it_can() {
        let rows = load(std::path::Path::new("bench/search/queries.json")).unwrap();
        let opts = parse(&strings(&["--set", "all", "--smoke"])).unwrap();
        let picked = select(&opts, rows);
        assert!(picked.len() <= SMOKE_MAX);
        for shape in SHAPES {
            assert!(
                picked.iter().any(|r| r.shape == *shape),
                "smoke missed {shape}"
            );
        }
    }
}
