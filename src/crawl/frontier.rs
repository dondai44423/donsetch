//! URL frontier: normalization, scoping, and the priority
//! queue that decides WHAT to fetch next.
//!
//! Normalization is where crawls live or die: `?utm_source=`
//! copies of every page kill token budgets. We strip tracking
//! params, fragments, and dedup on the canon form.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

use url::Url;

/// Query keys that never change page content. Stripped so the
/// same page reachable with 50 tracking variants dedups to 1.
const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "utm_id",
    "utm_reader",
    "utm_viz_id",
    "utm_pubreferrer",
    "utm_swu",
    "fbclid",
    "gclid",
    "gclsrc",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "twclid",
    "li_fat_id",
    "mc_cid",
    "mc_eid",
    "iref",
    "ref_src",
    "ref_url",
    "_ga",
    "_gl",
    "_hsenc",
    "_hsmi",
    "hsa_cam",
    "hsa_grp",
    "hsa_mt",
    "hsa_src",
    "hsa_ad",
    "hsa_acc",
    "hsa_net",
    "hsa_ver",
    "hsa_la",
    "hsa_ol",
    "hsa_kw",
    "igshid",
    "si",
    "spm",
    "scm",
    "bbid",
    "ocid",
    "oly_enc_id",
    "oly_anon_id",
    "vero_id",
    "wickedid",
    "wickedsource",
    "wt_mc",
    "yclid",
    "zanpid",
    "guccounter",
];

/// Lowercase schemes allowed in the crawl corpus.
fn web_scheme(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// Known locale codes that appear as the first path segment on
/// multi-language sites (MDN, Wikipedia, React docs, etc.).
/// When two URLs differ ONLY in this prefix, they are translations
/// of the same content : fetching both wastes crawl budget.
const LOCALE_PREFIXES: &[&str] = &[
    // ISO 639-1 two-letter codes
    "en", "de", "es", "fr", "ja", "ko", "ru", "it", "nl", "pl", "tr", "ar", "hi", "th", "vi", "id",
    "pt", "zh", "cs", "el", "he", "fa", "sv", "da", "fi", "no", "hu", "uk", "ro", "sk", "sl", "bg",
    "hr", "sr", "lt", "lv", "et", "ms", "bn", "ta", "te", "mr", "gu", "kn", "ml", "pa",
    // Common regional variants (BCP-47)
    "en-us", "en-gb", "en-au", "en-ca", "en-in", "en-nz", "en-za", "en-sg", "zh-cn", "zh-tw",
    "zh-hk", "zh-sg", "pt-br", "pt-pt", "fr-ca", "fr-fr", "es-es", "es-mx", "es-ar", "es-co",
    "es-cl", "de-at", "de-ch", "nl-be", "nl-nl", "sv-se", "da-dk", "nn-no", "nb-no", "fi-fi",
    "ru-ru", "pl-pl", "it-it", "tr-tr", "ar-sa", "ar-eg", "ko-kr", "ja-jp", "hi-in", "id-id",
    "vi-vn", "th-th", "ms-my",
];

/// Extract the locale prefix (if any) from a URL path and return
/// the locale-stripped canonical path. Returns (locale, rest).
///   `/en-US/docs/Web/JS`  → ("en-US", "/docs/Web/JS")
///   `/docs/Web/JS`         → (None,     "/docs/Web/JS")
///   `/api/v1`              → (None,     "/api/v1")
pub fn locale_split(path: &str) -> (Option<&str>, &str) {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let first = trimmed.split('/').next().unwrap_or("");
    if LOCALE_PREFIXES.contains(&first.to_lowercase().as_str()) {
        let rest = &trimmed[first.len()..];
        (Some(first), rest)
    } else {
        (None, path)
    }
}

/// Locale-canonical key for cross-locale dedup. Two URLs with
/// different locale prefixes but the same remainder are
/// translations of the same page.
pub fn locale_canonical(path: &str) -> String {
    let (_, rest) = locale_split(path);
    rest.to_lowercase()
}

/// Normalize a URL for frontier dedup.
///
/// - lowercase host, strip default ports
/// - drop fragment (never sent)
/// - strip tracking query params
/// - sort remaining query params for canon
/// - '/' for empty path
/// - trailing-slash collapse on non-root dirs is deliberately
///   NOT done (site-dependent meaning); dedup happens on hit.
pub fn normalize(url: &Url) -> String {
    let mut u = url.clone();
    u.set_fragment(None);
    let host = u.host_str().unwrap_or("").to_lowercase();
    let _ = u.set_host(Some(&host));
    // Explicit default ports normalize away.
    if (u.scheme() == "https" && u.port() == Some(443))
        || (u.scheme() == "http" && u.port() == Some(80))
    {
        let _ = u.set_port(None);
    }
    // Strip tracking params, sort the survivors.
    let pairs: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !TRACKING_PARAMS.contains(&k.to_lowercase().as_str()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut pairs = pairs;
    pairs.sort();
    if pairs.is_empty() {
        u.set_query(None);
    } else {
        let qs = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        u.set_query(Some(&qs));
    }
    if u.path().is_empty() {
        u.set_path("/");
    }
    u.to_string()
}

/// Resolve a possibly-relative link against the page URL.
/// Returns None for non-web schemes (mailto:, tel:, javascript:).
pub fn resolve(base: &Url, link: &str) -> Option<Url> {
    let joined = base.join(link).ok()?;
    if web_scheme(&joined) {
        Some(joined)
    } else {
        None
    }
}

/// One queued URL with its priority metadata.
#[derive(Clone, Debug)]
pub struct Frontier {
    pub url: String,
    pub score: f64,
    pub depth: u32,
    /// Consecutive transient-failure retries already spent.
    /// Network errors and 5xx requeue with this incremented;
    /// walls/404s are permanent skips and never retried.
    pub retries: u8,
    /// The URL that linked to this one (referer chain).
    /// None = seed / typed entry point.
    pub parent: Option<String>,
}

impl PartialEq for Frontier {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}
impl Eq for Frontier {}
impl PartialOrd for Frontier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Frontier {
    // Max-heap on score.
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.depth.cmp(&self.depth))
    }
}

