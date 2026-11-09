//! The resolver: a query that names one thing is answered by the registry that
//! owns it, not by a search engine guessing at it.
//!
//! A name is not a topic. `tokio` has a crate, an npm package, a PyPI project
//! and a Wikipedia article, and which one the caller means is in the query's
//! own words (`rust`, `npm`, `pypi`, `github`) — or, when the query is one bare
//! word, in which registry holds the strongest record of that name.
//!
//! Three rules keep it honest:
//!
//! - **One registry when the query says which**, so a name never crosses
//!   namespaces: `rust tokio` asks crates.io and nothing else.
//! - **At most three requests**, all in parallel, and a bare name is cached so
//!   the second ask costs none.
//! - **Ambiguity is shown, not guessed.** When two registries hold an equally
//!   strong record, both answers come back labelled; when one dominates, it is
//!   the answer.
//!
//! The result is a handful of canonical URLs (docs.rs and crates.io for a
//! crate, the package page for npm and PyPI, the repository for GitHub), which
//! enter the merge as a first-party lane and lead it by scope.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::contract::Profile;
use super::engines::Hit;
use crate::fetch::client::Fetcher;

/// A registry answer is believed for a week; a name that resolved to nothing is
/// asked again the next day (registries gain packages).
const TTL_SECS: u64 = 7 * 24 * 3600;
const NEGATIVE_TTL_SECS: u64 = 24 * 3600;
const MAX_ENTRIES: usize = 500;
const DISK_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ecosystem {
    Crates,
    Npm,
    Pypi,
    Github,
}

impl Ecosystem {
    fn label(self) -> &'static str {
        match self {
            Self::Crates => "crates.io",
            Self::Npm => "npm",
            Self::Pypi => "PyPI",
            Self::Github => "GitHub",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Crates => "crates",
            Self::Npm => "npm",
            Self::Pypi => "pypi",
            Self::Github => "github",
        }
    }

    /// Tie-break order when two registries are equally strong: the code
    /// ecosystem this tool is built in first, then the JavaScript one.
    fn order(self) -> u8 {
        match self {
            Self::Crates => 0,
            Self::Npm => 1,
            Self::Pypi => 2,
            Self::Github => 3,
        }
    }
}

/// What the query names, and which registries may answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub name: String,
    /// Set when the query says which ecosystem it means.
    pub ecosystem: Option<Ecosystem>,
    /// The registries to ask: one when the ecosystem is named, otherwise the
    /// three package registries (never more than three requests).
    pub ask: Vec<Ecosystem>,
}

/// The word in the query that names a registry's world.
fn ecosystem_word(word: &str) -> Option<Ecosystem> {
    match word {
        "rust" | "crate" | "crates" | "cargo" => Some(Ecosystem::Crates),
        "npm" | "node" | "nodejs" | "javascript" | "typescript" | "js" => Some(Ecosystem::Npm),
        "python" | "pip" | "pypi" | "py" => Some(Ecosystem::Pypi),
        "github" | "repo" | "repository" => Some(Ecosystem::Github),
        _ => None,
    }
}

/// Is this query a name, or a topic?
///
/// Operators, quoted phrases and exclusions make it a topic. So does more than
/// one word that is not an ecosystem word, or a token that is not a name at all
/// (CJK, punctuation, a bare number).
pub fn plan(query: &str) -> Option<Plan> {
    let lower = query.to_ascii_lowercase();
    if lower.contains("site:")
        || lower.contains('"')
        || lower.contains("::")
        || query.contains('(')
        || query.contains(')')
        || query.contains('?')
        || lower.contains("filetype:")
        || lower.contains("intitle:")
        || query
            .split_whitespace()
            .any(|word| word.starts_with('-') && word.chars().count() > 1)
    {
        return None;
    }
    let profile = Profile::new(query);
    let mut ecosystem: Option<Ecosystem> = None;
    let mut name: Option<String> = None;
    for term in profile.terms() {
        if let Some(eco) = ecosystem_word(term) {
            ecosystem.get_or_insert(eco);
            continue;
        }
        if name.is_some() {
            // Two content words is a topic ("starship launch", "react hooks").
            return None;
        }
        name = Some(term.to_string());
    }
    let name = name?;
    // A package or repository name is ASCII letters, digits and `- _ . @ /`.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '/'))
    {
        return None;
    }
    if name.chars().filter(|c| c.is_alphabetic()).count() < 2 {
        return None;
    }
    let ask = match ecosystem {
        Some(eco) => vec![eco],
        None => vec![Ecosystem::Crates, Ecosystem::Npm, Ecosystem::Pypi],
    };
    Some(Plan {
        name,
        ecosystem,
        ask,
    })
}

