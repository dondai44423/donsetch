//! The crawl orchestrator. Owns: frontier loop, worker pool,
//! budgets, stop conditions, near-dup detection, resume tokens.
//!
//! Fetch I/O goes through the `PageFetcher` trait : the real
//! crawl rides DonShadow; tests ride a mock. Never does the
//! orchestrator touch sockets directly.

pub mod digest;
pub mod executor;
pub mod frontier;
pub mod governor;
pub mod host_pace;
pub mod model;
pub mod real;
pub mod score;
pub mod sitemap;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use url::Url;

use crate::detect::walls::Verdict;
use crate::extract::ContentKind;

use governor::Governor;

/// A fetched page as the orchestrator sees it.
#[derive(Clone)]
pub struct FetchedPage {
    /// Governor assignment that produced this page, including buffered reads.
    pub lane: String,
    /// The actual HTTP policy; absent when no request produced a response.
    pub route: Option<crate::egress::request_route::RequestRoute>,
    /// Final URL after redirects; for a rule refusal, the refused URL
    /// (a redirect hop's target when the refusal came mid-chain).
    pub url: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub verdict: Verdict,
    pub latency: Duration,
    /// True when served from the revalidation/pool cache : these
    /// are FREE and must not count against pacing budgets.
    pub cached: bool,
    /// Human-readable failure note (network error, etc.).
    pub error_hint: Option<String>,
    /// Set when a local DonSeTch rule refused the fetch (the URL, its
    /// adapter rewrite or a redirect hop); `status` is then 0. Such a
    /// page is final: the worker never retries it.
    pub denied: Option<crate::rules::Denial>,
}

/// The skip reason a crawl records for a page a local rule refused:
/// `<policy code>: excluded by the operator's local DonSeTch rules `<key>``.
pub(crate) fn policy_skip_reason(d: &crate::rules::Denial) -> String {
    format!(
        "{}: excluded by the operator's local DonSeTch rules `{}`",
        d.code(),
        d.rule
    )
}

/// The URLs one rule denied in a crawl.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DeniedGroup {
    /// The rule's pattern key.
    pub rule: String,
    /// `"walled"` or `"permanent"`.
    pub kind: String,
    /// The operator's message, verbatim.
    pub message: String,
    /// Every denial recorded under this rule.
    pub count: usize,
    /// The first `[rules] crawl_denied_urls_per_rule` URLs only.
    pub urls: Vec<String>,
}

/// URLs a crawl refused to queue because a local rule denies them (or
/// their adapter rewrite), grouped by rule. Bounded: per rule it keeps a
/// count and a capped URL list.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DeniedLog {
    /// One group per rule, in first-seen order.
    pub groups: Vec<DeniedGroup>,
}

impl DeniedLog {
    /// Denials recorded across every rule.
    pub fn total(&self) -> usize {
        self.groups.iter().map(|g| g.count).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Count one denial of `url` under `d`'s rule, keeping the URL only
    /// while the group lists fewer than `cap`. Callers record each URL
    /// once; this does not dedup.
    pub fn record(&mut self, url: &str, d: &crate::rules::Denial, cap: usize) {
        let index = match self.groups.iter().position(|g| g.rule == d.rule) {
            Some(index) => index,
            None => {
                self.groups.push(DeniedGroup {
                    rule: d.rule.clone(),
                    kind: d.kind.clone(),
                    message: d.message.clone(),
                    count: 0,
                    urls: Vec::new(),
                });
                self.groups.len() - 1
            }
        };
        let group = &mut self.groups[index];
        group.count += 1;
        if group.urls.len() < cap {
            group.urls.push(url.to_string());
        }
    }
}

/// Why the seed produced nothing in a crawl that had nothing else to
/// fetch. `crawl()` still returns `Ok`; the tool turns this into its
/// error. Never set for a resumed crawl.
#[derive(Debug, Clone, PartialEq)]
pub struct SeedFailure {
    /// The seed as the caller gave it.
    pub requested: String,
    /// Where the seed fetch ended, when that differs from `requested`.
    pub landing: Option<String>,
    /// HTTP status of the failing seed fetch; 0 = no response.
    pub status: u16,
    /// The fetch's verdict, `{:?}`-formatted; None when not fetched.
    pub verdict: Option<String>,
    /// The seed's skip reason, or the fetch's error note.
    pub error: Option<String>,
    /// Set when robots.txt refused the seed, which was then not fetched.
    pub robots: Option<RobotsRefusal>,
    /// Set when a local rule refused the seed fetch (a redirect into a
    /// denied host).
    pub denial: Option<crate::rules::Denial>,
    /// False in content mode, which reads no sitemap.
    pub has_sitemap_phase: bool,
}

/// How robots.txt refused a seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobotsRefusal {
    /// The origin's published rules disallow it.
    Disallow,
    /// robots.txt could not be read (RFC 9309 "unreachable"), which
    /// reads as disallow-all.
    Unreachable,
}

