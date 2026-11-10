//! Item contracts: what a candidate must prove before it can enter the pool.
//!
//! The layer this replaces classified a *source's answer* after the fact with a
//! binary, whitespace-token test. That test could not tell a Diwali page
//! containing "this week" from an answer, called a real Japanese page off-query
//! because Japanese does not space its words, and admitted a page from another
//! crate because the crate's name appeared somewhere in the URL.
//!
//! Here every item is judged against the query that produced it, with graded
//! verdicts: exact, partial, off. A source is then judged by the verdicts its
//! own items earned — never by a score it cannot influence.
//!
//! The rules are lexical and native. There is no model here, and no language
//! list: CJK is matched by character bigrams, which is what a CJK index is.

use std::collections::{HashMap, HashSet};

use super::engines::{Hit, is_serp_url};
use super::query::{QueryFacts, matches};

/// What a URL is, before anything else is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlClass {
    /// A page a fetcher can read.
    Page,
    /// A known redirector: the real URL is not in it, or is in it undecoded.
    Redirect,
    /// A search results page.
    Serp,
    /// Not http(s) at all.
    Invalid,
}

/// Wrappers and redirectors. A source that returns one of these never returns a
/// result through it: either the link is decoded by the source's parser, or the
/// item is dropped (D1: Google News is dropped).
const REDIRECTORS: &[&str] = &[
    // The whole of Google News is a script-built app that points elsewhere:
    // an article, a topic hub or a story cluster, none of it readable by a
    // fetch. Its articles are found through their publishers instead.
    "news.google.com/",
    "google.com/url?",
    "bing.com/news/apiclick",
    "bing.com/ck/a",
    "duckduckgo.com/l/?uddg=",
    "lite.duckduckgo.com/l/",
    "search.yahoo.com/",
    "rd.search.yahoo.com/",
    "r.search.yahoo.com/",
    "startpage.com/do/",
    "webcache.googleusercontent.com/",
];

/// Is this query parameter tracking junk rather than part of the page?
fn is_tracking_param(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.starts_with("utm_")
        || matches!(
            key.as_str(),
            "fbclid"
                | "gclid"
                | "dclid"
                | "msclkid"
                | "msockid"
                | "mc_cid"
                | "mc_eid"
                | "igshid"
                | "ref_src"
                | "_ga"
                | "_gl"
                | "yclid"
        )
}

pub(crate) fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A redirect wrapper's payload, decoded locally: Google's `/url?q=`, DDG's
/// `uddg=`, Bing's `apiclick`, Yahoo's `/RU=`. `None` when the URL is not a
/// wrapper, or when the wrapper does not carry its target.
pub fn decode_redirect(raw: &str) -> Option<String> {
    let Ok(u) = url::Url::parse(raw) else {
        return None;
    };
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let mut target: Option<String> = None;
    if host.ends_with("google.com") && u.path() == "/url" {
        target = u
            .query_pairs()
            .find(|(k, _)| k == "q" || k == "url")
            .map(|(_, v)| v.into_owned());
    } else if host.ends_with("duckduckgo.com") && u.path().starts_with("/l/") {
        target = u
            .query_pairs()
            .find(|(k, _)| k == "uddg")
            .map(|(_, v)| v.into_owned());
    } else if host.ends_with("bing.com")
        && (u.path().contains("/news/apiclick") || u.path() == "/ck/a")
    {
        target = u
            .query_pairs()
            .find(|(k, _)| k == "url" || k == "u")
            .map(|(_, v)| v.into_owned());
    } else if host.contains("search.yahoo.com") {
        let decoded = percent_decode(u.path());
        let at = decoded.find("/RU=").map(|i| i + 4)?;
        let tail = &decoded[at..];
        let end = tail.find('/').unwrap_or(tail.len());
        target = Some(percent_decode(&tail[..end]));
    }
    let target = target?;
    let target = percent_decode(&target);
    let target = target.trim();
    let parsed = url::Url::parse(target).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    // A wrapper inside a wrapper is not an answer.
    let lower = target.to_ascii_lowercase();
    if lower.contains("news.google.com") || REDIRECTORS.iter().any(|p| lower.contains(p)) {
        return None;
    }
    Some(target.to_string())
}