/// One exact-name record from one registry.
#[derive(Debug, Clone)]
struct Found {
    eco: Ecosystem,
    name: String,
    version: String,
    description: String,
    /// How strong this registry's record of the name is, 0..=1. Comparable
    /// across registries only as a crude strength class, which is all the
    /// dominance test needs.
    strength: f64,
    /// The canonical page for the record.
    url: String,
    /// The documentation page, when the registry publishes one separately.
    docs: Option<String>,
    /// A short fact for the snippet (downloads, stars).
    fact: String,
}

fn crates_strength(downloads: u64) -> f64 {
    match downloads {
        100_000_000.. => 1.0,
        10_000_000.. => 0.9,
        1_000_000.. => 0.8,
        100_000.. => 0.6,
        10_000.. => 0.3,
        _ => 0.1,
    }
}

/// npm's search score is a blend that an exact name can inflate, so it is a
/// weaker class of evidence than crates.io's download count: capped below 1.0,
/// a real npm giant still dominates a thin crate, and a crate with a billion
/// downloads still dominates npm's name match.
fn npm_strength(popularity: f64) -> f64 {
    if popularity >= 0.8 {
        0.7
    } else if popularity >= 0.5 {
        0.6
    } else if popularity >= 0.2 {
        0.4
    } else {
        0.15
    }
}

/// PyPI's JSON carries no popularity, so a record there is a real package but
/// not evidence of dominance.
const PYPI_STRENGTH: f64 = 0.3;

fn github_strength(stars: u64) -> f64 {
    match stars {
        10_000.. => 1.0,
        1_000.. => 0.7,
        100.. => 0.4,
        _ => 0.2,
    }
}