/// Cross-check one URL against include/exclude path globs.
/// `None` patterns = wildcard pass. Globs: `*` = any run.
pub fn scope_allowed(path: &str, include: &[String], exclude: &[String]) -> bool {
    for pat in exclude {
        if glob_match(pat, path) {
            return false;
        }
    }
    if include.is_empty() {
        return true;
    }
    include.iter().any(|p| glob_match(p, path))
}

/// Glob match: `*` matches any run (including slashes),
/// everything else literal. `/docs/*` matches `/docs/a/b`.
pub fn glob_match(pat: &str, s: &str) -> bool {
    glob_at(pat.as_bytes(), s.as_bytes())
}

fn glob_at(pat: &[u8], s: &[u8]) -> bool {
    if pat.is_empty() {
        return s.is_empty();
    }
    match pat[0] {
        b'*' => {
            // '*' consumes zero or more.
            for skip in 0..=s.len() {
                if glob_at(&pat[1..], &s[skip..]) {
                    return true;
                }
            }
            false
        }
        c if !s.is_empty() && s[0] == c => glob_at(&pat[1..], &s[1..]),
        _ => false,
    }
}

/// The crawl frontier: queued URLs with per-URL priority.
pub struct FrontierQueue {
    heap: BinaryHeap<Frontier>,
    seen: HashSet<String>,
    /// v4 phase 3 crawl-shape: pop a reader-like jitter inside the
    /// head window instead of always the exact top, so repeated
    /// crawls of the same site do not replay an identical,
    /// score-eager fetch order to server logs. Ordering only.
    shaper: bool,
    /// xorshift64 state for the hop; a deliberate non-zero seed.
    rng: u64,
}

