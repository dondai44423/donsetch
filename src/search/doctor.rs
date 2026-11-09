//! Source health you can read and test: `donsetch search --sources` shows where
//! every source stands, and `donsetch search --doctor` runs a fixed battery of
//! canary queries to find out.
//!
//! A canary is a query whose answer is owned by a first-party interface: arXiv
//! knows where "Attention Is All You Need" lives, crates.io knows `tokio`. When
//! that owner returns the answer the canary is real for this run, and every
//! general source can then be judged on whether it returned it too. A source that
//! answers but does not return an answer the owner confirmed is serving something
//! other than the web (a bot-page, a stale index); one that does not answer says
//! why. If the owner does not confirm (down, throttled), the canary is dropped
//! for the run: a broken oracle must never convict a source.
//!
//! The doctor runs through the real search path, so what it learns lands in the
//! same trust and failure state every search uses.

use std::collections::BTreeMap;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::engines::Hit;
use super::intent::Intent;
use super::{Searcher, SourceState, rank};

/// A query with a known answer, and who owns it.
pub struct Canary {
    pub query: &'static str,
    pub intent: Intent,
    /// The first-party source whose raw answer must contain `expect` for the
    /// canary to count.
    pub owner: &'static str,
    pub expect: &'static str,
}

/// Eight answers that do not move: a paper, a crate, a package, two encyclopedia
/// entries, a repository, a biomedical paper, and a second paper.
pub const CANARIES: [Canary; 8] = [
    Canary {
        query: "attention is all you need",
        intent: Intent::Paper,
        owner: "arxiv",
        expect: r"arxiv\.org/abs/1706\.03762",
    },
    Canary {
        query: "deep residual learning for image recognition",
        intent: Intent::Paper,
        owner: "arxiv",
        expect: r"arxiv\.org/abs/1512\.03385",
    },
    Canary {
        query: "rust tokio crate",
        intent: Intent::Code,
        owner: "crates",
        expect: r"crates\.io/crates/tokio$|docs\.rs/tokio|tokio\.rs",
    },
    Canary {
        query: "npm express package",
        intent: Intent::Code,
        owner: "npm",
        expect: r"npmjs\.com/package/express$|expressjs\.com",
    },
    Canary {
        query: "python programming language",
        intent: Intent::Entity,
        owner: "wikipedia",
        expect: r"en\.wikipedia\.org/wiki/Python_\(programming_language\)|python\.org",
    },
    Canary {
        query: "linux kernel",
        intent: Intent::Entity,
        owner: "wikipedia",
        expect: r"en\.wikipedia\.org/wiki/Linux_kernel|kernel\.org",
    },
    Canary {
        query: "ripgrep",
        intent: Intent::Code,
        owner: "github",
        expect: r"github\.com/BurntSushi/ripgrep$",
    },
    Canary {
        query: "highly accurate protein structure prediction with alphafold",
        intent: Intent::Paper,
        owner: "europepmc",
        expect: r"pubmed\.ncbi\.nlm\.nih\.gov/34265844|nature\.com/articles/s41586-021-03819-2|pmc\.ncbi\.nlm\.nih\.gov/articles/PMC8371605",
    },
];

/// What one source did across the canaries that were real this run.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tally {
    /// Canaries the source was in the roster for.
    pub asked: u32,
    /// Of those, answers with content.
    pub answered: u32,
    /// Of those, answers that contained the owner-confirmed result.
    pub contained: u32,
    /// Canaries this source owns (first-party sources only).
    pub owned: u32,
    pub owned_confirmed: u32,
    /// What it said when it did not answer.
    pub said: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Verdict {
    /// Returns what the owner returns.
    Verified,
    /// Answers, but not with the owner-confirmed result.
    Suspect(String),
    /// Does not answer; what it said instead.
    Failing(String),
    /// Nothing in the battery speaks to it.
    Unjudged,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Verdict::Verified => write!(f, "verified"),
            Verdict::Suspect(why) => write!(f, "suspect: {why}"),
            Verdict::Failing(why) => write!(f, "failing: {why}"),
            Verdict::Unjudged => write!(f, "not covered by the canaries"),
        }
    }
}

fn most_said(t: &Tally) -> String {
    t.said
        .iter()
        .max_by_key(|(_, n)| **n)
        .map_or_else(|| "no answer".to_string(), |(s, n)| format!("{s} ×{n}"))
}

