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

use super::engines::{AnswerClass, Hit, is_serp_url};

/// Words that carry no topic: function words and the temporal vocabulary of a
/// news query. A title containing "this week" is not about the query because
/// the query also said "week".
const STOP: &[&str] = &[
    "the",
    "and",
    "for",
    "how",
    "what",
    "who",
    "why",
    "when",
    "where",
    "which",
    "does",
    "did",
    "can",
    "could",
    "should",
    "would",
    "with",
    "without",
    "from",
    "into",
    "about",
    "best",
    "vs",
    "versus",
    "you",
    "your",
    "are",
    "was",
    "were",
    "is",
    "it",
    "its",
    "this",
    "that",
    "these",
    "those",
    "there",
    "of",
    "to",
    "in",
    "on",
    "or",
    "a",
    "an",
    "latest",
    "today",
    "tonight",
    "yesterday",
    "week",
    "breaking",
    "recent",
    "update",
    "updates",
    "news",
    "live",
];

/// Capitalised words that are sentence furniture, never a named entity.
const ANCHOR_STOP: &[&str] = &[
    "the", "a", "an", "how", "what", "who", "why", "when", "where", "which", "is", "are", "was",
    "were", "do", "does", "did", "can", "could", "should", "would", "this", "that", "these",
    "those", "it", "its", "of", "to", "in", "on", "for", "and", "or", "with", "without", "from",
    "into", "about",
];

/// The scripts this matcher distinguishes. Only the CJK family changes the
/// matching rule; the rest are recorded so a locale decision can read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    Latin,
    Kana,
    Han,
    Hangul,
    Cyrillic,
    Arabic,
    Devanagari,
}

fn is_kana(c: char) -> bool {
    ('\u{3040}'..='\u{30ff}').contains(&c)
}
fn is_han(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
        || ('\u{3400}'..='\u{4dbf}').contains(&c)
        || ('\u{f900}'..='\u{faff}').contains(&c)
}
fn is_hangul(c: char) -> bool {
    ('\u{ac00}'..='\u{d7af}').contains(&c)
}
fn is_cjk(c: char) -> bool {
    is_kana(c) || is_han(c) || is_hangul(c)
}

pub fn script_of(text: &str) -> Script {
    for c in text.chars() {
        if is_kana(c) {
            return Script::Kana;
        }
        if is_hangul(c) {
            return Script::Hangul;
        }
        if is_han(c) {
            return Script::Han;
        }
        if ('\u{0400}'..='\u{04ff}').contains(&c) {
            return Script::Cyrillic;
        }
        if ('\u{0600}'..='\u{06ff}').contains(&c) {
            return Script::Arabic;
        }
        if ('\u{0900}'..='\u{097f}').contains(&c) {
            return Script::Devanagari;
        }
    }
    Script::Latin
}

/// One weighted query term. CJK terms are character bigrams; a Latin term is a
/// word. The weight is 2 for a quoted phrase, 1 otherwise.
#[derive(Debug, Clone)]
struct Term {
    text: String,
    weight: f64,
}

/// The query, compiled into the rubric its candidates are judged by.
#[derive(Debug, Clone)]
pub struct Profile {
    terms: Vec<Term>,
    /// Named entities: capitalised runs (an internal capital counts even in the
    /// first word, so "SpaceX" is one and a sentence-start "Latest" is not) and
    /// quoted spans.
    anchors: Vec<String>,
    /// `-word` exclusions.
    negated: Vec<String>,
}

