//! Bounded inventory: robots, llms.txt (links and a page-list API), sitemaps,
//! feeds and the seed's own links. Every read is counted; the map's inventory
//! budget is a hard cap, not a suggestion.
//!
//! The collector never fetches a content page except the seed fallback, and
//! it never runs a sitemap read it cannot account for. A site that publishes a
//! page list (as `llms.txt` may declare) is read as that list, not probed page
//! by page.

use super::infer::InvUrl;
use crate::crawl::frontier;
use crate::crawl::sitemap::{self, Robots, SitemapEntry};
use crate::crawl::{FetchedPage, PageFetcher, SeedFailure};
use crate::detect::walls::Verdict;
use scraper::{Html, Selector};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::time::Instant;
use url::Url;

/// A map's inventory reads: robots (preflight, not counted here), llms.txt,
/// the sitemap, one child, the page list or the seed fallback.
pub const INVENTORY_REQUEST_BUDGET: usize = 4;

/// The inventory upper bound. A map that needs more is not a map.
pub const URL_CAP: usize = 100_000;

pub struct InventoryOptions<'a> {
    pub seed: &'a Url,
    pub requested_seed: &'a str,
    /// The seed page, when the caller already fetched it (tests, preflight).
    /// None makes the fallback fetch it once.
    pub seed_page: Option<&'a FetchedPage>,
    /// The seed origin's robots rules, when the crawler read them.
    pub robots: Option<&'a Robots>,
    pub focus: Option<&'a str>,
    /// The seed's own last path segment as a ranking query when the caller
    /// set no focus (a topic seed's neighborhood).
    pub seed_query: Option<&'a str>,
    pub include: &'a [String],
    pub exclude: &'a [String],
    pub same_host: bool,
    pub respect_robots: bool,
    pub deadline: Instant,
    pub request_budget: usize,
    pub url_cap: usize,
    pub map_cap: usize,
}

pub struct InventoryResult {
    pub urls: Vec<InvUrl>,
    /// Sitemap/seed URLs seen before scope, locale and policy filters.
    pub raw: usize,
    pub map: Vec<String>,
    pub sources: BTreeMap<String, usize>,
    pub requests: usize,
    pub complete: bool,
    pub filtered: super::infer::FilterCounts,
    pub apis: BTreeMap<String, String>,
    pub deadline_hit: bool,
    pub source: &'static str,
    pub seed_failure: Option<SeedFailure>,
}

pub async fn collect(fetch: &PageFetcher, opts: &InventoryOptions<'_>) -> InventoryResult {
    let mut c = Collector {
        fetch,
        opts,
        urls: BTreeMap::new(),
        sources: BTreeMap::new(),
        requests: 0,
        deadline_hit: false,
        sitemap_found: false,
        sitemap_complete: false,
        sitemap_failed_child: false,
        pagelist_found: false,
        pagelist_complete: false,
        cap_hit: false,
        apis: BTreeMap::new(),
        filtered: super::infer::FilterCounts::default(),
        seed_failure: None,
    };
    c.llms().await;
    if !c.pagelist_found {
        c.sitemaps().await;
    }
    // A sitemap can be a site map for a different scope (docs.python.org/3/
    // versus the root sitemap). When it yields almost nothing in this
    // seed's scope, the seed's own links are the honest inventory.
    let raw_count = c.urls.len();
    let in_scope = c
        .urls
        .keys()
        .filter_map(|k| Url::parse(k).ok())
        .filter(|u| frontier::scope_allowed(u.path(), opts.include, opts.exclude))
        .count();
    let thin = !c.pagelist_found
        && (in_scope == 0 || (in_scope < 3 && raw_count > in_scope.saturating_mul(2)));
    if thin {
        c.seed_links().await;
    }
    c.finish()
}

struct Collector<'a> {
    fetch: &'a PageFetcher,
    opts: &'a InventoryOptions<'a>,
    urls: BTreeMap<String, InvUrl>,
    sources: BTreeMap<String, usize>,
    requests: usize,
    deadline_hit: bool,
    sitemap_found: bool,
    sitemap_complete: bool,
    sitemap_failed_child: bool,
    pagelist_found: bool,
    pagelist_complete: bool,
    cap_hit: bool,
    apis: BTreeMap<String, String>,
    filtered: super::infer::FilterCounts,
    seed_failure: Option<SeedFailure>,
}

impl<'a> Collector<'a> {
    fn origin(&self) -> String {
        sitemap::origin_of(self.opts.seed)
    }