/// The verdict for one source from what it did.
///
/// A first-party source is judged only on the canaries it owns: it must return
/// the answer it owns. A general source needs at least two answers to be judged
/// at all, and must return the owner's answer in at least half of them.
pub fn judge(source: &str, t: &Tally) -> Verdict {
    if rank::is_vertical(source) {
        return match (t.owned, t.owned_confirmed) {
            (0, _) => Verdict::Unjudged,
            (o, c) if c == o => Verdict::Verified,
            (o, c) => Verdict::Failing(format!("owner confirmed {c} of {o}")),
        };
    }
    if t.asked == 0 {
        return Verdict::Unjudged;
    }
    if t.answered < 2 {
        return Verdict::Failing(most_said(t));
    }
    if t.contained * 2 >= t.answered {
        Verdict::Verified
    } else {
        Verdict::Suspect(format!(
            "returned the owned answer in {} of {} answers",
            t.contained, t.answered
        ))
    }
}

/// Record the owner's answer: whether the first-party interface, asked directly,
/// returned the expected result. Only a confirmed canary is run through the
/// search path (see [`observe`]).
pub fn record_owner(
    canary: &Canary,
    owner_hits: &[Hit],
    tallies: &mut BTreeMap<String, Tally>,
) -> bool {
    let confirmed =
        Regex::new(canary.expect).is_ok_and(|re| owner_hits.iter().any(|h| re.is_match(&h.url)));
    let t = tallies.entry(canary.owner.to_string()).or_default();
    t.owned += 1;
    t.owned_confirmed += u32::from(confirmed);
    confirmed
}

/// Fold one confirmed canary's search into the tallies. `report` is the census
/// `(source, items, status)`, `raw` each source's hits before the merge. General
/// sources are judged here; first-party ones only by [`record_owner`].
pub fn observe(
    canary: &Canary,
    report: &[super::SourceLine],
    raw: &[(String, Vec<Hit>)],
    tallies: &mut BTreeMap<String, Tally>,
) {
    let Ok(expect) = Regex::new(canary.expect) else {
        return;
    };
    let contains = |name: &str| {
        raw.iter()
            .filter(|(n, _)| n == name)
            .any(|(_, hits)| hits.iter().any(|h| expect.is_match(&h.url)))
    };
    for line in report {
        let name = &line.id;
        // A source the plan did not ask, that the ledger rested, or that the
        // scheduler cut as late said nothing about this canary: it is neither
        // asked nor convicted.
        if rank::is_vertical(name) || matches!(line.state, "not asked" | "rested" | "late") {
            continue;
        }
        let t = tallies.entry(name.clone()).or_default();
        t.asked += 1;
        if matches!(line.status.as_str(), "ok" | "thin" | "cached") && line.items > 0 {
            t.answered += 1;
            if contains(name) {
                t.contained += 1;
            }
        } else {
            *t.said.entry(line.status.clone()).or_default() += 1;
        }
    }
}

// ── The last doctor run, for `--sources` ─────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct LastRun {
    at_unix: u64,
    verdicts: BTreeMap<String, Verdict>,
}

fn last_path() -> Option<std::path::PathBuf> {
    let dir = crate::paths::cache_dir();
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("search-canary.json"))
}

fn load_last() -> LastRun {
    last_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_last(run: &LastRun) {
    let Some(path) = last_path() else { return };
    let Ok(json) = serde_json::to_vec(run) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if crate::config::write_private(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{}m", s.div_ceil(60)),
        5400..=129_599 => format!("{}h", s.div_ceil(3600)),
        _ => format!("{}d", s / 86_400),
    }
}

fn standing(state: SourceState) -> String {
    match state {
        SourceState::Verified => "verified".into(),
        SourceState::Suspect => "suspect".into(),
        SourceState::Benched(ttl) => format!("benched, asked again in {}", span(ttl)),
    }
}

// ── The commands ─────────────────────────────────────────────────────────────

const USAGE: &str = "usage: donsetch search --sources [--json]\n       donsetch search --doctor [--json] [--pace-ms N]\n\n--sources  where every search source stands: verified, suspect, or benched until a time\n--doctor   run eight canary queries through the real search path and judge each source";

struct Args {
    doctor: bool,
    json: bool,
    pace: Duration,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        doctor: false,
        json: false,
        pace: Duration::from_millis(4000),
    };
    let mut sources = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--doctor" => a.doctor = true,
            "--sources" => sources = true,
            "--json" => a.json = true,
            "--pace-ms" => {
                i += 1;
                let ms = args
                    .get(i)
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| format!("--pace-ms needs a number\n{USAGE}"))?;
                a.pace = Duration::from_millis(ms);
            }
            "-h" | "--help" => return Err(USAGE.to_string()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
        i += 1;
    }
    if a.doctor == sources {
        return Err(format!("pick one of --sources or --doctor\n{USAGE}"));
    }
    Ok(a)
}