impl RobotsRefusal {
    fn of(robots: &sitemap::Robots) -> Self {
        if robots.unreachable {
            Self::Unreachable
        } else {
            Self::Disallow
        }
    }
}

impl SeedFailure {
    /// A seed fetch that came back without content. `requested`,
    /// `landing` and `has_sitemap_phase` are settled by the caller.
    pub(crate) fn from_page(page: &FetchedPage, error: Option<String>) -> Self {
        Self {
            requested: String::new(),
            landing: Some(page.url.clone()),
            status: page.status,
            verdict: Some(format!("{:?}", page.verdict)),
            error: error.or_else(|| page.error_hint.clone()),
            robots: None,
            denial: page.denied.clone(),
            has_sitemap_phase: false,
        }
    }

    /// A seed robots.txt refused; it was not fetched.
    fn from_robots(landing: &str, refusal: RobotsRefusal) -> Self {
        Self {
            requested: String::new(),
            landing: Some(landing.to_string()),
            status: 0,
            verdict: None,
            error: None,
            robots: Some(refusal),
            denial: None,
            has_sitemap_phase: false,
        }
    }

    /// Fill the fields only the crawl knows; `landing` survives only
    /// when it names another URL than `requested`.
    pub(crate) fn settle(mut self, requested: &str, has_sitemap_phase: bool) -> Self {
        let same = |a: &str, b: &str| match (Url::parse(a), Url::parse(b)) {
            (Ok(a), Ok(b)) => frontier::normalize(&a) == frontier::normalize(&b),
            _ => a == b,
        };
        self.landing = self.landing.filter(|l| !same(l, requested));
        self.requested = requested.to_string();
        self.has_sitemap_phase = has_sitemap_phase;
        self
    }
}

/// The preflight's view of the seed, kept for map mode's exit, where
/// the buffered seed page is gone by the time the empty-map branch runs.
struct SeedPreflight {
    /// The seed fetch, when robots let the preflight make it.
    page: Option<SeedFailure>,
    /// Content came back (2xx `ContentOk`).
    page_ok: bool,
    robots: Option<RobotsRefusal>,
}

impl SeedPreflight {
    /// The failure the preflight saw, if the seed failed.
    fn failure(&self, seed: &str) -> Option<SeedFailure> {
        if let Some(refusal) = self.robots {
            let mut failure = SeedFailure::from_robots(seed, refusal);
            failure.error = self.page.as_ref().and_then(|p| p.error.clone());
            return Some(failure);
        }
        match &self.page {
            Some(page) if !self.page_ok => Some(page.clone()),
            _ => None,
        }
    }
}

/// Pluggable fetch: real = DonShadow, tests = in-memory map.
pub type PageFetcher = Arc<
    dyn Fn(
            String,
            String,
            Option<String>,
            Option<crate::fetch::client::RedirectGate>,
        ) -> BoxFuture<'static, FetchedPage>
        + Send
        + Sync,
