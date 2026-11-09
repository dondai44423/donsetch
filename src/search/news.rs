//! The news program: the wires and the feeds first, the open web only when they
//! are thin.
//!
//! Three sources, each with a different job:
//!
//! - **Bing News RSS**, sorted by date — the wire. One request, real publisher
//!   URLs (its `apiclick` wrapper is decoded locally), dated items only.
//! - **The publisher mesh** — a bounded registry of first-party feeds, cached on
//!   disk with `If-None-Match` / `If-Modified-Since`. A warm mesh costs zero
//!   requests per search; a cold one refreshes at most two feeds.
//! - **GDELT** — a leased source: one request when its rest has expired, never a
//!   retry inside the call. On this IP it answers 429 to a spaced retry, so it
//!   is expected to rest, and the lane says so.
//!
//! Every item is dated: an undated item is not a news item, and the date
//! contract ([`super::contract::Window`]) decides whether it is inside the
//! query's window. Nothing here ranks — the lane reports, the merge orders.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::contract::{Profile, Verdict, Window, day_of, window_for};
use super::engines::Hit;
use super::intent::Intent;
use super::verticals::rss_date_to_iso;
use crate::fetch::client::Fetcher;

const TTL_SECS: u64 = 600;
const REFRESH_PER_SEARCH: usize = 2;
const MAX_ITEMS_PER_FEED: usize = 80;
const DISK_VERSION: u32 = 1;
const GDELT_MIN_SECS: u64 = 30;
const GDELT_REST_SECS: u64 = 300;

/// The mesh's registry. Bounded on purpose: a feed list is a budget, and every
/// entry here was measured to answer with dated items through our fetcher.
const FEEDS: &[(&str, &str)] = &[
    ("spacenews", "https://spacenews.com/feed/"),
    ("teslarati", "https://www.teslarati.com/feed/"),
    ("guardian", "https://www.theguardian.com/world/rss"),
    (
        "arstechnica",
        "https://feeds.arstechnica.com/arstechnica/index",
    ),
    ("bbc", "https://feeds.bbci.co.uk/news/rss.xml"),
    ("npr", "https://www.npr.org/rss/rss.php?id=1001"),
    ("techcrunch", "https://techcrunch.com/feed/"),
    ("aljazeera", "https://www.aljazeera.com/xml/rss/all.xml"),
    (
        "nytimes",
        "https://rss.nytimes.com/services/xml/rss/nyt/HomePage.xml",
    ),
    ("wired", "https://www.wired.com/feed/rss"),
    ("engadget", "https://www.engadget.com/rss.xml"),
];

/// One source's report for this call.
#[derive(Debug, Default)]
pub struct Answer {
    pub hits: Vec<Hit>,
    pub ms: u64,
    /// `ok`, `no-results`, `rested`, `failed` or `late` — the census vocabulary.
    pub status: &'static str,
}

/// Does this query want the news program? A News intent, or a query that states
/// a window and asks for an announcement-shaped thing.
pub fn wants_news(query: &str, intent: Intent) -> bool {
    if intent == Intent::News {
        return true;
    }
    const WORDS: &[&str] = &[
        "news",
        "release",
        "released",
        "announcement",
        "announced",
        "earnings",
        "winner",
        "decision",
        "results",
    ];
    let lower = query.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| WORDS.contains(&w))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn enc(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let entity = &rest[1..end];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            other => other.strip_prefix('#').and_then(|n| {
                let n = n.strip_prefix('x').map_or_else(
                    || n.parse::<u32>().ok(),
                    |hex| u32::from_str_radix(hex, 16).ok(),
                );
                n.and_then(char::from_u32)
            }),
        };
        match replacement {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn percent_decode(text: &str) -> String {
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

/// Bing wraps some items in `apiclick.aspx?…&url=<encoded>`; the publisher URL
/// is inside. A wrapper we cannot decode is not a result.
fn decode_link(raw: &str) -> Option<String> {
    let link = unescape(raw.trim());
    if !link.starts_with("http") {
        return None;
    }
    if link.contains("bing.com") {
        let query = link.split_once('?')?.1;
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("url=") {
                let decoded = percent_decode(value);
                if decoded.starts_with("http") {
                    return Some(decoded);
                }
            }
        }
        return None;
    }
    Some(link)
}

fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str, chars: usize) -> String {
    let mut s: String = text.chars().take(chars).collect();
    if text.chars().count() > chars {
        s.push('…');
    }
    s
}