/// True when `search`'s arguments ask for the health commands rather than a
/// search.
pub fn wants(args: &[String]) -> bool {
    args.iter().any(|a| a == "--doctor" || a == "--sources")
}

fn searcher() -> Result<Searcher, String> {
    let egress = std::sync::Arc::new(super::egress::EgressPool::from_env());
    super::egress::install_global(std::sync::Arc::clone(&egress));
    let fetcher =
        crate::fetch::client::Fetcher::new(crate::profile::BrowserProfile::host_default())
            .map_err(|e| e.to_string())?
            .with_egress(std::sync::Arc::clone(&egress));
    Ok(Searcher::new_shared(fetcher, egress))
}

pub async fn run(args: &[String]) -> i32 {
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            crate::diag!("{e}");
            return 2;
        }
    };
    let searcher = match searcher() {
        Ok(s) => s,
        Err(e) => {
            crate::diag!("search: {e}");
            return 1;
        }
    };
    let verdicts = if a.doctor {
        Some(doctor(&searcher, a.pace, a.json).await)
    } else {
        None
    };
    let last = if verdicts.is_some() {
        load_last()
    } else {
        let l = load_last();
        if l.at_unix == 0 {
            crate::diag!("(no canary run yet; `donsetch search --doctor` runs one)");
        }
        l
    };
    // Give the persisted health a moment to be what the table reads.
    let health = searcher.health_report();
    let age = if last.at_unix == 0 {
        None
    } else {
        Some(Duration::from_secs(now_unix().saturating_sub(last.at_unix)))
    };
    if a.json {
        let rows: Vec<serde_json::Value> = health
            .iter()
            .map(|h| {
                serde_json::json!({
                    "source": h.name,
                    "standing": standing(h.state),
                    "trust": (h.trust * 100.0).round() / 100.0,
                    "strikes": h.strikes,
                    "family": h.family,
                    "canary": last.verdicts.get(&h.name).map(ToString::to_string),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"sources": rows, "canary_age_s": age.map(|d| d.as_secs())})
        );
        return 0;
    }
    println!(
        "{:<12} {:<28} {:>5}  {:<10} last canary",
        "source", "standing", "trust", "family"
    );
    for h in &health {
        let canary = last.verdicts.get(&h.name).map_or_else(
            || match h.state {
                SourceState::Benched(_) => "not asked while benched".to_string(),
                _ => "-".to_string(),
            },
            ToString::to_string,
        );
        println!(
            "{:<12} {:<28} {:>5.2}  {:<10} {canary}",
            h.name,
            standing(h.state),
            h.trust,
            h.family
        );
    }
    if let Some(age) = age {
        println!("\ncanaries last ran {} ago", span(age));
    }
    0
}