impl Default for FrontierQueue {
    fn default() -> Self {
        Self::new()
    }
}
impl FrontierQueue {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            seen: HashSet::new(),
            shaper: false,
            rng: 0x9e3779b97f4a7c15,
        }
    }

    /// Construct a shaped frontier (reader-like pop jitter, default) or an unshaped one. The
    /// unshaped (classic) heap behavior comes back when callers pass false - which is what
    /// DONSETCH_NO_CRAWL_SHAPE does - and for tests that need deterministic score ordering.
    /// The seed may be zero: xorshift non-zeroes it before use.
    pub fn with_shaper(shaper_enabled: bool, seed: u64) -> Self {
        Self {
            heap: BinaryHeap::new(),
            seen: HashSet::new(),
            shaper: shaper_enabled,
            rng: seed,
        }
    }

    /// Push a URL if its normalized form is new.
    pub fn push(&mut self, url: Url, score: f64, depth: u32) -> bool {
        self.push_with_parent(url, score, depth, None)
    }

    /// Push a URL with a referer parent (the page that linked to it).
    pub fn push_with_parent(
        &mut self,
        url: Url,
        score: f64,
        depth: u32,
        parent: Option<String>,
    ) -> bool {
        let key = normalize(&url);
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.heap.push(Frontier {
            url: key,
            score,
            depth,
            retries: 0,
            parent,
        });
        true
    }

    /// Requeue a popped item (e.g. host boxed, try later).
    /// NOT dedup'd : the seen-set already knows it.
    pub fn requeue(&mut self, f: Frontier) {
        self.heap.push(f);
    }

    /// Restore the seen-set from a resume state (run-1 fetches
    /// must not refetch in run 2).
    pub fn restore_seen(&mut self, urls: Vec<String>) {
        for u in urls {
            self.seen.insert(u);
        }
    }

    /// Push an entry the seen-set already recorded (resume).
    pub fn push_to_heap(
        &mut self,
        url: String,
        score: f64,
        depth: u32,
        retries: u8,
        parent: Option<String>,
    ) {
        self.heap.push(Frontier {
            url,
            score,
            depth,
            retries,
            parent,
        });
    }

    /// Full seen-set snapshot for resume persistence.
    pub fn seen_snapshot(&self) -> Vec<String> {
        self.seen.iter().cloned().collect()
    }

    pub fn pop(&mut self) -> Option<Frontier> {
        if !self.shaper || self.heap.len() < 2 {
            return self.heap.pop();
        }
        // Take a small head window, pick with weights decaying head-first
        // (top twice more likely than second, halving down), put the
        // un-picked back. Every promised item still gets fetched; only
        // the order changes.
        const WINDOW: usize = 4;
        const WEIGHTS: [u32; WINDOW] = [8, 4, 2, 1];
        let take = WINDOW.min(self.heap.len());
        let mut head: Vec<Frontier> = (0..take).filter_map(|_| self.heap.pop()).collect();
        let pick = {
            let mut x = self.rng;
            if x == 0 {
                x = 0x9e3779b97f4a7c15;
            }
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.rng = x;
            let total: u32 = WEIGHTS[..take].iter().sum();
            let mut r = (x % (total as u64)) as u32;
            let mut idx = take - 1;
            for (i, w) in WEIGHTS[..take].iter().enumerate() {
                if r < *w {
                    idx = i;
                    break;
                }
                r -= *w;
            }
            idx
        };
        let chosen = head.swap_remove(pick);
        for f in head {
            self.heap.push(f);
        }
        Some(chosen)
    }

    /// Snapshot all queued entries (url, score, depth, retries,
    /// parent) for a resume token. Does not drain : the seen-set
    /// survives.
    pub fn snapshot_entries(&self) -> Vec<(String, f64, u32, u8, Option<String>)> {
        self.heap
            .iter()
            .map(|f| (f.url.clone(), f.score, f.depth, f.retries, f.parent.clone()))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Mark a URL as seen without enqueuing it. Used for
    /// canonical URL resolution: when a page declares a
    /// canonical URL different from its fetched URL, the
    /// canonical form is marked seen to prevent a separate
    /// fetch of the same content under a different URL.
    pub fn mark_seen(&mut self, url: String) {
        self.seen.insert(url);
    }
}