fn iso_day(stamp: &str) -> Option<String> {
    day_of(stamp)?;
    Some(stamp.chars().take(10).collect())
}

/// One RSS/Atom item, before matching.
struct Parsed {
    title: String,
    url: String,
    summary: String,
    day: String,
}

fn grab(item: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    item.split_once(&open)
        .and_then(|(_, rest)| rest.split_once(&close))
        .map(|(inner, _)| {
            inner
                .trim()
                .trim_start_matches("<![CDATA[")
                .trim_end_matches("]]>")
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

fn parse_rss_items(body: &str, limit: usize) -> Vec<Parsed> {
    let mut out = Vec::new();
    let atom = !body.contains("<item>") && body.contains("<entry>");
    let (open, close) = if atom {
        ("<entry>", "</entry>")
    } else {
        ("<item>", "</item>")
    };
    for chunk in body.split(open).skip(1) {
        let item = chunk.split(close).next().unwrap_or("");
        let title = unescape(&grab(item, "title"));
        let url = if atom {
            item.split_once("<link")
                .and_then(|(_, rest)| rest.split_once("href=\""))
                .and_then(|(_, rest)| rest.split_once('"'))
                .map(|(h, _)| h.to_string())
                .unwrap_or_default()
        } else {
            let raw = grab(item, "link");
            decode_link(&raw).unwrap_or_default()
        };
        let date = if atom {
            let updated = grab(item, "updated");
            if updated.is_empty() {
                grab(item, "published")
            } else {
                updated
            }
        } else {
            grab(item, "pubDate")
        };
        let Some(iso) = rss_date_to_iso(&date).or_else(|| iso_day(&date)) else {
            continue;
        };
        let summary = strip_tags(&unescape(&grab(
            item,
            if atom { "summary" } else { "description" },
        )));
        if title.is_empty() || !url.starts_with("http") {
            continue;
        }
        out.push(Parsed {
            title,
            url,
            summary: truncate(&summary, 220),
            day: iso,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// How many of the query's content terms the item's text carries. The wire is
/// date-sorted, and a week-old tangential piece must not sit above today's
/// story that actually covers the query.
fn coverage(profile: &Profile, h: &Hit) -> usize {
    let text = format!("{} {} {}", h.title, h.snippet, h.url).to_ascii_lowercase();
    profile.terms().filter(|t| text.contains(*t)).count()
}

fn hit_from(parsed: &Parsed, rank: usize) -> Hit {
    Hit {
        title: parsed.title.clone(),
        url: parsed.url.clone(),
        snippet: parsed.summary.clone(),
        rank,
        published: Some(parsed.day.clone()),
    }
}

/// Keep what the query is about: exact matches only. A news slot is scarce, and
/// a headline that shares one word with the query is noise next to the story.
fn grade(hits: Vec<Hit>, profile: &Profile, window: Option<Window>, cap: usize) -> Vec<Hit> {
    let mut kept: Vec<Hit> = hits
        .into_iter()
        .filter(|h| {
            if let Some(w) = window
                && h.published
                    .as_deref()
                    .and_then(day_of)
                    .is_none_or(|d| !w.contains(d))
            {
                return false;
            }
            profile.verdict(&h.title, &h.snippet, &h.url) == Verdict::Exact
        })
        .collect();
    kept.sort_by_key(|h| std::cmp::Reverse(coverage(profile, h)));
    kept.truncate(cap);
    for (rank, h) in kept.iter_mut().enumerate() {
        h.rank = rank;
    }
    kept
}

/// One request to the wire.
pub async fn bing_news(
    query: &str,
    fetcher: &Fetcher,
    proxy: Option<&crate::egress::proxy::Proxy>,
    budget: Duration,
) -> Answer {
    let started = Instant::now();
    let url = format!(
        "https://www.bing.com/news/search?format=rss&qft=sortbydate%3D%221%22&q={}",
        enc(query)
    );
    let out = match tokio::time::timeout(
        budget,
        fetcher.fetch_once_via(&url, &[], proxy, false, None),
    )
    .await
    {
        Ok(Ok(out)) => out,
        Ok(Err(_)) => {
            return Answer {
                ms: started.elapsed().as_millis() as u64,
                status: "failed",
                ..Default::default()
            };
        }
        Err(_) => {
            return Answer {
                ms: started.elapsed().as_millis() as u64,
                status: "late",
                ..Default::default()
            };
        }
    };
    if out.status != 200 {
        return Answer {
            ms: started.elapsed().as_millis() as u64,
            status: "failed",
            ..Default::default()
        };
    }
    let body = String::from_utf8_lossy(&out.body);
    let parsed = parse_rss_items(&body, 20);
    let hits: Vec<Hit> = parsed
        .iter()
        .enumerate()
        .map(|(i, p)| hit_from(p, i))
        .collect();
    let profile = Profile::new(query);
    let hits = grade(hits, &profile, window_for(query), 12);
    let ms = started.elapsed().as_millis() as u64;
    Answer {
        status: if hits.is_empty() { "no-results" } else { "ok" },
        hits,
        ms,
    }
}

// ── The mesh ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredItem {
    title: String,
    url: String,
    summary: String,
    published: String,
    source: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FeedEntry {
    at: u64,
    #[serde(default)]
    etag: String,
    #[serde(default)]
    last_modified: String,
    #[serde(default)]
    items: Vec<StoredItem>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FeedDisk {
    version: u32,
    #[serde(default)]
    feeds: BTreeMap<String, FeedEntry>,
}

impl Default for FeedDisk {
    fn default() -> Self {
        Self {
            version: DISK_VERSION,
            feeds: BTreeMap::new(),
        }
    }
}

fn feed_path() -> Option<std::path::PathBuf> {
    Some(crate::paths::cache_dir().join("search-feeds.json"))
}

fn load_feeds() -> FeedDisk {
    let Some(path) = feed_path() else {
        return FeedDisk::default();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return FeedDisk::default();
    };
    match serde_json::from_slice::<FeedDisk>(&bytes) {
        Ok(disk) if disk.version == DISK_VERSION => disk,
        _ => FeedDisk::default(),
    }
}

fn save_feeds(disk: &FeedDisk) {
    let Some(path) = feed_path() else { return };
    let Ok(json) = serde_json::to_vec(disk) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if crate::config::write_private(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Feeds whose cache is older than the TTL, oldest first, at most the refresh
/// budget. A warm mesh selects nothing — that is the zero-request case.
fn stale_feeds(disk: &FeedDisk, now: u64) -> Vec<(&'static str, &'static str)> {
    let mut stale: Vec<_> = FEEDS
        .iter()
        .copied()
        .filter(|(_, url)| {
            disk.feeds
                .get(*url)
                .is_none_or(|e| now.saturating_sub(e.at) > TTL_SECS)
        })
        .collect();
    stale.sort_by_key(|(_, url)| disk.feeds.get(*url).map(|e| e.at).unwrap_or(0));
    stale.truncate(REFRESH_PER_SEARCH);
    stale
}

enum Refresh {
    NotModified,
    Fetched {
        etag: String,
        last_modified: String,
        items: Vec<StoredItem>,
    },
    Failed,
}

fn header(out: &crate::fetch::client::FetchOutcome, name: &str) -> String {
    out.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

async fn refresh(
    fetcher: &Fetcher,
    name: &str,
    url: &str,
    etag: &str,
    last_modified: &str,
    budget: Duration,
) -> Refresh {
    let mut conditional: Vec<(String, String)> = Vec::new();
    if !etag.is_empty() {
        conditional.push(("If-None-Match".into(), etag.to_string()));
    }
    if !last_modified.is_empty() {
        conditional.push(("If-Modified-Since".into(), last_modified.to_string()));
    }
    let out = match tokio::time::timeout(
        budget,
        fetcher.fetch_once_via(url, &conditional, None, false, None),
    )
    .await
    {
        Ok(Ok(out)) => out,
        _ => return Refresh::Failed,
    };
    if out.status == 304 {
        return Refresh::NotModified;
    }
    if out.status != 200 {
        return Refresh::Failed;
    }
    let body = String::from_utf8_lossy(&out.body);
    let items: Vec<StoredItem> = parse_rss_items(&body, MAX_ITEMS_PER_FEED)
        .into_iter()
        .map(|p| StoredItem {
            title: p.title,
            url: p.url,
            summary: p.summary,
            published: p.day,
            source: name.to_string(),
        })
        .collect();
    if items.is_empty() {
        return Refresh::Failed;
    }
    Refresh::Fetched {
        etag: header(&out, "etag"),
        last_modified: header(&out, "last-modified"),
        items,
    }
}

/// The mesh: refresh what is stale (bounded), then answer from everything
/// cached. Matching is exact — a feed is keyword-blind, so a tangential item
/// is noise, not a candidate.
pub async fn mesh(
    profile: &Profile,
    window: Option<Window>,
    fetcher: &Fetcher,
    budget: Duration,
) -> Answer {
    let started = Instant::now();
    let mut disk = load_feeds();
    // A warm mesh that already holds the story answers immediately and spends
    // nothing; the refresh is only for a cache that has no answer to give.
    let cached = match_items(&disk, profile, window);
    if !cached.is_empty() {
        return Answer {
            hits: hits_from(cached),
            ms: started.elapsed().as_millis() as u64,
            status: "ok",
        };
    }
    let now = now_secs();
    let refresh_list = stale_feeds(&disk, now);
    if !refresh_list.is_empty() {
        let asks = refresh_list.iter().map(|(name, url)| {
            let entry = disk.feeds.get(*url).cloned().unwrap_or_default();
            async move {
                (
                    *url,
                    refresh(
                        fetcher,
                        name,
                        url,
                        &entry.etag,
                        &entry.last_modified,
                        budget,
                    )
                    .await,
                )
            }
        });
        let done = futures_util::future::join_all(asks).await;
        for (url, outcome) in done {
            let entry = disk.feeds.entry(url.to_string()).or_default();
            entry.at = now;
            if let Refresh::Fetched {
                etag,
                last_modified,
                items,
            } = outcome
            {
                entry.etag = etag;
                entry.last_modified = last_modified;
                entry.items = items;
            }
        }
        save_feeds(&disk);
    }
    let hits = hits_from(match_items(&disk, profile, window));
    let ms = started.elapsed().as_millis() as u64;
    Answer {
        status: if hits.is_empty() { "no-results" } else { "ok" },
        hits,
        ms,
    }
}

fn hits_from(matched: Vec<&StoredItem>) -> Vec<Hit> {
    matched
        .iter()
        .take(10)
        .enumerate()
        .map(|(rank, item)| Hit {
            title: item.title.clone(),
            url: item.url.clone(),
            snippet: item.summary.clone(),
            rank,
            published: Some(item.published.clone()),
        })
        .collect()
}

/// Cached items the query asks for: exact, dated, and inside the window —
/// newest first. A feed is keyword-blind, so a tangential item is noise.
fn match_items<'a>(
    disk: &'a FeedDisk,
    profile: &Profile,
    window: Option<Window>,
) -> Vec<&'a StoredItem> {
    let mut matched: Vec<(&StoredItem, i64, usize)> = Vec::new();
    for entry in disk.feeds.values() {
        for item in &entry.items {
            let Some(day) = day_of(&item.published) else {
                continue;
            };
            if let Some(w) = window
                && !w.contains(day)
            {
                continue;
            }
            if profile.verdict(&item.title, &item.summary, &item.url) != Verdict::Exact {
                continue;
            }
            let text = format!("{} {} {}", item.title, item.summary, item.url).to_ascii_lowercase();
            let cover = profile.terms().filter(|t| text.contains(*t)).count();
            matched.push((item, day, cover));
        }
    }
    matched.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| b.1.cmp(&a.1)));
    matched.truncate(10);
    matched.into_iter().map(|(item, _, _)| item).collect()
}

// ── GDELT, leased ──────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct Lease {
    #[serde(default)]
    next_at: u64,
}

fn lease_path() -> Option<std::path::PathBuf> {
    Some(crate::paths::cache_dir().join("search-gdelt.json"))
}

fn load_lease() -> Lease {
    let Some(path) = lease_path() else {
        return Lease::default();
    };
    std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_lease(lease: &Lease) {
    let Some(path) = lease_path() else { return };
    let Ok(json) = serde_json::to_vec(lease) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if crate::config::write_private(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn parse_gdelt(body: &str) -> Vec<Hit> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    for article in v["articles"].as_array().into_iter().flatten() {
        let url = article["url"].as_str().unwrap_or("");
        let title = unescape(article["title"].as_str().unwrap_or(""));
        // GDELT stamps are compact: `20261109T101500Z`.
        let seen = article["seendate"].as_str().unwrap_or("");
        let day = if seen.len() >= 8 && seen.as_bytes()[4].is_ascii_digit() {
            format!("{}-{}-{}", &seen[0..4], &seen[4..6], &seen[6..8])
        } else {
            match iso_day(seen) {
                Some(day) => day,
                None => continue,
            }
        };
        if day_of(&day).is_none() {
            continue;
        }
        if !url.starts_with("http") || title.is_empty() {
            continue;
        }
        hits.push(Hit {
            title,
            url: url.to_string(),
            snippet: article["domain"].as_str().unwrap_or("").to_string(),
            rank: hits.len(),
            published: Some(day),
        });
    }
    hits
}

pub async fn gdelt(query: &str, fetcher: &Fetcher, budget: Duration) -> Answer {
    let started = Instant::now();
    // GDELT is the last source to matter and the first to rest: it never owns
    // the call's latency.
    let budget = budget.min(Duration::from_millis(800));
    let now = now_secs();
    let mut lease = load_lease();
    if now < lease.next_at {
        return Answer {
            ms: 0,
            status: "rested",
            ..Default::default()
        };
    }
    let url = format!(
        "https://api.gdeltproject.org/api/v2/doc/doc?query={}&mode=artlist&format=json&maxrecords=10&sort=datedesc",
        enc(query)
    );
    let out =
        match tokio::time::timeout(budget, fetcher.fetch_once_via(&url, &[], None, false, None))
            .await
        {
            Ok(Ok(out)) => out,
            _ => {
                lease.next_at = now + GDELT_MIN_SECS;
                save_lease(&lease);
                return Answer {
                    ms: started.elapsed().as_millis() as u64,
                    status: "failed",
                    ..Default::default()
                };
            }
        };
    let status = match out.status {
        200 => {
            lease.next_at = now + GDELT_MIN_SECS;
            save_lease(&lease);
            let body = String::from_utf8_lossy(&out.body);
            let hits = grade(
                parse_gdelt(&body),
                &Profile::new(query),
                window_for(query),
                10,
            );
            return Answer {
                ms: started.elapsed().as_millis() as u64,
                status: if hits.is_empty() { "no-results" } else { "ok" },
                hits,
            };
        }
        429 => {
            lease.next_at = now + GDELT_REST_SECS;
            "rested"
        }
        _ => {
            lease.next_at = now + GDELT_MIN_SECS;
            "failed"
        }
    };
    save_lease(&lease);
    Answer {
        ms: started.elapsed().as_millis() as u64,
        status,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bing_wrapper_is_decoded_to_the_publisher() {
        let wrapped = "https://www.bing.com/news/apiclick.aspx?ref=FexRss&aid=1&url=https%3a%2f%2fwww.example.com%2fstory%3fa%3d1&amp;c=2";
        assert_eq!(
            decode_link(wrapped).as_deref(),
            Some("https://www.example.com/story?a=1")
        );
        assert_eq!(
            decode_link("https://example.com/direct").as_deref(),
            Some("https://example.com/direct")
        );
        assert_eq!(decode_link("https://www.bing.com/ck/a?u=abc"), None);
    }

    #[test]
    fn entities_in_a_title_survive() {
        assert_eq!(unescape("A &amp; B &#8211; C"), "A & B – C");
    }

    #[test]
    fn rss_items_carry_a_date_or_they_are_not_news() {
        let body = r#"<rss><channel>
        <item><title>One</title><link>https://e.com/1</link><pubDate>Mon, 09 Nov 2026 07:00:00 GMT</pubDate><description>Hello</description></item>
        <item><title>Two</title><link>https://e.com/2</link><description>No date</description></item>
        </channel></rss>"#;
        let items = parse_rss_items(body, 10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].day, "2026-11-09");
    }

    #[test]
    fn an_atom_feed_parses_too() {
        let body = r#"<feed><entry><title>Atom one</title><link href="https://e.com/a"/><updated>2026-11-08T10:00:00Z</updated><summary>Text</summary></entry></feed>"#;
        let items = parse_rss_items(body, 10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].url, "https://e.com/a");
        assert_eq!(items[0].day, "2026-11-08");
    }

    #[test]
    fn the_mesh_refreshes_nothing_when_it_is_warm() {
        let mut disk = FeedDisk::default();
        for (_, url) in FEEDS {
            disk.feeds.insert(
                url.to_string(),
                FeedEntry {
                    at: 1_000,
                    ..Default::default()
                },
            );
        }
        assert!(stale_feeds(&disk, 1_030).is_empty());
        // Ten minutes later the first two (oldest) are the refresh budget.
        let stale = stale_feeds(&disk, 1_000 + TTL_SECS + 1);
        assert_eq!(stale.len(), REFRESH_PER_SEARCH);
    }

    #[test]
    fn matching_is_exact_and_inside_the_window() {
        let profile = Profile::new("apple q4 2026 earnings");
        let window = window_for("apple q4 2026 earnings");
        let today = super::super::contract::today();
        let fresh = StoredItem {
            title: "Apple reports Q4 2026 earnings".into(),
            url: "https://e.com/a".into(),
            summary: "Record profit".into(),
            published: "2026-11-06".into(),
            source: "x".into(),
        };
        let mut old = fresh.clone();
        old.published = "2020-01-01".into();
        let mut other = fresh.clone();
        other.title = "Weather".into();
        other.summary = "Sunny".into();
        let disk = FeedDisk {
            version: DISK_VERSION,
            feeds: BTreeMap::from([(
                "x".to_string(),
                FeedEntry {
                    at: 1,
                    items: vec![fresh, old, other],
                    ..Default::default()
                },
            )]),
        };
        let kept = match_items(&disk, &profile, window);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].title, "Apple reports Q4 2026 earnings");
        let _ = today;
    }

    #[test]
    fn gdelt_articles_are_dated_or_dropped() {
        let body = r#"{"articles":[{"url":"https://e.com/1","title":"One","seendate":"20261109T101500Z","domain":"e.com"},{"url":"https://e.com/2","title":"Two","seendate":""}]}"#;
        let hits = parse_gdelt(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].published.as_deref(), Some("2026-11-09"));
    }

    #[test]
    fn a_news_shaped_dated_query_wants_the_program() {
        assert!(wants_news("apple q4 2026 earnings results", Intent::Web));
        assert!(wants_news("anything", Intent::News));
        assert!(!wants_news("rust tokio", Intent::Code));
        assert!(!wants_news("tokio 2026", Intent::Web));
    }
}