fn enc(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// What one registry said: here is the record, here is a definite no, or we do
/// not know (a timeout, a rate limit, a 5xx — none of which is evidence that
/// the name does not exist).
enum Answer {
    Found(Vec<Found>),
    Absent,
    Inconclusive,
}

/// Ask one registry for one exact name, inside `budget`.
async fn fetch_one(eco: Ecosystem, name: &str, fetcher: &Fetcher, budget: Duration) -> Answer {
    let url = match eco {
        Ecosystem::Crates => format!("https://crates.io/api/v1/crates?q={}&per_page=5", enc(name)),
        Ecosystem::Npm => format!(
            "https://registry.npmjs.org/-/v1/search?text={}&size=5",
            enc(name)
        ),
        Ecosystem::Pypi => format!("https://pypi.org/pypi/{}/json", enc(name)),
        Ecosystem::Github => format!(
            "https://api.github.com/search/repositories?q={}%20in%3Aname&per_page=5",
            enc(name)
        ),
    };
    let ask = async {
        let out = match fetcher.fetch_once_via(&url, &[], None, false, None).await {
            Ok(out) => out,
            Err(_) => return Answer::Inconclusive,
        };
        match out.status {
            // PyPI answers 404 for a project it does not have: a definite no.
            404 if eco == Ecosystem::Pypi => Answer::Absent,
            200 => {
                let body = String::from_utf8_lossy(&out.body);
                let found = parse(eco, name, &body);
                if found.is_empty() {
                    Answer::Absent
                } else {
                    Answer::Found(found)
                }
            }
            // A rate limit or a broken registry is not a "no".
            _ => Answer::Inconclusive,
        }
    };
    match tokio::time::timeout(budget, ask).await {
        Ok(answer) => answer,
        Err(_) => Answer::Inconclusive,
    }
}

/// Parse a registry's JSON into exact-name records. A record whose name is not
/// the one asked for is not an answer, however the registry ranked it.
fn parse(eco: Ecosystem, name: &str, body: &str) -> Vec<Found> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let want = name.to_ascii_lowercase();
    let mut out = Vec::new();
    match eco {
        Ecosystem::Crates => {
            for c in v["crates"].as_array().into_iter().flatten() {
                let got = c["name"].as_str().unwrap_or("");
                if !got.eq_ignore_ascii_case(&want) {
                    continue;
                }
                let downloads = c["downloads"].as_u64().unwrap_or(0);
                out.push(Found {
                    eco,
                    name: got.to_string(),
                    version: c["max_version"].as_str().unwrap_or("").to_string(),
                    description: c["description"].as_str().unwrap_or("").to_string(),
                    strength: crates_strength(downloads),
                    url: format!("https://crates.io/crates/{got}"),
                    docs: Some(format!("https://docs.rs/{got}/latest/{got}/")),
                    fact: format!("{} downloads", compact(downloads)),
                });
            }
        }
        Ecosystem::Npm => {
            for o in v["objects"].as_array().into_iter().flatten() {
                let p = &o["package"];
                let got = p["name"].as_str().unwrap_or("");
                if !got.eq_ignore_ascii_case(&want) {
                    continue;
                }
                let popularity = o["score"]["detail"]["popularity"]
                    .as_f64()
                    .or_else(|| o["score"]["final"].as_f64())
                    .unwrap_or(0.0);
                out.push(Found {
                    eco,
                    name: got.to_string(),
                    version: p["version"].as_str().unwrap_or("").to_string(),
                    description: p["description"].as_str().unwrap_or("").to_string(),
                    strength: npm_strength(popularity),
                    url: format!("https://www.npmjs.com/package/{got}"),
                    docs: None,
                    fact: "npm package".to_string(),
                });
            }
        }
        Ecosystem::Pypi => {
            // The endpoint is the exact lookup: it answers 200 only for this
            // project, under whatever spelling it canonicalised to.
            let got = v["info"]["name"].as_str().unwrap_or("");
            if !got.eq_ignore_ascii_case(&want) {
                return Vec::new();
            }
            out.push(Found {
                eco,
                name: got.to_string(),
                version: v["info"]["version"].as_str().unwrap_or("").to_string(),
                description: v["info"]["summary"].as_str().unwrap_or("").to_string(),
                strength: PYPI_STRENGTH,
                url: format!("https://pypi.org/project/{got}/"),
                docs: None,
                fact: "PyPI project".to_string(),
            });
        }
        Ecosystem::Github => {
            for r in v["items"].as_array().into_iter().flatten() {
                let got = r["name"].as_str().unwrap_or("");
                if !got.eq_ignore_ascii_case(&want) {
                    continue;
                }
                let full = r["full_name"].as_str().unwrap_or(got);
                let stars = r["stargazers_count"].as_u64().unwrap_or(0);
                out.push(Found {
                    eco,
                    name: full.to_string(),
                    version: String::new(),
                    description: r["description"].as_str().unwrap_or("").to_string(),
                    strength: github_strength(stars),
                    url: r["html_url"]
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| format!("https://github.com/{full}")),
                    docs: None,
                    fact: format!("{} stars", compact(stars)),
                });
            }
        }
    }
    out
}

/// 1_166_056_286 → "1.2B"; 27_000 → "27.0k".
fn compact(n: u64) -> String {
    let f = n as f64;
    if f >= 1e9 {
        format!("{:.1}B", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1}M", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.1}k", f / 1e3)
    } else {
        n.to_string()
    }
}

/// Pick the answer: the named ecosystem, or the strongest record when the query
/// did not say. A clear winner stands alone; an actual tie is shown in full.
fn choose(found: Vec<Found>, ecosystem: Option<Ecosystem>) -> Vec<Found> {
    let mut kept: Vec<Found> = match ecosystem {
        Some(eco) => found.into_iter().filter(|f| f.eco == eco).collect(),
        None => found,
    };
    if ecosystem.is_some() {
        return kept;
    }
    kept.sort_by(|a, b| {
        b.strength
            .total_cmp(&a.strength)
            .then_with(|| a.eco.order().cmp(&b.eco.order()))
    });
    kept.dedup_by(|a, b| a.eco == b.eco);
    let dominant = kept.first().is_some_and(|best| {
        kept.get(1)
            .is_none_or(|next| best.strength - next.strength >= 0.3)
    });
    if dominant {
        kept.truncate(1);
    }
    kept
}

/// Registry records become the results the agent sees: the documentation page
/// first for a crate (that is what a caller reads), then the registry record.
fn to_hits(chosen: &[Found]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut rank = 0usize;
    for f in chosen {
        let tail = |s: &str| {
            let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if s.is_empty() {
                String::new()
            } else {
                format!(" — {}", s.chars().take(140).collect::<String>())
            }
        };
        if let Some(docs) = &f.docs {
            hits.push(Hit {
                title: format!("{} — Rust crate documentation", f.name),
                url: docs.clone(),
                snippet: format!("docs.rs · v{}{}", f.version, tail(&f.description)),
                rank,
                published: None,
            });
            rank += 1;
        }
        hits.push(Hit {
            title: format!("{} — {}", f.name, f.eco.label()),
            url: f.url.clone(),
            snippet: format!(
                "{} · {}{}{}",
                f.eco.label(),
                f.fact,
                if f.version.is_empty() {
                    String::new()
                } else {
                    format!(" · v{}", f.version)
                },
                tail(&f.description)
            ),
            rank,
            published: None,
        });
        rank += 1;
    }
    hits
}