>;
//            (url, lane_id, referer, redirect_policy) -> page

/// Crawl surface mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrawlMode {
    /// Both phases (default): sitemap-map first, then content.
    Full,
    /// URL map only : cheap, extractable for agent decisions.
    Map,
    /// The site model plus the planner: a 10-page plan with reasons and
    /// reserves, no page fetch.
    Plan,
    /// Spend the plan through the executor; durable job, resumable.
    Run,
    /// Read one page from a run job by `pN` handle; no network.
    Get,
    /// Delta: only changed/new pages; removed pages are reported.
    Diff,
    /// List the bounded dossiers.
    Sites,
    /// Remove one host's dossier.
    Forget,
    /// Skip the sitemap phase, crawl links BFS-style only.
    Content,
}

// A popped page can enqueue descendants after a slow fetch. Idle workers
// must wait for it; every error/continue/cancellation releases this guard.
pub struct CrawlPage {
    pub focus: Option<FocusStats>,
    pub url: String,
    pub title: String,
    pub kind: ContentKind,
    pub markdown: String,
    pub chars: usize,
    /// Byte offset for a follow-up fetch when the returned page was capped.
    pub next_offset: Option<usize>,
    /// Source-side omission, independent of the returned byte budget.
    pub partial: Option<crate::extract::PartialContent>,
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
    /// Unix epoch seconds when this page was fetched (dataset
    /// mode: per-row freshness that survives crawl resumes).
    pub fetched_at: u64,
    /// Content fingerprint from DonSift; the durable dossier uses it
    /// to tell a changed page from a re-fetched one.
    pub fingerprint: Option<String>,
}

/// Why the crawl stopped. Agents MUST see this to decide
/// whether to resume or re-scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Frontier exhausted : the whole reachable scope was read.
    FrontierEmpty,
    MaxPages,
    CharBudget,
    /// Frontier exhausted, but links in scope were left unfollowed
    /// because of `max_depth` (`CrawlResult::depth_cut` lists them).
    /// Issues no resume token: the cut links were never queued, so a
    /// crawl with a larger `max_depth` is what reaches them.
    DepthLimit,
    Deadline,
    /// Client cancelled (MCP notifications/cancelled) : workers
    /// stopped gracefully; no resume token is issued.
    Cancelled,
    /// A classified wall (challenge, block, auth, paywall) declared the
    /// host; the run stopped instead of fighting page by page.
    Walled,
    /// Host boxed us out (all lanes walled) : resume later.
    ThrottledOut,
    /// Published inventory exceeded the output or discovery bound.
    InventoryLimit,
}