/// The publisher's own URL: no fragment, no tracking parameters, host as it
/// was registered. The one URL the caller is handed, so the fetch tool opens
/// the page the index meant, not a beacon.
pub fn canonical_url(raw: &str) -> String {
    let Ok(mut u) = url::Url::parse(raw) else {
        return raw.to_string();
    };
    u.set_fragment(None);
    if u.query().is_some() {
        let kept: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| !is_tracking_param(k))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if kept.is_empty() {
            u.set_query(None);
        } else {
            u.query_pairs_mut().clear().extend_pairs(kept);
        }
    }
    if let Some(host) = u.host_str() {
        let lower = host.to_ascii_lowercase();
        if lower != host {
            let _ = u.set_host(Some(&lower));
        }
    }
    u.to_string()
}

/// Decode a wrapper, then strip what the index added on the way.
pub(crate) fn publisher_url(raw: &str) -> String {
    canonical_url(&decode_redirect(raw).unwrap_or_else(|| raw.to_string()))
}

pub fn url_class(raw: &str) -> UrlClass {
    // A replacement character or a control byte is the mark of a bad decode:
    // the URL as printed is not the URL anyone can fetch. It hides percent-
    // encoded too (`%EF%BF%BD` is one literal byte sequence of it).
    if raw
        .chars()
        .any(|c| c == '\u{FFFD}' || c.is_control() || c.is_whitespace())
    {
        return UrlClass::Invalid;
    }
    let without_escapes = raw.replace("%EF%BF%BD", "").replace("%ef%bf%bd", "");
    if without_escapes.len() != raw.len() {
        return UrlClass::Invalid;
    }
    let Ok(u) = url::Url::parse(raw) else {
        return UrlClass::Invalid;
    };
    let decoded = percent_decode(u.path());
    if decoded.contains('\u{FFFD}') {
        return UrlClass::Invalid;
    }
    if !matches!(u.scheme(), "http" | "https") {
        return UrlClass::Invalid;
    }
    let lower = raw.to_lowercase();
    if REDIRECTORS.iter().any(|p| lower.contains(p)) {
        return UrlClass::Redirect;
    }
    if is_serp_url(raw) {
        return UrlClass::Serp;
    }
    UrlClass::Page
}

/// The namespace a scoped URL belongs to, and the name at its root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeShape {
    Docs,
    Crates,
    Npm,
    Pypi,
    Github,
}

fn normalize_package(name: &str) -> String {
    name.replace('_', "-").to_lowercase()
}

pub fn scoped_root(raw: &str) -> Option<(ScopeShape, String)> {
    let u = url::Url::parse(raw).ok()?;
    let host = u.host_str()?.trim_start_matches("www.").to_lowercase();
    let segs: Vec<&str> = u.path().split('/').filter(|s| !s.is_empty()).collect();
    match host.as_str() {
        "docs.rs" => segs
            .first()
            .map(|s| (ScopeShape::Docs, normalize_package(s))),
        "crates.io" if segs.first() == Some(&"crates") => segs
            .get(1)
            .map(|s| (ScopeShape::Crates, normalize_package(s))),
        "npmjs.com" if segs.first() == Some(&"package") => {
            let name = match segs.get(1) {
                Some(scope) if scope.starts_with('@') => match segs.get(2) {
                    Some(pkg) => format!("{scope}/{pkg}"),
                    None => (*scope).to_string(),
                },
                Some(pkg) => (*pkg).to_string(),
                None => String::new(),
            };
            (!name.is_empty()).then(|| (ScopeShape::Npm, normalize_package(&name)))
        }
        "pypi.org" if segs.first() == Some(&"project") => segs
            .get(1)
            .map(|s| (ScopeShape::Pypi, normalize_package(s))),
        // A repository is namespaced by its owner: `site:github.com/tokio-rs`
        // names the owner, and every repo under it is on the same side. The
        // repo name alone would veto its siblings as look-alikes.
        "github.com" if segs.len() >= 2 => Some((ScopeShape::Github, normalize_package(segs[0]))),
        _ => None,
    }
}

/// A scoped record from a package registry (not a GitHub repository) whose root
/// the query names.
pub fn package_scope_named(facts: &QueryFacts, raw: &str) -> bool {
    matches!(
        scoped_root(raw),
        Some((shape, root)) if shape != ScopeShape::Github && facts.names(&root)
    )
}

/// Why a candidate is refused, before it can influence anything.
///
/// This is the veto list and nothing else. There is no verdict, no grade and no
/// coverage floor: a candidate that is not vetoed enters the pool with its score,
/// and the *set* is judged once, in [`super::quality`]. The list is short because
/// every entry is a fact about the item — it is a wrapper, it is a SERP, it
/// contains a term the query excluded — never a guess about the query's topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Veto {
    /// A redirector or a SERP: a link to the answer, or a page of links.
    Shape,
    /// Not http(s) at all, or a decode that produced a replacement character.
    Invalid,
    /// Contains a `-term` the query excluded.
    Negative,
    /// A general encyclopedia or a video, for a query that names one identifier.
    /// The name's own record or documentation answers those; a page about the
    /// word does not (`tokio::spawn` → ja.wikipedia's article on the band TOKIO).
    Reference,
}