/// Run the battery, one canary at a time and paced like a person, and judge.
async fn doctor(searcher: &Searcher, pace: Duration, quiet: bool) -> BTreeMap<String, Verdict> {
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    let mut real = 0;
    for (n, canary) in CANARIES.iter().enumerate() {
        if n > 0 {
            tokio::time::sleep(pace).await;
        }
        // The owner first, asked directly: a canary is real only if the
        // first-party interface returns its answer right now.
        let owner_hits = searcher.first_party(canary.owner, canary.query).await;
        let counted = record_owner(canary, &owner_hits, &mut tallies);
        if counted {
            if canary.owner == "arxiv" {
                // arXiv asks for three seconds between requests, and the search
                // below asks it as well.
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            // A canary measures the sources now, never a remembered answer: a
            // cached outcome has no per-source hits and would convict everyone.
            searcher.forget_cached();
            if let Ok(o) = searcher.search(canary.query, 10, Some(canary.intent)).await
                && !o.cached
            {
                observe(
                    canary,
                    &super::source_states(&o.report),
                    &searcher.last_raw(),
                    &mut tallies,
                );
            }
        }
        real += usize::from(counted);
        if !quiet {
            crate::diag!(
                "canary {}/{}  {:<56} {}",
                n + 1,
                CANARIES.len(),
                canary.query,
                if counted {
                    "owner confirmed"
                } else {
                    "owner did not confirm; not counted"
                }
            );
        }
    }
    if !quiet {
        crate::diag!("{real} of {} canaries were real this run\n", CANARIES.len());
    }
    let verdicts: BTreeMap<String, Verdict> = tallies
        .iter()
        .map(|(name, t)| (name.clone(), judge(name, t)))
        .collect();
    save_last(&LastRun {
        at_unix: now_unix(),
        verdicts: verdicts.clone(),
    });
    verdicts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(url: &str) -> Hit {
        Hit {
            title: String::new(),
            url: url.into(),
            snippet: String::new(),
            rank: 0,
            published: None,
        }
    }

    // Built through `source_states`, the path the live doctor takes: a test
    // that fed `observe` hand-made statuses once passed while production
    // handed it census words and convicted every source.
    fn st(name: &str, items: usize, status: &str) -> crate::search::SourceLine {
        let report = crate::search::EngineReport {
            engine: name.into(),
            profile: None,
            status: status.into(),
            hits: items,
            ms: 0,
            egress: "direct".into(),
            requests: 1,
        };
        crate::search::source_states(&[report]).remove(0)
    }

    #[test]
    fn the_battery_compiles_and_each_answer_is_owned_by_a_first_party_source() {
        for c in &CANARIES {
            Regex::new(c.expect).expect(c.expect);
            assert!(rank::is_vertical(c.owner), "{} is not first-party", c.owner);
        }
        assert_eq!(CANARIES.len(), 8);
    }

    #[test]
    fn a_source_is_judged_by_whether_it_returns_what_the_owner_confirmed() {
        let canary = &CANARIES[0];
        let owned = hit("https://arxiv.org/abs/1706.03762");
        let junk = hit("https://example.com/javascript-libraries");
        let raw = vec![
            ("arxiv".to_string(), vec![owned.clone()]),
            ("ddg".to_string(), vec![junk.clone(), owned]),
            ("bing".to_string(), vec![junk]),
        ];
        let report = vec![
            st("arxiv", 1, "ok"),
            st("ddg", 2, "ok"),
            st("bing", 1, "ok"),
            st("mojeek", 0, "403"),
        ];
        let mut tallies = BTreeMap::new();
        for _ in 0..3 {
            assert!(record_owner(canary, &raw[0].1, &mut tallies));
            observe(canary, &report, &raw, &mut tallies);
        }
        assert_eq!(judge("arxiv", &tallies["arxiv"]), Verdict::Verified);
        assert_eq!(judge("ddg", &tallies["ddg"]), Verdict::Verified);
        assert!(matches!(
            judge("bing", &tallies["bing"]),
            Verdict::Suspect(_)
        ));
        assert_eq!(
            judge("mojeek", &tallies["mojeek"]),
            Verdict::Failing("403 ×3".into())
        );
    }

    #[test]
    fn a_canary_its_owner_does_not_confirm_convicts_nobody() {
        let canary = &CANARIES[0];
        // arXiv is down: asked directly it returns nothing, so the answer is
        // unconfirmed and the search path is never run for this canary.
        let mut tallies = BTreeMap::new();
        assert!(!record_owner(canary, &[], &mut tallies));
        assert!(
            !record_owner(
                canary,
                &[hit("https://arxiv.org/abs/9999.00000")],
                &mut tallies
            ),
            "an unrelated result is not the answer"
        );
        assert!(!tallies.contains_key("ddg"), "ddg was not charged");
        assert_eq!(
            judge("arxiv", &tallies["arxiv"]),
            Verdict::Failing("owner confirmed 0 of 2".into())
        );
        assert_eq!(judge("ddg", &Tally::default()), Verdict::Unjudged);
        // A first-party source that owns nothing in the battery is not judged.
        assert_eq!(judge("stackexchange", &Tally::default()), Verdict::Unjudged);
    }

    #[test]
    fn one_answer_is_too_few_to_judge_a_source_that_otherwise_fails() {
        let t = Tally {
            asked: 6,
            answered: 1,
            contained: 1,
            said: BTreeMap::from([("late".to_string(), 5)]),
            ..Tally::default()
        };
        assert_eq!(judge("yahoo", &t), Verdict::Failing("late ×5".into()));
    }

    #[test]
    fn the_health_commands_are_recognized_and_exclusive() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(wants(&a(&["--sources"])) && wants(&a(&["x", "--doctor"])));
        assert!(!wants(&a(&["rust", "tokio"])));
        assert!(parse(&a(&["--sources", "--doctor"])).is_err());
        assert!(parse(&a(&["--json"])).is_err());
        assert!(parse(&a(&["--doctor", "--pace-ms", "oops"])).is_err());
        assert!(parse(&a(&["--doctor", "--pace-ms", "10"])).unwrap().doctor);
    }

    #[test]
    fn spans_read_naturally() {
        assert_eq!(span(Duration::from_secs(45)), "45s");
        assert_eq!(span(Duration::from_secs(600)), "10m");
        assert_eq!(span(Duration::from_secs(7200)), "2h");
        assert_eq!(span(Duration::from_secs(3 * 86_400)), "3d");
    }
}