    fn origin_url(&self) -> Url {
        Url::parse(&format!("{}/", self.origin())).unwrap_or_else(|_| self.opts.seed.clone())
    }

    fn remaining(&self) -> usize {
        self.opts.request_budget.saturating_sub(self.requests)
    }

    async fn get(&mut self, url: &str) -> Option<FetchedPage> {
        if self.remaining() == 0 {
            return None;
        }
        if Instant::now() >= self.opts.deadline {
            self.deadline_hit = true;
            return None;
        }
        self.requests += 1;
        let page = (self.fetch)(url.to_string(), "direct".to_string(), None, None).await;
        if page
            .error_hint
            .as_deref()
            .is_some_and(|e| e.contains("deadline"))
            || Instant::now() >= self.opts.deadline
        {
            self.deadline_hit = true;
        }
        Some(page)
    }

    /// Add one candidate. Returns true when it is a new inventory URL.
    fn add(
        &mut self,
        raw: &str,
        lastmod: Option<String>,
        source: &'static str,
        base: &Url,
    ) -> bool {
        if raw.is_empty() {
            return false;
        }
        let Ok(mut u) = base.join(raw) else {
            self.filtered.other += 1;
            return false;
        };
        u.set_fragment(None);
        if !matches!(u.scheme(), "http" | "https") {
            self.filtered.other += 1;
            return false;
        }
        let u = frontier::canonical_scheme(&u, self.opts.seed);
        if self.opts.same_host && !same_host(&u, self.opts.seed) {
            self.filtered.other += 1;
            return false;
        }
        if asset(u.path()) {
            self.filtered.assets += 1;
            return false;
        }
        if self.urls.len() >= self.opts.url_cap {
            self.cap_hit = true;
            return false;
        }
        let key = frontier::normalize(&u);
        match self.urls.get_mut(&key) {
            Some(existing) => {
                existing.lastmod = newer(existing.lastmod.take(), lastmod);
                false
            }
            None => {
                self.urls.insert(
                    key.clone(),
                    InvUrl {
                        url: key,
                        lastmod,
                        source: source.to_string(),
                    },
                );
                *self.sources.entry(source.to_string()).or_default() += 1;
                true
            }
        }
    }

    async fn llms(&mut self) {
        let url = format!("{}/llms.txt", self.origin());
        let Some(page) = self.get(&url).await else {
            return;
        };
        if page.status != 200 {
            return;
        }
        let text = text_of(&page);
        let base = self.origin_url();
        let mut page_list = None;
        let mut body_api = None;
        for (title, link) in md_links(&text) {
            let low = link.to_lowercase();
            let tl = title.to_lowercase();
            if tl.contains("page list") && page_list.is_none() {
                page_list = Some(link.clone());
            }
            if tl.contains("article body") && body_api.is_none() {
                body_api = Some(link.clone());
            }
            // API endpoints are contracts, not pages: they never enter the
            // inventory's page count.
            if low.contains("/pagelist") || low.contains("/article/body") {
                continue;
            }
            self.add(&link, None, "llms", &base);
        }
        if page_list.is_none() {
            for u in raw_urls(&text) {
                if u.to_lowercase().contains("/pagelist/") {
                    page_list = Some(u);
                    break;
                }
            }
        }
        if body_api.is_none() {
            for u in raw_urls(&text) {
                if u.to_lowercase().contains("/article/body") {
                    body_api = Some(u);
                    break;
                }
            }
        }
        if let Some(list) = page_list {
            self.apis.insert("pagelist".to_string(), list.clone());
            self.pagelist(&list).await;
        }
        if let Some(body) = body_api {
            self.apis.insert("body".to_string(), body);
        }
    }

    async fn pagelist(&mut self, url: &str) {
        let Some(page) = self.get(url).await else {
            return;
        };
        if page.status != 200 {
            return;
        }
        let text = text_of(&page);
        let paths = pagelist_paths(&text);
        if paths.is_empty() {
            return;
        }
        let base = self.origin_url();
        let mut added = 0usize;
        for p in paths {
            if self.add(&p, None, "pagelist", &base) {
                added += 1;
            }
        }
        if added > 0 {
            self.pagelist_found = true;
            self.pagelist_complete = true;
        }
    }