/// Hosts that answer a *topic* and never a *name*. A dictionary definition of
/// "select" is not the `select!` macro, and an encyclopedia entry about the band
/// TOKIO is not `tokio::spawn` — both were returned for one-identifier queries on
/// this corpus.
const REFERENCE_HOSTS: &[&str] = &[
    "wikipedia.org",
    "britannica.com",
    "youtube.com",
    "youtu.be",
    // Dictionaries: a definition is a page about the word, not the thing.
    "merriam-webster.com",
    "dictionary.com",
    "thesaurus.com",
    "collinsdictionary.com",
    "oxfordlearnersdictionaries.com",
    "dictionary.cambridge.org",
    "vocabulary.com",
    "wiktionary.org",
];

fn is_reference(raw: &str) -> bool {
    url::Url::parse(raw)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
        })
        .is_some_and(|h| {
            REFERENCE_HOSTS
                .iter()
                .any(|r| h == *r || h.ends_with(&format!(".{r}")))
        })
}

/// The reason a candidate may not enter the pool, if any.
pub fn veto(facts: &QueryFacts, hit: &Hit) -> Option<Veto> {
    match url_class(&hit.url) {
        UrlClass::Page => {}
        UrlClass::Invalid => return Some(Veto::Invalid),
        UrlClass::Redirect | UrlClass::Serp => return Some(Veto::Shape),
    }
    let text = format!("{} {} {}", hit.title, hit.snippet, hit.url);
    if facts
        .negated()
        .iter()
        .any(|n| matches(n, &text) || text.to_lowercase().contains(n))
    {
        return Some(Veto::Negative);
    }
    None
}

/// The veto list as it applies to a **search engine's** answer.
///
/// A SERP's own furniture is not an answer: the encyclopedia entry for the word
/// a one-identifier query named, or the site's bare front door. A first-party
/// API is judged by [`veto`] alone — its own ranking is its contract, and the
/// registry's root page is that registry's answer, not furniture.
pub fn veto_lane(facts: &QueryFacts, hit: &Hit) -> Option<Veto> {
    if let Some(v) = veto(facts, hit) {
        return Some(v);
    }
    if facts.is_identifier() && is_reference(&hit.url) {
        return Some(Veto::Reference);
    }
    if is_bare_homepage(&hit.url) && !names_host(facts, &hit.url) {
        return Some(Veto::Shape);
    }
    None
}

/// A site's front door, not an answer: no path, or `/` alone.
///
/// Measured in the 5.3.0 A block-paired run: `go context package cancel`
/// returned `https://www.google.com/` at rank 1.
fn is_bare_homepage(raw: &str) -> bool {
    url::Url::parse(raw)
        .ok()
        .is_some_and(|u| u.path().trim_matches('/').is_empty() && u.query().is_none())
}

/// A homepage is the answer when the query names the site: `tokio` may answer
/// with `tokio.rs/`, and `go.dev` with its own front page.
fn names_host(facts: &QueryFacts, raw: &str) -> bool {
    let Ok(u) = url::Url::parse(raw) else {
        return false;
    };
    let Some(host) = u
        .host_str()
        .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
    else {
        return false;
    };
    let labels: Vec<&str> = host.split('.').collect();
    facts.terms().any(|t| {
        let t = t.replace('_', "-");
        t == host || labels.iter().any(|l| *l == t)
    })
}

/// A candidate's graded relevance to the query, 0..=1.
///
/// The query's terms weighted by where they were found — title ×3, snippet ×1,
/// URL ×1.5, host ×1 — scaled by how much of the query's named entity the text
/// carries. This is the coverage gradient, not BM25: BM25+ over the merged
/// candidate set (with its idf and field lengths) needs the set this function
/// cannot see, and it is B2's work. What is needed here is one number per hit
/// that orders the pool and lets a set be judged as a whole, without a verdict
/// being taken on any single item.
pub fn relevance(facts: &QueryFacts, title: &str, snippet: &str, url: &str) -> f64 {
    if url_class(url) != UrlClass::Page {
        return 0.0;
    }
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_string())
        })
        .unwrap_or_default();
    let weighted = (3.0 * facts.coverage(title)
        + facts.coverage(snippet)
        + 1.5 * facts.coverage(url)
        + facts.coverage(&host))
        / 6.5;
    let text = format!("{title} {snippet} {url}");
    let (any, all) = facts.anchor_state(&text);
    let anchor = if facts.anchors().is_empty() || all {
        1.0
    } else if any {
        0.7
    } else {
        0.35
    };
    (weighted * anchor).clamp(0.0, 1.0)
}

