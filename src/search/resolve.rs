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

use super::engines::Hit;
use crate::fetch::client::Fetcher;

/// A registry answer is believed for a week; a name that resolved to nothing is
/// asked again the next day (registries gain packages).
const TTL_SECS: u64 = 7 * 24 * 3600;
const NEGATIVE_TTL_SECS: u64 = 24 * 3600;
const MAX_ENTRIES: usize = 500;
/// 3: a name the query did not call a package is answered only by a strong record.
const DISK_VERSION: u32 = 3;

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
    /// Set when the query calls the name a package ("crate", "npm", "pip",
    /// "repo"). A language word alone ("rust ownership", "python decorators")
    /// or a bare word may be a topic, so only a strong record answers it.
    pub package_named: bool,
}

/// A word that says the query is about a package, not about a language.
fn package_word(word: &str) -> bool {
    matches!(
        word,
        "crate" | "crates" | "cargo" | "npm" | "pip" | "pypi" | "github" | "repo" | "repository"
    )
}

/// The strength a record needs to answer a query that did not call the name a
/// package: about 100k all-time downloads on crates.io, 5k a week on npm,
/// a few hundred GitHub stars. PyPI publishes no popularity, so a PyPI record
/// answers only a query that names it.
const NAME_PROOF: f64 = 0.6;

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
    // Whitespace words, not extracted terms: a package name is one word, and
    // `serde_json` or `react-router` must not be split into a topic.
    let mut ecosystem: Option<Ecosystem> = None;
    let mut package_named = false;
    let mut name: Option<String> = None;
    for word in query.split_whitespace() {
        let word = word.trim_matches(|c: char| {
            !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '/'))
        });
        if word.is_empty() {
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if let Some(eco) = ecosystem_word(&lower) {
            ecosystem.get_or_insert(eco);
            package_named |= package_word(&lower);
            continue;
        }
        if name.is_some() {
            // Two content words is a topic ("starship launch", "react hooks").
            return None;
        }
        name = Some(word.to_string());
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
        package_named,
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
    /// The project's own pages as the registry lists them: documentation,
    /// homepage, source repository. Free: they came in the same answer.
    links: Vec<(&'static str, String)>,
}

/// A registry's link as a page the caller can read: `git+https://…/x.git` and
/// `git://` are clone addresses of the same repository page.
fn page_link(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("git+").unwrap_or(raw);
    let raw = match raw.strip_prefix("git://") {
        Some(rest) => format!("https://{rest}"),
        None => raw.to_string(),
    };
    let raw = raw
        .strip_suffix(".git")
        .unwrap_or(&raw)
        .trim_end_matches('/');
    let u = url::Url::parse(raw).ok()?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return None;
    }
    Some(raw.to_string())
}