/// Auto-derive a path scope from the seed URL's path. When
/// `include_paths` is empty, this provides a default scope that
/// keeps the crawl within the seed's section of the site.
///
/// Algorithm: the seed URL's path defines the scope boundary.
/// - Path ending with `/` (directory): use full path as prefix.
///   `/tokio/latest/tokio/` -> `/tokio/latest/tokio/*`
/// - Path not ending with `/` (page): use parent directory.
///   `/tokio-rs/tokio/wiki` -> `/tokio-rs/tokio/*`
/// - Single-segment path (e.g. `/tokio`): scope to that segment.
///   `/tokio` -> `/tokio/*` (NOT None : multi-tenant hosts like
///   docs.rs, crates.io, npmjs.com have each crate/package as
///   a top-level segment; returning None crawls the entire site).
/// - Root path `/`: no scope (crawl the whole host).
pub fn auto_scope(seed_path: &str) -> Option<String> {
    let path = if seed_path.starts_with('/') {
        seed_path.to_string()
    } else {
        format!("/{seed_path}")
    };

    if path == "/" || path.is_empty() {
        return None;
    }

    // Directory path: full path as prefix.
    if path.ends_with('/') {
        if path == "/" {
            return None;
        }
        return Some(format!("{path}*"));
    }

    // Page path: use parent directory.
    let prefix = match path.rfind('/') {
        Some(0) => {
            // Single-segment path like `/tokio` : scope to
            // `/tokio/*` instead of returning None. This is
            // critical for multi-tenant hosts (docs.rs,
            // crates.io, npmjs.com) where each top-level segment
            // is a different project.
            return Some(format!("{path}/*"));
        }
        Some(i) => path[..i + 1].to_string(),
        None => return None,
    };

    if prefix == "/" || prefix.is_empty() {
        return None;
    }

    Some(format!("{prefix}*"))
}

/// Common non-content path patterns. Safety net merged with
/// user exclude_paths. Auto-scope + focus filtering handle
/// most cases; these catch the obvious junk.
const JUNK_PATHS: &[&str] = &[
    "/login*",
    "/signin*",
    "/signup*",
    "/register*",
    "/auth*",
    "/oauth*",
    "/account*",
    "/settings*",
    "/cart*",
    "/checkout*",
    "/favicon*",
];