impl Profile {
    pub fn new(query: &str) -> Self {
        let mut terms: Vec<Term> = Vec::new();
        let mut anchors: Vec<String> = Vec::new();
        let mut negated: Vec<String> = Vec::new();
        // Whitespace tokens carry the operators; each token is then split into
        // words. Quotes are stripped but remembered: a quoted word is stronger.
        let mut first_word = true;
        for raw in query.split_whitespace() {
            if let Some(rest) = raw.strip_prefix('-')
                && !rest.is_empty()
            {
                negated.push(rest.trim_matches('"').to_lowercase());
                continue;
            }
            // `site:` names a host and, often, a path: the path words are part
            // of what the query asked for, so they are terms too. The host is
            // already the lane's filter (`tasks` drops hits off it).
            let lower = raw.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("site:") {
                if let Some((_host, path)) = value.split_once('/') {
                    for word in path.split(|c: char| !c.is_alphanumeric()) {
                        if word.chars().count() >= 2 && !STOP.contains(&word) {
                            push_term(&mut terms, word.to_string(), 1.0);
                        }
                    }
                }
                continue;
            }
            let quoted = raw.starts_with('"') || raw.ends_with('"');
            let bare = raw.trim_matches('"');
            if quoted && !bare.is_empty() {
                anchors.push(bare.to_lowercase());
            }
            // A CJK run is one unit: bigrams, never whitespace words.
            if script_of(bare) != Script::Latin {
                for run in cjk_runs(bare) {
                    for term in bigrams(&run) {
                        push_term(&mut terms, term, 1.0);
                    }
                }
                first_word = false;
                continue;
            }
            let capitalised = bare.chars().next().is_some_and(char::is_uppercase);
            let internal_capital = bare.chars().skip(1).any(char::is_uppercase);
            let anchor_ok = capitalised
                && (internal_capital || !first_word)
                && !ANCHOR_STOP.contains(&bare.to_lowercase().as_str())
                && bare.chars().count() >= 2;
            if anchor_ok {
                anchors.push(bare.to_lowercase());
            }
            for word in bare.split(|c: char| !c.is_alphanumeric() && c != '+' && c != '#') {
                let lower = word.to_lowercase();
                if lower.is_empty() || STOP.contains(&lower.as_str()) {
                    continue;
                }
                if lower.chars().count() < 2 && !lower.chars().any(|c| c.is_numeric()) {
                    continue;
                }
                push_term(&mut terms, lower, if quoted { 2.0 } else { 1.0 });
            }
            first_word = false;
        }
        anchors.sort();
        anchors.dedup();
        negated.sort();
        negated.dedup();
        Self {
            terms,
            anchors,
            negated,
        }
    }

    /// The query's content terms, lowercased and stripped of operators — what
    /// the resolver reads to decide whether the query names one thing.
    pub fn terms(&self) -> impl Iterator<Item = &str> {
        self.terms.iter().map(|t| t.text.as_str())
    }

    /// Does `root` name one of the query's terms? `_` and `-` are the same in a
    /// package name.
    fn names(&self, root: &str) -> bool {
        let root = root.replace('_', "-").to_lowercase();
        self.terms.iter().any(|t| {
            t.text.replace('_', "-").to_lowercase() == root
                || t.text.eq_ignore_ascii_case(root.as_str())
        })
    }

    fn coverage(&self, text: &str) -> f64 {
        let total: f64 = self.terms.iter().map(|t| t.weight).sum();
        if total == 0.0 {
            return 1.0;
        }
        let matched: f64 = self
            .terms
            .iter()
            .filter(|t| matches(&t.text, text))
            .map(|t| t.weight)
            .sum();
        matched / total
    }

    fn anchor_state(&self, text: &str) -> (bool, bool) {
        if self.anchors.is_empty() {
            return (true, true);
        }
        let hit: Vec<bool> = self.anchors.iter().map(|a| matches(a, text)).collect();
        (hit.iter().any(|b| *b), hit.iter().all(|b| *b))
    }

    /// The verdict for one candidate.
    pub fn verdict(&self, title: &str, snippet: &str, url: &str) -> Verdict {
        if url_class(url) != UrlClass::Page {
            return Verdict::Off;
        }
        let text = format!("{title} {snippet} {url}");
        if self
            .negated
            .iter()
            .any(|n| matches(n, &text) || text.to_lowercase().contains(n))
        {
            return Verdict::Off;
        }
        if self.terms.is_empty() {
            // Nothing to judge (a bare symbol, a number): take the item at its word.
            return Verdict::Exact;
        }
        // Nothing is vetoed here on a single term: the answer set proves which
        // name the query meant (see `accept`), and a `site:` query names a host,
        // not an entity.
        let coverage = self.coverage(&text);
        let (any_anchor, all_anchors) = self.anchor_state(&text);
        // A scoped URL whose root is one of the query's terms is the record the
        // query named: the URL itself is the evidence, so a thin title cannot
        // put it out of the pool. Exact still needs the text to cover the query.
        let named = scoped_root(url).is_some_and(|(_, root)| self.names(&root));
        if coverage >= 0.6 && all_anchors {
            Verdict::Exact
        } else if named || (coverage >= 0.2 && any_anchor) {
            Verdict::Partial
        } else {
            Verdict::Off
        }
    }
}