/// First-party items keep their own ranking (the API's relevance is the
/// contract), but they still lose what the veto list refuses, and look-alike
/// namespaces whose root the query names elsewhere.
pub fn accept_first_party(facts: &QueryFacts, hits: Vec<Hit>) -> Vec<Hit> {
    if hits.is_empty() {
        return hits;
    }
    let hits: Vec<Hit> = hits
        .into_iter()
        .map(|mut h| {
            h.url = publisher_url(&h.url);
            h
        })
        .collect();
    let mut named: HashMap<ScopeShape, HashSet<String>> = HashMap::new();
    for hit in hits.iter().filter(|h| veto(facts, h).is_none()) {
        if let Some((shape, root)) = scoped_root(&hit.url)
            && facts.names(&root)
        {
            named.entry(shape).or_default().insert(root);
        }
    }
    hits.into_iter()
        .filter(|hit| {
            if veto(facts, hit).is_some() {
                return false;
            }
            if let Some((shape, root)) = scoped_root(&hit.url)
                && named.contains_key(&shape)
                && !facts.names(&root)
            {
                return false;
            }
            github_engages(facts, hit)
        })
        .collect()
}

/// GitHub's search matches a word anywhere in a README or an issue body, so
/// its own ranking is no contract: "python 3.14 release date" returned a
/// voice-assistant script and "latest rust release" a PROJ binding. A
/// repository answers only when its own owner or name carries a query word
/// (a prefix counts: `dataclass` names `dataclasses`); an issue or anything
/// deeper is judged like any other page. Other hosts pass untouched.
fn github_engages(facts: &QueryFacts, hit: &Hit) -> bool {
    let Ok(u) = url::Url::parse(&hit.url) else {
        return true;
    };
    if u.host_str().map(|h| h.trim_start_matches("www.")) != Some("github.com") {
        return true;
    }
    let segs: Vec<&str> = u.path().split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() == 2 {
        let terms: Vec<&str> = facts.terms().collect();
        if terms.is_empty() {
            return true;
        }
        return segs
            .iter()
            .flat_map(|s| s.split(|c: char| !c.is_alphanumeric()))
            .map(str::to_lowercase)
            .filter(|w| !w.is_empty())
            .any(|w| {
                terms.iter().any(|t| {
                    w == *t
                        || (t.chars().count() >= 4 && w.starts_with(t))
                        || (w.chars().count() >= 4 && t.starts_with(w.as_str()))
                })
            });
    }
    facts.is_exact(&hit.title, &hit.snippet, &hit.url)
}

// ── Date ────────────────────────────────────────────────────────────────
//
// The date contract: a dated query means a window, and an item is part of the
// answer only when its own date falls inside it. Days are UTC day numbers, so
// the comparison is one subtraction — no timezone rules, no calendar crate.

/// A query's date window: `start` inclusive, `end` exclusive, UTC days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: i64,
    pub end: i64,
}

impl Window {
    pub fn contains(&self, day: i64) -> bool {
        day >= self.start && day < self.end
    }
}

/// Days since the Unix epoch, UTC.
pub fn today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0)
}

