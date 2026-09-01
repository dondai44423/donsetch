//! The crawl orchestrator. Owns: frontier loop, worker pool,
//! budgets, stop conditions, near-dup detection, resume tokens.
//!
//! Fetch I/O goes through the `PageFetcher` trait — the real
//! crawl rides DonShadow; tests ride a mock. Never does the
//! orchestrator touch sockets directly.

pub mod frontier;
pub mod governor;
pub mod real;
pub mod score;
pub mod sitemap;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use url::Url;

use crate::detect::walls::Verdict;
use crate::extract::{self, ContentKind, ExtractOptions};

use frontier::{FrontierQueue, scope_allowed};
use governor::Governor;
use sitemap::SitemapEntry;

/// A fetched page as the orchestrator sees it.
pub struct FetchedPage {
    /// Final URL after redirects.
    pub url: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub verdict: Verdict,
    pub latency: Duration,
    /// True when served from the revalidation/pool cache — these
    /// are FREE and must not count against pacing budgets.
    pub cached: bool,
    /// Human-readable failure note (network error, etc.).
    pub error_hint: Option<String>,
}

/// Ghost-rendered page from tier-2 browser escalation.
/// The orchestrator stays ghost-agnostic — this is the
/// payload the injected `GhostHook` returns.
pub struct GhostRender {
    pub html: String,
}

/// Injected ghost escalation hook. Takes a URL, returns
/// rendered HTML + cookies on success, Err(reason) on failure
/// (captcha, timeout, launch error) — the reason flows into the
/// crawl's skipped[] so the agent sees WHY the browser tier
/// declined, not just that it did. The orchestrator calls it
/// when a page is a JS shell (thin extraction) or a bot wall
/// (Challenge verdict). Capped at 3 per crawl.
pub type GhostHook =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<GhostRender, String>> + Send + Sync>;

/// Pluggable fetch: real = DonShadow, tests = in-memory map.
pub type PageFetcher =
    Arc<dyn Fn(String, String, Option<String>) -> BoxFuture<'static, FetchedPage> + Send + Sync>;
//            (url, lane_id, referer) -> page

/// Crawl surface mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrawlMode {
    /// Both phases (default): sitemap-map first, then content.
    Full,
    /// URL map only — cheap, extractable for agent decisions.
    Map,
    /// Skip the sitemap phase, crawl links BFS-style only.
    Content,
}

/// One harvested crawl page.
pub struct CrawlPage {
    pub url: String,
    pub title: String,
    pub kind: ContentKind,
    pub markdown: String,
    pub chars: usize,
    pub quality: f32,
    /// Same-content duplicate of an already-kept page.
    pub duplicate: bool,
    /// The URL that linked to this page (referer chain).
    /// None = seed / typed entry point.
    pub parent: Option<String>,
    /// Frontier relevance score (from focus scoring + sitemap priority).
    pub score: f64,
    /// Sitemap `<lastmod>` if available (ISO 8601 date string).
    pub lastmod: Option<String>,
}

/// Why the crawl stopped. Agents MUST see this to decide
/// whether to resume or re-scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Frontier exhausted — the whole reachable scope was read.
    FrontierEmpty,
    MaxPages,
    CharBudget,
    DepthLimit,
    Deadline,
    /// Client cancelled (MCP notifications/cancelled) — workers
    /// stopped gracefully, resume token persisted.
    Cancelled,
    /// Host boxed us out (all lanes walled) — resume later.
    ThrottledOut,
}

pub struct CrawlResult {
    pub seed: String,
    pub pages: Vec<CrawlPage>,
    /// URLs discovered but not fetched (budget/depth/scope).
    pub queued: Vec<String>,
    /// URLs skipped by robots/scope rules.
    pub filtered_out: usize,
    /// URLs fetched but skipped (wall/dup/error), with reason.
    pub skipped: Vec<(String, String)>,
    pub stop: StopReason,
    pub elapsed: Duration,
    /// Sitemap map (Map phase) — capped URLs.
    pub map: Vec<String>,
    /// robots.txt Crawl-delay honored by pacing (seconds). Surfaces
    /// in the output so a slow crawl explains itself.
    pub crawl_delay: Option<f64>,
    /// Resume token when stopped early, for `resume=`.
    pub resume: Option<String>,
}

/// v3: (done, queued) — fired per completed page, throttled by the caller.
pub type ProgressFn = std::sync::Arc<dyn Fn(usize, usize) + Send + Sync>;
/// v3: true = skip the URL entirely (recorded fingerprint still fresh).
pub type SkipFn = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;
/// v3: (url, fingerprint, markdown, title) — the delta-crawl memory feed.
pub type OnPageFn = std::sync::Arc<dyn Fn(&str, Option<&str>, &str, Option<&str>) + Send + Sync>;

#[derive(Clone)]
pub struct CrawlOptions {
    pub focus: Option<String>,
    pub mode: CrawlMode,
    /// Pages to fetch+extract beyond the seed.
    pub max_pages: usize,
    pub max_depth: u32,
    /// Sum of extracted chars across ALL pages.
    pub max_total_chars: usize,
    /// DonSift max_chars per page.
    pub per_page_max: usize,
    /// Path globs: only crawl these (empty = all).
    pub include_paths: Vec<String>,
    /// Path globs: never crawl these.
    pub exclude_paths: Vec<String>,
    /// Restrict to seed's host (default true).
    pub same_host: bool,
    /// Hard crawl deadline; partial results returned after.
    pub deadline: Duration,
    /// Worker concurrency (same host). 1 = pure polite serial.
    pub concurrency: usize,
    /// Obey robots.txt Disallow rules.
    pub respect_robots: bool,
    /// v3: cancellation — the client aborted the request. Workers
    /// observe this between pages and stop gracefully.
    pub cancel: Option<tokio::sync::watch::Receiver<bool>>,
    /// v3: progress callback (done, queued) — fired per completed
    /// page, throttled by the caller.
    pub progress: Option<ProgressFn>,
    /// v3: delta crawl — URLs for which this returns true are
    /// skipped entirely (recorded fingerprint still fresh).
    pub skip_unchanged: Option<SkipFn>,
    /// v3: record a fetched page's fingerprint (url, fingerprint,
    /// markdown, title) — the delta-crawl memory feed.
    pub on_page: Option<OnPageFn>,
    /// Map hard cap.
    pub map_cap: usize,
    /// Minimum content quality (0.0-1.0). Pages below this
    /// are skipped (still counted against page budget).
    pub min_quality: f32,
}

impl Default for CrawlOptions {
    fn default() -> Self {
        Self {
            focus: None,
            mode: CrawlMode::Full,
            max_pages: 10,
            max_depth: 2,
            max_total_chars: 60_000,
            per_page_max: 8_000,
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            same_host: true,
            deadline: Duration::from_secs(120),
            concurrency: 1,
            respect_robots: true,
            cancel: None,
            progress: None,
            skip_unchanged: None,
            on_page: None,
            map_cap: 120,
            min_quality: 0.05,
        }
    }
}