    async fn sitemaps(&mut self) {
        let origin = self.origin();
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut conventional: HashSet<String> = HashSet::new();
        if let Some(robots) = self.opts.robots {
            for s in &robots.sitemaps {
                queue.push_back(s.clone());
            }
        }
        if queue.is_empty() {
            // Two conventional probes are enough for the common shapes and
            // leave one request for the seed fallback on a sitemap-less site.
            for path in ["/sitemap.xml", "/sitemap_index.xml"] {
                let u = format!("{origin}{path}");
                conventional.insert(u.clone());
                queue.push_back(u);
            }
        }
        let mut visited: HashSet<String> = HashSet::new();
        let mut found = false;
        let mut saw_page = false;
        while let Some(loc) = queue.pop_front() {
            if self.remaining() == 0 || self.deadline_hit {
                break;
            }
            if !visited.insert(loc.clone()) {
                continue;
            }
            let Some(page) = self.get(&loc).await else {
                break;
            };
            if page.status != 200 {
                if !conventional.contains(&loc) {
                    self.sitemap_failed_child = true;
                }
                continue;
            }
            let text = text_of(&page);
            if !looks_sitemap(&text) {
                if !conventional.contains(&loc) {
                    self.sitemap_failed_child = true;
                }
                continue;
            }
            found = true;
            self.sitemap_found = true;
            let mut entries: Vec<SitemapEntry> = Vec::new();
            sitemap::parse_sitemap(&text, &mut entries, 200_000);
            if entries.is_empty() {
                // A plain-text sitemap: one URL per line.
                for line in text.lines() {
                    let l = line.trim();
                    if l.starts_with("http") && self.add(l, None, "sitemap", self.opts.seed) {
                        saw_page = true;
                    }
                }
            }
            for e in entries {
                if e.is_index {
                    if child_locale_ok(&e.loc, self.seed_locale().as_deref()) && queue.len() < 64 {
                        queue.push_back(e.loc);
                    }
                } else if self.add(&e.loc, e.lastmod, "sitemap", self.opts.seed) {
                    saw_page = true;
                }
            }
            if saw_page {
                // Once a sitemap answers, the alternative conventional probes
                // are not read: they are fallbacks, not more inventory.
                queue.retain(|u| !conventional.contains(u));
            }
        }
        self.sitemap_found = saw_page;
        self.sitemap_complete = found
            && saw_page
            && queue.is_empty()
            && !self.sitemap_failed_child
            && !self.deadline_hit
            && !self.cap_hit;
    }

    async fn seed_links(&mut self) {
        let page = match self.opts.seed_page {
            Some(p) => p.clone(),
            None => match self.get(self.opts.seed.as_str()).await {
                Some(p) => p,
                None => return,
            },
        };
        if !(200..300).contains(&page.status) || !matches!(page.verdict, Verdict::ContentOk) {
            if !self.deadline_hit {
                let failure =
                    SeedFailure::from_page(&page, None).settle(self.opts.requested_seed, true);
                self.seed_failure = Some(failure);
            }
            return;
        }
        let html = text_of(&page);
        let base = Url::parse(&page.url).unwrap_or_else(|_| self.opts.seed.clone());
        // Scraper's Html/Selector are not Send: extract every href inside a
        // synchronous block and drop the document before the feed await.
        let (links, feed_href) = {
            let doc = Html::parse_document(&html);
            let mut links = Vec::new();
            if let Ok(selector) = Selector::parse("a[href]") {
                for el in doc.select(&selector) {
                    if let Some(href) = el.value().attr("href") {
                        links.push(href.to_string());
                    }
                }
            }
            let feed_href = Selector::parse(
                "link[type=\"application/rss+xml\"], link[type=\"application/atom+xml\"]",
            )
            .ok()
            .and_then(|selector| {
                doc.select(&selector)
                    .next()
                    .and_then(|e| e.value().attr("href"))
                    .map(str::to_string)
            });
            (links, feed_href)
        };
        for href in links {
            self.add(&href, None, "seed-link", &base);
        }
        if self.remaining() > 0
            && let Some(href) = feed_href
            && let Ok(feed_url) = base.join(&href)
            && let Some(feed) = self.get(feed_url.as_str()).await
            && feed.status == 200
        {
            self.feed(&feed, &feed_url);
        }
    }

    fn feed(&mut self, page: &FetchedPage, base: &Url) {
        let text = text_of(page);
        let doc = Html::parse_document(&text);
        let Ok(selector) = Selector::parse("item link, entry link") else {
            return;
        };
        for el in doc.select(&selector) {
            if let Some(href) = el.value().attr("href") {
                self.add(href, None, "feed", base);
            } else {
                let text = el.text().collect::<String>();
                let link = text.trim();
                if link.starts_with("http") {
                    self.add(link, None, "feed", base);
                }
            }
        }
    }