/// Howard Hinnant's civil-days conversion: 1970-01-01 is day 0.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The UTC day a `YYYY-MM-DD…` stamp falls on.
pub fn day_of(stamp: &str) -> Option<i64> {
    let bytes = stamp.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let y: i64 = stamp.get(0..4)?.parse().ok()?;
    let m: i64 = stamp.get(5..7)?.parse().ok()?;
    let d: i64 = stamp.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// A rolling window of the last `days` days, including today.
pub fn window_days(days: i64) -> Window {
    let now = today();
    Window {
        start: now - days + 1,
        end: now + 1,
    }
}

/// A bench row's window: `"7d"`, `"365d"`, `"12h"`, `"4w"`.
pub fn window_from_str(span: &str) -> Option<Window> {
    let span = span.trim();
    let (num, unit) = span.split_at(span.len().checked_sub(1)?);
    let n: i64 = num.trim().parse().ok()?;
    if n <= 0 || n > 3650 {
        return None;
    }
    Some(window_days(match unit {
        "d" => n,
        "w" => n * 7,
        "m" => n * 31,
        "y" => n * 365,
        "h" => (n + 23) / 24,
        _ => return None,
    }))
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

fn window_words(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// A window measured back from today ("this week", "yesterday", "past 3
/// days"): the query asks what happened lately, which is the news program's
/// question. A named year or month is a window too, but not a recent one.
pub fn relative_window(query: &str) -> Option<Window> {
    let owned = window_words(query);
    let words: Vec<&str> = owned.iter().map(String::as_str).collect();
    let text = format!(" {} ", words.join(" "));
    for (i, w) in words.iter().enumerate() {
        if !matches!(*w, "past" | "last" | "next") {
            continue;
        }
        let Some(n) = words.get(i + 1).and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        let Some(unit) = words.get(i + 2) else {
            continue;
        };
        let days = match *unit {
            "hour" | "hours" => (n + 23) / 24,
            "day" | "days" => n,
            "week" | "weeks" => n * 7,
            "month" | "months" => n * 31,
            "year" | "years" => n * 365,
            _ => continue,
        };
        if (1..=3650).contains(&days) {
            return Some(window_days(days));
        }
    }
    let has = |phrases: &[&str]| phrases.iter().any(|p| text.contains(&format!(" {p} ")));
    // Widest first: a query that states two windows ("breaking news this
    // week") keeps the wider one, which can never drop the page it asked for.
    if has(&["this year", "past year"]) {
        return Some(window_days(365));
    }
    if has(&["this month", "past month", "last month"]) {
        return Some(window_days(31));
    }
    if has(&["this week", "past week", "last week"]) {
        return Some(window_days(7));
    }
    if has(&["this weekend", "last weekend"]) {
        return Some(window_days(4));
    }
    if has(&["yesterday", "last night"]) {
        return Some(window_days(2));
    }
    if has(&[
        "today",
        "breaking",
        "this morning",
        "tonight",
        "this afternoon",
        "this evening",
    ]) {
        return Some(window_days(1));
    }
    None
}

/// The window a query states, if it states one. `latest` alone is not a window
/// — it is a ranking hint, and a week would silently drop the right page.
pub fn window_for(query: &str) -> Option<Window> {
    if let Some(w) = relative_window(query) {
        return Some(w);
    }
    let owned = window_words(query);
    let words: Vec<&str> = owned.iter().map(String::as_str).collect();
    // A named month inside a year: "november 2026" is that month, not the year.
    if let Some(mi) = MONTHS.iter().position(|m| words.contains(m)) {
        let year = words
            .iter()
            .find_map(|w| w.parse::<i64>().ok().filter(|y| (2020..=2100).contains(y)));
        if let Some(y) = year {
            let month = mi as i64 + 1;
            let (end_year, end_month) = if month == 12 {
                (y + 1, 1)
            } else {
                (y, month + 1)
            };
            return Some(Window {
                start: days_from_civil(y, month, 1),
                end: days_from_civil(end_year, end_month, 1),
            });
        }
    }
    let year = words
        .iter()
        .find_map(|w| w.parse::<i64>().ok().filter(|y| (2020..=2100).contains(y)))?;
    Some(Window {
        start: days_from_civil(year, 1, 1),
        end: days_from_civil(year + 1, 1, 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(title: &str, url: &str) -> Hit {
        Hit {
            title: title.into(),
            url: url.into(),
            snippet: String::new(),
            rank: 1,
            published: None,
        }
    }

    #[test]
    fn a_trending_page_is_not_the_answer_but_the_covering_page_is() {
        let q = QueryFacts::new("SpaceX Starship latest launch news this week");
        let diwali = (
            "Diwali 2026: celebrations this week - AP News",
            "https://apnews.com/x",
        );
        assert!(!q.is_exact(diwali.0, "", diwali.1));
        assert_eq!(
            relevance(&q, diwali.0, "", diwali.1),
            0.0,
            "no query term at all"
        );
        let teslarati = (
            "SpaceX will perform Starship double catch attempt with Flight 15",
            "https://www.teslarati.com/x",
        );
        assert!(q.is_exact(teslarati.0, "", teslarati.1));
        assert!(relevance(&q, teslarati.0, "", teslarati.1) > 0.25);
    }

    #[test]
    fn a_temporal_word_is_not_evidence() {
        let q = QueryFacts::new("starship latest news this week");
        assert_eq!(q.coverage("Diwali celebrations this week"), 0.0);
    }

    #[test]
    fn a_scoped_record_is_recognised_by_the_root_the_query_names() {
        // The docs page's title is just the crate name; the URL root is what
        // proves it answers the query, so it is never vetoed and never a cousin.
        let q = QueryFacts::new("rust clap command line argument parser crate");
        assert!(q.names("clap"));
        assert!(!q.names("taos"));
        assert!(veto(&q, &hit("clap 4.5", "https://docs.rs/clap/latest/clap/")).is_none());
    }

    #[test]
    fn an_unspaced_japanese_query_covers_its_answers() {
        let q = QueryFacts::new("地震速報");
        assert!(
            q.coverage("地震速報 - NHK のニュース") > 0.9,
            "{}",
            q.coverage("地震速報 - NHK のニュース")
        );
        // The spacing a caller types does not change the question.
        let spaced = QueryFacts::new("東京 地震 速報");
        assert!(spaced.coverage("地震速報 - NHK") >= 0.6);
    }

    #[test]
    fn a_bare_homepage_is_not_an_answer_unless_the_query_names_the_site() {
        // Measured: `go context package cancel` returned google.com's front page
        // at rank 1 in the 5.3.0 A block-paired run.
        let q = QueryFacts::new("go context package cancel");
        assert_eq!(
            veto_lane(&q, &hit("Google", "https://www.google.com/")),
            Some(Veto::Shape)
        );
        // The site the query names may answer with its own front page.
        let tokio = QueryFacts::new("tokio");
        assert!(veto_lane(&tokio, &hit("Tokio", "https://tokio.rs/")).is_none());
        // A deeper page is never a homepage.
        assert!(veto_lane(&q, &hit("context", "https://pkg.go.dev/context")).is_none());
    }

    #[test]
    fn a_redirect_a_serp_an_excluded_term_or_a_bad_url_is_vetoed() {
        let q = QueryFacts::new("rust async -python");
        assert_eq!(
            veto(
                &q,
                &hit(
                    "Starship launch",
                    "https://news.google.com/rss/articles/CBMi"
                )
            ),
            Some(Veto::Shape)
        );
        assert_eq!(
            veto(&q, &hit("x", "https://www.bing.com/search?q=x")),
            Some(Veto::Shape)
        );
        assert_eq!(
            veto(&q, &hit("Async Python", "https://example.com/python")),
            Some(Veto::Negative)
        );
        assert_eq!(
            veto(&q, &hit("alert", "javascript:alert(1)")),
            Some(Veto::Invalid)
        );
        assert!(veto(&q, &hit("Rust async", "https://example.com/rust")).is_none());
    }

    #[test]
    fn a_page_that_carries_the_query_outranks_one_that_does_not() {
        let q = QueryFacts::new("tokio spawn_blocking");
        let on = relevance(
            &q,
            "tokio::task::spawn_blocking",
            "Runs a blocking closure",
            "https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html",
        );
        let off = relevance(
            &q,
            "Python decorators",
            "",
            "https://example.com/decorators",
        );
        assert!(on > 0.6, "{on}");
        assert!(off < 0.1, "{off}");
        // The title outweighs a snippet mention of the same term.
        let title = relevance(&q, "tokio", "", "https://example.com/x");
        let snippet = relevance(&q, "unrelated", "tokio", "https://example.com/x");
        assert!(title > snippet, "title {title} vs snippet {snippet}");
    }

    #[test]
    fn one_identifier_is_never_answered_by_an_encyclopedia_or_a_video() {
        // Measured: 5.2.1 answered `tokio::spawn` and `#[tokio::main]` with
        // ja.wikipedia's article on the band TOKIO, twice, in one run.
        let q = QueryFacts::new("tokio::spawn");
        assert!(q.is_identifier());
        assert_eq!(
            veto_lane(
                &q,
                &hit("TOKIO - Wikipedia", "https://ja.wikipedia.org/wiki/TOKIO")
            ),
            Some(Veto::Reference)
        );
        assert_eq!(
            veto_lane(&q, &hit("Tokio spawn", "https://www.youtube.com/watch?v=x")),
            Some(Veto::Reference)
        );
        assert!(
            veto_lane(
                &q,
                &hit("tokio::spawn", "https://tokio.rs/tokio/tutorial/spawn")
            )
            .is_none()
        );
        // The shape is one identifier, not any query containing a code word.
        assert!(!QueryFacts::new("tokio spawn tutorial").is_identifier());
        assert!(!QueryFacts::new("what is tokio").is_identifier());
        // And a topic query keeps its encyclopedia page.
        let topic = QueryFacts::new("quantum entanglement");
        assert!(
            veto(
                &topic,
                &hit(
                    "Quantum entanglement",
                    "https://en.wikipedia.org/wiki/Quantum_entanglement"
                )
            )
            .is_none()
        );
    }

    #[test]
    fn the_identifiers_that_count_as_one_are_the_code_shapes() {
        for q in [
            "select!",
            "std::mem::take",
            "serde_json::Value",
            "@types/node",
            "v8::Isolate",
            "itertools::izip",
            "#[tokio::main]",
            "__dirname",
            "ngOnDestroy",
            "v1.2.3",
        ] {
            assert!(
                QueryFacts::new(q).is_identifier(),
                "{q} should be one identifier"
            );
        }
        for q in [
            "rust programming",
            "what is a monad",
            "git rev-parse --short HEAD",
            "tokio",
            "C++",
        ] {
            assert!(
                !QueryFacts::new(q).is_identifier(),
                "{q} is a question, not an identifier"
            );
        }
    }

    #[test]
    fn a_search_results_page_is_never_a_result() {
        assert_eq!(url_class("https://www.bing.com/search?q=x"), UrlClass::Serp);
        assert_eq!(url_class("https://example.com/page"), UrlClass::Page);
        assert_eq!(url_class("javascript:alert(1)"), UrlClass::Invalid);
    }

    #[test]
    fn a_first_party_answer_loses_redirects_and_look_alikes() {
        let hits = vec![
            hit("crate tokio", "https://crates.io/crates/tokio"),
            hit("crate taos", "https://crates.io/crates/taos"),
            hit("tokio news", "https://news.google.com/rss/articles/x"),
        ];
        let kept = accept_first_party(&QueryFacts::new("tokio"), hits);
        let urls: Vec<&str> = kept.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(urls, vec!["https://crates.io/crates/tokio"]);
    }

    /// The two repositories GitHub's loose search returned on live queries are
    /// out; repositories whose own name carries the query's words stay.
    #[test]
    fn a_github_repository_answers_only_through_its_own_name() {
        let kept = |q: &str, hits: Vec<Hit>| -> Vec<String> {
            accept_first_party(&QueryFacts::new(q), hits)
                .into_iter()
                .map(|h| h.url)
                .collect()
        };
        let urls = kept(
            "python 3.14 release date",
            vec![hit(
                "Rynkll696/HH import datetime from datetime import date",
                "https://github.com/Rynkll696/HH",
            )],
        );
        assert!(urls.is_empty(), "{urls:?}");
        let urls = kept(
            "latest rust release",
            vec![
                hit(
                    "georust/proj bindings for the latest release",
                    "https://github.com/georust/proj",
                ),
                hit("rust-lang/rust", "https://github.com/rust-lang/rust"),
            ],
        );
        assert_eq!(urls, vec!["https://github.com/rust-lang/rust"]);
        let urls = kept(
            "docker compose healthcheck example",
            vec![hit(
                "peter-evans/docker-compose-healthcheck",
                "https://github.com/peter-evans/docker-compose-healthcheck",
            )],
        );
        assert_eq!(urls.len(), 1);
        // A prefix names a plural.
        let urls = kept(
            "dataclass frozen hash",
            vec![hit(
                "ericvsmith/dataclasses",
                "https://github.com/ericvsmith/dataclasses",
            )],
        );
        assert_eq!(urls, vec!["https://github.com/ericvsmith/dataclasses"]);
        // An issue is judged on its own text.
        let urls = kept(
            "frozen dataclass hash error",
            vec![
                hit(
                    "acme/models: frozen dataclass hash error",
                    "https://github.com/acme/models/issues/1",
                ),
                hit(
                    "acme/widgets: unrelated crash",
                    "https://github.com/acme/widgets/issues/2",
                ),
            ],
        );
        assert_eq!(urls, vec!["https://github.com/acme/models/issues/1"]);
    }

    #[test]
    fn a_scope_root_is_the_name_at_the_root_not_a_path_segment() {
        assert_eq!(
            scoped_root("https://docs.rs/taos/latest/taos/tokio/macro.select.html"),
            Some((ScopeShape::Docs, "taos".to_string()))
        );
        assert_eq!(
            scoped_root("https://www.npmjs.com/package/@scope/pkg"),
            Some((ScopeShape::Npm, "@scope/pkg".to_string()))
        );
        assert_eq!(
            scoped_root("https://pypi.org/project/foo_bar/"),
            Some((ScopeShape::Pypi, "foo-bar".to_string()))
        );
        assert_eq!(scoped_root("https://example.com/x"), None);
    }

    #[test]
    fn a_returned_url_is_the_publishers_own() {
        assert_eq!(
            canonical_url("https://Example.com/Post?utm_source=x&id=7&fbclid=z#top"),
            "https://example.com/Post?id=7"
        );
        assert_eq!(
            canonical_url("https://example.com/"),
            "https://example.com/"
        );
        assert_eq!(canonical_url("not a url"), "not a url");
        // A meaningful parameter survives: the page is the same page, the
        // beacon is not part of it.
        assert_eq!(
            canonical_url("https://docs.rs/tokio/1.40.0?search=tokio&utm_medium=email"),
            "https://docs.rs/tokio/1.40.0?search=tokio"
        );
    }

    #[test]
    fn a_wrapper_is_decoded_to_its_target_and_nothing_else() {
        assert_eq!(
            decode_redirect("https://www.google.com/url?q=https%3A%2F%2Ftokio.rs%2F&sa=D")
                .as_deref(),
            Some("https://tokio.rs/")
        );
        assert_eq!(
            decode_redirect("https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa")
                .as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(
            decode_redirect("https://www.bing.com/news/apiclick?url=https%3A%2F%2Fexample.com%2Fb")
                .as_deref(),
            Some("https://example.com/b")
        );
        // A plain page is not a wrapper.
        assert_eq!(decode_redirect("https://example.com/page"), None);
        // A wrapper inside a wrapper is not an answer.
        assert_eq!(
            decode_redirect("https://www.google.com/url?q=https%3A%2F%2Fnews.google.com%2Frss"),
            None
        );
    }

    #[test]
    fn days_are_counted_from_the_epoch() {
        assert_eq!(day_of("1970-01-01"), Some(0));
        assert_eq!(day_of("2000-01-01"), Some(10_957));
        assert_eq!(day_of("2026-11-09"), Some(20_766));
        assert_eq!(day_of("2026-11-08"), Some(20_765));
        assert_eq!(day_of("not a date"), None);
        assert_eq!(day_of("2026-13-01"), None);
    }

    #[test]
    fn a_query_that_states_a_window_gets_one() {
        let week = window_days(7);
        assert_eq!(
            window_for("spacex starship latest news this week"),
            Some(week)
        );
        assert_eq!(
            window_for("what happened last 3 days"),
            Some(window_days(3))
        );
        assert_eq!(window_for("tokio"), None, "a bare name has no window");
        assert_eq!(
            window_for("latest release"),
            None,
            "latest alone is a ranking hint, not a window"
        );
        let november = window_for("federal reserve november 2026").expect("a month window");
        assert_eq!(november.start, day_of("2026-11-01").unwrap());
        assert_eq!(november.end, day_of("2026-12-01").unwrap());
        let year = window_for("nobel prize 2026 winner").expect("a year window");
        assert_eq!(year.start, day_of("2026-01-01").unwrap());
        assert_eq!(year.end, day_of("2027-01-01").unwrap());
    }

    #[test]
    fn a_relative_window_is_read_from_the_words_people_use() {
        assert_eq!(
            relative_window("fed rate decision yesterday"),
            Some(window_days(2))
        );
        assert_eq!(
            relative_window("what happened last night in gaza"),
            Some(window_days(2))
        );
        assert_eq!(
            relative_window("f1 race this weekend"),
            Some(window_days(4))
        );
        assert_eq!(relative_window("Apple event today"), Some(window_days(1)));
        assert_eq!(relative_window("today's wordle"), Some(window_days(1)));
        // Two windows: the wider one, which cannot drop the page asked for.
        assert_eq!(
            relative_window("breaking news this week"),
            Some(window_days(7))
        );
        // Whole words only, and a named year is a window but not a recent one.
        assert_eq!(relative_window("todayville bakery"), None);
        assert_eq!(relative_window("lewis 2020 rag paper"), None);
        assert!(window_for("lewis 2020 rag paper").is_some());
    }

    #[test]
    fn a_bench_window_parses_and_stays_inside_the_calendar() {
        assert_eq!(window_from_str("7d"), Some(window_days(7)));
        assert_eq!(window_from_str("2w"), Some(window_days(14)));
        assert_eq!(window_from_str("12h"), Some(window_days(1)));
        assert_eq!(window_from_str("0d"), None);
        assert_eq!(window_from_str("7x"), None);
    }
}