#[derive(Clone, Debug, Default)]
pub struct InventoryStatus {
    pub complete: bool,
    pub source: &'static str,
    pub limit: usize,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct FocusStats {
    pub omitted: bool,
    pub blocks_total: usize,
    pub blocks_shown: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeltaSummary {
    pub new: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub removed: Vec<String>,
    pub inventory_changed: bool,
    pub pages_fetched: usize,
}

pub struct CrawlResult {
    pub inventory: InventoryStatus,
    pub seed: String,
    pub pages: Vec<CrawlPage>,
    /// URLs discovered but not fetched (budget/scope).
    pub queued: Vec<String>,
    /// In-scope URLs the crawl did not follow because of `max_depth`,
    /// sorted, in frontier-key form: links on pages at the limit that
    /// no shallower page led to, and queued entries deeper than the
    /// limit. Carried across resumes.
    pub depth_cut: Vec<String>,
    /// URLs skipped by robots/scope rules.
    pub filtered_out: usize,
    /// URLs fetched but skipped (wall/dup/error), with reason.
    pub skipped: Vec<(String, String)>,
    pub stop: StopReason,
    pub elapsed: Duration,
    /// Sitemap map (Map phase) : capped URLs.
    pub map: Vec<String>,
    /// robots.txt Crawl-delay honored by pacing (seconds). Surfaces
    /// in the output so a slow crawl explains itself.
    pub crawl_delay: Option<f64>,
    /// Resume token when stopped early, for `resume=`.
    pub resume: Option<String>,
    /// URLs refused at queue time by a local rule, never fetched;
    /// includes the denials a resumed crawl's earlier runs recorded.
    /// A page refused mid-fetch (a redirect into a denied host) is a
    /// `skipped` row instead, never listed here.
    pub denied: DeniedLog,
    /// Set when the seed failed and the crawl had nothing else to fetch.
    pub seed_failure: Option<SeedFailure>,
    /// The site model a map built; None for a content crawl.
    pub model: Option<model::ModelSummary>,
    /// The plan a plan mode built; None otherwise.
    pub plan: Option<model::plan::Plan>,
    /// The executor summary for mode=run; None otherwise.
    pub run: Option<executor::RunSummary>,
    /// Full current inventory for delta/removed reporting.
    pub inventory_urls: Vec<String>,
    /// Delta accounting; None outside diff.
    pub delta: Option<DeltaSummary>,
}

/// v3: (done, queued) : fired per completed page, throttled by the caller.
pub type ProgressFn = std::sync::Arc<dyn Fn(usize, usize) + Send + Sync>;
/// delta crawl: (url, new_fingerprint) -> true when the
/// page is unchanged since the last crawl (fingerprint on file matches).
pub type UnchangedFn = std::sync::Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;
/// v3: (url, fingerprint, markdown, title) : the delta-crawl memory feed.
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
    /// v3: cancellation : the client aborted the request. Workers
    /// observe this between pages and stop gracefully.
    pub cancel: Option<tokio::sync::watch::Receiver<bool>>,
    /// v3: progress callback (done, queued) : fired per completed
    /// page, throttled by the caller.
    pub progress: Option<ProgressFn>,
    /// delta crawl: called after extraction with the
    /// page's fresh fingerprint; true = unchanged since the last
    /// crawl. Unchanged pages are re-verified (history refreshed),
    /// their outlinks still harvested, but they are excluded from
    /// results and consume no page/char budgets. Replaces the old
    /// presence-only pre-fetch skip, which could never see changes.
    pub delta_unchanged: Option<UnchangedFn>,
    /// v3: record a fetched page's fingerprint (url, fingerprint,
    /// markdown, title) : the delta-crawl memory feed.
    pub on_page: Option<OnPageFn>,
    /// dataset mode: render one JSON object per page
    /// (JSON Lines) instead of a markdown document. Output-format
    /// only; traversal, budgets, and pacing are unchanged.
    pub dataset: bool,
    /// plan mode: include the bounded candidate table.
    pub plan_explain: bool,
    /// crawl-shape: reader-like pop jitter in the frontier
    /// (default true). False restores exact score-order traversal.
    /// Zero latency cost; DONSETCH_NO_CRAWL_SHAPE flips it at runtime
    /// without any rebuild (belt for operators, env wins over the
    /// option).
    pub shape: bool,
    /// Overrides the automatic shape seed (tests pin ordering; prod
    /// derives it from the seed URL + clock). None = auto.
    pub shape_seed: Option<u64>,
    /// Map hard cap.
    pub map_cap: usize,
    /// Minimum content quality (0.0-1.0). Pages below this
    /// are skipped (still counted against page budget).
    pub min_quality: f32,
    /// Diff/plan: bypass the warm dossier so the current inventory is known.
    pub force_fresh_map: bool,
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
            dataset: false,
            plan_explain: false,
            shape: true,
            shape_seed: None,
            cancel: None,
            progress: None,
            delta_unchanged: None,
            on_page: None,
            map_cap: 120,
            min_quality: 0.05,
            force_fresh_map: false,
        }
    }
}

pub struct Crawler {
    fetch: PageFetcher,
    governor: Arc<Governor>,
}