/// State carried in a resume token.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct ResumeState {
    seed: String,
    /// (url, score, depth, retries, parent)
    queue: Vec<(String, f64, u32, u8, Option<String>)>,
    /// Seen-set from run 1 — without it, run-2 pages re-link
    /// to already-fetched pages and they crawl AGAIN.
    seen: Vec<String>,
}

/// Disk-backed resume store: tokens survive process restarts,
/// so both the MCP daemon AND one-shot CLI runs can continue a
/// crawl. ~/.cache/donsetch/crawl-resumes.json, 30-min TTL.
fn resumes_path() -> std::path::PathBuf {
    dirs_cache().join("crawl-resumes.json")
}

fn dirs_cache() -> std::path::PathBuf {
    crate::paths::cache_dir()
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ResumeFile {
    /// token -> (state, issued_at_unix)
    entries: std::collections::HashMap<String, (ResumeState, u64)>,
}

impl ResumeFile {
    fn load() -> Self {
        std::fs::read_to_string(resumes_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let p = resumes_path();
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = serde_json::to_string(self) {
            // Atomic write: write to temp file, then rename.
            // Without this, a concurrent process reading the file
            // mid-write gets partial JSON → empty store → "token
            // expired or unknown" on a freshly-issued token.
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }

    fn sweep(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.entries
            .retain(|_, (_, at)| now.saturating_sub(*at) < 120 * 60);
    }
}

pub struct Crawler {
    fetch: PageFetcher,
    governor: Arc<Governor>,
    ghost: Option<GhostHook>,
    token_seq: AtomicUsize,
}

impl Crawler {
    pub fn new(fetch: PageFetcher, governor: Arc<Governor>) -> Self {
        Self {
            fetch,
            governor,
            ghost: None,
            token_seq: AtomicUsize::new(0),
        }
    }

    /// Attach a ghost escalation hook for JS-only pages.
    /// When a page's extraction is thin (JS shell) or its
    /// verdict is Challenge (bot wall), the orchestrator calls
    /// the hook to render the page in a headless browser.
    pub fn with_ghost(mut self, ghost: GhostHook) -> Self {
        self.ghost = Some(ghost);
        self
    }

    /// Run one crawl. Returns when a stop condition hits.
    #[allow(clippy::field_reassign_with_default)]
    pub async fn crawl(
        &self,
        seed: &str,
        opts: CrawlOptions,
        resume_token: Option<&str>,
    ) -> Result<CrawlResult, String> {
        let started = Instant::now();

        // Resume without url: load the seed from the resume state.
        let (seed, seed_url, seed_host) = if seed.is_empty() {
            let mut store = ResumeFile::load();
            store.sweep();
            let tok = resume_token.ok_or("resume token required when url is empty")?;
            let state = store
                .entries
                .get(tok)
                .map(|(s, _)| s.clone())
                .ok_or(format!("resume token expired or unknown: {tok}"))?;
            let u = Url::parse(&state.seed)
                .map_err(|_| format!("bad seed in resume state: {}", state.seed))?;
            let h = u.host_str().ok_or("seed must have a host")?.to_string();
            (state.seed.clone(), u, h)
        } else {
            let u = Url::parse(seed).map_err(|_| format!("bad seed url: {seed}"))?;
            let h = u.host_str().ok_or("seed must have a host")?.to_string();
            (seed.to_string(), u, h)
        };

        let host_ok = {
            let sh = seed_host.clone();
            let same = opts.same_host;
            move |u: &Url| !same || u.host_str().map(|h| host_matches(h, &sh)).unwrap_or(false)
        };

        // ── Auto-scope: derive path prefix from seed URL ──
        // When include_paths is empty, auto-derive a scope from
        // the seed URL's path. This keeps the crawl within the
        // seed's section on multi-tenant sites (docs.rs,
        // github.com) and multi-section sites (stripe.com).
        // Also merge default junk-path excludes (login, cart,
        // etc.) as a safety net.
        let mut opts = opts;
        if opts.include_paths.is_empty()
            && let Some(scope) = frontier::auto_scope(seed_url.path())
        {
            opts.include_paths = vec![scope];
        }
        opts.exclude_paths = frontier::effective_excludes(&opts.exclude_paths);

        // ── Phase 1: the map ───────────────────────────────
        let mut map: Vec<String> = Vec::new();
        let mut sitemap_entries: Vec<SitemapEntry> = Vec::new();
        let mut robots = sitemap::Robots::default();
        if opts.mode != CrawlMode::Content {
            let (mut r, mut entries) =
                sitemap::discover(&self.fetch, &seed_host, opts.map_cap * 4).await;
            robots = sitemap::Robots::default();
            std::mem::swap(&mut robots, &mut r);
            // Newest first: lastmod-sorted sitemaps put fresh
            // content at the front of the crawl budget.
            entries.sort_by(|a, b| b.lastmod.cmp(&a.lastmod));
            sitemap_entries = entries;
            let mut map_locales: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for e in &sitemap_entries {
                if map.len() >= opts.map_cap {
                    break;
                }
                if let Ok(u) = Url::parse(&e.loc) {
                    if !host_ok(&u) {
                        continue;
                    }
                    if !scope_allowed(u.path(), &opts.include_paths, &opts.exclude_paths) {
                        continue;
                    }
                    if let Some(q) = &opts.focus
                        && !score::focus_match("", u.path(), q)
                    {
                        continue;
                    }
                    // Dedup translated variants in the map.
                    let lcanon = frontier::locale_canonical(u.path());
                    if !map_locales.insert(lcanon) {
                        continue;
                    }
                    map.push(e.loc.clone());
                }
            }
        } else {
            // Content-only still reads robots for Disallow
            // rules when respect_robots is on.
            if opts.respect_robots {
                let (r, _) = sitemap::discover(&self.fetch, &seed_host, 0).await;
                robots = r;
            }
        }
        if opts.respect_robots {
            self.governor.set_crawl_delay(robots.crawl_delay);
        }
        if opts.mode == CrawlMode::Map {
            // Map-only crawl: cheap exit. Guide the agent when no
            // sitemap was found.
            let skipped = if map.is_empty() {
                vec![(
                    seed.to_string(),
                    "no sitemap found at common locations — use mode=content to BFS from the seed"
                        .into(),
                )]
            } else {
                Vec::new()
            };
            return Ok(CrawlResult {
                crawl_delay: robots.crawl_delay,
                seed: seed.to_string(),
                pages: Vec::new(),
                queued: Vec::new(),
                filtered_out: 0,
                skipped,
                stop: StopReason::FrontierEmpty,
                elapsed: started.elapsed(),
                map,
                resume: None,
            });
        }

        // ── Frontier seeding ───────────────────────────────
        let mut queue = FrontierQueue::new();
        // Budgets are PER-CALL: a resume continues from the saved
        // position but the caller's page/char budgets apply to
        // the NEW work. (Run 2 must not instantly exhaust itself
        // against run 1's spend.)
        let fetched_pages = 0usize;
        let total_chars = 0usize;
        let seed_norm = frontier::normalize(&seed_url);
        if let Some(tok) = resume_token {
            let mut store = ResumeFile::load();
            store.sweep();
            if let Some((state, _)) = store.entries.remove(tok) {
                queue.restore_seen(state.seen);
                for (u, s, d, r, p) in state.queue {
                    queue.push_to_heap(u, s, d, r, p);
                }
                store.save();
            } else {
                return Err(format!("resume token expired or unknown: {tok}"));
            }
        }
        // Site-wide IDF for BM25-lite frontier scoring: built from
        // the sitemap inventory when one exists; None otherwise.
        let mut focus_idf: Option<std::sync::Arc<score::FocusIdf>> = None;
        if resume_token.is_none() {
            let _ = queue.push(seed_url.clone(), 10.0, 0);
            // Sitemap entries seed frontier at depth 1.
            // Dedup by locale-canonical path: don't queue
            // multiple language variants of the same page
            // (de/, es/, fr/ copies waste crawl budget).
            let mut seeded_locales: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            // Build the IDF table from the full inventory BEFORE
            // scoring the seeds: document frequency is a corpus
            // property, not a per-entry one.
            if opts.focus.is_some() && !sitemap_entries.is_empty() {
                focus_idf = Some(std::sync::Arc::new(score::FocusIdf::from_paths(
                    sitemap_entries.iter().filter_map(|e| {
                        Url::parse(&e.loc)
                            .ok()
                            .filter(|u| host_ok(u))
                            .map(|u| u.path().to_string())
                    }),
                )));
            }
            for e in &sitemap_entries {
                if let Ok(u) = Url::parse(&e.loc)
                    && host_ok(&u)
                    && scope_allowed(u.path(), &opts.include_paths, &opts.exclude_paths)
                    && (!opts.respect_robots || robots.allowed(u.path()))
                {
                    // Focus gate: skip sitemap entries that don't
                    // match the focus query (if set).
                    if let Some(q) = &opts.focus
                        && !score::focus_match("", u.path(), q)
                    {
                        continue;
                    }
                    let lcanon = frontier::locale_canonical(u.path());
                    if !seeded_locales.insert(lcanon) {
                        continue; // Another variant already queued
                    }
                    let s = score::score_candidate_with_idf(
                        "",
                        u.path(),
                        opts.focus.as_deref(),
                        focus_idf.as_deref(),
                    ) + e.priority.unwrap_or(0.0) as f64 * 2.0;
                    queue.push(u, s, 1);
                }
            }
        }

        if queue.is_empty() {
            return Ok(CrawlResult {
                crawl_delay: robots.crawl_delay,
                seed: seed.to_string(),
                pages: Vec::new(),
                queued: Vec::new(),
                filtered_out: 0,
                skipped: vec![(seed.to_string(), "empty frontier (all filtered)".into())],
                stop: StopReason::FrontierEmpty,
                elapsed: started.elapsed(),
                map,
                resume: None,
            });
        }

        // ── Phase 2: page loop ─────────────────────────────
        let sh_queue = Arc::new(Mutex::new(queue));
        let pages: Arc<Mutex<Vec<CrawlPage>>> = Arc::new(Mutex::new(Vec::new()));
        let skipped: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let filtered_out = Arc::new(AtomicUsize::new(0));
        let dup_sigs: Arc<Mutex<HashSet<u64>>> = Arc::new(Mutex::new(HashSet::new()));
        // Locale-canonical paths of fetched pages — prevents
        // crawling translated variants (de/, es/, fr/ copies).
        let locale_seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let chars_total = Arc::new(AtomicUsize::new(total_chars));
        let pages_done = Arc::new(AtomicUsize::new(fetched_pages));
        // Safety valve: counts ALL fetches (incl low-quality,
        // duplicates, out-of-scope). Prevents infinite loops on
        // sites full of junk when low-quality pages don't count
        // against the quality budget.
        let total_fetched = Arc::new(AtomicUsize::new(0));
        let stop_flag: Arc<Mutex<Option<StopReason>>> = Arc::new(Mutex::new(None));
        let deadline_at = started + opts.deadline;
        let focus = Arc::new(opts.focus.clone());

        let workers = opts.concurrency.max(1);
        let ghost_budget = Arc::new(AtomicUsize::new(3));
        let mut handles = Vec::new();
        for wid in 0..workers {
            let queue = Arc::clone(&sh_queue);
            let pages = Arc::clone(&pages);
            let skipped = Arc::clone(&skipped);
            let filtered_out = Arc::clone(&filtered_out);
            let dup_sigs = Arc::clone(&dup_sigs);
            let locale_seen = Arc::clone(&locale_seen);
            let chars_total = Arc::clone(&chars_total);
            let pages_done = Arc::clone(&pages_done);
            let total_fetched = Arc::clone(&total_fetched);
            let stop_flag = Arc::clone(&stop_flag);
            let focus = Arc::clone(&focus);
            let focus_idf = focus_idf.clone();
            let fetch = self.fetch.clone();
            let governor = Arc::clone(&self.governor);
            let ghost_hook = self.ghost.clone();
            let ghost_budget = Arc::clone(&ghost_budget);
            let opts_worker = opts.clone();
            let seed_host2 = seed_host.clone();
            let seed_norm_w = seed_norm.clone();
            let robots = robots.clone();
            let max_pages = opts.max_pages;
            // Sitemap found ⇒ link discovery does not depend on the
            // seed fetch ⇒ even the seed is skippable in delta mode.
            let sitemap_found = !sitemap_entries.is_empty();
            let max_total = opts.max_total_chars;
            let max_depth = opts.max_depth;

            handles.push(tokio::spawn(async move {
                let mut seq = wid as u64 * 1000;

                'work: loop {
                    // ── Stop conditions ──
                    if Instant::now() >= deadline_at {
                        let mut s = stop_flag
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if s.is_none() {
                            *s = Some(StopReason::Deadline);
                        }
                        break 'work;
                    }
                    // v3: client cancellation — graceful stop; the
                    // resume checkpoint keeps everything gathered.
                    if let Some(rx) = &opts_worker.cancel
                        && (*rx.borrow() || rx.has_changed().unwrap_or(false))
                    {
                        let mut s = stop_flag
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if s.is_none() {
                            *s = Some(StopReason::Cancelled);
                        }
                        break 'work;
                    }
                    if stop_flag
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some()
                    {
                        break 'work;
                    }
                    if pages_done.load(Ordering::SeqCst) >= max_pages {
                        let mut s = stop_flag
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if s.is_none() {
                            *s = Some(StopReason::MaxPages);
                        }
                        break 'work;
                    }
                    if chars_total.load(Ordering::SeqCst) >= max_total {
                        let mut s = stop_flag
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if s.is_none() {
                            *s = Some(StopReason::CharBudget);
                        }
                        break 'work;
                    }
                    // Safety valve: stop if we've fetched 3x max_pages
                    // without finding enough quality content. Prevents
                    // infinite loops on sites full of low-quality pages.
                    if total_fetched.load(Ordering::SeqCst) >= max_pages * 3 {
                        let mut s = stop_flag
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if s.is_none() {
                            *s = Some(StopReason::MaxPages);
                        }
                        break 'work;
                    }

                    // ── Pop next ──
                    let next = queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pop();
                    let Some(item) = next else {
                        // Frontier empty — but other workers may add.
                        // Grace: spin briefly, then exit.
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        if queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_empty()
                        {
                            let mut s = stop_flag
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if s.is_none() {
                                *s = Some(StopReason::FrontierEmpty);
                            }
                            break 'work;
                        }
                        continue 'work;
                    };

                    let parsed = match Url::parse(&item.url) {
                        Ok(u) => u,
                        Err(_) => {
                            skipped
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push((item.url.clone(), "unparseable".into()));
                            continue 'work;
                        }
                    };
                    if item.depth > max_depth {
                        let mut s = stop_flag
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if s.is_none() {
                            *s = Some(StopReason::DepthLimit);
                        }
                        break 'work;
                    }
                    let host = parsed.host_str().unwrap_or("");
                    if opts_worker.same_host && !host_matches(host, &seed_host2) {
                        filtered_out.fetch_add(1, Ordering::Relaxed);
                        continue 'work;
                    }
                    // The seed is always fetched (entry point for
                    // link discovery) but its content is scope-gated
                    // post-extraction. Non-seed URLs are filtered here.
                    // v3 delta crawl: skip pages with a fresh recorded
                    // fingerprint. Counted as skipped, not fetched.
                    if let Some(should_skip) = &opts_worker.skip_unchanged
                        && (item.url != seed_norm_w || sitemap_found)
                        && should_skip(&item.url)
                    {
                        skipped
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((item.url.clone(), "unchanged (since_last)".into()));
                        continue 'work;
                    }
                    let is_seed = item.url == seed_norm_w;
                    if !is_seed
                        && !scope_allowed(
                            parsed.path(),
                            &opts_worker.include_paths,
                            &opts_worker.exclude_paths,
                        )
                    {
                        filtered_out.fetch_add(1, Ordering::Relaxed);
                        continue 'work;
                    }
                    if opts_worker.respect_robots && !robots.allowed(parsed.path()) {
                        filtered_out.fetch_add(1, Ordering::Relaxed);
                        continue 'work;
                    }
                    // Locale dedup: skip translated variants of
                    // already-fetched pages. The first variant
                    // fetched claims the canonical path; all other
                    // language versions (de/, es/, fr/, zh-CN/...)
                    // are blocked. The seed is exempt — it's the
                    // entry point and always fetched.
                    if !is_seed {
                        let lcanon = frontier::locale_canonical(parsed.path());
                        if locale_seen
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .contains(&lcanon)
                        {
                            filtered_out.fetch_add(1, Ordering::Relaxed);
                            continue 'work;
                        }
                    }

                    // ── Governor-paced fetch ──
                    // Workers aren't lane-pinned: whoever's
                    // least-blocked for this host takes it.
                    let Some(lane) = governor.best_lane(host).cloned() else {
                        if governor.wait_for(host, "*", seq) > Duration::ZERO {
                            // Whole host boxed — if the frontier
                            // holds only this host, we're done.
                            if queue
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .is_empty()
                            {
                                let mut s = stop_flag
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                if s.is_none() {
                                    *s = Some(StopReason::ThrottledOut);
                                }
                                break 'work;
                            }
                            // Requeue and yield.
                            queue
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .requeue(frontier::Frontier {
                                    url: item.url.clone(),
                                    score: item.score,
                                    depth: item.depth,
                                    retries: item.retries,
                                    parent: item.parent.clone(),
                                });
                            tokio::time::sleep(Duration::from_millis(400)).await;
                            continue 'work;
                        }
                        continue 'work;
                    };
                    seq += 1;
                    let wait = governor.wait_for(host, &lane.id, seq);
                    // Cap wait inside remaining deadline.
                    let remain = deadline_at.saturating_duration_since(Instant::now());
                    let wait = wait.min(remain);
                    if wait > Duration::ZERO {
                        tokio::time::sleep(wait).await;
                    }

                    let page = fetch(item.url.clone(), lane.id.clone(), item.parent.clone()).await;
                    if page.cached {
                        // Warm-cache hit: free — no governor signal.
                    } else {
                        match (page.status, &page.verdict) {
                            (200, Verdict::ContentOk) => {
                                // Skim dwell: proportional to page size,
                                // capped at 300ms. v1 used up to 2s/page
                                // ("a human reads a 50KB article") — but
                                // an agent skims for extraction, not
                                // reading, and the dwell's real job is
                                // anti-metronome entropy, which jitter +
                                // this small size-proportional term
                                // already provide. 2s/page of pure sleep
                                // was the single biggest crawl latency
                                // cost (6.29s median in the 50-case
                                // benchmark).
                                let dwell = (page.body.len() / 64).min(100) as u64;
                                governor.on_success(host, &lane.id, page.latency, dwell)
                            }
                            (429, _) | (503, _) => {
                                governor.on_throttled(host, &lane.id);
                            }
                            _ => governor.on_error(host, &lane.id),
                        }
                    }

                    // Transient retry: network errors (status 0) and
                    // 5xx non-challenge get requeued up to 2 times.
                    // Walls, 404s, 429s, and challenges are permanent.
                    let is_transient = page.status == 0
                        || (page.status >= 500 && !matches!(page.verdict, Verdict::Challenge(_)));
                    if is_transient && item.retries < 2 {
                        queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .requeue(frontier::Frontier {
                                url: item.url.clone(),
                                score: item.score * 0.8,
                                depth: item.depth,
                                retries: item.retries + 1,
                                parent: item.parent.clone(),
                            });
                        continue 'work;
                    }

                    // Ghost escalation for bot walls: if the page
                    // is a Challenge verdict and ghost is available,
                    // try rendering in the headless browser before
                    // skipping. Ghost may solve the challenge.
                    let mut ghost_html: Option<String> = None;
                    if matches!(page.verdict, Verdict::Challenge(_))
                        && let Some(ref ghost_hook) = ghost_hook
                    {
                        let remaining = deadline_at.saturating_duration_since(Instant::now());
                        if remaining > Duration::from_secs(25)
                            && ghost_budget.load(Ordering::SeqCst) > 0
                        {
                            ghost_budget.fetch_sub(1, Ordering::SeqCst);
                            match ghost_hook(item.url.clone()).await {
                                Ok(gp) => ghost_html = Some(gp.html),
                                Err(why) => {
                                    skipped
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push((
                                            item.url.clone(),
                                            format!("ghost escalation failed: {why}"),
                                        ));
                                }
                            }
                        }
                    }

                    // Wall/denylist verdicts → skip honestly.
                    // (Unless ghost rendered the page — then treat
                    // as ContentOk and proceed to extraction.)
                    if ghost_html.is_none() && !matches!(page.verdict, Verdict::ContentOk) {
                        let why = if item.retries > 0 {
                            format!(
                                "{} (failed after {} retries)",
                                page.error_hint
                                    .clone()
                                    .unwrap_or_else(|| format!("{:?}", page.verdict)),
                                item.retries
                            )
                        } else {
                            page.error_hint
                                .clone()
                                .unwrap_or_else(|| format!("{:?}", page.verdict))
                        };
                        skipped
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((item.url.clone(), why));
                        continue 'work;
                    }
                    // Count every successful fetch (safety valve
                    // against sites full of low-quality pages).
                    total_fetched.fetch_add(1, Ordering::SeqCst);

                    // ── Canonical URL resolution ──
                    // Extract <link rel="canonical" href="..."> to
                    // prevent double-fetching the same page under
                    // different URLs (trailing slash, index.html,
                    // tracking variants).
                    let page_url = page.url.clone();
                    if let Some(canon_href) =
                        extract_canonical(&String::from_utf8_lossy(&page.body))
                        && let Ok(canon_parsed) = Url::parse(&canon_href)
                    {
                        let canon_norm = frontier::normalize(&canon_parsed);
                        let fetched_norm = frontier::normalize(&parsed);
                        if canon_norm != fetched_norm {
                            // Mark the canonical form as seen so
                            // it won't be fetched separately.
                            queue
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .mark_seen(canon_norm.clone());
                            // Record the canonical as the page's
                            // true URL for output.
                            // (page_url is updated below.)
                        }
                    }

                    // ── Binary content guard ──
                    // Skip non-HTML (images, video, fonts, archives)
                    // and PDFs before feeding bytes to DonSift.
                    let ctype = page
                        .headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("text/html");
                    if ghost_html.is_none() && crate::fetch::guards::is_binary(&page.body, ctype) {
                        let kind = ctype.split(';').next().unwrap_or("unknown").trim();
                        skipped
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((item.url.clone(), format!("binary ({kind})")));
                        continue 'work;
                    }

                    // ── Extract with DonSift ──
                    let mut eo = ExtractOptions::default();
                    eo.focus = focus.as_ref().clone();
                    eo.max_chars = Some(opts_worker.per_page_max);
                    let body_bytes: &[u8] = ghost_html
                        .as_deref()
                        .map(|s| s.as_bytes())
                        .unwrap_or(&page.body);
                    let body_ctype = if ghost_html.is_some() {
                        crate::extract::charset::GHOST_TEXT_CT
                    } else {
                        ctype
                    };
                    // ── PDFium hang isolation ──
                    // `extract::extract` -> `pdf::parse` -> `engine::load_document`
                    // holds a global `Mutex<PdfiumCore>` and calls blocking
                    // `FPDF_RenderPageBitmap`. On aarch64 a malformed PDF can
                    // hang indefinitely while holding the lock, stalling the
                    // tokio worker thread. Offload PDF bodies to the blocking
                    // pool with a generous timeout (5 min) that covers large
                    // PDFs (a 28 MB archive.org PDF takes ~70s) while still
                    // preventing infinite hangs. HTML stays on the fast path.
                    let is_pdf = body_bytes.starts_with(b"%PDF-")
                        || body_ctype.to_ascii_lowercase().contains("pdf");
                    let extract_res: Result<
                        crate::extract::Extracted,
                        crate::extract::ExtractError,
                    > = if is_pdf {
                        if let Ok(handle) = tokio::runtime::Handle::try_current() {
                            let body_owned = body_bytes.to_vec();
                            let ctype_owned = body_ctype.to_owned();
                            let url_owned = page_url.clone();
                            let eo_owned = eo.clone();
                            let task = handle.spawn_blocking(move || {
                                extract::extract(&body_owned, &ctype_owned, &url_owned, &eo_owned)
                            });
                            match tokio::time::timeout(Duration::from_secs(300), task).await {
                                Ok(Ok(Ok(r))) => Ok(r),
                                Ok(Ok(Err(e))) => Err(e),
                                Ok(Err(join_err)) => {
                                    Err(crate::extract::ExtractError::BadSelector(format!(
                                        "extract task failed: {join_err}"
                                    )))
                                }
                                Err(_) => Err(crate::extract::ExtractError::BadSelector(
                                    "extract timed out after 300s".into(),
                                )),
                            }
                        } else {
                            extract::extract(body_bytes, body_ctype, &page_url, &eo)
                        }
                    } else {
                        extract::extract(body_bytes, body_ctype, &page_url, &eo)
                    };
                    let mut r = match extract_res {
                        Ok(r) => r,
                        Err(e) => {
                            skipped
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push((item.url.clone(), format!("extract failed: {e}")));
                            continue 'work;
                        }
                    };

                    // Ghost escalation for JS-only pages: if the
                    // extraction is thin (JS shell) and the page is
                    // large enough (> 5KB, not a 404), try rendering
                    // in the headless browser. Capped at 3 per crawl;
                    // requires 25s remaining deadline. Non-JS sites
                    // never hit this path.
                    if r.thin
                        && ghost_html.is_none()
                        && page.body.len() > 5_000
                        && let Some(ref ghost_hook) = ghost_hook
                    {
                        let remaining = deadline_at.saturating_duration_since(Instant::now());
                        if remaining > Duration::from_secs(25)
                            && ghost_budget.load(Ordering::SeqCst) > 0
                        {
                            ghost_budget.fetch_sub(1, Ordering::SeqCst);
                            match ghost_hook(item.url.clone()).await {
                                Ok(gp) => {
                                    if let Ok(r2) = extract::extract(
                                        gp.html.as_bytes(),
                                        crate::extract::charset::GHOST_TEXT_CT,
                                        &page_url,
                                        &eo,
                                    ) && !r2.thin
                                    {
                                        r = r2;
                                        ghost_html = Some(gp.html);
                                    }
                                }
                                Err(why) => {
                                    skipped
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push((
                                            item.url.clone(),
                                            format!("ghost escalation failed: {why}"),
                                        ));
                                }
                            }
                        }
                    }

                    let md = r.markdown;

                    // Claim the locale-canonical path: translated
                    // variants of this page (de/, es/, fr/, ...) are
                    // now blocked from the crawl budget.
                    {
                        let lcanon = frontier::locale_canonical(parsed.path());
                        locale_seen
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(lcanon);
                    }

                    // Near-dup signature: title + first 200 normalized
                    // chars of the CONTENT (frontmatter carries the
                    // page URL — identical docs at different URLs
                    // must still dedup).
                    let body_md = md.split_once("\n\n").map(|x| x.1).unwrap_or(md.as_str());
                    let sig_str = format!(
                        "{}|{}",
                        r.title.as_deref().unwrap_or("").trim().to_lowercase(),
                        body_md
                            .chars()
                            .filter(|c| !c.is_whitespace())
                            .take(200)
                            .collect::<String>()
                            .to_lowercase()
                    );
                    let sig = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        sig_str.hash(&mut h);
                        h.finish()
                    };
                    let duplicate = !dup_sigs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(sig);

                    // Quality gate: skip near-empty pages (boilerplate,
                    // redirects, error pages). Does NOT count against
                    // the page budget — low-quality pages should not
                    // steal slots from real content. The total_fetched
                    // safety valve (3x cap) prevents infinite loops.
                    if !duplicate && r.quality < opts_worker.min_quality {
                        skipped
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((page.url.clone(), format!("low quality ({:.2})", r.quality)));
                        continue 'work;
                    }

                    // Scope gate for results: the seed is ALWAYS
                    // in scope — the user explicitly asked to crawl it.
                    // Non-seed pages already passed scope before fetching.
                    let in_scope = is_seed
                        || scope_allowed(
                            parsed.path(),
                            &opts_worker.include_paths,
                            &opts_worker.exclude_paths,
                        );

                    let chars = md.chars().count();

                    if !in_scope {
                        // Navigation-only: don't add to results,
                        // don't count against page budget. Still
                        // harvest outlinks (seed → in-scope pages).
                        skipped
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((page.url.clone(), "out of scope (navigation-only)".into()));
                    } else {
                        let done = pages_done.fetch_add(1, Ordering::SeqCst) + 1;
                        if let Some(cb) = &opts_worker.progress {
                            cb(
                                done,
                                queue
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .len(),
                            );
                        }
                        if !duplicate {
                            chars_total.fetch_add(chars, Ordering::SeqCst);
                        }
                        if let Some(rec) = &opts_worker.on_page {
                            rec(&page.url, r.fingerprint.as_deref(), &md, r.title.as_deref());
                        }
                        pages
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(CrawlPage {
                                url: page.url.clone(),
                                title: r.title.clone().unwrap_or_default(),
                                kind: r.content_kind,
                                markdown: md,
                                chars,
                                quality: r.quality,
                                duplicate,
                                parent: item.parent.clone(),
                                score: item.score,
                                lastmod: None, // filled after worker loop from sitemap
                            });
                        if duplicate {
                            skipped
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push((page.url.clone(), "near-duplicate".into()));
                            continue 'work;
                        }
                    }

                    // ── Harvest outlinks into the frontier ──
                    if item.depth < max_depth {
                        let html = ghost_html
                            .clone()
                            .unwrap_or_else(|| String::from_utf8_lossy(&page.body).into_owned());
                        // <base href> handling: resolve relative
                        // links against the base URL when present.
                        let base = if let Some(bh) = extract_base_href(&html) {
                            Url::parse(&bh).unwrap_or_else(|_| {
                                Url::parse(&page.url).unwrap_or_else(|_| parsed.clone())
                            })
                        } else {
                            Url::parse(&page.url).unwrap_or_else(|_| parsed.clone())
                        };

                        // Harvest <a href> links early so we can check
                        // focus match across all link sources (pagination,
                        // feeds, outlinks).
                        let links = self_harvest_static(&html, &base);
                        let any_focus_match = focus.as_ref().as_ref().is_some_and(|fq| {
                            links.iter().any(|(child, anchor)| {
                                frontier::resolve(&base, child)
                                    .map(|cu| score::focus_match(anchor, cu.path(), fq))
                                    .unwrap_or(false)
                            })
                        });

                        // Pagination: <link rel="next"> continues a
                        // linear chain — push at the SAME depth so
                        // pagination doesn't consume depth budget.
                        let next_hrefs = extract_link_rel(&html, "next");
                        {
                            let ls = locale_seen
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let mut q = queue
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            for nh in &next_hrefs {
                                if let Some(nu) = frontier::resolve(&base, nh) {
                                    if opts_worker.same_host
                                        && !nu
                                            .host_str()
                                            .map(|h| host_matches(h, &seed_host2))
                                            .unwrap_or(false)
                                    {
                                        continue;
                                    }
                                    if !scope_allowed(
                                        nu.path(),
                                        &opts_worker.include_paths,
                                        &opts_worker.exclude_paths,
                                    ) {
                                        continue;
                                    }
                                    if opts_worker.respect_robots && !robots.allowed(nu.path()) {
                                        continue;
                                    }
                                    // Focus gate for pagination: hard filter
                                    // when page has matching links or depth
                                    // >= 1; soft filter for multi-hop at seed.
                                    if let Some(fq) = focus.as_ref()
                                        && !score::focus_match("", nu.path(), fq)
                                    {
                                        if any_focus_match || item.depth > 0 {
                                            continue;
                                        }
                                        let s = score::score_candidate_with_idf(
                                            "",
                                            nu.path(),
                                            focus.as_deref(),
                                            focus_idf.as_deref(),
                                        ) - 100.0;
                                        q.push_with_parent(
                                            nu,
                                            s,
                                            item.depth,
                                            Some(item.url.clone()),
                                        );
                                        continue;
                                    }
                                    let lcanon = frontier::locale_canonical(nu.path());
                                    if ls.contains(&lcanon) {
                                        continue;
                                    }
                                    let s = score::score_candidate_with_idf(
                                        "",
                                        nu.path(),
                                        focus.as_deref(),
                                        focus_idf.as_deref(),
                                    );
                                    q.push_with_parent(nu, s, item.depth, Some(item.url.clone()));
                                }
                            }
                        } // ls + q dropped before feed discovery's await

                        // RSS/Atom feed discovery: <link rel="alternate"
                        // type="application/rss+xml" href="...">. Fetch
                        // the feed, parse entry URLs, seed the frontier.
                        // A blog's feed is its full URL inventory in one
                        // request.
                        let feed_hrefs = extract_feed_links(&html);
                        for fh in &feed_hrefs {
                            let fu = match frontier::resolve(&base, fh) {
                                Some(u) => u,
                                None => continue,
                            };
                            if opts_worker.same_host
                                && !fu
                                    .host_str()
                                    .map(|h| host_matches(h, &seed_host2))
                                    .unwrap_or(false)
                            {
                                continue;
                            }
                            // Governor-pace the feed fetch.
                            let fw = governor.wait_for(host, &lane.id, seq);
                            seq += 1;
                            let remain = deadline_at.saturating_duration_since(Instant::now());
                            if fw > remain {
                                break;
                            }
                            if !fw.is_zero() {
                                tokio::time::sleep(fw).await;
                            }
                            let feed_page =
                                fetch(fu.to_string(), lane.id.clone(), Some(item.url.clone()))
                                    .await;
                            total_fetched.fetch_add(1, Ordering::SeqCst);
                            if !matches!(feed_page.verdict, Verdict::ContentOk) {
                                continue;
                            }
                            let feed_text = String::from_utf8_lossy(&feed_page.body);
                            let entries = parse_feed_urls(&feed_text, 200);
                            {
                                let ls = locale_seen
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                let mut q = queue
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                for eu in &entries {
                                    if let Ok(u) = Url::parse(eu) {
                                        if opts_worker.same_host
                                            && !u
                                                .host_str()
                                                .map(|h| host_matches(h, &seed_host2))
                                                .unwrap_or(false)
                                        {
                                            continue;
                                        }
                                        if !scope_allowed(
                                            u.path(),
                                            &opts_worker.include_paths,
                                            &opts_worker.exclude_paths,
                                        ) {
                                            continue;
                                        }
                                        if opts_worker.respect_robots && !robots.allowed(u.path()) {
                                            continue;
                                        }
                                        let lcanon = frontier::locale_canonical(u.path());
                                        if ls.contains(&lcanon) {
                                            continue;
                                        }
                                        // Focus gate for feed entries: hard
                                        // filter when page has matching links
                                        // or depth >= 1; soft at seed.
                                        if let Some(fq) = focus.as_ref()
                                            && !score::focus_match("", u.path(), fq)
                                        {
                                            if any_focus_match || item.depth > 0 {
                                                continue;
                                            }
                                            let s = score::score_candidate_with_idf(
                                                "",
                                                u.path(),
                                                focus.as_deref(),
                                                focus_idf.as_deref(),
                                            ) - 100.0;
                                            q.push_with_parent(
                                                u,
                                                s,
                                                item.depth + 1,
                                                Some(item.url.clone()),
                                            );
                                            continue;
                                        }
                                        let s = score::score_candidate_with_idf(
                                            "",
                                            u.path(),
                                            focus.as_deref(),
                                            focus_idf.as_deref(),
                                        );
                                        q.push_with_parent(
                                            u,
                                            s,
                                            item.depth + 1,
                                            Some(item.url.clone()),
                                        );
                                    }
                                }
                            } // ls + q dropped before next iteration's await
                        }

                        // Standard <a href> harvest.
                        let filtered: Vec<(url::Url, String, f64)> = {
                            let ls = locale_seen
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            links
                                .into_iter()
                                .filter_map(|(child, anchor)| {
                                    let cu = frontier::resolve(&base, &child)?;
                                    if opts_worker.same_host
                                        && !cu
                                            .host_str()
                                            .map(|h| host_matches(h, &seed_host2))
                                            .unwrap_or(false)
                                    {
                                        filtered_out.fetch_add(1, Ordering::Relaxed);
                                        return None;
                                    }
                                    if !scope_allowed(
                                        cu.path(),
                                        &opts_worker.include_paths,
                                        &opts_worker.exclude_paths,
                                    ) {
                                        filtered_out.fetch_add(1, Ordering::Relaxed);
                                        return None;
                                    }
                                    if opts_worker.respect_robots && !robots.allowed(cu.path()) {
                                        filtered_out.fetch_add(1, Ordering::Relaxed);
                                        return None;
                                    }
                                    let lcanon = frontier::locale_canonical(cu.path());
                                    if ls.contains(&lcanon) {
                                        filtered_out.fetch_add(1, Ordering::Relaxed);
                                        return None;
                                    }
                                    let s = score::score_candidate_with_idf(
                                        &anchor,
                                        cu.path(),
                                        focus.as_deref(),
                                        focus_idf.as_deref(),
                                    );
                                    // Focus gate: when a focus query is set,
                                    // non-matching links are filtered.
                                    // Hard filter when: (a) this page has
                                    // matching links (only crawl relevant),
                                    // or (b) depth >= 1 (prevent runaway).
                                    // Soft filter when: this page has no
                                    // matching links AND it's the seed page
                                    // (multi-hop discovery).
                                    if let Some(fq) = focus.as_ref()
                                        && !score::focus_match(&anchor, cu.path(), fq)
                                    {
                                        if any_focus_match || item.depth > 0 {
                                            filtered_out.fetch_add(1, Ordering::Relaxed);
                                            return None;
                                        }
                                        return Some((cu, anchor, s - 100.0));
                                    }
                                    Some((cu, anchor, s))
                                })
                                .collect()
                        }; // ls dropped here

                        {
                            let mut q = queue
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            for (cu, _anchor, s) in filtered {
                                q.push_with_parent(cu, s, item.depth + 1, Some(item.url.clone()));
                            }
                        } // q dropped here
                    }
                }
            }));
        }

        for h in handles {
            let _ = h.await;
        }

        // ── Result + resume token ──────────────────────────
        let elapsed = started.elapsed();
        let (final_pages, stop, queued_entries) = {
            let p = std::mem::take(
                &mut *pages
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            let s = stop_flag
                .lock()
                .unwrap()
                .unwrap_or(StopReason::FrontierEmpty);
            let q = sh_queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .snapshot_entries();
            (p, s, q)
        };

        let skipped_v = std::mem::take(
            &mut *skipped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let filtered = filtered_out.load(Ordering::Relaxed);

        // Lastmod lookup from sitemap entries + relevance-sorted
        // output (seed first, then by score desc).
        let lastmod_map: std::collections::HashMap<&str, &str> = sitemap_entries
            .iter()
            .filter_map(|e| e.lastmod.as_ref().map(|lm| (e.loc.as_str(), lm.as_str())))
            .collect();
        let mut final_pages = final_pages;
        for p in &mut final_pages {
            if let Some(lm) = lastmod_map.get(p.url.as_str()) {
                p.lastmod = Some(lm.to_string());
            }
        }
        final_pages.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Resume token only when stopped by budget (not frontier-empty).
        let resume = match stop {
            StopReason::MaxPages | StopReason::CharBudget | StopReason::Deadline => {
                if !queued_entries.is_empty() {
                    let id = {
                        let n = self.token_seq.fetch_add(1, Ordering::Relaxed);
                        let micros = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_micros())
                            .unwrap_or(0);
                        format!("c{micros:x}{n:x}")
                    };
                    let state = ResumeState {
                        seed: seed.to_string(),
                        queue: queued_entries
                            .iter()
                            .map(|(u, s, d, r, p)| (u.clone(), *s, *d, *r, p.clone()))
                            .collect(),
                        seen: sh_queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .seen_snapshot(),
                    };
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let mut store = ResumeFile::load();
                    store.sweep();
                    store.entries.insert(id.clone(), (state, now));
                    // Cap the file: drop oldest beyond 50 tokens.
                    if store.entries.len() > 50 {
                        let mut keyed: Vec<(u64, String)> = store
                            .entries
                            .iter()
                            .map(|(k, (_, at))| (*at, k.clone()))
                            .collect();
                        keyed.sort();
                        for (_, k) in keyed.into_iter().take(store.entries.len() - 50) {
                            store.entries.remove(&k);
                        }
                    }
                    store.save();
                    Some(id)
                } else {
                    None
                }
            }
            _ => None,
        };

        Ok(CrawlResult {
            crawl_delay: robots.crawl_delay,
            seed: seed.to_string(),
            pages: final_pages,
            queued: queued_entries
                .into_iter()
                .map(|(u, _, _, _, _)| u)
                .collect(),
            filtered_out: filtered,
            skipped: skipped_v,
            stop,
            elapsed,
            map,
            resume,
        })
    }
}

/// Extract `<link rel="canonical" href="...">` from HTML.
/// Byte-scan, no DOM parse. Handles both attribute orders
/// (`rel` before `href` and `href` before `rel`).
fn extract_canonical(html: &str) -> Option<String> {
    extract_link_rel(html, "canonical").into_iter().next()
}

/// Extract all href values from `<link>` tags with a given `rel`
/// attribute value. Byte-scan, no DOM parse.
fn extract_link_rel(html: &str, rel: &str) -> Vec<String> {
    let lower = html.to_lowercase();
    let rel_pat = format!("rel=\"{rel}\"");
    let rel_pat2 = format!("rel='{rel}'");
    let rel_pat3 = format!("rel={rel}");
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(link_start) = lower[pos..].find("<link") {
        let abs = pos + link_start;
        let Some(tag_end) = lower[abs..].find('>') else {
            break;
        };
        let tag_end_abs = abs + tag_end + 1;
        let tag = &lower[abs..tag_end_abs];
        pos = tag_end_abs;
        if !(tag.contains(&rel_pat) || tag.contains(&rel_pat2) || tag.contains(&rel_pat3)) {
            continue;
        }
        let orig_tag = &html[abs..tag_end_abs];
        if let Some(href) = extract_href(orig_tag) {
            out.push(href);
        }
    }
    out
}

/// Extract `<base href="...">` from HTML. First one wins.
fn extract_base_href(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let mut pos = 0usize;
    while let Some(base_start) = lower[pos..].find("<base") {
        let abs = pos + base_start;
        let Some(tag_end) = lower[abs..].find('>') else {
            break;
        };
        let tag_end_abs = abs + tag_end + 1;
        let tag = &lower[abs..tag_end_abs];
        pos = tag_end_abs;
        if !tag.contains("href") {
            continue;
        }
        let orig_tag = &html[abs..tag_end_abs];
        return extract_href(orig_tag);
    }
    None
}

/// Extract RSS/Atom feed links from HTML `<head>`:
/// `<link rel="alternate" type="application/rss+xml" href="...">`
/// or `type="application/atom+xml"`.
fn extract_feed_links(html: &str) -> Vec<String> {
    let lower = html.to_lowercase();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(link_start) = lower[pos..].find("<link") {
        let abs = pos + link_start;
        let Some(tag_end) = lower[abs..].find('>') else {
            break;
        };
        let tag_end_abs = abs + tag_end + 1;
        let tag = &lower[abs..tag_end_abs];
        pos = tag_end_abs;
        if !tag.contains("rel=\"alternate\"")
            && !tag.contains("rel='alternate'")
            && !tag.contains("rel=alternate")
        {
            continue;
        }
        if !tag.contains("application/rss+xml") && !tag.contains("application/atom+xml") {
            continue;
        }
        let orig_tag = &html[abs..tag_end_abs];
        if let Some(href) = extract_href(orig_tag) {
            out.push(href);
        }
    }
    out
}

/// Parse RSS/Atom feed XML for entry URLs. Handles both:
/// RSS: `<link>URL</link>` inside `<item>`
/// Atom: `<link href="URL"/>` inside `<entry>`
/// Skips `rel="self"` and `rel="enclosure"` (feed metadata).
fn parse_feed_urls(xml: &str, cap: usize) -> Vec<String> {
    let mut urls = Vec::new();
    let lower = xml.to_lowercase();
    // RSS: <link>URL</link>
    let mut pos = 0usize;
    while urls.len() < cap {
        let Some(open) = lower[pos..].find("<link>") else {
            break;
        };
        let abs = pos + open;
        let after = abs + 6;
        let Some(close_rel) = lower[after..].find("</link>") else {
            break;
        };
        let text = xml[after..after + close_rel].trim();
        if text.starts_with("http") {
            urls.push(text.to_string());
        }
        pos = after + close_rel + 7;
    }
    // Atom: <link href="URL" .../>
    if urls.len() < cap {
        pos = 0;
        while urls.len() < cap {
            let Some(link_start) = lower[pos..].find("<link ") else {
                break;
            };
            let abs = pos + link_start;
            let Some(tag_end) = lower[abs..].find('>') else {
                break;
            };
            let tag_end_abs = abs + tag_end + 1;
            let tag = &lower[abs..tag_end_abs];
            pos = tag_end_abs;
            // Skip non-content links.
            if tag.contains("rel=\"self\"")
                || tag.contains("rel='self'")
                || tag.contains("rel=\"enclosure\"")
                || tag.contains("rel='enclosure'")
            {
                continue;
            }
            let orig_tag = &html_orig(xml, abs, tag_end_abs);
            if let Some(href) = extract_href(orig_tag)
                && href.starts_with("http")
            {
                urls.push(href);
            }
        }
    }
    urls
}

/// Safe slice of the original XML (not lowered) for href extraction.
fn html_orig(xml: &str, from: usize, to: usize) -> &str {
    &xml[from..to.min(xml.len())]
}

/// Extract the `href` attribute value from an HTML tag string.
fn extract_href(tag: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let href_pos = lower.find("href")?;
    let after = &tag[href_pos + 4..];
    // Skip whitespace and =.
    let after = after.trim_start();
    let after = after.strip_prefix('=')?;
    let after = after.trim_start();
    // Extract quoted value.
    if let Some(rest) = after.strip_prefix('"') {
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    } else if let Some(rest) = after.strip_prefix('\'') {
        let end = rest.find('\'')?;
        Some(rest[..end].to_string())
    } else {
        // Unquoted: read until whitespace or >.
        let end = after.find(|c: char| c.is_whitespace() || c == '>')?;
        Some(after[..end].to_string())
    }
}

/// Anchor+href harvest without holding `&self` (worker closure).
fn self_harvest_static(html: &str, _base: &Url) -> Vec<(String, String)> {
    let doc = scraper::Html::parse_document(html);
    let sel = scraper::Selector::parse("a[href]").unwrap();
    let mut out = Vec::new();
    for a in doc.select(&sel) {
        let Some(href) = a.value().attr("href") else {
            continue;
        };
        let anchor: String = a.text().collect::<String>().trim().to_string();
        out.push((href.to_string(), anchor));
    }
    out
}

/// Compare two hosts, treating `www.` prefix as equivalent.
/// `example.com` matches `www.example.com` and vice versa.
fn host_matches(a: &str, b: &str) -> bool {
    let a = a.strip_prefix("www.").unwrap_or(a);
    let b = b.strip_prefix("www.").unwrap_or(b);
    a.eq_ignore_ascii_case(b)
}