/// A resolved answer, ready to enter the merge as a first-party lane.
#[derive(Debug)]
pub struct Resolved {
    pub hits: Vec<Hit>,
    pub ms: u64,
    /// The ecosystem that answered, for the census.
    pub label: String,
}

fn cache_key(plan: &Plan) -> String {
    format!(
        "{}:{}",
        plan.ecosystem.map(Ecosystem::key).unwrap_or("any"),
        plan.name.to_ascii_lowercase()
    )
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Disk {
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    at: u64,
    #[serde(default)]
    hits: Vec<StoredHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredHit {
    title: String,
    url: String,
    snippet: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path() -> Option<std::path::PathBuf> {
    Some(crate::paths::cache_dir().join("search-namespaces.json"))
}

fn load_disk() -> Disk {
    let Some(path) = path() else {
        return Disk::default();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Disk::default();
    };
    match serde_json::from_slice::<Disk>(&bytes) {
        Ok(mut disk) if disk.version == DISK_VERSION => {
            disk.version = DISK_VERSION;
            disk
        }
        _ => Disk::default(),
    }
}

fn save_disk(disk: &Disk) {
    let Some(path) = path() else { return };
    let Ok(json) = serde_json::to_vec(disk) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if crate::config::write_private(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

enum Cached {
    Fresh(Vec<Hit>),
    Negative,
    Miss,
}

fn cache_get(key: &str) -> Cached {
    let disk = load_disk();
    let Some(entry) = disk.entries.get(key) else {
        return Cached::Miss;
    };
    let age = now_secs().saturating_sub(entry.at);
    if entry.hits.is_empty() {
        return if age < NEGATIVE_TTL_SECS {
            Cached::Negative
        } else {
            Cached::Miss
        };
    }
    if age >= TTL_SECS {
        return Cached::Miss;
    }
    Cached::Fresh(
        entry
            .hits
            .iter()
            .enumerate()
            .map(|(rank, h)| Hit {
                title: h.title.clone(),
                url: h.url.clone(),
                snippet: h.snippet.clone(),
                rank,
                published: None,
            })
            .collect(),
    )
}

fn cache_put(key: &str, hits: &[Hit]) {
    let mut disk = load_disk();
    disk.version = DISK_VERSION;
    disk.entries.insert(
        key.to_string(),
        Entry {
            at: now_secs(),
            hits: hits
                .iter()
                .map(|h| StoredHit {
                    title: h.title.clone(),
                    url: h.url.clone(),
                    snippet: h.snippet.clone(),
                })
                .collect(),
        },
    );
    while disk.entries.len() > MAX_ENTRIES {
        let Some(oldest) = disk
            .entries
            .iter()
            .min_by_key(|(_, e)| e.at)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        disk.entries.remove(&oldest);
    }
    save_disk(&disk);
}

/// What a set of registry answers settles to.
enum Settled {
    /// A record (or several, when the name is genuinely ambiguous).
    Chosen(Vec<Found>),
    /// Every registry asked answered, and none holds the name: a definite no.
    Nothing,
    /// At least one registry did not answer: nothing can be concluded.
    Unknown,
}

/// A bare name is only disambiguated when every registry asked has spoken: one
/// fast registry answering while another is rate-limited is not evidence that
/// the name belongs to the fast one.
fn settle(answers: Vec<Answer>, ecosystem: Option<Ecosystem>) -> Settled {
    if answers.iter().any(|a| matches!(a, Answer::Inconclusive)) {
        return Settled::Unknown;
    }
    let found: Vec<Found> = answers
        .into_iter()
        .flat_map(|a| match a {
            Answer::Found(v) => v,
            _ => Vec::new(),
        })
        .collect();
    let chosen = choose(found, ecosystem);
    if chosen.is_empty() {
        Settled::Nothing
    } else {
        Settled::Chosen(chosen)
    }
}

/// What resolving a name produced.
pub enum Resolution {
    Hits(Resolved),
    /// Every registry asked answered, and none holds the name.
    Nothing,
    /// A registry did not answer: the name is not disproved, only unproven.
    Unknown,
}

/// Resolve a planned name. A name no registry holds falls through to the
/// ordinary fan-out; a registry we could not reach is not a "no".
pub async fn run(plan: &Plan, fetcher: &Fetcher, budget: Duration) -> Resolution {
    let key = cache_key(plan);
    match cache_get(&key) {
        Cached::Fresh(hits) => {
            let label = plan
                .ecosystem
                .map(Ecosystem::label)
                .unwrap_or("registry")
                .to_string();
            return Resolution::Hits(Resolved { hits, ms: 0, label });
        }
        Cached::Negative => return Resolution::Nothing,
        Cached::Miss => {}
    }
    let started = Instant::now();
    let asks = plan
        .ask
        .iter()
        .map(|eco| fetch_one(*eco, &plan.name, fetcher, budget));
    let answers: Vec<Answer> = futures_util::future::join_all(asks).await;
    let ms = started.elapsed().as_millis() as u64;
    let chosen = match settle(answers, plan.ecosystem) {
        Settled::Chosen(chosen) => chosen,
        Settled::Nothing => {
            cache_put(&key, &[]);
            return Resolution::Nothing;
        }
        Settled::Unknown => return Resolution::Unknown,
    };
    let hits = to_hits(&chosen);
    cache_put(&key, &hits);
    let label = chosen
        .first()
        .map(|f| {
            if chosen.len() > 1 {
                "registry".to_string()
            } else {
                f.eco.label().to_string()
            }
        })
        .unwrap_or_else(|| "registry".to_string());
    Resolution::Hits(Resolved { hits, ms, label })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_ecosystem_asks_one_registry() {
        let p = plan("rust tokio").expect("a name");
        assert_eq!(p.name, "tokio");
        assert_eq!(p.ecosystem, Some(Ecosystem::Crates));
        assert_eq!(p.ask.len(), 1, "one request");
        assert_eq!(plan("npm react").unwrap().ecosystem, Some(Ecosystem::Npm));
        assert_eq!(
            plan("pypi requests").unwrap().ecosystem,
            Some(Ecosystem::Pypi)
        );
        assert_eq!(
            plan("github tokio").unwrap().ecosystem,
            Some(Ecosystem::Github)
        );
    }

    #[test]
    fn a_bare_name_asks_at_most_three_registries() {
        let p = plan("tokio").expect("a name");
        assert_eq!(p.ecosystem, None);
        assert_eq!(p.ask.len(), 3);
    }

    #[test]
    fn a_topic_is_not_a_name() {
        for query in [
            "rust tokio spawn_blocking",
            "attention is all you need",
            "how to replace a bicycle tire",
            "site:github.com/tokio-rs",
            "\"exact phrase\"",
            "react -native",
            "tokio 2026",
            "rust",
            "??",
        ] {
            assert!(plan(query).is_none(), "{query:?} is a topic, not a name");
        }
    }

    #[test]
    fn a_crate_is_the_strongest_record_for_its_name() {
        let crates = parse(
            Ecosystem::Crates,
            "tokio",
            r#"{"crates":[{"name":"tokio","max_version":"1.54.1","description":"An async runtime","downloads":1166056286}]}"#,
        );
        let npm = parse(
            Ecosystem::Npm,
            "tokio",
            r#"{"objects":[{"package":{"name":"tokio","version":"0.1.2","description":"Web scraping"},"score":{"detail":{"popularity":0.02}}}]}"#,
        );
        let pypi = parse(
            Ecosystem::Pypi,
            "tokio",
            r#"{"info":{"name":"tokio","version":"0.1","summary":"Tokio"}}"#,
        );
        assert_eq!(choose(crates.clone(), None).len(), 1);
        let mut all = crates;
        all.extend(npm);
        all.extend(pypi);
        let chosen = choose(all, None);
        assert_eq!(chosen.len(), 1, "one dominant record");
        assert_eq!(chosen[0].eco, Ecosystem::Crates);
    }

    #[test]
    fn an_npm_giant_wins_its_own_name() {
        let crates = parse(
            Ecosystem::Crates,
            "react",
            r#"{"crates":[{"name":"react","max_version":"0.1.0","description":"","downloads":20000}]}"#,
        );
        let npm = parse(
            Ecosystem::Npm,
            "react",
            r#"{"objects":[{"package":{"name":"react","version":"19.0.0"},"score":{"detail":{"popularity":0.95}}}]}"#,
        );
        let mut all = crates;
        all.extend(npm);
        let chosen = choose(all, None);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].eco, Ecosystem::Npm);
    }

    #[test]
    fn a_silent_registry_makes_the_name_unknowable() {
        let crates = parse(
            Ecosystem::Crates,
            "tokio",
            r#"{"crates":[{"name":"tokio","max_version":"1.0.0","downloads":1000000000}]}"#,
        );
        // npm timed out: the crate is strong, but we do not know what npm would
        // have said, so the namespace is not settled.
        assert!(matches!(
            settle(
                vec![
                    Answer::Found(crates.clone()),
                    Answer::Inconclusive,
                    Answer::Absent
                ],
                None
            ),
            Settled::Unknown
        ));
        // Every registry spoke, and only crates holds the name.
        assert!(matches!(
            settle(
                vec![Answer::Found(crates), Answer::Absent, Answer::Absent],
                None
            ),
            Settled::Chosen(v) if v.len() == 1
        ));
        // Every registry spoke, and nobody holds it.
        assert!(matches!(
            settle(vec![Answer::Absent, Answer::Absent, Answer::Absent], None),
            Settled::Nothing
        ));
    }

    #[test]
    fn an_actual_tie_is_shown_in_full_and_labelled() {
        let crates = parse(
            Ecosystem::Crates,
            "requests",
            r#"{"crates":[{"name":"requests","max_version":"0.1.0","description":"","downloads":12000}]}"#,
        );
        let pypi = parse(
            Ecosystem::Pypi,
            "requests",
            r#"{"info":{"name":"requests","version":"2.32.0","summary":"HTTP for Humans"}}"#,
        );
        let mut all = crates;
        all.extend(pypi);
        let chosen = choose(all, None);
        assert_eq!(chosen.len(), 2, "no dominant winner: show both");
        let hits = to_hits(&chosen);
        assert!(
            hits.iter()
                .any(|h| h.url.contains("pypi.org/project/requests"))
        );
        assert!(
            hits.iter()
                .any(|h| h.url.contains("crates.io/crates/requests"))
        );
    }

    #[test]
    fn a_named_ecosystem_never_crosses_into_another() {
        let crates = parse(
            Ecosystem::Crates,
            "tokio",
            r#"{"crates":[{"name":"tokio","max_version":"1.0.0","description":"","downloads":1}]}"#,
        );
        let npm = parse(
            Ecosystem::Npm,
            "tokio",
            r#"{"objects":[{"package":{"name":"tokio","version":"0.1.2"},"score":{"detail":{"popularity":0.9}}}]}"#,
        );
        let mut all = crates;
        all.extend(npm);
        let chosen = choose(all, Some(Ecosystem::Crates));
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].eco, Ecosystem::Crates);
    }

    #[test]
    fn a_crate_answers_with_its_documentation_first() {
        let crates = parse(
            Ecosystem::Crates,
            "tokio",
            r#"{"crates":[{"name":"tokio","max_version":"1.54.1","description":"An async runtime","downloads":1166056286}]}"#,
        );
        let hits = to_hits(&crates);
        assert_eq!(hits[0].url, "https://docs.rs/tokio/latest/tokio/");
        assert_eq!(hits[1].url, "https://crates.io/crates/tokio");
        assert!(hits[0].snippet.contains("1.54.1"));
        assert_eq!(hits[0].rank, 0);
        assert_eq!(hits[1].rank, 1);
    }

    #[test]
    fn a_record_that_is_not_the_name_asked_for_is_not_an_answer() {
        let found = parse(
            Ecosystem::Crates,
            "tokio",
            r#"{"crates":[{"name":"taos","max_version":"0.20.0","downloads":999999999}]}"#,
        );
        assert!(found.is_empty(), "a cousin is not the name");
    }

    #[test]
    fn registry_json_that_is_not_json_is_not_an_answer() {
        assert!(parse(Ecosystem::Npm, "react", "<html>no</html>").is_empty());
        assert!(parse(Ecosystem::Pypi, "requests", "").is_empty());
    }

    #[test]
    fn compact_reads_like_a_person_wrote_it() {
        assert_eq!(compact(1_166_056_286), "1.2B");
        assert_eq!(compact(27_000), "27.0k");
        assert_eq!(compact(9), "9");
    }
}
