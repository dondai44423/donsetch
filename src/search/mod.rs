//! DonSeek — keyless multi-engine search.
//!
//! Intent → fan-out (engines across egresses + verticals
//! direct) → weighted RRF merge → ranked results with
//! honest engine reporting.

pub mod byok;
pub mod coverage;
pub mod egress;
pub mod engines;
pub mod intent;
pub mod rank;
pub mod rerank;
pub mod verticals;

mod authority;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::detect::walls::Verdict;
use crate::error::FetchError;
use crate::fetch::client::Fetcher;

use egress::EgressPool;
use intent::Intent;
use rank::Merged;
use scraper::Selector;

const ENGINE_TIMEOUT: Duration = Duration::from_secs(8);

/// Intent + recency-aware cache TTL. Every cached query
/// is a query that never touches an egress — the #1 rate
/// reducer. But a cached answer presented as fresh is
/// WORSE than honest latency when the world moved:
/// time-sensitive queries (even outside news intent —
/// "X release date", "inflation 2026") get news-grade
/// TTLs regardless of detected intent.
fn cache_ttl(intent: Intent, query: &str) -> Duration {
    const RECENCY: &[&str] = &[
        "latest",
        "today",
        "breaking",
        "recent",
        "this week",
        "this month",
        "price",
        "stock",
        "weather",
        "deadline",
        "release date",
        "news",
        "2024",
        "2025",
        "2026",
        "2027",
    ];
    let q = query.to_lowercase();
    if RECENCY.iter().any(|s| q.contains(s)) {
        return Duration::from_secs(300);
    }
    match intent {
        Intent::News => Duration::from_secs(300),
        Intent::Code => Duration::from_secs(900),
        _ => Duration::from_secs(1800),
    }
}

/// Normalize a query for cache keys: casing, punctuation
/// and stopwords don't change intent, so they don't get
/// to spend egress budget twice.
fn norm_query(q: &str) -> String {
    const STOP: &[&str] = &[
        "a", "an", "the", "is", "are", "was", "were", "of", "in", "on", "at", "to", "for", "and",
        "or", "what", "which", "how", "do", "does", "i", "you", "it",
    ];
    q.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !STOP.contains(w))
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Searcher {
    fetcher: Fetcher,
    pool: EgressPool,
    /// engine -> trust EWMA (1.0 seed; 0.2..2.0 clamp).
    trust: Mutex<HashMap<String, f64>>,
    /// normalized-query cache: zero egress cost on repeats.
    /// Stores up to 12 results; reads truncate to the
    /// requested max so max_results variants share entries.
    #[allow(clippy::type_complexity)]
    cache: Mutex<HashMap<String, (Instant, Vec<Merged>, usize)>>,
    /// Chronic-failure quarantine: engine -> (consecutive
    /// failures, last failure). 3 strikes across any
    /// egresses = benched for QUARANTINE_TTL so a walled
    /// engine stops wasting a fan-out slot every query.
    failures: Mutex<HashMap<String, (u32, Instant)>>,
    /// Single-flight: two identical in-flight queries spend
    /// egress budget ONCE — the follower awaits the
    /// leader's result. Stampedes are an agent reality
    /// (parallel tool calls love the same query).
    inflight: Mutex<std::collections::HashSet<String>>,
    /// v3 warm handoff: enrichment bodies cached for the
    /// subsequent `web_fetch` of a top result (search → fetch
    /// is THE agent pipeline). One-shot, TTL'd, bounded.
    prewarms: std::sync::Arc<std::sync::Mutex<PrewarmCache>>,
}

/// v3 F1: search→fetch warm handoff store.
pub struct PrewarmCache {
    entries: HashMap<String, PrewarmEntry>,
}

pub struct PrewarmEntry {
    pub body: Vec<u8>,
    pub content_type: String,
    pub at: Instant,
}

const PREWARM_CAP: usize = 10;
const PREWARM_BODY_MAX: usize = 1_500_000;
const PREWARM_TTL: Duration = Duration::from_secs(600);

impl PrewarmCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn put(&mut self, url: &str, body: Vec<u8>, content_type: String) {
        if body.len() > PREWARM_BODY_MAX {
            return; // huge pages: extraction is cheap, RAM isn't
        }
        // Bound: evict oldest beyond cap.
        if self.entries.len() >= PREWARM_CAP
            && !self.entries.contains_key(url)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(
            url.to_string(),
            PrewarmEntry {
                body,
                content_type,
                at: Instant::now(),
            },
        );
    }

    /// One-shot: a served prewarm is consumed — the second
    /// fetch of the same URL goes to the network for freshness.
    pub fn take(&mut self, url: &str) -> Option<PrewarmEntry> {
        let e = self.entries.remove(url)?;
        (e.at.elapsed() < PREWARM_TTL).then_some(e)
    }
}

/// Per-engine outcome for honest reporting.
#[derive(Debug, Clone)]
pub struct EngineReport {
    pub engine: String,
    pub status: String,
    pub hits: usize,
    pub ms: u64,
    /// Which lane carried it (observability for the
    /// governor's routing decisions).
    pub egress: String,
}

#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<Merged>,
    pub weak: bool,
    pub intent: Intent,
    pub report: Vec<EngineReport>,
    pub cached: bool,
    pub elapsed: Duration,
    /// BYOK provider name (None = local search).
    pub provider: Option<String>,
    /// Cross-encoder reranking applied (feature on + model loaded).
    pub reranked: bool,
}

impl Searcher {
    pub fn new(fetcher: Fetcher, pool: EgressPool) -> Self {
        Self {
            fetcher,
            pool,
            trust: Mutex::new(HashMap::new()),
            cache: Mutex::new(load_cache_disk()),
            failures: Mutex::new(HashMap::new()),
            inflight: Mutex::new(std::collections::HashSet::new()),
            prewarms: std::sync::Arc::new(std::sync::Mutex::new(PrewarmCache::new())),
        }
    }

    /// v3 F1: warm-handoff store — filled by enrichment, drained
    /// by the fetch tool.
    pub fn prewarms(&self) -> &std::sync::Arc<std::sync::Mutex<PrewarmCache>> {
        &self.prewarms
    }