    fn seed_locale(&self) -> Option<String> {
        let first = self.opts.seed.path().split('/').find(|s| !s.is_empty())?;
        locale_like(first).then(|| first.to_string())
    }

    fn finish(mut self) -> InventoryResult {
        let raw = self.urls.len();
        let seed_locale = self.seed_locale();
        let mut locale_owner: HashMap<String, String> = HashMap::new();
        let mut kept: BTreeMap<String, InvUrl> = BTreeMap::new();
        for (key, entry) in std::mem::take(&mut self.urls) {
            let Ok(u) = Url::parse(&key) else {
                self.filtered.other += 1;
                continue;
            };
            // One page per translation. The locale-canonical key collapses
            // /en/x, /de/x and an unprefixed twin; when the seed names a
            // locale, its variant is the one kept even if a sibling sorts
            // first.
            let lcanon = frontier::locale_canonical(&u, self.opts.seed);
            if let Some(owner) = locale_owner.get(&lcanon).cloned() {
                let better = seed_locale.as_deref().is_some_and(|loc| {
                    first_segment_is(&u, loc)
                        && Url::parse(&owner)
                            .map(|old| !first_segment_is(&old, loc))
                            .unwrap_or(false)
                });
                if better {
                    kept.remove(&owner);
                    kept.insert(key.clone(), entry);
                    locale_owner.insert(lcanon, key);
                } else {
                    self.filtered.locale += 1;
                }
                continue;
            }
            locale_owner.insert(lcanon, key.clone());
            if !frontier::scope_allowed(u.path(), self.opts.include, self.opts.exclude) {
                self.filtered.scope += 1;
                continue;
            }
            if self.opts.respect_robots
                && self
                    .opts
                    .robots
                    .is_some_and(|robots| !robots.allows_url(&u))
            {
                self.filtered.robots += 1;
                continue;
            }
            kept.insert(key, entry);
        }
        self.sources.clear();
        for entry in kept.values() {
            *self.sources.entry(entry.source.clone()).or_default() += 1;
        }
        let complete = if self.pagelist_found {
            self.pagelist_complete && !self.cap_hit && !self.deadline_hit
        } else {
            self.sitemap_complete && !self.cap_hit && !self.deadline_hit
        };
        let seed_links = self.sources.get("seed-link").copied().unwrap_or(0)
            + self.sources.get("feed").copied().unwrap_or(0);
        let sitemap_count = self.sources.get("sitemap").copied().unwrap_or(0);
        let complete = complete && seed_links == 0;
        let source = if self.pagelist_found {
            "pagelist"
        } else if seed_links > 0 && sitemap_count < 3 {
            "seed-links"
        } else if self.sitemap_found {
            "sitemap"
        } else if self.sources.contains_key("llms") {
            "llms"
        } else {
            "seed-links"
        };
        let urls: Vec<InvUrl> = kept.into_values().collect();
        let view = ranked(&urls, self.opts.focus, self.opts.seed_query);
        let view_len = view.len();
        let complete = complete && view_len <= self.opts.map_cap;
        let map: Vec<String> = view
            .into_iter()
            .take(self.opts.map_cap)
            .map(|u| u.url.clone())
            .collect();
        InventoryResult {
            urls,
            raw,
            map,
            sources: self.sources,
            requests: self.requests,
            complete,
            filtered: self.filtered,
            apis: self.apis,
            deadline_hit: self.deadline_hit,
            source,
            seed_failure: self.seed_failure,
        }
    }
}

fn ranked<'a>(
    urls: &'a [InvUrl],
    focus: Option<&str>,
    seed_query: Option<&str>,
) -> Vec<&'a InvUrl> {
    let query = focus.or(seed_query);
    let mut candidates: Vec<&InvUrl> = urls
        .iter()
        .filter(|u| match focus {
            Some(q) => Url::parse(&u.url)
                .map(|p| crate::crawl::score::focus_match("", p.path(), q))
                .unwrap_or(false),
            None => true,
        })
        .collect();
    if let Some(q) = query {
        let idf = crate::crawl::score::FocusIdf::from_paths(
            urls.iter()
                .filter_map(|u| Url::parse(&u.url).ok().map(|p| p.path().to_string())),
        );
        let mut scored: Vec<(f64, &InvUrl)> = candidates
            .into_iter()
            .map(|u| {
                let path = Url::parse(&u.url)
                    .map(|p| p.path().to_string())
                    .unwrap_or_default();
                let score =
                    crate::crawl::score::score_candidate_with_idf("", &path, Some(q), Some(&idf));
                (score, u)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.1.lastmod.cmp(&a.1.lastmod))
                .then_with(|| a.1.url.cmp(&b.1.url))
        });
        scored.into_iter().map(|(_, u)| u).collect()
    } else {
        candidates.sort_by(|a, b| b.lastmod.cmp(&a.lastmod).then_with(|| a.url.cmp(&b.url)));
        candidates
    }
}