fn push_term(terms: &mut Vec<Term>, text: String, weight: f64) {
    if let Some(existing) = terms.iter_mut().find(|t| t.text == text) {
        existing.weight = existing.weight.max(weight);
    } else {
        terms.push(Term { text, weight });
    }
}

/// Runs of CJK characters inside one token, e.g. "地震速報" or "東京の".
fn cjk_runs(token: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for c in token.chars() {
        if is_cjk(c) {
            current.push(c);
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

/// Character bigrams of a run; a one-character run is its own term.
fn bigrams(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    if chars.len() <= 1 {
        return vec![run.to_string()];
    }
    chars
        .windows(2)
        .map(|w| w.iter().collect())
        .collect::<Vec<String>>()
}

/// Does `term` occur in `text`? Latin terms are whole words, with a prefix
/// match for long ones ("dataclass" finds "dataclasses"); CJK terms are
/// substrings, because a Japanese title has no spaces to split on.
fn matches(term: &str, text: &str) -> bool {
    if term.chars().any(is_cjk) {
        return text.contains(term);
    }
    let lower = text.to_lowercase();
    let words = lower.split(|c: char| !c.is_alphanumeric() && c != '+' && c != '#');
    words
        .into_iter()
        .any(|w| w == term || (term.chars().count() >= 5 && w.starts_with(term)))
}

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
    "news.google.com/rss/articles",
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

pub fn url_class(raw: &str) -> UrlClass {
    let Ok(u) = url::Url::parse(raw) else {
        return UrlClass::Invalid;
    };
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
pub fn package_scope_named(profile: &Profile, raw: &str) -> bool {
    matches!(
        scoped_root(raw),
        Some((shape, root)) if shape != ScopeShape::Github && profile.names(&root)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Exact,
    Partial,
    Off,
}

impl Verdict {
    pub fn admitted(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// A source's answer after the contracts ran.
#[derive(Debug)]
pub struct Accepted {
    pub class: AnswerClass,
    /// Items that may enter the merge.
    pub kept: Vec<Hit>,
    /// Items the source returned, before any were dropped.
    pub items: usize,
}

/// Judge every item of a web source's answer, then judge the answer by them.
pub fn accept(query: &str, hits: Vec<Hit>) -> Accepted {
    let items = hits.len();
    if hits.is_empty() {
        return Accepted {
            class: AnswerClass::Empty,
            kept: Vec::new(),
            items,
        };
    }
    let profile = Profile::new(query);
    let verdicts: Vec<Verdict> = hits
        .iter()
        .map(|h| profile.verdict(&h.title, &h.snippet, &h.url))
        .collect();

    // A look-alike is only a look-alike when the answer set proves the name it
    // meant: if some scoped page's root is a query term, pages of the same shape
    // with another root are cousins, not answers. Without that anchor the query
    // is topical and its scoped results are left alone.
    let mut named: HashMap<ScopeShape, HashSet<String>> = HashMap::new();
    for (hit, verdict) in hits.iter().zip(&verdicts) {
        if verdict.admitted()
            && let Some((shape, root)) = scoped_root(&hit.url)
            && profile.names(&root)
        {
            named.entry(shape).or_default().insert(root);
        }
    }

    let mut kept = Vec::new();
    let mut engaged = 0usize;
    for (hit, verdict) in hits.into_iter().zip(verdicts) {
        let mut verdict = verdict;
        if verdict.admitted()
            && let Some((shape, root)) = scoped_root(&hit.url)
            && let Some(roots) = named.get(&shape)
            && !roots.contains(&root)
        {
            verdict = Verdict::Off;
        }
        if verdict.admitted() {
            engaged += 1;
            kept.push(hit);
        }
    }

    let top = items.min(10);
    let share = engaged.min(top) as f64 / top as f64;
    let class = if items >= 3 {
        if share >= 0.5 {
            AnswerClass::Content
        } else if share < 0.25 {
            AnswerClass::OffQuery
        } else {
            AnswerClass::Thin
        }
    } else {
        AnswerClass::Thin
    };
    Accepted { class, kept, items }
}

/// First-party items keep their own ranking (the API's relevance is the
/// contract), but they still lose redirects and look-alike namespaces.
pub fn accept_first_party(query: &str, hits: Vec<Hit>) -> Vec<Hit> {
    if hits.is_empty() {
        return hits;
    }
    let profile = Profile::new(query);
    let admissible = |hit: &Hit| url_class(&hit.url) == UrlClass::Page;
    let mut named: HashMap<ScopeShape, HashSet<String>> = HashMap::new();
    for hit in hits.iter().filter(|h| admissible(h)) {
        if let Some((shape, root)) = scoped_root(&hit.url)
            && profile.names(&root)
        {
            named.entry(shape).or_default().insert(root);
        }
    }
    hits.into_iter()
        .filter(|hit| {
            if !admissible(hit) {
                return false;
            }
            if let Some((shape, root)) = scoped_root(&hit.url)
                && named.contains_key(&shape)
                && !profile.names(&root)
            {
                return false;
            }
            true
        })
        .collect()
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

/// The window a query states, if it states one. `latest` alone is not a window
/// — it is a ranking hint, and a week would silently drop the right page.
pub fn window_for(query: &str) -> Option<Window> {
    let lower = query.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let text = words.join(" ");
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
    if text.contains("this week") || text.contains("past week") || text.contains("last week") {
        return Some(window_days(7));
    }
    if text.contains("this month") || text.contains("past month") || text.contains("last month") {
        return Some(window_days(31));
    }
    if text.contains("this year") {
        return Some(window_days(365));
    }
    if text.contains("today") || text.contains("breaking") || text.contains("this morning") {
        return Some(window_days(1));
    }
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

    fn verdict(query: &str, title: &str, url: &str) -> Verdict {
        Profile::new(query).verdict(title, "", url)
    }

    #[test]
    fn a_trending_page_does_not_answer_a_named_query() {
        let q = "SpaceX Starship latest launch news this week";
        assert_eq!(
            verdict(
                q,
                "Diwali 2026: celebrations this week - AP News",
                "https://apnews.com/x"
            ),
            Verdict::Off
        );
        assert_eq!(
            verdict(
                q,
                "SpaceX will perform Starship double catch attempt with Flight 15",
                "https://www.teslarati.com/x"
            ),
            Verdict::Exact
        );
    }

    #[test]
    fn a_temporal_word_is_not_evidence() {
        let p = Profile::new("starship latest news this week");
        let text = "Diwali celebrations this week";
        assert_eq!(p.coverage(text), 0.0, "only temporal words matched");
    }

    #[test]
    fn a_scope_named_page_is_admitted_on_its_url_alone() {
        // The docs page's title is just the crate name; the URL root is what
        // proves it answers the query.
        assert_eq!(
            verdict(
                "rust clap command line argument parser crate",
                "clap 4.5",
                "https://docs.rs/clap/latest/clap/"
            ),
            Verdict::Partial
        );
        assert_eq!(
            verdict(
                "rust clap command line argument parser crate",
                "select",
                "https://docs.rs/taos/latest/taos/"
            ),
            Verdict::Off
        );
    }

    #[test]
    fn an_unspaced_japanese_query_finds_its_answers() {
        let hits = vec![
            hit(
                "地震情報 - ウェザーニュース",
                "https://weathernews.jp/quake/",
            ),
            hit("地震速報 - NHK", "https://news.web.nhk/earthquake/"),
            hit("気象庁 地震情報", "https://www.data.jma.go.jp/quake/"),
            hit("東京都の地震情報", "https://map.bosai.metro.tokyo.lg.jp/"),
            hit(
                "地震速報 - Yahoo!天気",
                "https://typhoon.yahoo.co.jp/earthquake/",
            ),
        ];
        let accepted = accept("地震速報", hits);
        assert_eq!(accepted.class, AnswerClass::Content);
        assert_eq!(accepted.kept.len(), 5);
    }

    #[test]
    fn spaces_do_not_change_a_japanese_answer() {
        let hits = vec![
            hit(
                "地震情報 - ウェザーニュース",
                "https://weathernews.jp/quake/",
            ),
            hit("地震速報 - NHK", "https://news.web.nhk/earthquake/"),
            hit("気象庁 地震情報", "https://www.data.jma.go.jp/quake/"),
            hit("東京都の地震情報", "https://map.bosai.metro.tokyo.lg.jp/"),
            hit(
                "地震速報 - Yahoo!天気",
                "https://typhoon.yahoo.co.jp/earthquake/",
            ),
        ];
        let spaced = accept("東京 地震 速報", hits);
        assert_eq!(spaced.class, AnswerClass::Content);
    }

    #[test]
    fn a_look_alike_crate_is_rejected_by_the_name_it_names() {
        let hits = vec![
            hit("tokio - Rust", "https://docs.rs/tokio/latest/tokio/"),
            hit(
                "select in taos::tokio",
                "https://docs.rs/taos/latest/taos/tokio/macro.select.html",
            ),
        ];
        let accepted = accept("tokio", hits);
        let urls: Vec<&str> = accepted.kept.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(urls, vec!["https://docs.rs/tokio/latest/tokio/"]);
    }

    #[test]
    fn a_topical_query_does_not_veto_other_crates() {
        let hits = vec![
            hit(
                "taos — an async runtime",
                "https://docs.rs/taos/latest/taos/",
            ),
            hit(
                "tokio — an async runtime",
                "https://docs.rs/tokio/latest/tokio/",
            ),
        ];
        let accepted = accept("async runtime rust", hits);
        assert_eq!(
            accepted.kept.len(),
            2,
            "no query term names either crate, so neither is a look-alike"
        );
    }

    #[test]
    fn an_aggregator_redirect_is_never_a_result() {
        assert_eq!(
            url_class("https://news.google.com/rss/articles/CBMiabc?oc=5"),
            UrlClass::Redirect
        );
        assert_eq!(
            verdict(
                "starship launch",
                "SpaceX Starship launch",
                "https://news.google.com/rss/articles/CBMiabc?oc=5"
            ),
            Verdict::Off
        );
    }

    #[test]
    fn a_search_results_page_is_never_a_result() {
        assert_eq!(url_class("https://www.bing.com/search?q=x"), UrlClass::Serp);
        assert_eq!(url_class("https://example.com/page"), UrlClass::Page);
        assert_eq!(url_class("javascript:alert(1)"), UrlClass::Invalid);
    }

    #[test]
    fn latin_prefix_matching_survives() {
        assert!(matches("dataclass", "Python Dataclasses — documentation"));
        assert!(!matches("dataclass", "Python decorators"));
    }

    #[test]
    fn an_exclusion_term_rejects_the_item() {
        assert_eq!(
            verdict(
                "rust async -python",
                "Async Python",
                "https://example.com/python"
            ),
            Verdict::Off
        );
    }

    #[test]
    fn an_off_query_answer_is_named_and_kept_out() {
        let hits: Vec<Hit> = (0..6)
            .map(|i| {
                hit(
                    &format!("Diwali celebration {i}"),
                    &format!("https://example.com/diwali/{i}"),
                )
            })
            .collect();
        let accepted = accept("SpaceX Starship launch", hits);
        assert_eq!(accepted.class, AnswerClass::OffQuery);
        assert!(accepted.kept.is_empty());
    }

    #[test]
    fn a_short_answer_is_thin_not_off_query() {
        let hits = vec![
            hit("Starship launch date", "https://example.com/a"),
            hit("Falcon 9 booster recovery", "https://example.com/b"),
        ];
        let accepted = accept("starship launch", hits);
        assert_eq!(accepted.class, AnswerClass::Thin);
        assert_eq!(accepted.kept.len(), 1);
    }

    #[test]
    fn a_first_party_answer_loses_redirects_and_look_alikes() {
        let hits = vec![
            hit("crate tokio", "https://crates.io/crates/tokio"),
            hit("crate taos", "https://crates.io/crates/taos"),
            hit("tokio news", "https://news.google.com/rss/articles/x"),
        ];
        let kept = accept_first_party("tokio", hits);
        let urls: Vec<&str> = kept.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(urls, vec!["https://crates.io/crates/tokio"]);
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
    fn a_bench_window_parses_and_stays_inside_the_calendar() {
        assert_eq!(window_from_str("7d"), Some(window_days(7)));
        assert_eq!(window_from_str("2w"), Some(window_days(14)));
        assert_eq!(window_from_str("12h"), Some(window_days(1)));
        assert_eq!(window_from_str("0d"), None);
        assert_eq!(window_from_str("7x"), None);
    }
}