impl Crawler {
    pub fn new(fetch: PageFetcher, governor: Arc<Governor>) -> Self {
        Self { fetch, governor }
    }

    /// Differential crawl: build the current model, compare its inventory
    /// and per-URL lastmod with the dossier, and fetch only changed/new
    /// pages. Removed URLs are reported; an unchanged stable inventory makes
    /// no page request.
    async fn diff_mode(&self, seed: &str, opts: CrawlOptions) -> Result<CrawlResult, String> {
        let host = url::Url::parse(seed)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_default();
        let previous = model::dossier::load(&host);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // A warm dossier answers an unchanged diff without touching the
        // network. `--refresh` is the explicit revalidation path; the
        // contract TTL is the cheap default.
        if !opts.force_fresh_map
            && let Some(prev) = previous.as_ref()
            && prev.focus.as_deref() == opts.focus.as_deref()
            && prev
                .summary
                .generated_at
                .saturating_add(model::MAP_TTL_SECS)
                >= now
            && !prev.map.is_empty()
            && prev.contract.is_some()
        {
            let sections = model::infer::sections_from_families(&prev.families);
            let inventory: Vec<model::infer::InvUrl> = prev
                .inventory
                .iter()
                .map(|url| model::infer::InvUrl {
                    url: url.clone(),
                    lastmod: None,
                    source: "dossier".to_string(),
                    title: prev.labels.get(url).cloned(),
                })
                .collect();
            let plan = model::plan::build(model::plan::PlanRequest {
                seed: &prev.seed,
                host: &prev.summary.host,
                site_kind: prev.summary.site_kind,
                focus: opts.focus.as_deref(),
                max_pages: opts.max_pages,
                families: &prev.families,
                sections: &sections,
                urls: &inventory,
                explain: false,
            });
            let unchanged = plan.pages.len();
            let delta = DeltaSummary {
                new: 0,
                changed: 0,
                unchanged,
                removed: Vec::new(),
                inventory_changed: false,
                pages_fetched: 0,
            };
            let mut summary = prev.summary.clone();
            summary.cached = true;
            summary.requests = 0;
            let source = match prev.summary.source.as_str() {
                "pagelist" => "pagelist",
                "sitemap" => "sitemap",
                "llms" => "llms",
                _ => "seed-links",
            };
            return Ok(CrawlResult {
                inventory: InventoryStatus {
                    complete: summary.complete,
                    source,
                    limit: opts.map_cap,
                },
                seed: summary.seed.clone(),
                pages: Vec::new(),
                queued: Vec::new(),
                depth_cut: Vec::new(),
                filtered_out: summary.filtered.total(),
                skipped: Vec::new(),
                stop: StopReason::FrontierEmpty,
                elapsed: std::time::Duration::from_millis(1),
                map: prev.map.clone(),
                crawl_delay: None,
                resume: None,
                denied: DeniedLog::default(),
                seed_failure: None,
                model: Some(summary),
                plan: Some(plan),
                run: None,
                inventory_urls: prev.inventory.clone(),
                delta: Some(delta),
            });
        }
        let plan_opts = CrawlOptions {
            mode: CrawlMode::Plan,
            force_fresh_map: opts.force_fresh_map,
            ..opts.clone()
        };
        let mut planned = Box::pin(self.crawl(seed, plan_opts, None)).await?;
        let mut plan = planned
            .plan
            .take()
            .ok_or_else(|| "diff: plan mode returned no plan".to_string())?;
        let current_inventory = if planned.inventory_urls.is_empty() {
            previous
                .as_ref()
                .map(|d| d.inventory.clone())
                .unwrap_or_default()
        } else {
            planned.inventory_urls.clone()
        };
        let current_set: std::collections::HashSet<&str> =
            current_inventory.iter().map(String::as_str).collect();
        let removed: Vec<String> = previous
            .as_ref()
            .map(|d| {
                d.inventory
                    .iter()
                    .filter(|u| !current_set.contains(u.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let inventory_hash = planned
            .model
            .as_ref()
            .map(|m| m.inventory_hash.clone())
            .unwrap_or_default();
        let contract_same = previous
            .as_ref()
            .and_then(|d| d.contract.as_ref())
            .is_some_and(|c| c.inventory_hash == inventory_hash && c.page_list_stable);
        let old_pages = previous
            .as_ref()
            .map(|d| d.pages.clone())
            .unwrap_or_default();
        let mut new = 0usize;
        let mut changed = 0usize;
        let mut unchanged = 0usize;
        if contract_same {
            unchanged = plan.pages.len();
            plan.pages.clear();
            plan.reserves.clear();
            plan.stop = model::plan::PlanStop::Covered;
        } else {
            let mut keep = Vec::new();
            for row in plan.pages.iter().chain(plan.reserves.iter()) {
                match old_pages.get(&row.url) {
                    Some(mem) if row.freshness.is_some() && row.freshness == mem.lastmod => {
                        unchanged += 1;
                    }
                    Some(_) => {
                        changed += 1;
                        keep.push(row.clone());
                    }
                    None => {
                        new += 1;
                        keep.push(row.clone());
                    }
                }
            }
            plan.pages = keep;
            plan.reserves.clear();
            if plan.pages.is_empty() {
                plan.stop = model::plan::PlanStop::Covered;
            }
        }
        let delta = DeltaSummary {
            new,
            changed,
            unchanged,
            removed,
            inventory_changed: !contract_same,
            pages_fetched: 0,
        };
        if plan.pages.is_empty() {
            planned.plan = Some(plan);
            planned.delta = Some(delta);
            planned.inventory_urls = current_inventory;
            planned.stop = StopReason::FrontierEmpty;
            return Ok(planned);
        }
        let store = executor::JobStore::default();
        let job = store.create(&planned.seed, plan, planned.model.clone())?;
        let mut result = executor::execute(
            self.fetch.clone(),
            self.governor.clone(),
            &opts,
            &store,
            job,
        )
        .await?;
        let fetched = result.pages.len();
        result.delta = Some(DeltaSummary {
            pages_fetched: fetched,
            ..delta
        });
        result.inventory_urls = current_inventory;
        Ok(result)
    }

    /// Spend a plan through the v5 executor. The plan is built by the
    /// normal plan mode first; a resume id loads the durable job instead.
    async fn run_mode(
        &self,
        seed: &str,
        opts: CrawlOptions,
        resume: Option<&str>,
    ) -> Result<CrawlResult, String> {
        let store = executor::JobStore::default();
        if let Some(id) = resume {
            let job = store.load(id)?;
            if !seed.is_empty() && seed != job.seed {
                return Err(format!(
                    "run resume seed mismatch: job is {}, asked {seed}",
                    job.seed
                ));
            }
            return executor::execute(
                self.fetch.clone(),
                self.governor.clone(),
                &opts,
                &store,
                job,
            )
            .await;
        }
        let plan_opts = CrawlOptions {
            mode: CrawlMode::Plan,
            ..opts.clone()
        };
        let planned = Box::pin(self.crawl(seed, plan_opts, None)).await?;
        let plan = planned
            .plan
            .ok_or_else(|| "run: plan mode returned no plan".to_string())?;
        let model = planned.model;
        let job = store.create(&planned.seed, plan, model)?;
        executor::execute(
            self.fetch.clone(),
            self.governor.clone(),
            &opts,
            &store,
            job,
        )
        .await
    }

    /// Run one crawl. Returns when a stop condition hits.
    #[allow(clippy::field_reassign_with_default)]
    pub async fn crawl(
        &self,
        seed: &str,
        opts: CrawlOptions,
        resume_token: Option<&str>,
    ) -> Result<CrawlResult, String> {
        if opts.mode == CrawlMode::Diff {
            return self.diff_mode(seed, opts).await;
        }
        if opts.mode == CrawlMode::Run {
            return self.run_mode(seed, opts, resume_token).await;
        }
        // The new crawl owns the default full path. Content remains an
        // accepted alias for callers from the old surface.
        if matches!(opts.mode, CrawlMode::Full | CrawlMode::Content) {
            let mut opts = opts;
            opts.mode = CrawlMode::Run;
            return self.run_mode(seed, opts, resume_token).await;
        }
        let started = Instant::now();
        let deadline_at = started + opts.deadline;
        // One clock covers discovery, robots, redirects and page I/O. Keep the
        // resolved seed response so following its redirect costs no second GET.
        let buffered = Arc::new(Mutex::new(
            std::collections::HashMap::<String, FetchedPage>::new(),
        ));
        let fetch: PageFetcher = {
            let inner = self.fetch.clone();
            let buffered = buffered.clone();
            Arc::new(move |url, lane, referer, redirect_gate| {
                let inner = inner.clone();
                let buffered = buffered.clone();
                Box::pin(async move {
                    let saved = buffered
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&url);
                    if let Some(page) = saved {
                        return page;
                    }
                    match tokio::time::timeout_at(
                        deadline_at.into(),
                        inner(url.clone(), lane.clone(), referer, redirect_gate),
                    )
                    .await
                    {
                        Ok(page) => page,
                        Err(_) => FetchedPage {
                            lane,
                            route: None,
                            url,
                            status: 0,
                            headers: Vec::new(),
                            body: Vec::new(),
                            verdict: Verdict::Blocked,
                            latency: Duration::ZERO,
                            cached: false,
                            error_hint: Some("crawl deadline exceeded".into()),
                            denied: None,
                        },
                    }
                })
            })
        };

        // Map and plan are the only modes left here; full/run/diff were
        // routed to the v6 executor above. A resume token belongs to a job.
        let mut seed = seed.to_string();
        if seed.is_empty() {
            return Err("resume requires a run job".into());
        }
        crate::fetch::guards::validate_url_basic(&seed)
            .map_err(|e| format!("bad seed url: {e}"))?;
        let mut seed_url = Url::parse(&seed).map_err(|_| format!("bad seed url: {seed}"))?;
        let seed_host = seed_url
            .host_str()
            .ok_or("seed must have a host")?
            .to_string();
        let requested_seed = seed.clone();

        let map_mode = opts.mode == CrawlMode::Map;
        let mut preflight: Option<SeedPreflight> = None;
        {
            let origin = sitemap::origin_of(&seed_url);
            let seed_lane = self
                .governor
                .best_lane(&seed_host)
                .map(|lane| lane.id.clone());
            let allowed = if opts.respect_robots {
                let robots_url = format!("{origin}/robots.txt");
                let page = fetch(
                    robots_url.clone(),
                    seed_lane.clone().unwrap_or_else(|| "direct".into()),
                    None,
                    None,
                )
                .await;
                let robots = sitemap::robots_for_origin(page.status, &page.body, &origin);
                self.governor
                    .set_host_crawl_delay(&seed_host, robots.crawl_delay);
                buffered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(robots_url, page.clone());
                let allowed = robots.allows_url(&seed_url);
                if !allowed {
                    preflight = Some(SeedPreflight {
                        page: Some(SeedFailure::from_page(&page, None)),
                        page_ok: false,
                        robots: Some(RobotsRefusal::of(&robots)),
                    });
                }
                allowed
            } else {
                true
            };
            if allowed
                && !map_mode
                && Instant::now() < deadline_at
                && let Some(lane) = seed_lane
            {
                let wait = self
                    .governor
                    .wait_for(&seed_host, &lane, 0)
                    .min(deadline_at.saturating_duration_since(Instant::now()));
                tokio::time::sleep(wait).await;
                // The seed hop is identity resolution, not a scoped
                // crawl step: its redirect may adopt a new host by
                // design (short URLs, www moves), so it stays
                // ungated; worker hops carry the policy.
                let page = fetch(seed.clone(), lane, None, None).await;
                preflight = Some(SeedPreflight {
                    page: Some(SeedFailure::from_page(&page, None)),
                    page_ok: matches!(page.verdict, Verdict::ContentOk)
                        && (200..300).contains(&page.status),
                    robots: None,
                });
                if matches!(page.verdict, Verdict::ContentOk)
                    && (200..300).contains(&page.status)
                    && let Ok(resolved) = Url::parse(&page.url)
                    && crate::fetch::guards::validate_url_basic(&page.url).is_ok()
                {
                    seed = resolved.to_string();
                    seed_url = resolved;
                }
                buffered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(frontier::normalize(&seed_url), page);
            }
        }

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
        // Map mode is the v6 site model: it reads the inventory and answers
        // with sections, families and a dossier, never a content page. It
        // returns here so the frontier phase below is full/content only.
        if opts.mode == CrawlMode::Map || opts.mode == CrawlMode::Plan {
            let seed_origin = sitemap::origin_of(&seed_url);
            let robots = if opts.respect_robots {
                let page = fetch(
                    format!("{seed_origin}/robots.txt"),
                    "direct".to_string(),
                    None,
                    None,
                )
                .await;
                sitemap::robots_for_origin(page.status, &page.body, &seed_origin)
            } else {
                sitemap::Robots::default()
            };
            let seed_page = buffered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&frontier::normalize(&seed_url));
            let seed_failure = preflight
                .as_ref()
                .and_then(|p| p.failure(&seed))
                .map(|f| f.settle(&requested_seed, true));
            let outcome = model::map(
                &fetch,
                model::MapRequest {
                    seed: &seed_url,
                    requested_seed: &requested_seed,
                    seed_page: seed_page.as_ref(),
                    robots: Some(&robots),
                    opts: &opts,
                    deadline: deadline_at,
                    seed_failure,
                },
            )
            .await;
            let skipped = if outcome.map.is_empty()
                && outcome.seed_failure.is_none()
                && outcome.stop != StopReason::Deadline
            {
                vec![(
                    seed.clone(),
                    "no sitemap, llms map or seed links found : try mode=full or mode=content"
                        .to_string(),
                )]
            } else {
                Vec::new()
            };
            let plan = if opts.mode == CrawlMode::Plan {
                Some(model::plan::build(model::plan::PlanRequest {
                    seed: &seed,
                    host: seed_url.host_str().unwrap_or(""),
                    site_kind: outcome.summary.site_kind,
                    focus: opts.focus.as_deref(),
                    max_pages: opts.max_pages,
                    families: &outcome.families,
                    sections: &outcome.sections,
                    urls: &outcome.urls,
                    explain: opts.plan_explain,
                }))
            } else {
                None
            };
            let inventory_urls: Vec<String> = outcome.urls.iter().map(|u| u.url.clone()).collect();
            let stop = match plan.as_ref().map(|p| p.stop) {
                Some(model::plan::PlanStop::Budget) => StopReason::MaxPages,
                Some(_) => StopReason::FrontierEmpty,
                None => outcome.stop,
            };
            return Ok(CrawlResult {
                inventory: outcome.inventory,
                crawl_delay: robots.crawl_delay,
                seed: seed.clone(),
                pages: Vec::new(),
                queued: Vec::new(),
                depth_cut: Vec::new(),
                filtered_out: outcome.filtered_out,
                skipped,
                stop,
                elapsed: started.elapsed(),
                map: outcome.map,
                resume: None,
                denied: DeniedLog::default(),
                seed_failure: outcome.seed_failure,
                model: Some(outcome.summary),
                plan,
                run: None,
                inventory_urls,
                delta: None,
            });
        }

        unreachable!("full/content are handled by the v6 run path; map/plan return above")
    }
}