    /// Proxy preflight: probe every proxy at startup so
    /// dead lines are benched BEFORE a query ever gets
    /// assigned to them. Runs in the background; the first
    /// queries just use healthy lanes.
    pub fn preflight(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let proxies = this.pool.proxies();
            let total = proxies.len();
            let mut dead = 0usize;
            for proxy in proxies {
                let id = proxy.id();
                let probe = this.fetcher.fetch_once_via(
                    "https://api.ipify.org/",
                    &[],
                    Some(&proxy),
                    false,
                    None,
                );
                match tokio::time::timeout(Duration::from_secs(6), probe).await {
                    Ok(Ok(o)) if o.status == 200 => {}
                    Ok(Err(e)) if format!("{e}").contains("CONNECT -> 407") => {
                        this.pool.report_auth_fail(&id);
                    }
                    _ => {
                        dead += 1;
                        this.pool.report_dead(&id);
                    }
                }
            }
            // ALL proxies failing means the PROBE endpoint
            // died, not the pool — clear the marks rather
            // than bench every lane over our own bug.
            if total > 0 && dead == total {
                this.pool.revive_all();
            }
        });
    }

    /// True when an engine is benched for chronic failure.
    fn quarantined(&self, engine: &str) -> bool {
        const QUARANTINE_TTL: Duration = Duration::from_secs(600);
        let f = self.failures.lock().unwrap();
        matches!(f.get(engine), Some(&(n, at)) if n >= 3 && at.elapsed() < QUARANTINE_TTL)
    }

    fn record_outcome(&self, engine: &str, ok: bool) {
        let mut f = self.failures.lock().unwrap();
        if ok {
            f.remove(engine);
        } else {
            let e = f.entry(engine.to_string()).or_insert((0, Instant::now()));
            e.0 += 1;
            e.1 = Instant::now();
        }
    }

    pub async fn search(
        &self,
        query: &str,
        max_results: usize,
        forced_intent: Option<Intent>,
    ) -> Result<SearchOutcome, FetchError> {
        let started = Instant::now();
        let intent_probe = forced_intent.unwrap_or_else(|| intent::detect(query));
        let sf_key = format!("{}|{intent_probe:?}|{max_results}", norm_query(query));
        let leader = {
            let mut m = self.inflight.lock().unwrap();
            m.insert(sf_key.clone())
        };
        if !leader {
            // Follower: poll for the leader's cache write.
            // The leader publishes into the query cache on
            // completion, so followers read it from there.
            for _ in 0..120 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let hit = self
                    .cache
                    .lock()
                    .unwrap()
                    .get(&format!("{}|{intent_probe:?}", norm_query(query)))
                    .cloned();
                if let Some((at, cached, total)) = hit
                    && at.elapsed() < cache_ttl(intent_probe, query)
                {
                    let weak = rank::is_weak(&cached, total);
                    let mut results = cached.iter().take(max_results).cloned().collect();
                    site_filter(query, &mut results);
                    return Ok(SearchOutcome {
                        results,
                        weak,
                        intent: intent_probe,
                        report: Vec::new(),
                        cached: true,
                        elapsed: started.elapsed(),
                        provider: None,
                        reranked: crate::search::rerank::active(),
                    });
                }
            }
            // Leader died or timed out — compute ourselves.
        }
        let _inflight_guard = InflightGuard {
            map: &self.inflight,
            key: sf_key,
        };
        self.search_inner(query, max_results, forced_intent, started)
            .await
    }

    async fn search_inner(
        &self,
        query: &str,
        max_results: usize,
        forced_intent: Option<Intent>,
        started: Instant,
    ) -> Result<SearchOutcome, FetchError> {
        // Cache stores top-12; asking for more just
        // re-lists the same tail.
        let max_results = max_results.clamp(1, 12);
        let intent = forced_intent.unwrap_or_else(|| intent::detect(query));
        let cache_key = format!("{}|{intent:?}", norm_query(query));

        if let Some((at, cached, total)) = self.cache.lock().unwrap().get(&cache_key)
            && at.elapsed() < cache_ttl(intent, query)
        {
            let weak = rank::is_weak(cached, *total);
            let mut results = cached.iter().take(max_results).cloned().collect();
            site_filter(query, &mut results);
            return Ok(SearchOutcome {
                results,
                weak,
                intent,
                report: Vec::new(),
                cached: true,
                elapsed: started.elapsed(),
                provider: None,
                reranked: crate::search::rerank::active(),
            });
        }

        let engines = intent::engines_for(intent);
        let verticals = intent::verticals_for(intent, query);

        // Fan out: engines each get their own egress
        // (spreading is the anti-rate-limit move).
        let mut futures: Vec<TaskFut> = Vec::new();
        let mut used_egresses: Vec<String> = Vec::new();
        let mut queries: Vec<String> = vec![query.to_string()];
        if let Some(v) = intent::variant(query) {
            queries.push(v);
        }
        // Engines get the original query; the recall variant
        // goes only to the first two engines (top trust).
        let mut live: Vec<&str> = engines
            .iter()
            .filter(|e| !self.quarantined(e))
            .copied()
            .collect();
        // Rank engines by learned trust so width cuts drop
        // the weakest first.
        {
            let trust = self.trust.lock().unwrap();
            live.sort_by(|a, b| {
                trust
                    .get(*b)
                    .copied()
                    .unwrap_or(1.0)
                    .total_cmp(&trust.get(*a).copied().unwrap_or(1.0))
            });
        }
        // ── Adaptive fan-out width: the governor. Under
        // stress the system shrinks its appetite instead of
        // burning lanes — consensus survives at width 2 by
        // construction (two independent index families).
        let width = width_for_stress(self.pool.stress(), live.len());
        live.truncate(width);
        let mut assignments: Vec<(String, String)> = live
            .iter()
            .map(|e| (e.to_string(), query.to_string()))
            .collect();
        // Recall variants spend lanes — only when the
        // governor did NOT cut the roster (healthy pool).
        if queries.len() > 1 && self.pool.stress() < 0.15 {
            for e in live.iter().take(2) {
                assignments.push((e.to_string(), queries[1].clone()));
            }
        }

        // ── Egress assignment ──
        //
        // PROXY_AVERSE engines (brave, ddg) prefer the direct
        // lane because proxy IPs get CAPTCHA'd/429'd. Multiple
        // proxy-averse engines can share direct (with pacing).
        // Non-averse engines spread across proxies.
        //
        // We only exclude proxy egresses from reuse — direct
        // is shared, not exclusive.
        let has_proxies = self.pool.has_proxies();
        for (engine, q) in assignments {
            let Some(eg) = self.pool.pick(&engine, &used_egresses, true) else {
                break;
            };
            // Exclude proxy egresses (spread across proxies)
            // but NOT direct (multiple PROXY_AVERSE engines
            // share the direct lane with pacing).
            if has_proxies && eg.proxy.is_some() {
                used_egresses.push(eg.id.clone());
            }
            futures.push(Box::pin(engine_task(
                engine,
                q,
                eg.id,
                eg.proxy,
                &self.fetcher,
                &self.pool,
            )));
        }
        // Verticals: direct, friendly APIs.
        let verticals: Vec<&&str> = verticals.iter().filter(|v| !self.quarantined(v)).collect();
        for v in verticals {
            futures.push(Box::pin(vertical_task(
                v.to_string(),
                query.to_string(),
                &self.fetcher,
                None,
            )));
        }

        let outcomes = futures_util::future::join_all(futures).await;

        // ── Retry wave: failed engines get one more shot
        // through a fresh egress — but ONLY when the first
        // wave left the merge thin. A healthy merge never
        // pays retry latency; a degraded one recovers.
        let ok_engines = outcomes.iter().filter(|(_, r)| r.is_ok()).count();
        let ok_hits: usize = outcomes
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .map(|(h, _, _, _)| h.len())
            .sum();
        let merge_thin = ok_engines < 3 || ok_hits < 15;
        let failed: Vec<String> = if merge_thin {
            outcomes
                .iter()
                .filter(|(_, r)| matches!(r, Err((s, _, _)) if s != "no-results"))
                .map(|(e, _)| e.split('@').next().unwrap_or(e).to_string())
                .collect()
        } else {
            Vec::new()
        };
        let mut retry_futures: Vec<TaskFut> = Vec::new();
        for engine in &failed {
            let is_vertical = matches!(
                engine.as_str(),
                "github"
                    | "hn"
                    | "wikipedia"
                    | "scholar"
                    | "news"
                    | "arxiv"
                    | "stackexchange"
                    | "mdn"
            );
            if is_vertical {
                // Vertical retry rides a proxy egress (their
                // direct IP is what got rate-limited).
                let Some(eg) = self.pool.pick("github", &[], false) else {
                    continue;
                };
                retry_futures.push(Box::pin(vertical_task(
                    engine.clone(),
                    query.to_string(),
                    &self.fetcher,
                    eg.proxy,
                )));
                continue;
            }
            // ddg's html endpoint is the fallback when lite fails.
            let retry_engine = if engine == "ddg" { "ddg_html" } else { engine };
            let Some(eg) = self.pool.pick(engine, &used_egresses, true) else {
                continue;
            };
            retry_futures.push(Box::pin(engine_task(
                retry_engine.to_string(),
                query.to_string(),
                eg.id,
                eg.proxy,
                &self.fetcher,
                &self.pool,
            )));
        }
        let retry_outcomes = if retry_futures.is_empty() {
            Vec::new()
        } else {
            tokio::time::timeout(
                Duration::from_secs(3),
                futures_util::future::join_all(retry_futures),
            )
            .await
            .unwrap_or_default()
        };

        let mut per_engine: Vec<(String, Vec<engines::Hit>)> = Vec::new();
        let mut report = Vec::new();
        let all: Vec<(String, EngineResult)> = outcomes.into_iter().chain(retry_outcomes).collect();
        for (engine, outcome) in all {
            match outcome {
                Ok((hits, ms, egress_id, was_engine)) => {
                    let base = engine.split('_').next().unwrap_or(&engine);
                    self.record_outcome(base, true);
                    if was_engine {
                        self.pool.report_ok(base, &egress_id);
                        self.bump_trust(base, true);
                    }
                    report.push(EngineReport {
                        engine: engine.clone(),
                        status: "ok".into(),
                        hits: hits.len(),
                        ms,
                        egress: egress_id.clone(),
                    });
                    per_engine.push((engine, hits));
                }
                Err((status, egress_id, was_engine)) => {
                    let base = engine.split('_').next().unwrap_or(&engine);
                    // Dead proxies are egress failures, not
                    // engine failures — don't quarantine.
                    if !status.starts_with("dead")
                        && status != "auth-fail"
                        && status != "no-results"
                    {
                        self.record_outcome(base, false);
                    }
                    if was_engine {
                        if status.starts_with("dead") {
                            self.pool.report_dead(&egress_id);
                        } else if status == "auth-fail" {
                            self.pool.report_auth_fail(&egress_id);
                        } else if status != "no-results" {
                            self.pool.report_blocked(base, &egress_id);
                            self.bump_trust(base, false);
                        }
                    }
                    report.push(EngineReport {
                        engine,
                        status,
                        hits: 0,
                        ms: 0,
                        egress: egress_id,
                    });
                }
            }
        }

        if per_engine.is_empty() {
            return Err(FetchError::Http(format!(
                "search: all engines failed — {}",
                report
                    .iter()
                    .map(|r| format!("{}:{}", r.engine, r.status))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        let trust = self.trust.lock().unwrap().clone();
        let total = rank::merged_total(&per_engine);
        // Always merge 12 results for the cache, then trim to
        // max_results for the response. Without this, a first
        // search with max=2 caches only 2 results, and a later
        // search with max=10 returns the stale 2 from cache.
        let mut results = rank::merge(&per_engine, query, intent, &trust, 12);
        let weak = rank::is_weak(&results, total);

        // ── Result enrichment: prefetch top results to extract
        // real <title> and <meta description> from the actual
        // pages. Richer than SERP snippets, filters dead links.
        // The genius feature: results carry the page's own title
        // and description, not the SERP's truncated version.
        self.enrich_results(&mut results).await;

        // ── site: operator enforcement: engines don't strictly
        // respect `site:domain.com` — some results leak through
        // from other domains. Filter them out post-merge so the
        // agent only gets results from the requested domain.
        site_filter(query, &mut results);
        // Poisoning guard: a merge built while engines
        // were down must NOT persist for 30 minutes —
        // degraded-period results expire with the moment.
        let cacheable = ok_engines >= 2 && total >= 8;
        if cacheable {
            let mut cache = self.cache.lock().unwrap();
            // LRU-ish cap: drop oldest when full.
            if cache.len() >= 500
                && let Some(oldest) = cache
                    .iter()
                    .max_by_key(|(_, (at, _, _))| at.elapsed())
                    .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
            cache.insert(
                cache_key,
                (
                    Instant::now(),
                    results.iter().take(12).cloned().collect(),
                    total,
                ),
            );
            save_cache_disk(&cache);
        }

        Ok(SearchOutcome {
            results: results.into_iter().take(max_results).collect(),
            weak,
            intent,
            report,
            cached: false,
            elapsed: started.elapsed(),
            provider: None,
            reranked: crate::search::rerank::active(),
        })
    }

    fn bump_trust(&self, base_engine: &str, ok: bool) {
        let mut trust = self.trust.lock().unwrap();
        let t = trust.entry(base_engine.to_string()).or_insert(1.0);
        let target = if ok { 1.2 } else { 0.3 };
        *t = (*t * 0.7 + target * 0.3).clamp(0.2, 2.0);
    }

    /// Enrich top results by prefetching destination pages.
    ///
    /// Extracts real <title> and <meta name="description">
    /// from the actual page HTML — richer than any SERP
    /// snippet. Dead links (404/timeout) get demoted 50%.
    /// Pages behind bot walls are left untouched (still
    /// valid results, agent fetches via tier 2).
    ///
    /// This is what makes our search better than any
    /// individual engine: results carry the page's own
    /// title and description, not the SERP's truncated
    /// version. Works even when SERP parsers return empty
    /// snippets. Dead links that rank well are demoted.
    async fn enrich_results(&self, results: &mut [Merged]) {
        const ENRICH_TOP: usize = 5;
        const ENRICH_TIMEOUT: Duration = Duration::from_secs(4);

        let n = results.len().min(ENRICH_TOP);
        if n == 0 {
            return;
        }

        // Spawn parallel fetches for top N results.
        let fetcher = &self.fetcher;
        type EnrichFut<'a> = std::pin::Pin<
            Box<
                dyn std::future::Future<Output = (usize, Option<String>, Option<String>)>
                    + Send
                    + 'a,
            >,
        >;
        let prewarms = self.prewarms.clone();
        let mut futures: Vec<EnrichFut> = Vec::new();
        for (i, r) in results.iter().take(n).enumerate() {
            let url = r.url.clone();
            let sink = prewarms.clone();
            futures.push(Box::pin(async move {
                let out = tokio::time::timeout(
                    ENRICH_TIMEOUT,
                    fetcher.fetch_once_via(&url, &[], None, false, None),
                )
                .await;
                match out {
                    Err(_) | Ok(Err(_)) => (i, None, None),
                    Ok(Ok(o)) => {
                        // Dead link (4xx/5xx) → demote.
                        if o.status >= 400 {
                            return (i, None, None);
                        }
                        // Bot wall (200 but not ContentOk) →
                        // don't enrich, don't demote.
                        if !matches!(o.verdict, Verdict::ContentOk) {
                            return (i, None, Some(String::new()));
                        }
                        let ct = o
                            .headers
                            .iter()
                            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default();
                        let html = crate::extract::charset::decode(&o.body, &ct);
                        let title = extract_title(&html);
                        let desc = extract_description(&html);
                        // v3 F1: keep the body for the warm handoff.
                        sink.lock().unwrap().put(&url, o.body.clone(), ct);
                        (i, title, desc)
                    }
                }
            }));
        }

        let enriched = futures_util::future::join_all(futures).await;

        for (i, title, desc) in enriched {
            if i >= results.len() {
                continue;
            }
            let r = &mut results[i];
            match (&title, &desc) {
                (None, None) => {
                    // Dead link — demote 50%.
                    r.score *= 0.5;
                }
                (None, Some(d)) if d.is_empty() => {
                    // Bot wall — leave untouched.
                }
                _ => {
                    if let Some(t) = title {
                        let bad =
                            |t: &str| t.contains(" › ") || t.starts_with("http") || t.len() < 3;
                        if !bad(&t) && (bad(&r.title) || t.len() > r.title.len()) {
                            r.title = t;
                        }
                    }
                    if let Some(d) = desc
                        && !d.is_empty()
                        && d.len() > r.snippet.len()
                    {
                        r.snippet = d;
                    }
                }
            }
        }

        // Re-sort after enrichment (dead links demoted).
        results.sort_by(|a, b| b.score.total_cmp(&a.score));
    }
}

type EngineResult = Result<(Vec<engines::Hit>, u64, String, bool), (String, String, bool)>;

type TaskFut<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = (String, EngineResult)> + Send + 'a>>;

async fn engine_task(
    engine: String,
    query: String,
    egress_id: String,
    proxy: Option<crate::transport::proxy::Proxy>,
    fetcher: &Fetcher,
    pool: &EgressPool,
) -> (String, EngineResult) {
    let label = engine.clone();
    pool.pace(&engine, &egress_id).await;
    let started = Instant::now();
    let Some(url) = engines::serp_url(&engine, &query) else {
        return (label, Err(("no-url".into(), egress_id, true)));
    };
    let out = match tokio::time::timeout(
        ENGINE_TIMEOUT,
        fetcher.fetch_once_via(&url, &[], proxy.as_ref(), false, None),
    )
    .await
    {
        Err(_) => return (label, Err(("timeout".into(), egress_id, true))),
        Ok(Err(e)) => {
            let status = match &e {
                FetchError::Timeout => "timeout",
                FetchError::Http(m) if m.contains("CONNECT -> 407") => "auth-fail",
                FetchError::Http(m) if m.contains("CONNECT") => "dead-proxy",
                _ => "net",
            };
            return (label, Err((status.into(), egress_id, true)));
        }
        Ok(Ok(o)) => o,
    };
    let ms = started.elapsed().as_millis() as u64;
    if out.status == 429 || !matches!(out.verdict, Verdict::ContentOk) {
        return (
            label,
            Err((format!("blocked:{}", out.status), egress_id, true)),
        );
    }
    let html = crate::extract::charset::decode(
        &out.body,
        out.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or(""),
    );
    let hits = engines::parse(&engine, &html);
    if hits.len() < 3 {
        // Honest "no results" is NOT an engine failure —
        // don't burn trust/lanes for a dry query.
        let lower = html.to_lowercase();
        let dry = lower.contains("no results")
            || lower.contains("did not match any")
            || lower.contains("no good results")
            || lower.contains("nothing found");
        let status = if dry { "no-results" } else { "empty-parse" };
        return (label, Err((status.into(), egress_id, true)));
    }
    (label, Ok((hits, ms, egress_id, true)))
}

async fn vertical_task(
    vertical: String,
    query: String,
    fetcher: &Fetcher,
    proxy: Option<crate::transport::proxy::Proxy>,
) -> (String, EngineResult) {
    let started = Instant::now();
    match tokio::time::timeout(
        ENGINE_TIMEOUT,
        verticals::run(fetcher, &vertical, &query, proxy.as_ref()),
    )
    .await
    {
        Err(_) => (vertical, Err(("timeout".into(), "direct".into(), false))),
        Ok(Err(e)) => (vertical, Err((format!("{e}"), "direct".into(), false))),
        Ok(Ok(hits)) => {
            let ms = started.elapsed().as_millis() as u64;
            (vertical, Ok((hits, ms, "direct".into(), false)))
        }
    }
}

/// Disk cache path (ghost-state pattern).
fn cache_path() -> Option<std::path::PathBuf> {
    let dir = dirs_cache()?;
    Some(dir.join("search-cache.json"))
}

fn dirs_cache() -> Option<std::path::PathBuf> {
    let dir = crate::paths::cache_dir();
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// On disk: (key, age_secs, results, total) — age lets us
/// re-base Instant across process restarts.
fn save_cache_disk(cache: &HashMap<String, (Instant, Vec<Merged>, usize)>) {
    let Some(path) = cache_path() else { return };
    let now = Instant::now();
    let entries: Vec<(String, u64, Vec<Merged>, usize)> = cache
        .iter()
        .map(|(k, (at, r, t))| {
            (
                k.clone(),
                now.saturating_duration_since(*at).as_secs(),
                r.clone(),
                *t,
            )
        })
        .collect();
    if let Ok(json) = serde_json::to_string(&entries) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

fn load_cache_disk() -> HashMap<String, (Instant, Vec<Merged>, usize)> {
    let mut map = HashMap::new();
    let Some(path) = cache_path() else { return map };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return map;
    };
    let Ok(entries) = serde_json::from_str::<Vec<(String, u64, Vec<Merged>, usize)>>(&raw) else {
        return map;
    };
    for (key, age, results, total) in entries {
        // TTL is intent + recency keyed (the query text
        // is the key's first segment).
        let (qpart, ipart) = key.rsplit_once('|').unwrap_or((key.as_str(), ""));
        let intent = match ipart {
            "News" => Intent::News,
            "Code" => Intent::Code,
            "Paper" => Intent::Paper,
            "Entity" => Intent::Entity,
            _ => Intent::Web,
        };
        let ttl = cache_ttl(intent, qpart);
        if Duration::from_secs(age) < ttl {
            map.insert(
                key,
                (Instant::now() - Duration::from_secs(age), results, total),
            );
        }
    }
    map
}

/// Governor: fan-out width under stress. Healthy pool →
/// all engines; stressed → shrink appetite (you can't be
/// rate-limited if you never exceed the rate); starved →
/// top engine + verticals. Consensus survives at width 2
/// by construction (two independent index families).
fn width_for_stress(stress: f64, available: usize) -> usize {
    if stress < 0.15 {
        available
    } else if stress < 0.40 {
        available.min(3)
    } else if stress < 0.65 {
        available.min(2)
    } else {
        available.min(1)
    }
}

/// Enforce `site:domain.com` operator: extract the target domain
/// from the query and remove results whose host doesn't match.
/// Engines (especially Bing/DDG) don't strictly respect `site:` —
/// they often inject related results from other domains. This
/// post-merge filter ensures the agent only sees results from the
/// requested domain.
///
/// Matches `domain.com` and any subdomain `*.domain.com`.
/// Case-insensitive. Strips `www.` prefix before comparison.
fn site_filter(query: &str, results: &mut Vec<Merged>) {
    let q = query.to_lowercase();
    let mut site_domain: Option<String> = None;
    for token in q.split_whitespace() {
        if let Some(rest) = token.strip_prefix("site:") {
            let domain = rest.trim_end_matches('/');
            if !domain.is_empty() {
                site_domain = Some(domain.to_string());
                break;
            }
        }
    }
    let Some(domain) = site_domain else {
        return;
    };
    // Use raw host (not host_of which strips www.) so
    // `site:www.wikipedia.org` matches only www., while
    // `site:wikipedia.org` matches all subdomains.
    results.retain(|r| {
        let host = url::Url::parse(&r.url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
            .unwrap_or_default();
        host == domain || host.ends_with(&format!(".{domain}"))
    });
}

/// Snippet budget for the markdown list. 120 cut mid-phrase far
/// too often — the detail that distinguishes two results sat just
/// past the cut, and the agent paid a whole fetch to learn what the
/// snippet nearly said. 200 is where a snippet reliably carries one
/// complete claim; the 300 the JSON keeps is past diminishing
/// returns at ~45 tokens per result.
const SNIPPET_CHARS: usize = 200;

/// Below this fraction (4/5) of the budget, a word-boundary cut
/// throws away more than it saves — see `clip_snippet`.
const CLIP_FLOOR_NUM: usize = 4;
const CLIP_FLOOR_DEN: usize = 5;

/// Trailing marks dropped before the ellipsis: each one JOINS
/// clauses, so ending on it reads as a typo rather than a cut.
///
/// Sentence terminators (. ! ? 。！？) are deliberately KEPT: a cut
/// that lands after one means the snippet ended at a complete
/// sentence, and saying so is worth more than tidiness. Stripping
/// them would make a clean ending look like a severed one.
/// CJK marks are the same codepoints in Chinese and Japanese, so
/// one list serves both: 、and ，join clauses, 《》【】「」（ open
/// spans. 。！？ are absent on purpose — they end sentences.
const CLIP_TRIM: &[char] = &[
    ',', ';', ':', '-', '–', '—', '(', '[', '{', '/', '|', '…', '、', '，', '；', '：', '（', '「',
    '『', '《', '〈', '【', '〔', '［', '｛', '·', '／', '｜', '〜',
];

/// Truncate to `max` chars on a word boundary, marking the cut with
/// an ellipsis ONLY when text was actually dropped.
///
/// Trims back, never extends: extending to finish the straddling
/// word would make the output size unbounded by `max` (one long
/// token and a "200-char snippet" is 280), and the fragment dropped
/// is a partial word the agent cannot use anyway.
///
/// The 4/5 floor bounds the pathological case: a long URL, hash or
/// compound word straddling the boundary would otherwise back off to
/// almost nothing, which is worse than a mid-word cut the ellipsis
/// already flags.
fn clip_snippet(s: &str, max: usize) -> String {
    // Materialized rather than iterated because the window is read
    // three ways: indexed (chars[max]), scanned BACKWARDS for the
    // last space, and sliced for the head. `Chars` cannot be
    // rewound, so an iterator version re-decodes UTF-8 from the
    // start once per pass — and `rposition` is not even available
    // on it (it needs ExactSizeIterator, which `Chars` is not),
    // leaving manual position bookkeeping. Decode once, index
    // freely.
    //
    // Char positions, not &str byte offsets, for the same reason:
    // the budget and the floor are counted in chars, so `rfind`'s
    // byte index would need converting before every comparison —
    // and mixing the two on multi-byte text is where UTF-8 bugs
    // breed.
    //
    // max + 1 and no further: the only index past the window we
    // inspect is chars[max], the "does the next char end a word?"
    // test. Collecting the whole string would allocate 4 bytes a
    // char for input we discard — BYOK snippets carry raw page
    // text and run to thousands of chars.
    let chars: Vec<char> = s.chars().take(max + 1).collect();
    if chars.len() <= max {
        return s.to_string();
    }
    // The char PAST the window decides whether the window already
    // ends cleanly. If it is whitespace, the last word inside is
    // whole and backing off would drop a complete word for nothing.
    let cut = if chars[max].is_whitespace() {
        max
    } else {
        match chars[..max].iter().rposition(|c| c.is_whitespace()) {
            Some(pos) if pos * CLIP_FLOOR_DEN >= max * CLIP_FLOOR_NUM => pos,
            _ => max,
        }
    };
    let mut head: String = chars[..cut].iter().collect();
    // trim_end_matches only slices — it is the `.to_string()` that
    // would copy. Truncating to the trimmed length shortens in
    // place instead, leaving one allocation for the whole function.
    let keep = head
        .trim_end_matches(|c: char| c.is_whitespace() || CLIP_TRIM.contains(&c))
        .len();
    head.truncate(keep);
    // Only whitespace left behind means nothing was really dropped;
    // an ellipsis there would promise content that does not exist.
    // Iterates the ORIGINAL string, not the bounded window: a
    // Vec capped at max + 1 cannot answer "is everything after
    // the cut whitespace?". `all` short-circuits on the first
    // non-whitespace, so this is O(1) in practice.
    if s.chars().skip(cut).all(char::is_whitespace) {
        return head;
    }
    format!("{head}…")
}

/// Markdown rendering for the MCP/CLI surface.
pub fn render_markdown(
    out: &SearchOutcome,
    query: &str,
    handles: Option<&[String]>,
    hints: &[Option<String>],
) -> String {
    // Search answers ONE question: "what should I fetch?"
    // Snippets carry just enough to decide — content is
    // the fetch tool's job.
    let mut md = format!("# Search: {query}\n\n");
    for (i, r) in out.results.iter().enumerate() {
        let host = rank::host_of(&r.url);
        md.push_str(&format!("{}. **{}** — {}\n", i + 1, r.title, host));
        if !r.snippet.is_empty() {
            let snip = clip_snippet(&r.snippet, SNIPPET_CHARS);
            md.push_str(&format!("   {snip}\n"));
        }
        // v3 handles: the position handle (S1, S2, …) replaces the
        // raw URL — `fetch S3` is worth more than 80 tokens of URL.
        match handles {
            Some(hs) if let Some(h) = hs.get(i) => {
                // v3 F2: a known-walled domain carries its route
                // cost — pick a faster source or budget time
                // BEFORE spending the fetch.
                match hints.get(i).and_then(|h| h.as_deref()) {
                    Some(hint) => md.push_str(&format!("   {h} {hint}\n")),
                    None => md.push_str(&format!("   {h}\n")),
                }
            }
            _ => {
                md.push_str(&format!("   {}\n", r.url));
            }
        }
        // Provenance, text-side. Which engines returned a URL is the
        // signal that separates two equally plausible results: three
        // independent indexes agreeing usually means canonical, a
        // lone vertical hit often means tangential. Until now it
        // existed only in structuredContent, so a client that drops
        // that field could not tell the two apart.
        //
        // NAMES, not a count: `consensus` in the JSON is
        // sources.len(), which double-counts an engine that returned
        // the URL at two ranks (live: ddg, yahoo, yahoo, brave = 4
        // for 3 engines). Ranking counts index FAMILIES instead, so
        // deduped names are both cheaper to read and more honest
        // than the number — and they say WHICH source, which a count
        // never can.
        let mut engines: Vec<&str> = Vec::new();
        for (engine, _) in &r.sources {
            if !engines.contains(&engine.as_str()) {
                engines.push(engine);
            }
        }
        if !engines.is_empty() {
            // 2dp, not the JSON's 3: this is a blended heuristic, and
            // 0.831 reads like a measurement.
            md.push_str(&format!(
                "   engines: {} · score: {:.2}\n",
                engines.join(", "),
                r.score
            ));
        }
    }
    if out.weak {
        md.push_str("\n*weak results: low cross-engine consensus — treat with care*\n");
    }
    // Zero hits is a success-shaped answer with nothing in it —
    // tell the agent which levers exist instead of leaving it
    // staring at an empty list.
    if out.results.is_empty() {
        md.push_str(
            "\n*0 results — try a simpler query, a different intent (news/code/paper), \
or add an API-key provider (`donsetch keys add`)*\n",
        );
    }
    let source = out.provider.as_deref().unwrap_or("local engine");
    md.push_str(&format!(
        "\n*{} results in {}ms via {}*\n",
        out.results.len(),
        out.elapsed.as_millis(),
        source
    ));
    // v3: degraded engines are named, never silently fewer. A merge
    // built while engines were down must never pass as full-strength.
    let failed: Vec<String> = out
        .report
        .iter()
        .filter(|r| r.status != "ok")
        .map(|r| format!("{}: {}", r.engine, r.status))
        .collect();
    if !failed.is_empty() {
        md.push_str(&format!(
            "*degraded: {}/{} engines ok ({}) — results may skew*\n",
            out.report.len() - failed.len(),
            out.report.len(),
            failed.join(", ")
        ));
    }
    if handles.is_some() && !out.results.is_empty() {
        md.push_str("*fetch S1… by handle (raw urls in structuredContent)*\n");
    }
    md
}

/// structuredContent metadata.
pub fn render_meta(out: &SearchOutcome) -> Value {
    json!({
        "intent": format!("{:?}", out.intent),
        "weak": out.weak,
        "cached": out.cached,
        "elapsed_ms": out.elapsed.as_millis() as u64,
        "provider": out.provider,
        "rerank": if out.reranked { "on" } else { "off (RRF+BM25 fallback)" },
        "results": out.results.iter().map(|r| json!({
            "title": r.title,
            "url": r.url,
            "snippet": r.snippet.chars().take(300).collect::<String>(),
            "score": (r.score * 1000.0).round() / 1000.0,
            "consensus": r.sources.len(),
            "engines": r.sources.iter().map(|(e, _)| e.clone()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "engines": out.report.iter().map(|r| json!({
            "engine": r.engine, "status": r.status, "hits": r.hits, "ms": r.ms,
            "egress": if r.egress == "direct" { "direct".to_string() } else if r.egress == "byok" { "byok".to_string() } else { "proxy".to_string() },
        })).collect::<Vec<_>>(),
    })
}

/// Extract <title> from raw HTML.
fn extract_title(html: &str) -> Option<String> {
    let doc = scraper::Html::parse_document(html);
    let sel = Selector::parse("title").ok()?;
    doc.select(&sel)
        .next()
        .map(|e| e.text().collect::<Vec<_>>().join(" ").trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Extract <meta name="description"> (or og:description)
/// from raw HTML.
fn extract_description(html: &str) -> Option<String> {
    let doc = scraper::Html::parse_document(html);
    let sel =
        Selector::parse(r#"meta[name="description"], meta[property="og:description"]"#).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|e| e.value().attr("content"))
        .map(|s| s.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Removes the inflight key when the leader finishes
/// (success or failure) so the set never grows unbounded.
struct InflightGuard<'a> {
    map: &'a Mutex<std::collections::HashSet<String>>,
    key: String,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.map.lock().unwrap().remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn governor_shrinks_width_under_stress() {
        assert_eq!(width_for_stress(0.05, 4), 4);
        assert_eq!(width_for_stress(0.30, 4), 3);
        assert_eq!(width_for_stress(0.50, 4), 2);
        assert_eq!(width_for_stress(0.90, 4), 1);
        assert_eq!(width_for_stress(0.90, 0), 0);
    }

    #[test]
    fn norm_query_collapses_variants() {
        assert_eq!(
            norm_query("Rust Async: the Runtime, comparison!"),
            norm_query("rust async runtime comparison")
        );
        assert_ne!(norm_query("kafka vs nats"), norm_query("kafka"));
    }

    #[test]
    fn cache_ttl_is_intent_and_recency_aware() {
        assert!(cache_ttl(Intent::News, "anything") < cache_ttl(Intent::Web, "anything"));
        assert!(cache_ttl(Intent::Code, "anything") < cache_ttl(Intent::Web, "anything"));
        // recency signal forces news-grade TTL even for web intent
        assert_eq!(
            cache_ttl(Intent::Web, "nepal inflation 2026 rate"),
            cache_ttl(Intent::News, "x")
        );
        assert_eq!(
            cache_ttl(Intent::Web, "rust ownership explained"),
            Duration::from_secs(1800)
        );
    }

    // ── snippet truncation ───────────────────────────────────
    // A small budget keeps the expectations readable; the logic
    // is identical at SNIPPET_CHARS.

    #[test]
    fn clip_leaves_short_snippets_alone() {
        assert_eq!(clip_snippet("short one", 20), "short one");
    }

    #[test]
    fn clip_leaves_exactly_full_snippets_unmarked() {
        let exact = "a".repeat(20);
        // Nothing dropped, so no ellipsis may be promised.
        assert_eq!(clip_snippet(&exact, 20), exact);
    }

    #[test]
    fn clip_keeps_whole_window_when_next_char_is_space() {
        // The 21st char is a space: the window already ends on a
        // word boundary, so backing off would drop "ddddd" for
        // nothing. This is the edge case a naive "last space
        // inside the window" rule gets wrong.
        assert_eq!(
            clip_snippet("aaaa bbbb cccc ddddd eee", 20),
            "aaaa bbbb cccc ddddd…"
        );
    }

    #[test]
    fn clip_omits_ellipsis_when_only_whitespace_follows() {
        assert_eq!(
            clip_snippet("aaaa bbbb cccc ddddd ", 20),
            "aaaa bbbb cccc ddddd"
        );
    }

    #[test]
    fn clip_backs_off_to_word_boundary() {
        // Straddling word, last space at 16 of 20 — exactly the
        // 4/5 floor, so the partial word goes.
        assert_eq!(
            clip_snippet("aaaaaaaaaaaaaaaa bbbbbbbbbb", 20),
            "aaaaaaaaaaaaaaaa…"
        );
    }

    #[test]
    fn clip_hard_cuts_just_below_the_floor() {
        // Same shape, space one char earlier (15 of 20): below the
        // floor, so a mid-word cut beats losing a quarter of the
        // budget.
        assert_eq!(
            clip_snippet("aaaaaaaaaaaaaaa bbbbbbbbbb", 20),
            "aaaaaaaaaaaaaaa bbbb…"
        );
    }

    #[test]
    fn clip_hard_cuts_when_backing_off_would_cost_too_much() {
        // Last space at 4 of 20: backing off would return a
        // quarter of the budget. A mid-word cut the ellipsis
        // flags is the better trade.
        assert_eq!(
            clip_snippet("aaaa bbbbbbbbbbbbbbbbbbbb", 20),
            "aaaa bbbbbbbbbbbbbbb…"
        );
    }

    #[test]
    fn clip_hard_cuts_a_single_long_token() {
        assert_eq!(
            clip_snippet(&"x".repeat(30), 20),
            format!("{}…", "x".repeat(20))
        );
    }

    #[test]
    fn clip_strips_joining_punctuation_before_the_ellipsis() {
        // Without the trim this reads "aaaaaaaaaaaaaaa,…", which
        // looks like a typo rather than a truncation.
        assert_eq!(
            clip_snippet("aaaaaaaaaaaaaaa, bbbbbbbbbb", 20),
            "aaaaaaaaaaaaaaa…"
        );
    }

    #[test]
    fn clip_keeps_a_sentence_terminator() {
        // A cut landing after '.' means the snippet ended on a
        // COMPLETE sentence — stripping it would make a clean
        // ending look severed.
        assert_eq!(
            clip_snippet("Tokio is a runtime. Axum builds on it.", 20),
            "Tokio is a runtime.…"
        );
    }

    /// Sōseki's opening line — 3-byte chars, and no spaces at
    /// all, which is the real reason Japanese is the right test:
    /// there is no word boundary to back off to, so every cut is
    /// a hard cut and byte slicing would panic outright.
    const JA: &str = "「吾輩は猫である。名前はまだ無い。どこで生れたかとんと見当がつかぬ。何でも薄暗いじめじめした所でニャーニャー泣いていた事だけは記憶している。」";

    #[test]
    fn clip_counts_chars_not_bytes() {
        assert_eq!(
            clip_snippet(JA, 20),
            "「吾輩は猫である。名前はまだ無い。どこで…"
        );
    }

    #[test]
    fn clip_keeps_a_cjk_sentence_terminator() {
        // Cut lands right after 。 (U+3002) — the same codepoint
        // in Chinese and Japanese. It ends a sentence, so it stays.
        assert_eq!(clip_snippet(JA, 17), "「吾輩は猫である。名前はまだ無い。…");
    }

    #[test]
    fn clip_keeps_chinese_terminators_and_strips_chinese_separators() {
        // ！ (U+FF01) ends a sentence — kept.
        assert_eq!(
            clip_snippet("这是一个测试。第二句话！第三句", 12),
            "这是一个测试。第二句话！…"
        );
        // 、 (U+3001) is the enumeration comma — it joins, so it
        // goes rather than dangling before the ellipsis.
        assert_eq!(clip_snippet("第一项、第二项、第三项", 8), "第一项、第二项…");
    }

    fn outcome(results: Vec<Merged>) -> SearchOutcome {
        SearchOutcome {
            results,
            weak: false,
            intent: Intent::Web,
            report: Vec::new(),
            cached: false,
            elapsed: Duration::from_millis(10),
            provider: None,
            reranked: false,
        }
    }

    #[test]
    fn markdown_names_engines_and_score_per_result() {
        let mut r = merged("https://tokio.rs/");
        r.sources = vec![("bing".into(), 0), ("ddg".into(), 2)];
        r.score = 0.8312;
        let md = render_markdown(&outcome(vec![r]), "rust async", None, &[]);
        assert!(
            md.contains("engines: bing, ddg · score: 0.83"),
            "provenance line missing:\n{md}"
        );
    }

    #[test]
    fn markdown_dedupes_repeated_engines() {
        // One engine returning the same URL at two ranks must not
        // read as extra consensus — the JSON's `consensus` count
        // does exactly that.
        let mut r = merged("https://tokio.rs/");
        r.sources = vec![
            ("ddg".into(), 0),
            ("yahoo".into(), 1),
            ("yahoo".into(), 4),
            ("brave".into(), 2),
        ];
        let md = render_markdown(&outcome(vec![r]), "rust async", None, &[]);
        assert!(md.contains("engines: ddg, yahoo, brave"), "{md}");
        assert_eq!(md.matches("yahoo").count(), 1, "must dedupe:\n{md}");
    }

    fn merged(url: &str) -> Merged {
        Merged {
            title: "test".into(),
            url: url.into(),
            snippet: "test".into(),
            sources: vec![("bing".into(), 0)],
            score: 1.0,
            published: None,
        }
    }

    #[test]
    fn site_filter_removes_non_matching() {
        let mut results = vec![
            merged("https://stackoverflow.com/questions/123"),
            merged("https://github.com/owner/repo"),
            merged("https://stackoverflow.com/a/456"),
            merged("https://blog.example.com/post"),
            merged("https://docs.stackoverflow.com/faq"),
        ];
        site_filter("rust site:stackoverflow.com", &mut results);
        assert_eq!(
            results.len(),
            3,
            "should keep SO + subdomain, drop github + example.com"
        );
        assert!(results.iter().all(|r| r.url.contains("stackoverflow.com")));
    }

    #[test]
    fn site_filter_noop_without_operator() {
        let mut results = vec![
            merged("https://stackoverflow.com/q/1"),
            merged("https://github.com/owner/repo"),
        ];
        site_filter("rust async runtime", &mut results);
        assert_eq!(results.len(), 2, "no site: operator = no filtering");
    }

    #[test]
    fn site_filter_matches_subdomains() {
        let mut results = vec![
            merged("https://docs.python.org/3/library"),
            merged("https://python.org/about"),
            merged("https://github.com/python/cpython"),
        ];
        site_filter("asyncio site:python.org", &mut results);
        assert_eq!(results.len(), 2, "should match domain + subdomains");
    }

    #[test]
    fn site_filter_strips_www_prefix() {
        let mut results = vec![
            merged("https://www.wikipedia.org/wiki/Rust"),
            merged("https://en.wikipedia.org/wiki/Rust"),
            merged("https://github.com/rust-lang/rust"),
        ];
        site_filter("rust site:www.wikipedia.org", &mut results);
        assert_eq!(results.len(), 1, "www.wikipedia.org matches www. only");
    }
}