/// Build the effective exclude list: user excludes + default junk.
pub fn effective_excludes(user: &[String]) -> Vec<String> {
    let mut out: Vec<String> = user.to_vec();
    for j in JUNK_PATHS {
        if !out.iter().any(|u| u == j) {
            out.push(j.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_tracking() {
        let a = Url::parse("https://ex.com/page?utm_source=x&utm_medium=y&id=1").unwrap();
        let b = Url::parse("https://ex.com/page?id=1&fbclid=zzz").unwrap();
        assert_eq!(normalize(&a), normalize(&b));
    }

    #[test]
    fn normalize_strips_fragment_and_case() {
        let a = Url::parse("https://Ex.Com/Path#sec").unwrap();
        let b = Url::parse("https://ex.com/Path").unwrap();
        assert_eq!(normalize(&a), normalize(&b));
    }

    #[test]
    fn normalize_sorts_query() {
        let a = Url::parse("https://ex.com/p?b=2&a=1").unwrap();
        let b = Url::parse("https://ex.com/p?a=1&b=2").unwrap();
        assert_eq!(normalize(&a), normalize(&b));
    }

    #[test]
    fn normalize_default_port() {
        let a = Url::parse("https://ex.com:443/x").unwrap();
        let b = Url::parse("https://ex.com/x").unwrap();
        assert_eq!(normalize(&a), normalize(&b));
    }

    #[test]
    fn resolve_web_only() {
        let base = Url::parse("https://ex.com/a").unwrap();
        assert!(resolve(&base, "mailto:a@b.c").is_none());
        assert!(resolve(&base, "javascript:void(0)").is_none());
        assert!(resolve(&base, "/b").is_some());
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("/docs/*", "/docs/a/b"));
        assert!(!glob_match("/docs/*", "/blog/a"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*/x", "a/b/x"));
        assert!(!glob_match("*/x", "a/b/y"));
    }

    #[test]
    fn scope_include_exclude() {
        let inc = vec!["/docs/*".to_string()];
        let exc = vec!["*/admin*".to_string()];
        assert!(scope_allowed("/docs/guide", &inc, &exc));
        assert!(!scope_allowed("/other", &inc, &exc));
        assert!(!scope_allowed("/docs/admin/x", &inc, &exc));
        assert!(scope_allowed("/anything", &[], &[]));
    }

    #[test]
    fn queue_dedups() {
        let mut q = FrontierQueue::new();
        let u1 = Url::parse("https://ex.com/a?utm_source=x").unwrap();
        let u2 = Url::parse("https://ex.com/a?fbclid=y").unwrap();
        assert!(q.push(u1, 1.0, 0));
        assert!(!q.push(u2, 1.0, 0));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn queue_pops_max_score_first() {
        let mut q = FrontierQueue::new();
        let u1 = Url::parse("https://ex.com/a").unwrap();
        let u2 = Url::parse("https://ex.com/b").unwrap();
        q.push(u1, 1.0, 0);
        q.push(u2, 5.0, 0);
        assert!(q.pop().unwrap().url.ends_with("/b"));
    }

    fn seed_urls(n: usize) -> Vec<(Url, f64)> {
        (0..n)
            .map(|i| {
                (
                    Url::parse(&format!("https://ex.com/p{i}")).unwrap(),
                    10.0 - i as f64,
                )
            })
            .collect()
    }

    #[test]
    fn shaper_is_deterministic_per_seed() {
        // Same pushed list + same seed = same traversal order.
        let urls = seed_urls(6);
        let mut qa = FrontierQueue::with_shaper(true, 0x5eed);
        let mut qb = FrontierQueue::with_shaper(true, 0x5eed);
        for (u, s) in &urls {
            qa.push(u.clone(), *s, 0);
            qb.push(u.clone(), *s, 0);
        }
        for _ in 0..6 {
            assert_eq!(qa.pop().unwrap().url, qb.pop().unwrap().url);
        }
        assert!(qa.pop().is_none());
    }

    #[test]
    fn shape_kill_switch_path_matches_score_order() {
        // Explicit kill switch == shaper off == classic heap order.
        let urls = seed_urls(6);
        let mut q = FrontierQueue::with_shaper(false, 0x5eed);
        for (u, s) in &urls {
            q.push(u.clone(), *s, 0);
        }
        for i in 0..6 {
            assert!(
                q.pop().unwrap().url.ends_with(&format!("/p{i}")),
                "unshaped pop {i}"
            );
        }
    }

    #[test]
    fn shaper_is_reader_biased_not_uniform() {
        // The head window reordering is not a lottery: the exact top
        // element is still picked a majority of the time across seeds.
        // (Weights: 8/4/2/1 over the window.)
        let urls = seed_urls(4);
        let mut top = 0;
        let mut second = 0;
        for b in 1u64..41 {
            let mut q = FrontierQueue::with_shaper(true, b * 0x9e37);
            for (u, s) in &urls {
                q.push(u.clone(), *s, 0);
            }
            let got = q.pop().unwrap().url.to_string();
            if got.ends_with("/p0") {
                top += 1;
            } else if got.ends_with("/p1") {
                second += 1;
            }
        }
        assert!(top > second + 4, "top {top} vs second {second}");
    }

    #[test]
    fn shaper_never_loses_items() {
        // Requeue mechanics: every pushed URL still comes out exactly once.
        let urls = seed_urls(9);
        let mut q = FrontierQueue::with_shaper(true, 0x123456789);
        for (u, s) in &urls {
            q.push(u.clone(), *s, 0);
        }
        let mut got = Vec::new();
        while let Some(f) = q.pop() {
            got.push(f.url.to_string());
        }
        assert_eq!(got.len(), 9);
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 9);
    }

    #[test]
    fn locale_split_detects_known_locale() {
        assert_eq!(
            locale_split("/en-US/docs/Web/JS"),
            (Some("en-US"), "/docs/Web/JS")
        );
        assert_eq!(
            locale_split("/de/docs/Web/JS"),
            (Some("de"), "/docs/Web/JS")
        );
        assert_eq!(
            locale_split("/zh-CN/docs/Web/JS"),
            (Some("zh-CN"), "/docs/Web/JS")
        );
    }

    #[test]
    fn locale_split_none_for_non_locale() {
        assert_eq!(locale_split("/api/v1/users"), (None, "/api/v1/users"));
        assert_eq!(locale_split("/docs/guide"), (None, "/docs/guide"));
        assert_eq!(locale_split("/"), (None, "/"));
    }

    #[test]
    fn locale_canonical_strips_locale_prefix() {
        assert_eq!(locale_canonical("/en-US/docs/Web/JS"), "/docs/web/js");
        assert_eq!(locale_canonical("/de/docs/Web/JS"), "/docs/web/js");
        assert_eq!(locale_canonical("/docs/Web/JS"), "/docs/web/js");
        assert_eq!(locale_canonical("/api/v1"), "/api/v1");
    }

    #[test]
    fn locale_canonical_makes_translations_dedup() {
        let en = locale_canonical("/en-US/docs/Web/JavaScript/Array/map");
        let de = locale_canonical("/de/docs/Web/JavaScript/Array/map");
        let fr = locale_canonical("/fr/docs/Web/JavaScript/Array/map");
        assert_eq!(en, de);
        assert_eq!(en, fr);
    }

    #[test]
    fn auto_scope_directory_path() {
        assert_eq!(
            auto_scope("/tokio/latest/tokio/"),
            Some("/tokio/latest/tokio/*".into())
        );
        assert_eq!(auto_scope("/3/tutorial/"), Some("/3/tutorial/*".into()));
    }

    #[test]
    fn auto_scope_page_path_uses_parent() {
        assert_eq!(
            auto_scope("/tokio-rs/tokio/wiki"),
            Some("/tokio-rs/tokio/*".into())
        );
        assert_eq!(auto_scope("/docs/payments"), Some("/docs/*".into()));
    }

    #[test]
    fn auto_scope_root_is_none() {
        assert_eq!(auto_scope("/"), None);
        assert_eq!(auto_scope(""), None);
    }

    #[test]
    fn auto_scope_single_segment_is_scope() {
        // /tokio on docs.rs -> /tokio/* (NOT None : multi-tenant host)
        assert_eq!(auto_scope("/tokio"), Some("/tokio/*".into()));
        assert_eq!(auto_scope("/learn"), Some("/learn/*".into()));
        assert_eq!(auto_scope("/blog"), Some("/blog/*".into()));
    }

    #[test]
    fn effective_excludes_merges_junk() {
        let user = vec!["/api/*".to_string()];
        let eff = effective_excludes(&user);
        assert!(eff.contains(&"/api/*".to_string()));
        assert!(eff.contains(&"/login*".to_string()));
        assert!(eff.contains(&"/cart*".to_string()));
    }
}