/// A page URL is not an asset. PDFs stay: a crawl may read them.
fn asset(path: &str) -> bool {
    let p = path.to_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".ico", ".css", ".js", ".woff", ".woff2",
        ".ttf", ".mp4", ".mp3", ".zip", ".tar", ".gz",
    ]
    .iter()
    .any(|ext| p.ends_with(ext))
        || p.contains("/login")
        || p.contains("/logout")
        || p.contains("/signin")
}

fn same_host(a: &Url, b: &Url) -> bool {
    match (a.host_str(), b.host_str()) {
        (Some(ah), Some(bh)) => {
            ah.trim_start_matches("www.")
                .eq_ignore_ascii_case(bh.trim_start_matches("www."))
                && a.port_or_known_default() == b.port_or_known_default()
        }
        _ => false,
    }
}

fn locale_like(seg: &str) -> bool {
    let b = seg.as_bytes();
    (b.len() == 2 && b.iter().all(u8::is_ascii_alphabetic))
        || (b.len() == 5
            && b[..2].iter().all(u8::is_ascii_alphabetic)
            && b[2] == b'-'
            && b[3..].iter().all(u8::is_ascii_alphanumeric))
}

fn child_locale_ok(loc: &str, seed_locale: Option<&str>) -> bool {
    let Ok(u) = Url::parse(loc) else {
        return true;
    };
    let locales: Vec<&str> = u.path().split('/').filter(|s| locale_like(s)).collect();
    match seed_locale {
        Some(locale) => {
            locales.is_empty() || locales.iter().any(|l| l.eq_ignore_ascii_case(locale))
        }
        None => true,
    }
}

fn first_segment_is(u: &Url, locale: &str) -> bool {
    u.path()
        .split('/')
        .find(|s| !s.is_empty())
        .is_some_and(|s| s.eq_ignore_ascii_case(locale))
}

fn text_of(page: &FetchedPage) -> String {
    let bytes = if page.body.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        let mut decoder = flate2::read::GzDecoder::new(&page.body[..]);
        let _ = decoder.read_to_end(&mut out);
        out
    } else {
        page.body.clone()
    };
    String::from_utf8_lossy(&bytes)
        .trim_start_matches('\u{feff}')
        .to_string()
}

fn looks_sitemap(text: &str) -> bool {
    text.contains("<urlset")
        || text.contains("<sitemapindex")
        || text.contains("<url>")
        || text.contains("<sitemap>")
        || text
            .lines()
            .find(|l| !l.trim().is_empty())
            .is_some_and(|l| l.trim_start().starts_with("http"))
}

fn newer(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a >= b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Markdown links `[title](url)`: the shape `llms.txt` uses for its map.
fn md_links(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(open) = text[i..].find('[') {
        let open = i + open;
        let Some(close) = text[open..].find("](") else {
            break;
        };
        let close = open + close;
        let Some(end) = text[close + 2..].find(')') else {
            break;
        };
        let end = close + 2 + end;
        let title = text[open + 1..close].to_string();
        let url = text[close + 2..end]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches('"');
        if url.starts_with("http") {
            out.push((title, url.to_string()));
        }
        i = end + 1;
    }
    out
}

fn raw_urls(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || "()[]<>\"'`".contains(c))
        .filter(|s| s.starts_with("http"))
        .map(|s| s.trim_end_matches(['.', ',', ';', ':']).to_string())
        .collect()
}

/// A page-list answer: newline paths (`/en/docs`) or JSON strings carrying
/// paths. GitHub Docs publishes a newline list; another site may publish JSON.
fn pagelist_paths(text: &str) -> Vec<String> {
    let trimmed = text.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
    {
        let mut out = Vec::new();
        collect_paths(&value, &mut out);
        if !out.is_empty() {
            return out;
        }
    }
    trimmed
        .lines()
        .map(str::trim)
        .map(|l| l.trim_matches('"'))
        .filter(|l| l.starts_with('/') || l.starts_with("http"))
        .map(str::to_string)
        .collect()
}

fn collect_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) if s.starts_with('/') || s.starts_with("http") => {
            out.push(s.clone())
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_paths(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                collect_paths(item, out);
            }
        }
        _ => {}
    }
}