/// The project's pages, deduplicated, and never a page the answer already
/// carries (the registry record, docs.rs).
fn links_from(
    already: &[&str],
    candidates: &[(&'static str, Option<&str>)],
) -> Vec<(&'static str, String)> {
    let key = |u: &str| crate::search::rank::norm_key(u);
    let mut seen: Vec<String> = already.iter().map(|u| key(u)).collect();
    let mut out = Vec::new();
    for (kind, raw) in candidates {
        let Some(url) = raw.and_then(page_link) else {
            continue;
        };
        let k = key(&url);
        if seen.contains(&k) {
            continue;
        }
        seen.push(k);
        out.push((*kind, url));
    }
    out
}

/// npm's weekly downloads as a strength class. npm's own `popularity` is now
/// 1 for every package (a 13-download package scored like React), so the
/// download count is the evidence; a crate's count is all-time, npm's weekly.
fn npm_downloads_strength(weekly: u64) -> f64 {
    match weekly {
        5_000_000.. => 1.0,
        500_000.. => 0.9,
        50_000.. => 0.8,
        5_000.. => 0.6,
        500.. => 0.3,
        _ => 0.1,
    }
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
                let docs = format!("https://docs.rs/{got}/latest/{got}/");
                let url = format!("https://crates.io/crates/{got}");
                // docs.rs is already the first row; a crate's own site and its
                // repository follow the record.
                let links = links_from(
                    &[&docs, &url, &format!("https://docs.rs/{got}")],
                    &[
                        ("homepage", c["homepage"].as_str()),
                        ("documentation", c["documentation"].as_str()),
                        ("source", c["repository"].as_str()),
                    ],
                );
                out.push(Found {
                    eco,
                    name: got.to_string(),
                    version: c["max_version"].as_str().unwrap_or("").to_string(),
                    description: c["description"].as_str().unwrap_or("").to_string(),
                    strength: crates_strength(downloads),
                    url,
                    docs: Some(docs),
                    fact: format!("{} downloads", compact(downloads)),
                    links,
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
                // Weekly downloads are the evidence when npm sends them; an
                // answer without them falls back to the score it used to carry.
                let weekly = o["downloads"]["weekly"].as_u64();
                let strength = match weekly {
                    Some(n) => npm_downloads_strength(n),
                    None => npm_strength(
                        o["score"]["detail"]["popularity"]
                            .as_f64()
                            .or_else(|| o["score"]["final"].as_f64())
                            .unwrap_or(0.0),
                    ),
                };
                let url = format!("https://www.npmjs.com/package/{got}");
                let links = links_from(
                    &[&url],
                    &[
                        ("homepage", p["links"]["homepage"].as_str()),
                        ("source", p["links"]["repository"].as_str()),
                    ],
                );
                out.push(Found {
                    eco,
                    name: got.to_string(),
                    version: p["version"].as_str().unwrap_or("").to_string(),
                    description: p["description"].as_str().unwrap_or("").to_string(),
                    strength,
                    url,
                    docs: None,
                    fact: match weekly {
                        Some(n) => format!("{} weekly downloads", compact(n)),
                        None => String::new(),
                    },
                    links,
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
            let info = &v["info"];
            let url = format!("https://pypi.org/project/{got}/");
            // `project_urls` keys are free text; these spellings cover the
            // common ones, and the legacy fields come after them.
            let urls = &info["project_urls"];
            let pick = |keys: &[&str]| {
                urls.as_object().and_then(|m| {
                    m.iter()
                        .find(|(k, _)| keys.iter().any(|w| k.eq_ignore_ascii_case(w)))
                        .and_then(|(_, v)| v.as_str())
                })
            };
            let links = links_from(
                &[&url],
                &[
                    ("documentation", pick(&["Documentation", "Docs"])),
                    ("documentation", info["docs_url"].as_str()),
                    ("homepage", pick(&["Homepage", "Home", "Home Page"])),
                    ("homepage", info["home_page"].as_str()),
                    (
                        "source",
                        pick(&["Source", "Source Code", "Repository", "Code", "GitHub"]),
                    ),
                ],
            );
            out.push(Found {
                eco,
                name: got.to_string(),
                version: info["version"].as_str().unwrap_or("").to_string(),
                description: info["summary"].as_str().unwrap_or("").to_string(),
                strength: PYPI_STRENGTH,
                url,
                docs: None,
                fact: String::new(),
                links,
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
                    links: links_from(
                        &[r["html_url"].as_str().unwrap_or_default()],
                        &[("homepage", r["homepage"].as_str())],
                    ),
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
/// first for a crate (that is what a caller reads), then the registry record,
/// then — when the answer is one package — the project's own pages the record
/// lists (documentation, homepage, source repository), at no extra request.
fn to_hits(chosen: &[Found]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut rank = 0usize;
    let one = chosen.len() == 1;
    for f in chosen {
        let desc: String = f
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(140)
            .collect();
        let line = |parts: &[&str]| -> String {
            let head = parts
                .iter()
                .filter(|p| !p.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" · ");
            if desc.is_empty() {
                head
            } else {
                format!("{head} — {desc}")
            }
        };
        let version = if f.version.is_empty() {
            String::new()
        } else {
            format!("v{}", f.version)
        };
        if let Some(docs) = &f.docs {
            hits.push(Hit {
                title: format!("{} — Rust crate documentation", f.name),
                url: docs.clone(),
                snippet: line(&["docs.rs", &version]),
                rank,
                published: None,
            });
            rank += 1;
        }
        hits.push(Hit {
            title: format!("{} — {}", f.name, f.eco.label()),
            url: f.url.clone(),
            snippet: line(&[f.eco.label(), &f.fact, &version]),
            rank,
            published: None,
        });
        rank += 1;
        if !one {
            continue;
        }
        for (kind, url) in &f.links {
            let what = match *kind {
                "source" => "source repository",
                other => other,
            };
            hits.push(Hit {
                title: format!("{} — {what}", f.name),
                url: url.clone(),
                snippet: line(&[what]),
                rank,
                published: None,
            });
            rank += 1;
        }
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
    /// Outbound requests the resolution spent.
    pub requests: usize,
    pub cache_age: Option<Duration>,
}

fn cache_key(plan: &Plan) -> String {
    format!(
        "{}{}:{}",
        plan.ecosystem.map(Ecosystem::key).unwrap_or("any"),
        if plan.package_named { "" } else { "?" },
        plan.name.to_ascii_lowercase()
    )
}

/// Keep only the records that prove the query is about them.
fn proven(chosen: Vec<Found>, package_named: bool) -> Vec<Found> {
    if package_named {
        return chosen;
    }
    chosen
        .into_iter()
        .filter(|f| f.strength >= NAME_PROOF)
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    #[serde(default)]
    label: Option<String>,
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
    if crate::config::cfg().state.no_disk_state {
        return None;
    }
    Some(crate::paths::cache_dir().join("search-namespaces.json"))
}

fn memory_cache() -> &'static std::sync::Mutex<Disk> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Disk>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn load_disk() -> Disk {
    let Some(path) = path() else {
        return memory_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
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
    let Some(path) = path() else {
        *memory_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = disk.clone();
        return;
    };
    let Ok(json) = serde_json::to_vec(disk) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if crate::config::write_private(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

enum Cached {
    Fresh(Vec<Hit>, Duration, Option<String>),
    Negative,
    Miss,
}

fn cache_get(key: &str) -> Cached {
    let disk = load_disk();
    let Some(entry) = disk.entries.get(key) else {
        return Cached::Miss;
    };
    let Some(age) = now_secs().checked_sub(entry.at) else {
        return Cached::Miss;
    };
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
        Duration::from_secs(age),
        entry.label.clone(),
    )
}

fn cache_put(key: &str, hits: &[Hit]) {
    cache_put_owned(key, hits, None);
}

fn cache_put_owned(key: &str, hits: &[Hit], label: Option<&str>) {
    if crate::config::cfg().state.no_disk_state {
        let mut disk = memory_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        insert_entry(&mut disk, key, hits, label);
        return;
    }
    let mut disk = load_disk();
    insert_entry(&mut disk, key, hits, label);
    save_disk(&disk);
}

fn insert_entry(disk: &mut Disk, key: &str, hits: &[Hit], label: Option<&str>) {
    disk.version = DISK_VERSION;
    disk.entries.insert(
        key.to_string(),
        Entry {
            at: now_secs(),
            label: label.map(str::to_string),
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
        Cached::Fresh(hits, age, cached_label) => {
            let label = cached_label.unwrap_or_else(|| {
                plan.ecosystem
                    .map(Ecosystem::label)
                    .unwrap_or("registry")
                    .to_string()
            });
            return Resolution::Hits(Resolved {
                hits,
                ms: 0,
                label,
                requests: 0,
                cache_age: Some(age),
            });
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
        Settled::Chosen(chosen) if !proven(chosen.clone(), plan.package_named).is_empty() => {
            proven(chosen, plan.package_named)
        }
        Settled::Chosen(_) | Settled::Nothing => {
            cache_put(&key, &[]);
            return Resolution::Nothing;
        }
        Settled::Unknown => return Resolution::Unknown,
    };
    let hits = to_hits(&chosen);
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
    cache_put_owned(&key, &hits, Some(&label));
    Resolution::Hits(Resolved {
        hits,
        ms,
        label,
        requests: plan.ask.len(),
        cache_age: None,
    })
}

// ── Documentation: a path identifier answered by the page that owns it ──────
//
// `itertools::izip` is not a topic: it is one item of one crate, and the answer
// is that item's own page. The site's interface for its items is the module
// index page, which lists every item with its href — so the href is **read**,
// never guessed (`izip` is a macro, `Value` is a struct, `take` is a function,
// and a guessed file name is a 404 returned as an answer).

/// A path identifier: the name that owns it, the module path between them, and
/// the item itself. `std::mem::take` → root `std`, module `["mem"]`, item
/// `take`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub root: String,
    pub module: Vec<String>,
    pub name: String,
}

impl Item {
    /// The rustdoc page that lists the owning module's items.
    fn index_url(&self, base: &str) -> String {
        if self.module == ["*"] {
            return format!("{base}all.html");
        }
        let mut path = String::new();
        for segment in &self.module {
            path.push_str(segment);
            path.push('/');
        }
        format!("{base}{path}index.html")
    }

    /// `std::mem::take`, the way the docs spell it.
    fn path(&self) -> String {
        let mut out = self.root.clone();
        for segment in &self.module {
            out.push_str("::");
            out.push_str(segment);
        }
        out.push_str("::");
        out.push_str(&self.name);
        out
    }

    fn key(&self) -> String {
        format!("docs:{}", self.path())
    }
}

/// The language's own documentation, which no registry owns.
fn std_base(root: &str) -> Option<String> {
    let base = match root {
        "std" => "https://doc.rust-lang.org/std/",
        "core" => "https://doc.rust-lang.org/core/",
        "alloc" => "https://doc.rust-lang.org/alloc/",
        "proc_macro" => "https://doc.rust-lang.org/proc_macro/",
        _ => return None,
    };
    Some(base.to_string())
}

/// The item a query names, when it names one item of one owner.
///
/// A bare name has no owner to ask: `select!` may be a macro of any crate, and
/// a guessed owner is a wrong answer wearing a confident title, so the caller's
/// fan-out answers those instead.
pub fn item(facts: &super::query::QueryFacts) -> Option<Item> {
    let (root, mut rest) = if let Some(parts) = facts.identifier_parts() {
        parts
    } else {
        let words: Vec<_> = facts.compiled().text.split_whitespace().collect();
        if words.len() != 3 || !words.iter().any(|w| w.eq_ignore_ascii_case("rust")) {
            return None;
        }
        let names: Vec<_> = words
            .into_iter()
            .filter(|w| !w.eq_ignore_ascii_case("rust"))
            .collect();
        if names.len() != 2
            || !names[0]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            return None;
        }
        let named_item = names[1];
        let strong = named_item.contains('_')
            || named_item.contains("::")
            || named_item.ends_with('!')
            || named_item.chars().any(char::is_uppercase);
        if !strong {
            return None;
        }
        let mut rest: Vec<String> = names[1]
            .trim_end_matches('!')
            .split("::")
            .map(str::to_string)
            .collect();
        if rest.iter().any(|part| {
            part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }) {
            return None;
        }
        if rest.len() == 1 {
            rest.insert(0, "*".into());
        }
        (names[0].to_string(), rest)
    };
    let name = rest.pop()?;
    Some(Item {
        root,
        module: rest,
        name,
    })
}

/// One documentation answer: at most one hit, the item's own page.
#[derive(Debug, Default)]
pub struct Doc {
    pub hits: Vec<Hit>,
    pub ms: u64,
    pub requests: usize,
    pub cache_age: Option<Duration>,
}

/// Ask the documentation that owns the item.
///
/// ONE request: the module index page, which proves the owner exists, names
/// the item's real href and carries the sentence the docs write about it.
/// Cached for a week — a published item's URL does not move. A failure here is
/// not a "no": the name is unproven, and the fan-out is still there to answer
/// it.
pub async fn docs(item: &Item, fetcher: &Fetcher, budget: Duration) -> Doc {
    let started = std::time::Instant::now();
    let key = item.key();
    if let Cached::Fresh(hits, age, _) = cache_get(&key) {
        return Doc {
            hits,
            ms: 0,
            requests: 0,
            cache_age: Some(age),
        };
    }
    let base = std_base(&item.root)
        .unwrap_or_else(|| format!("https://docs.rs/{root}/latest/{root}/", root = item.root));
    let index = item.index_url(&base);
    let mut doc = Doc {
        hits: Vec::new(),
        ms: 0,
        requests: 1,
        cache_age: None,
    };
    let out = match tokio::time::timeout(
        budget,
        fetcher.fetch_once_via(&index, &[], None, false, None),
    )
    .await
    {
        Ok(Ok(out)) => out,
        _ => {
            doc.ms = started.elapsed().as_millis() as u64;
            return doc;
        }
    };
    if out.status != 200 {
        doc.ms = started.elapsed().as_millis() as u64;
        return doc;
    }
    let page = String::from_utf8_lossy(&out.body);
    let Some(found) = find_item(&page, &item.name) else {
        doc.ms = started.elapsed().as_millis() as u64;
        return doc;
    };
    // Relative hrefs resolve against the page's FINAL url: docs.rs redirects
    // `latest` to a concrete version, and a href joined against the pre-redirect
    // base lands on another item's page or a 404.
    let Ok(base_url) = url::Url::parse(&out.url) else {
        doc.ms = started.elapsed().as_millis() as u64;
        return doc;
    };
    let Ok(page_url) = base_url.join(&found.href) else {
        doc.ms = started.elapsed().as_millis() as u64;
        return doc;
    };
    if page_url.origin() != base_url.origin() {
        return doc;
    }
    let title = format!("{} in {} - Rust", found.label, item.root);
    // The snippet is the sentence the docs themselves write beside the item.
    // The index's `title` attribute (`fn std::mem::take`) is the honest
    // fallback, and neither is invented.
    let snippet = found
        .description
        .clone()
        .filter(|d| !d.trim().is_empty())
        .or_else(|| {
            let attr = found.title_attr.trim();
            (!attr.is_empty()).then(|| attr.to_string())
        })
        .unwrap_or_else(|| format!("{} · {} documentation", found.label, item.root));
    doc.hits.push(Hit {
        title,
        url: page_url.to_string(),
        snippet: super::evidence::cut(&snippet, 220),
        rank: 0,
        published: None,
    });
    cache_put(&key, &doc.hits);
    doc.ms = started.elapsed().as_millis() as u64;
    doc
}

/// The anchor that names `item` on a docs page.
struct DocLink {
    href: String,
    /// The link's own text, `!` trimmed: `izip` for `macro.izip.html`.
    label: String,
    /// The anchor's `title` attribute, `fn std::mem::take`.
    title_attr: String,
    /// The docs' own sentence about the item, when the index page carries one
    /// (a rustdoc item table puts it in the `<dd>` after the link).
    description: Option<String>,
}

/// Find the item's anchor: its visible text first (`izip`, `take`, `Value`),
/// then the file stem of its href (`macro.izip.html` → `izip`), because a
/// rustdoc index may label a link differently from the item's name.
fn find_item(page: &str, item: &str) -> Option<DocLink> {
    let want = item.trim().trim_end_matches('!').to_string();
    if want.is_empty() {
        return None;
    }
    let doc = scraper::Html::parse_document(page);
    let Ok(sel) = scraper::Selector::parse("a[href]") else {
        return None;
    };
    let mut by_href: Option<DocLink> = None;
    for el in doc.select(&sel) {
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        // Only links this module owns: `../other/index.html` is another page's
        // item, and `#…`/`https://…` are not items of this module.
        if href.contains(':') || href.starts_with(['#', '/']) || href.split('/').any(|p| p == "..")
        {
            continue;
        }
        let text = el.text().collect::<String>();
        let label = text.trim().trim_end_matches('!').trim().to_string();
        // The anchor's text is the item's name: rustdoc writes `<a>izip</a>`
        // and an inline `<code>` never splits it (`text()` walks the subtree).
        let owned_file = href_file_stem(href).is_some_and(|stem| stem == want)
            || href
                .strip_suffix("/index.html")
                .and_then(|path| path.rsplit('/').next())
                .is_some_and(|module| module == want);
        if !owned_file {
            continue;
        }
        if label == want {
            return Some(DocLink {
                href: href.to_string(),
                label,
                title_attr: el.value().attr("title").unwrap_or("").to_string(),
                description: description_after(page, href),
            });
        }
        if by_href.is_none() && href_file_stem(href).is_some_and(|stem| stem == want) {
            by_href = Some(DocLink {
                href: href.to_string(),
                label: if label.is_empty() {
                    item.to_string()
                } else {
                    label
                },
                title_attr: el.value().attr("title").unwrap_or("").to_string(),
                description: description_after(page, href),
            });
        }
    }
    by_href
}

/// `fn.take.html` → `take`; `macro.izip.html` → `izip`.
fn href_file_stem(href: &str) -> Option<&str> {
    let file = href.rsplit('/').next()?;
    let stem = file.strip_suffix(".html")?;
    stem.split_once('.').map(|(_, name)| name).or(Some(stem))
}

/// The sentence the docs write about an item on its module's index page: the
/// `<dd>` that follows the link in a rustdoc item table. Nothing is invented —
/// a page that carries no sentence (the standard library lists functions in
/// prose) yields none, and the caller falls back to the link's own title.
fn description_after(page: &str, href: &str) -> Option<String> {
    let needle = format!("href=\"{href}\"");
    let at = page.find(&needle)?;
    let rest = &page[at + needle.len()..];
    // The `<dd>` must belong to this row: one far away is another item's.
    let dd = rest.find("<dd")?;
    if dd > 160 {
        return None;
    }
    let rest = &rest[dd..];
    let body = &rest[rest.find('>')? + 1..];
    let text = fragment_text(&body[..body.find("</dd>")?]);
    (!text.is_empty()).then_some(text)
}

/// The visible text of an HTML fragment, entities decoded and whitespace
/// collapsed.
fn fragment_text(html: &str) -> String {
    let doc = scraper::Html::parse_fragment(html);
    doc.root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    // Requires nextest: configuration and the cache root are process-global.
    #[test]
    fn no_disk_state_keeps_namespace_and_search_learning_in_memory() {
        unsafe { std::env::set_var("DONSETCH_STATE__NO_DISK_STATE", "true") };
        let temp = crate::paths::cache_dir();
        assert!(crate::config::cfg().state.no_disk_state);
        assert!(super::path().is_none());
        super::cache_put("test", &[]);
        assert!(
            matches!(super::cache_get("test"), super::Cached::Negative),
            "no-disk cache still works in memory"
        );
        super::super::families::save(&Default::default());
        let mut ledger = super::super::ledger::Ledger::load();
        ledger.record(
            "bing|serp",
            super::super::ledger::Outcome::Throttled,
            Some(60),
        );
        ledger.save_if_dirty();
        assert_eq!(std::fs::read_dir(temp).unwrap().count(), 0);
    }

    use super::*;

    #[test]
    fn namespace_cache_keeps_owner_and_age_and_rejects_future_entries() {
        let hit = Hit {
            title: "Tokio".into(),
            url: "https://crates.io/crates/tokio".into(),
            snippet: "An asynchronous runtime".into(),
            rank: 0,
            published: None,
        };
        cache_put_owned("any?:tokio", &[hit], Some("crates.io"));
        match cache_get("any?:tokio") {
            Cached::Fresh(hits, age, label) => {
                assert_eq!(hits.len(), 1);
                assert_eq!(label.as_deref(), Some("crates.io"));
                assert!(age < Duration::from_secs(TTL_SECS));
            }
            _ => panic!("a proved owner should be replayable"),
        }
        let mut disk = load_disk();
        disk.entries.get_mut("any?:tokio").unwrap().at = now_secs() + 3600;
        save_disk(&disk);
        assert!(matches!(cache_get("any?:tokio"), Cached::Miss));
    }

    // ── the documentation lane ──────────────────────────

    fn item_of(query: &str) -> Option<Item> {
        item(&crate::search::query::QueryFacts::new(query))
    }

    #[test]
    fn a_named_rust_owner_can_use_its_all_items_index_without_guessing_a_module() {
        let item = item_of("rust tokio spawn_blocking").unwrap();
        assert_eq!(item.root, "tokio");
        assert_eq!(
            item.index_url("https://docs.rs/tokio/latest/tokio/"),
            "https://docs.rs/tokio/latest/tokio/all.html"
        );
        assert!(item.key().contains("::*::"));
        let page = r#"<a href="task/fn.spawn_blocking.html">task::spawn_blocking</a>"#;
        assert_eq!(
            find_item(page, &item.name).unwrap().href,
            "task/fn.spawn_blocking.html"
        );
        assert!(item_of("python tokio spawn_blocking").is_none());
        assert!(item_of("rust tokio a longer topical query").is_none());
    }

    #[test]
    fn documentation_links_are_local_item_files_and_identifier_case_is_exact() {
        for href in [
            "//evil.example/fn.take.html",
            "javascript:take",
            "other/../fn.take.html",
            "other-page.html",
        ] {
            let page = format!("<a href=\"{href}\">take</a>");
            assert!(find_item(&page, "take").is_none(), "{href}");
        }
        assert!(find_item(r#"<a href="struct.Value.html">Value</a>"#, "value").is_none());
        assert_eq!(
            find_item(r#"<a href="value/index.html">value</a>"#, "value")
                .unwrap()
                .href,
            "value/index.html"
        );
    }

    #[test]
    fn a_path_identifier_names_its_owner_and_its_item() {
        assert_eq!(
            item_of("itertools::izip"),
            Some(Item {
                root: "itertools".into(),
                module: vec![],
                name: "izip".into()
            })
        );
        assert_eq!(
            item_of("std::mem::take"),
            Some(Item {
                root: "std".into(),
                module: vec!["mem".into()],
                name: "take".into()
            })
        );
        assert_eq!(
            item_of("#[tokio::main]"),
            Some(Item {
                root: "tokio".into(),
                module: vec![],
                name: "main".into()
            })
        );
        assert_eq!(
            item_of("serde_json::Value"),
            Some(Item {
                root: "serde_json".into(),
                module: vec![],
                name: "Value".into()
            })
        );
    }

    #[test]
    fn a_bare_name_has_no_owner_to_ask() {
        // `select!` may be any crate's macro: a guessed owner is a wrong answer
        // with a confident title, so the fan-out answers it instead.
        assert!(item_of("select!").is_none());
        // A scoped package is one name, not an owner and an item.
        assert!(item_of("@types/node").is_none());
        // A version is not a namespace path.
        assert!(item_of("v1.2.3").is_none());
        // Two words are a topic.
        assert!(item_of("ngOnDestroy lifecycle hook").is_none());
    }

    #[test]
    fn the_item_is_found_by_its_label_and_read_with_its_description() {
        let page = r#"<dl class="item-table">
            <dt><a class="macro" href="macro.iproduct.html" title="macro itertools::iproduct">iproduct</a></dt>
            <dd>Create an iterator over the cartesian product of iterators.</dd>
            <dt><a class="macro" href="macro.izip.html" title="macro itertools::izip">izip</a></dt>
            <dd>Create an iterator running multiple iterators in lockstep.</dd>
        </dl>"#;
        let found = find_item(page, "izip").expect("the item");
        assert_eq!(found.href, "macro.izip.html");
        assert_eq!(found.label, "izip");
        assert_eq!(found.title_attr, "macro itertools::izip");
        assert_eq!(
            found.description.as_deref(),
            Some("Create an iterator running multiple iterators in lockstep.")
        );
    }

    #[test]
    fn the_item_is_found_by_its_file_stem_when_the_label_differs() {
        // rustdoc labels a link `<code>take</code>` in prose; the href is the
        // item's identity, so the stem answers when the text does not.
        let page = r#"<p><a href="fn.take.html" title="fn std::mem::take"><code>take</code></a> is safe.</p>"#;
        let found = find_item(page, "take").expect("the item");
        assert_eq!(found.href, "fn.take.html");
        // No item-table row: no invented description.
        assert!(found.description.is_none());
    }

    #[test]
    fn an_item_that_is_not_on_the_page_is_not_an_answer() {
        let page = r#"<a href="fn.other.html">other</a>"#;
        assert!(find_item(page, "izip").is_none());
        // Another module's item is not this module's item.
        let page = r#"<a href="../other/fn.izip.html">izip</a>"#;
        assert!(find_item(page, "izip").is_none());
    }

    #[test]
    fn the_language_docs_have_a_base_and_a_crate_does_not() {
        assert_eq!(
            std_base("std").as_deref(),
            Some("https://doc.rust-lang.org/std/")
        );
        assert!(std_base("itertools").is_none());
    }

    #[test]
    fn the_index_url_is_the_module_page() {
        let item = Item {
            root: "std".into(),
            module: vec!["mem".into()],
            name: "take".into(),
        };
        assert_eq!(
            item.index_url("https://doc.rust-lang.org/std/"),
            "https://doc.rust-lang.org/std/mem/index.html"
        );
        assert_eq!(item.path(), "std::mem::take");
    }

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
    fn a_package_name_keeps_its_hyphens_and_underscores() {
        // A name is one word: splitting it on `_` or `-` turned a package into a
        // topic and sent the query to the engines instead of the registry.
        let p = plan("cargo serde_json").expect("a name");
        assert_eq!(p.name, "serde_json");
        assert_eq!(p.ecosystem, Some(Ecosystem::Crates));
        let p = plan("npm react-router").expect("a name");
        assert_eq!(p.name, "react-router");
        assert_eq!(p.ecosystem, Some(Ecosystem::Npm));
        let p = plan("pip scikit-learn").expect("a name");
        assert_eq!(p.name, "scikit-learn");
        assert_eq!(p.ecosystem, Some(Ecosystem::Pypi));
        let p = plan("rust tokio").expect("a name");
        assert_eq!(p.name, "tokio");
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

    /// npm now scores every package's popularity as 1; the weekly download
    /// count in the same answer is the evidence. Live shapes, 2026-11-09.
    #[test]
    fn npm_strength_is_its_weekly_downloads_not_its_flat_score() {
        let flat =
            r#""score":{"final":393.6,"detail":{"popularity":1,"quality":1,"maintenance":1}}"#;
        let npm = parse(
            Ecosystem::Npm,
            "tokio",
            &format!(
                r#"{{"objects":[{{"package":{{"name":"tokio","version":"0.1.2"}},{flat},"downloads":{{"monthly":37,"weekly":22}}}}]}}"#
            ),
        );
        assert!(npm[0].strength <= 0.1, "{}", npm[0].strength);
        assert_eq!(npm[0].fact, "22 weekly downloads");
        let crates = parse(
            Ecosystem::Crates,
            "tokio",
            r#"{"crates":[{"name":"tokio","max_version":"1.54.1","downloads":1166056286}]}"#,
        );
        let mut all = crates;
        all.extend(npm);
        let chosen = choose(all, None);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].eco, Ecosystem::Crates);
        let giant = parse(
            Ecosystem::Npm,
            "zod",
            &format!(
                r#"{{"objects":[{{"package":{{"name":"zod","version":"4.6.7"}},{flat},"downloads":{{"weekly":456171850}}}}]}}"#
            ),
        );
        assert_eq!(giant[0].strength, 1.0);
    }

    /// One package's answer carries its own pages from the same response: no
    /// extra request, clone addresses turned into the repository page, and no
    /// page twice.
    #[test]
    fn one_package_answers_with_its_documentation_homepage_and_source() {
        let npm = parse(
            Ecosystem::Npm,
            "zod",
            r#"{"objects":[{"package":{"name":"zod","version":"4.6.7","description":"TypeScript-first schema validation","links":{"homepage":"https://zod.dev","repository":"git+https://github.com/colinhacks/zod.git","bugs":"https://github.com/colinhacks/zod/issues","npm":"https://www.npmjs.com/package/zod"}},"downloads":{"weekly":456171850}}]}"#,
        );
        let urls: Vec<String> = to_hits(&npm).into_iter().map(|h| h.url).collect();
        assert_eq!(
            urls,
            vec![
                "https://www.npmjs.com/package/zod",
                "https://zod.dev",
                "https://github.com/colinhacks/zod"
            ]
        );
        let pypi = parse(
            Ecosystem::Pypi,
            "httpx",
            r#"{"info":{"name":"httpx","version":"0.28.1","summary":"The next generation HTTP client.","home_page":null,"docs_url":null,"project_urls":{"Changelog":"https://github.com/encode/httpx/blob/master/CHANGELOG.md","Documentation":"https://www.python-httpx.org","Homepage":"https://github.com/encode/httpx","Source":"https://github.com/encode/httpx"}}}"#,
        );
        let hits = to_hits(&pypi);
        let urls: Vec<&str> = hits.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://pypi.org/project/httpx/",
                "https://www.python-httpx.org",
                "https://github.com/encode/httpx"
            ]
        );
        assert!(!hits[0].snippet.contains("·  ·"), "{}", hits[0].snippet);
        let crates = parse(
            Ecosystem::Crates,
            "serde_json",
            r#"{"crates":[{"name":"serde_json","max_version":"1.0.154","downloads":416135489,"homepage":null,"documentation":"https://docs.rs/serde_json","repository":"https://github.com/serde-rs/json"}]}"#,
        );
        let urls: Vec<String> = to_hits(&crates).into_iter().map(|h| h.url).collect();
        assert_eq!(
            urls,
            vec![
                "https://docs.rs/serde_json/latest/serde_json/",
                "https://crates.io/crates/serde_json",
                "https://github.com/serde-rs/json"
            ]
        );
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
    fn a_language_word_is_not_a_package_word() {
        // "rust ownership" is the borrow checker, not the 45k-download crate
        // called `ownership` that answered it alone.
        let topic = plan("rust ownership").expect("planned");
        assert!(!topic.package_named);
        assert!(plan("crate ownership").unwrap().package_named);
        assert!(plan("npm react").unwrap().package_named);
        assert!(!plan("tokio").unwrap().package_named);
        assert_ne!(
            cache_key(&topic),
            cache_key(&plan("crate ownership").unwrap())
        );
        let weak = parse(
            Ecosystem::Crates,
            "ownership",
            r#"{"crates":[{"name":"ownership","max_version":"0.4.0","description":"","downloads":45700}]}"#,
        );
        let strong = parse(
            Ecosystem::Crates,
            "tokio",
            r#"{"crates":[{"name":"tokio","max_version":"1.0.0","description":"","downloads":400000000}]}"#,
        );
        assert!(proven(weak.clone(), false).is_empty());
        assert_eq!(
            proven(weak, true).len(),
            1,
            "a package word takes any record"
        );
        assert_eq!(
            proven(strong, false).len(),
            1,
            "a strong record needs no package word"
        );
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
