//! DonSeek : keyless multi-engine search.
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
mod enrich;
mod persist;
mod render;
mod tasks;

pub use render::{render_compact_markdown, render_markdown, render_meta};

use enrich::PrewarmCache;
use persist::{load_cache_disk, load_health_disk, save_cache_disk, save_health_disk_if_dirty};
use tasks::{EngineResult, TaskFut, engine_task, ghost_engine_task, vertical_task};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::FetchError;
use crate::fetch::client::Fetcher;

use egress::EgressPool;
use intent::Intent;
use rank::Merged;

const ENGINE_TIMEOUT: Duration = Duration::from_secs(8);
const RETRY_TIMEOUT: Duration = Duration::from_secs(3);

/// Chronic-failure bench time. A walled engine stops wasting a
/// fan-out slot for this long after 3 consecutive strikes.
const QUARANTINE_TTL: Duration = Duration::from_secs(600);

/// Intent + recency-aware cache TTL. Every cached query
/// is a query that never touches an egress : the #1 rate
/// reducer. But a cached answer presented as fresh is
/// WORSE than honest latency when the world moved:
/// time-sensitive queries (even outside news intent :
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
    ];
    let q = query.to_lowercase();
    if RECENCY.iter().any(|s| q.contains(s)) || recency_year_in(&q) {
        return Duration::from_secs(300);
    }
    match intent {
        Intent::News => Duration::from_secs(300),
        Intent::Code => Duration::from_secs(900),
        _ => Duration::from_secs(1800),
    }
}

/// Year mentions inside the [current-2, current+1] window are
/// time-sensitive ("inflation 2026"); outside years ("cars 1998",
/// "medieval 1400") cache normally. Generated from the clock so the
/// window rolls forward automatically (was a hardcoded list that
/// would rot in 2028).
fn recency_year_in(q: &str) -> bool {
    let y = current_year();
    ((y - 2)..=(y + 1)).any(|yy| q.contains(&yy.to_string()))
}

/// UTC calendar year without a date dependency: days since epoch ->
/// civil year (Howard Hinnant's algorithm).
fn current_year() -> i64 {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    era * 400 + yoe + 1
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

/// Cache key for BYOK provider results (issue #195). The `byok|`
/// prefix keeps them out of the local path's `query|intent` slot, so
/// switching the `default` between local and a provider can never
/// serve one path's results under the other's name.
fn byok_cache_key(query: &str, intent: Intent) -> String {
    format!("byok|{}|{}", norm_query(query), intent.code())
}

/// Whether a failure `status` reflects the engine actually behaving
/// badly (worth quarantining via `record_outcome` and eroding trust
/// via `bump_trust`), as opposed to infra noise -- a dead egress
/// (`"dead-proxy"`), a BYOK key problem (`"auth-fail"`), or simply no
/// results (`"no-results"`) -- none of which are the engine's fault.
/// A single predicate so quarantine and trust tracking can't drift
/// out of sync with each other again.
fn is_engine_fault(status: &str) -> bool {
    !status.starts_with("dead")
        && status != "auth-fail"
        && status != "no-results"
        && status != "invalid-config"
        && status != "pacing-timeout"
}

pub struct Searcher {
    fetcher: Fetcher,
    pool: EgressPool,
    google: engines::google_wml::ProfileSelector,
    /// engine -> trust EWMA (1.0 seed; 0.2..2.0 clamp).
    /// Persisted to disk: an engine that learned "this walled me"
    /// keeps that memory across daemon restarts instead of
    /// re-paying the same failure every boot.
    trust: Mutex<HashMap<String, f64>>,
    /// Set on any health-map mutation; the disk save swaps it off
    /// and skips the write entirely when nothing changed (was: a
    /// clone + serialize + write on every uncached search).
    health_dirty: std::sync::atomic::AtomicBool,
    /// normalized-query cache: zero egress cost on repeats.
    /// Stores up to 12 results; reads truncate to the
    /// requested max so max_results variants share entries.
    #[allow(clippy::type_complexity)]
    cache: Mutex<HashMap<String, (Instant, Vec<Merged>, usize, Vec<EngineReport>)>>,
    /// Chronic-failure quarantine: engine -> (consecutive
    /// failures, last failure). 3 strikes across any
    /// egresses = benched for QUARANTINE_TTL so a walled
    /// engine stops wasting a fan-out slot every query.
    /// Failure streaks persist too: a benched engine stays
    /// benched across a crash + restart instead of being
    /// re-paid three times from zero.
    failures: Mutex<HashMap<String, (u32, Instant)>>,
    /// Single-flight: two identical in-flight queries spend
    /// egress budget ONCE : the follower awaits the
    /// leader's result. Stampedes are an agent reality
    /// (parallel tool calls love the same query).
    inflight: Mutex<std::collections::HashSet<String>>,
    /// v3 warm handoff: enrichment bodies cached for the
    /// subsequent `web_fetch` of a top result (search → fetch
    /// is THE agent pipeline). One-shot, TTL'd, bounded.
    prewarms: std::sync::Arc<std::sync::Mutex<PrewarmCache>>,
    /// Browser render capability (2026 Google serves a JS
    /// shell to plain HTTP but renders fine in our own
    /// headless Chrome: live-proven). Used ONLY by the
    /// thinness-gated cascade lane; None in test builds.
    ghost: Option<crate::crawl::GhostHook>,
}

#[cfg(feature = "rerank")]
/// Run synchronous ranking outside Tokio's async worker set.
///
/// Semantic reranking can enter ONNX inference and wait on the shared session
/// mutex. The ranking API stays synchronous, so the blocking pool is the narrow
/// boundary that keeps unrelated async work progressing.
async fn run_blocking_ranking<F, T>(job: F) -> Result<T, FetchError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|e| FetchError::Http(format!("search: ranking worker failed: {e}")))
}

// v3 F1: search→fetch warm handoff store : filled by enrichment, drained
// by the fetch tool. Implementation + the enrichment pass live in
// `search::enrich`.

/// Per-engine outcome for honest reporting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EngineReport {
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
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
        let (trust, failures) = load_health_disk();
        Self {
            fetcher,
            pool,
            google: engines::google_wml::ProfileSelector::from_env(),
            trust: Mutex::new(trust),
            health_dirty: std::sync::atomic::AtomicBool::new(false),
            cache: Mutex::new(load_cache_disk()),
            failures: Mutex::new(failures),
            inflight: Mutex::new(std::collections::HashSet::new()),
            prewarms: std::sync::Arc::new(std::sync::Mutex::new(PrewarmCache::new())),
            ghost: None,
        }
    }

    /// Attach the browser-render capability (cascade lane).
    pub fn with_ghost(mut self, ghost: crate::crawl::GhostHook) -> Self {
        self.ghost = Some(ghost);
        self
    }

    /// v3 F1: warm-handoff store : filled by enrichment, drained
    /// by the fetch tool.
    pub fn prewarms(&self) -> &std::sync::Arc<std::sync::Mutex<PrewarmCache>> {
        &self.prewarms
    }

    /// Issue #195: serve a repeat BYOK query from the same TTL'd
    /// cache the local path uses, so a metered provider is not
    /// re-billed for an identical query inside the freshness window.
    /// Returns a `cached: true` outcome (provider recovered from the
    /// stored report) when a fresh entry exists. BYOK entries live in
    /// their own key namespace so a `default` switch between local and
    /// a provider never cross-serves one for the other.
    pub fn byok_cache_get(
        &self,
        query: &str,
        intent: Intent,
        max_results: usize,
    ) -> Option<SearchOutcome> {
        let t0 = Instant::now();
        let key = byok_cache_key(query, intent);
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (at, cached, _total, report) = cache.get(&key)?;
        if at.elapsed() >= cache_ttl(intent, query) {
            return None;
        }
        // The provider name is the engine label the BYOK path stored.
        let provider = report.first().map(|r| r.engine.clone());
        let mut results: Vec<Merged> = cached
            .iter()
            .take(max_results.clamp(1, 12))
            .cloned()
            .collect();
        site_filter(query, &mut results);
        Some(SearchOutcome {
            results,
            // BYOK results are provider-ranked and never flagged weak,
            // matching the live BYOK path.
            weak: false,
            intent,
            report: report.clone(),
            cached: true,
            elapsed: t0.elapsed(),
            provider,
            reranked: false,
        })
    }

    /// Issue #195: persist a fresh BYOK outcome under the BYOK key
    /// namespace, TTL'd like the local cache. Skips an empty result
    /// set (the provider path errors on empty, so this is defensive).
    pub fn byok_cache_put(&self, query: &str, out: &SearchOutcome) {
        if out.results.is_empty() {
            return;
        }
        let key = byok_cache_key(query, out.intent);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // LRU-ish cap, same rule as the local write path.
        if cache.len() >= 500
            && let Some(oldest) = cache
                .iter()
                .max_by_key(|(_, (at, _, _, _))| at.elapsed())
                .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
        let results: Vec<Merged> = out.results.iter().take(12).cloned().collect();
        let total = results.len();
        cache.insert(key, (Instant::now(), results, total, out.report.clone()));
        save_cache_disk(&cache);
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
            // died, not the pool : clear the marks rather
            // than bench every lane over our own bug.
            if total > 0 && dead == total {
                this.pool.revive_all();
            }
        });
    }

    /// True when an engine is benched for chronic failure.
    fn quarantined(&self, engine: &str) -> bool {
        let f = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matches!(f.get(engine), Some(&(n, at)) if n >= 3 && at.elapsed() < QUARANTINE_TTL)
    }

    fn record_outcome(&self, engine: &str, ok: bool) {
        let mut f = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ok {
            f.remove(engine);
        } else {
            let e = f.entry(engine.to_string()).or_insert((0, Instant::now()));
            e.0 += 1;
            e.1 = Instant::now();
        }
        self.health_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub async fn search(
        &self,
        query: &str,
        max_results: usize,
        forced_intent: Option<Intent>,
    ) -> Result<SearchOutcome, FetchError> {
        let started = Instant::now();
        if let Some(problem) = validate_query(query) {
            return Err(FetchError::Http(format!("search: {problem}")));
        }
        let intent_probe = forced_intent.unwrap_or_else(|| intent::detect(query));
        // Single-flight keys on the CACHE key (query + intent), NOT
        // max_results: the leader publishes the full top-12 into
        // the cache, so a query run once at max=2 and again at
        // max=10 shares one fan-out instead of paying two.
        let sf_key = format!("{}|{}", norm_query(query), intent_probe.code());
        let leader = {
            let mut m = self
                .inflight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&format!("{}|{}", norm_query(query), intent_probe.code()))
                    .cloned();
                if let Some((at, cached, total, reports)) = hit
                    && at.elapsed() < cache_ttl(intent_probe, query)
                {
                    let weak = rank::is_weak(&cached, total);
                    let mut results = cached.iter().take(max_results).cloned().collect();
                    site_filter(query, &mut results);
                    return Ok(SearchOutcome {
                        results,
                        weak,
                        intent: intent_probe,
                        report: reports,
                        cached: true,
                        elapsed: started.elapsed(),
                        provider: None,
                        reranked: crate::search::rerank::active(),
                    });
                }
            }
            // Leader died or timed out : compute ourselves.
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
        let cache_key = format!("{}|{}", norm_query(query), intent.code());

        if let Some((at, cached, total, reports)) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&cache_key)
            && at.elapsed() < cache_ttl(intent, query)
        {
            let weak = rank::is_weak(cached, *total);
            let mut results = cached.iter().take(max_results).cloned().collect();
            site_filter(query, &mut results);
            return Ok(SearchOutcome {
                results,
                weak,
                intent,
                report: reports.clone(),
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
            .filter(|e| !self.quarantined(engine_health_key(e)))
            .copied()
            .collect();
        // Rank engines by learned trust so width cuts drop
        // the weakest first.
        {
            let trust = self
                .trust
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            live.sort_by(|a, b| {
                trust
                    .get(engine_health_key(b))
                    .copied()
                    .unwrap_or(1.0)
                    .total_cmp(&trust.get(engine_health_key(a)).copied().unwrap_or(1.0))
            });
        }
        // ── Adaptive fan-out width: the governor. Under
        // stress the system shrinks its appetite instead of
        // burning lanes : consensus survives at width 2 by
        // construction (two independent index families).
        let width = width_for_stress(self.pool.stress(), live.len());
        live.truncate(width);
        let mut assignments: Vec<(String, String)> = live
            .iter()
            .map(|e| (e.to_string(), query.to_string()))
            .collect();
        // Recall variants spend lanes : only when the
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
        // We only exclude proxy egresses from reuse : direct
        // is shared, not exclusive.
        let context = tasks::EngineContext {
            fetcher: &self.fetcher,
            pool: &self.pool,
            google: &self.google,
        };
        for (engine, q, eg) in assign_egresses(&self.pool, assignments, &mut used_egresses) {
            futures.push(Box::pin(engine_task(engine, q, eg.id, eg.proxy, context)));
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
        // through a fresh egress : but ONLY when the first
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
                .filter(|(_, r)| matches!(r, Err((s, _, _)) if retry_engine_failure(s)))
                .map(|(e, _)| e.split('@').next().unwrap_or(e).to_string())
                .collect()
        } else {
            Vec::new()
        };
        let mut retry_futures: Vec<TaskFut> = Vec::new();
        let mut retried = std::collections::HashSet::new();
        for engine in &failed {
            if !retried.insert(engine) {
                continue;
            }
            if outcomes
                .iter()
                .any(|(label, outcome)| engine_name(label) == engine && outcome.is_ok())
            {
                continue;
            }
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
                let task = Box::pin(vertical_task(
                    engine.clone(),
                    query.to_string(),
                    &self.fetcher,
                    eg.proxy,
                ));
                retry_futures.push(Box::pin(bounded_retry(
                    task,
                    engine.clone(),
                    eg.id,
                    false,
                    RETRY_TIMEOUT,
                )));
                continue;
            }
            // ddg's html endpoint is the fallback when lite fails.
            let retry_engine = if engine == "ddg" { "ddg_html" } else { engine };
            let Some(eg) = self.pool.pick(engine, &used_egresses, true) else {
                continue;
            };
            let previous = outcomes.iter().find_map(|(label, result)| {
                if engine_name(label) == engine
                    && let Err((status, _, _)) = result
                {
                    Some((label.as_str(), status.as_str()))
                } else {
                    None
                }
            });
            retry_futures.push(Box::pin(tasks::engine_task_with_budget(
                retry_engine.to_string(),
                query.to_string(),
                eg.id,
                eg.proxy,
                context,
                RETRY_TIMEOUT,
                previous,
            )));
        }
        let retry_outcomes = futures_util::future::join_all(retry_futures).await;

        // ── Ghost SERP cascade lane ──
        // Google's desktop endpoint may serve a JS shell to plain HTTP.
        // The WML HTTP lane uses a separate layout and legacy User-Agent.
        // When the plain fan-out AND its retry wave still left the
        // merge thin, one browser render buys a genuinely independent
        // index family instead of shipping weak results.
        let retry_ok: usize = ok_engines + retry_outcomes.iter().filter(|(_, r)| r.is_ok()).count();
        let retry_hits: usize = ok_hits
            + retry_outcomes
                .iter()
                .filter_map(|(_, r)| r.as_ref().ok())
                .map(|(h, _, _, _)| h.len())
                .sum::<usize>();
        let force_lane = std::env::var_os("DONSEEK_FORCE_GHOST_LANE").is_some();
        let google_http_ok = outcomes
            .iter()
            .chain(&retry_outcomes)
            .any(|(engine, result)| engine_name(engine) == "google" && result.is_ok());
        let lane_permitted = self.ghost.is_some()
            && std::env::var_os("DONSEEK_NO_GHOST_LANES").is_none()
            && !self.quarantined("google_ghost");
        let lane_outcomes: Vec<(String, EngineResult)> = if lane_permitted
            && google_ghost_wanted(force_lane, google_http_ok, retry_ok, retry_hits)
        {
            let hook = self.ghost.as_ref().unwrap().clone();
            let task = ghost_engine_task("google_ghost".to_string(), query.to_string(), hook);
            match tokio::time::timeout(Duration::from_secs(30), task).await {
                Ok(outcome) => vec![outcome],
                Err(_) => vec![(
                    "google_ghost".to_string(),
                    Err(("ghost-timeout".into(), "ghost".into(), true)),
                )],
            }
        } else {
            Vec::new()
        };

        let mut per_engine: Vec<(String, Vec<engines::Hit>)> = Vec::new();
        let mut report = Vec::new();
        let all: Vec<(String, EngineResult)> = outcomes
            .into_iter()
            .chain(retry_outcomes)
            .chain(lane_outcomes)
            .collect();
        for (label, outcome) in all {
            let profile = label.split_once('@').map(|(_, p)| p.to_string());
            let engine = engine_name(&label).to_string();
            let ghost_lane = engine == "google_ghost";
            match outcome {
                Ok((hits, ms, egress_id, was_engine)) => {
                    let base = engine_health_key(&engine);
                    self.record_outcome(base, true);
                    if was_engine && !ghost_lane {
                        // "ghost" is not an egress id: pool
                        // bookkeeping must not record lanes that
                        // the pool never assigned.
                        self.pool.report_ok(&engine, &egress_id);
                    }
                    if was_engine || ghost_lane {
                        self.bump_trust(base, true);
                    }
                    report.push(EngineReport {
                        engine: engine.clone(),
                        profile: profile.clone(),
                        status: "ok".into(),
                        hits: hits.len(),
                        ms,
                        egress: egress_id.clone(),
                    });
                    per_engine.push((engine, hits));
                }
                Err((status, egress_id, was_engine)) => {
                    let base = engine_health_key(&engine);
                    // Dead proxies and auth failures are egress/BYOK
                    // problems, not engine failures : don't quarantine
                    // or distrust the engine over them.
                    if is_engine_fault(&status) {
                        self.record_outcome(base, false);
                    }
                    if (was_engine || ghost_lane) && !ghost_lane {
                        if status.starts_with("dead") {
                            self.pool.report_dead(&egress_id);
                        } else if status == "auth-fail" {
                            self.pool.report_auth_fail(&egress_id);
                        } else if is_engine_fault(&status) {
                            self.pool.report_blocked(&engine, &egress_id);
                        }
                    }
                    if (was_engine || ghost_lane) && is_engine_fault(&status) {
                        self.bump_trust(base, false);
                    }
                    report.push(EngineReport {
                        engine,
                        profile,
                        status,
                        hits: 0,
                        ms: 0,
                        egress: egress_id,
                    });
                }
            }
        }

        if per_engine.is_empty() {
            {
                let t = self
                    .trust
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let f = self
                    .failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                save_health_disk_if_dirty(self, &t, &f);
            }
            return Err(FetchError::Http(format!(
                "search: all engines failed : {}",
                report
                    .iter()
                    .map(|r| format!("{}:{}", r.engine, r.status))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        let mut trust = self
            .trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // Public result names stay stable; ranking must use the new HTTP
        // health namespace, never legacy browser health stored as `google`.
        let google_trust = trust
            .get(engine_health_key("google"))
            .copied()
            .unwrap_or(1.0);
        trust.insert("google".into(), google_trust);
        let total = rank::merged_total(&per_engine);
        // Always merge 12 results for the cache, then trim to
        // max_results for the response. Without this, a first
        // search with max=2 caches only 2 results, and a later
        // search with max=10 returns the stale 2 from cache.
        // With semantic reranking enabled, merge includes synchronous ONNX
        // inference. Core builds keep the existing inline fast path below.
        #[cfg(feature = "rerank")]
        let mut results = {
            let query = query.to_string();
            run_blocking_ranking(move || rank::merge(&per_engine, &query, intent, &trust, 12))
                .await?
        };
        #[cfg(not(feature = "rerank"))]
        let mut results = rank::merge(&per_engine, query, intent, &trust, 12);
        let weak = rank::is_weak(&results, total);

        // ── Result enrichment: prefetch top results to extract
        // real <title> and <meta description> from the actual
        // pages. Richer than SERP snippets, filters dead links.
        // The genius feature: results carry the page's own title
        // and description, not the SERP's truncated version.
        self.enrich_results(&mut results).await;

        // ── site: operator enforcement: engines don't strictly
        // respect `site:domain.com` : some results leak through
        // from other domains. Filter them out post-merge so the
        // agent only gets results from the requested domain.
        site_filter(query, &mut results);

        // Post-enrichment top-up: the cross-encoder now sees the
        // real page titles/descriptions on the top slice, not the
        // SERP fragments. Bounded additive nudge, then re-sort.
        // DONSEEK_NO_TOPUP is the A/B kill switch for benching.
        #[cfg(feature = "rerank")]
        if std::env::var_os("DONSEEK_NO_TOPUP").is_none() {
            let q = query.to_string();
            let mut owned = std::mem::take(&mut results);
            results = run_blocking_ranking(move || {
                crate::search::rerank::topup(&q, &mut owned, 8);
                owned
            })
            .await?;
        }
        // Poisoning guard: a merge built while engines
        // were down must NOT persist for 30 minutes :
        // degraded-period results expire with the moment.
        let cacheable = ok_engines >= 2 && total >= 8;
        if cacheable {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // LRU-ish cap: drop oldest when full.
            if cache.len() >= 500
                && let Some(oldest) = cache
                    .iter()
                    .max_by_key(|(_, (at, _, _, _))| at.elapsed())
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
                    report.clone(),
                ),
            );
            save_cache_disk(&cache);
        }

        // Persist learned engine health once per search (single
        // small write; failures inside the loop are already
        // recorded, so this snapshot is always consistent).
        {
            let t = self
                .trust
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let f = self
                .failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            save_health_disk_if_dirty(self, &t, &f);
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
        let mut trust = self
            .trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let t = trust.entry(base_engine.to_string()).or_insert(1.0);
        let target = if ok { 1.2 } else { 0.3 };
        *t = (*t * 0.7 + target * 0.3).clamp(0.2, 2.0);
        self.health_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Assignment only: eligibility remains owned by EgressPool::pick.
/// Keep this pass shared by the real fan-out and its offline regression tests.
fn assign_egresses(
    pool: &EgressPool,
    assignments: Vec<(String, String)>,
    used: &mut Vec<String>,
) -> Vec<(String, String, egress::Egress)> {
    let mut selected = Vec::new();
    let has_proxies = pool.has_proxies();
    for (engine, query) in assignments {
        let Some(egress) = pool.pick(&engine, used, true) else {
            // Unavailable for this engine does not mean unavailable for peers.
            continue;
        };
        // Spread configured proxies; direct remains shared with pacing.
        if has_proxies && egress.proxy.is_some() {
            used.push(egress.id.clone());
        }
        selected.push((engine, query, egress));
    }
    selected
}

fn engine_name(label: &str) -> &str {
    label.split('@').next().unwrap_or(label)
}

fn engine_health_key(engine: &str) -> &str {
    let engine = engine_name(engine);
    if engine == "google" {
        return "google_http_v1";
    }
    // Different Google transports can fail independently; only ranking groups
    // them into one index family. A WML block must not quarantine the browser.
    if engine == "google_ghost" {
        engine
    } else {
        egress::health_key(engine)
    }
}

fn retry_engine_failure(status: &str) -> bool {
    !matches!(status, "no-results" | "pacing-timeout" | "invalid-config")
}

/// Vertical retries retain explicit timeout outcomes; engine tasks own their
/// deadline internally so diagnostics include the identity actually attempted.
async fn bounded_retry(
    task: TaskFut<'_>,
    engine: String,
    egress: String,
    was_engine: bool,
    budget: Duration,
) -> (String, EngineResult) {
    tokio::time::timeout(budget, task)
        .await
        .unwrap_or_else(|_| (engine, Err(("retry-timeout".into(), egress, was_engine))))
}

fn google_ghost_wanted(force: bool, http_ok: bool, engines_ok: usize, hits_ok: usize) -> bool {
    force || (!http_ok && ghost_lane_wanted(engines_ok, hits_ok))
}

/// Thinness gate for the ghost cascade lane. A successful HTTP Google
/// response already supplies that index; the caller skips the browser then.
fn ghost_lane_wanted(engines_ok: usize, hits_ok: usize) -> bool {
    engines_ok < 3 || hits_ok < 15
}

/// Input hygiene for the search surface. Empty queries waste a
/// fan-out (and cached-homepage SERPs would poison the merge);
/// oversized queries break every endpoint's URL budget. Returns
/// the human-readable problem, None = valid.
pub(crate) fn validate_query(query: &str) -> Option<String> {
    let t = query.trim();
    if t.is_empty() {
        return Some("empty query : pass a non-empty query string".into());
    }
    let chars = t.chars().count();
    if chars > 512 {
        return Some(format!(
            "query is {chars} characters : search endpoints cap near 512; trim it or split it into two searches"
        ));
    }
    None
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
/// Engines (especially Bing/DDG) don't strictly respect `site:` :
/// they often inject related results from other domains. This
/// post-merge filter ensures the agent only sees results from the
/// requested domain.
///
/// Matches `domain.com` and any subdomain `*.domain.com`.
/// Case-insensitive. Strips `www.` prefix before comparison.
/// (#190) site: filters BYOK results too: the same filtering and count
/// semantics as the local engine sweeps. Redirect rows whose target
/// host can not match the scanned domain (google goto proxies) drop;
/// fail closed.
pub(crate) fn site_filter(query: &str, results: &mut Vec<Merged>) {
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

/// Removes the inflight key when the leader finishes
/// (success or failure) so the set never grows unbounded.
struct InflightGuard<'a> {
    map: &'a Mutex<std::collections::HashSet<String>>,
    key: String,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::render::clip_snippet;
    use super::*;

    fn test_searcher() -> Searcher {
        // Hermetic: point disk cache/health at a throwaway dir so the
        // real user cache is neither read nor polluted, then clear the
        // in-memory map so the entry set is exactly what the test puts
        // (robust even if a sibling test changed the env first).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "donsetch-byok-cache-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &dir) };
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let s = Searcher::new(fetcher, EgressPool::new(Vec::new()));
        s.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        s
    }

    fn byok_outcome(provider: &str, urls: &[&str]) -> SearchOutcome {
        let results = urls
            .iter()
            .enumerate()
            .map(|(i, u)| Merged {
                title: format!("t{i}"),
                url: (*u).to_string(),
                snippet: "s".into(),
                sources: vec![(provider.to_string(), i)],
                score: 1.0 - i as f64 * 0.1,
                published: None,
            })
            .collect::<Vec<_>>();
        let report = vec![EngineReport {
            engine: provider.to_string(),
            profile: None,
            status: "ok".into(),
            hits: results.len(),
            ms: 12,
            egress: "byok".into(),
        }];
        SearchOutcome {
            results,
            weak: false,
            intent: Intent::Web,
            report,
            cached: false,
            elapsed: Duration::ZERO,
            provider: Some(provider.to_string()),
            reranked: false,
        }
    }

    // Issue #195: a repeat BYOK query must be served from the cache
    // (cached: true, provider preserved), never re-billing the
    // provider. The store/serve roundtrip is the mechanism the wiring
    // in search_tool relies on.
    #[test]
    fn byok_results_roundtrip_the_cache_with_provider_and_cached_flag() {
        let s = test_searcher();
        // Cold: nothing cached, so a caller must go bill the provider.
        assert!(s.byok_cache_get("quic test 42", Intent::Web, 3).is_none());

        let out = byok_outcome("tinyfish", &["https://a.test/", "https://b.test/"]);
        s.byok_cache_put("quic test 42", &out);

        // Warm: served from cache, marked cached, provider intact.
        let hit = s
            .byok_cache_get("quic test 42", Intent::Web, 3)
            .expect("a fresh BYOK entry must hit");
        assert!(hit.cached, "a served BYOK cache entry must report cached");
        assert_eq!(hit.provider.as_deref(), Some("tinyfish"));
        assert!(!hit.weak);
        assert_eq!(hit.results.len(), 2);
        assert_eq!(hit.results[0].url, "https://a.test/");
    }

    // The BYOK namespace must not collide with the local path: a
    // provider result must never be served for a local-default query
    // of the same text/intent, nor vice versa.
    #[test]
    fn byok_cache_is_isolated_from_the_local_namespace() {
        let s = test_searcher();
        let out = byok_outcome("serper", &["https://only-byok.test/"]);
        s.byok_cache_put("shared query", &out);
        // The local cache key (query|intent) is a different slot, so a
        // local read finds nothing the BYOK write left behind.
        let local_key = format!("{}|{}", norm_query("shared query"), Intent::Web.code());
        assert!(
            !s.cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&local_key),
            "a BYOK write must not populate the local cache slot"
        );
        // And the BYOK read still finds its own entry.
        assert!(s.byok_cache_get("shared query", Intent::Web, 5).is_some());
        // A different intent is a different entry (miss).
        assert!(s.byok_cache_get("shared query", Intent::News, 5).is_none());
    }

    #[test]
    fn regression_unavailable_google_does_not_stop_other_assignments() {
        let proxy = crate::transport::proxy::Proxy::parse("http://127.0.0.1:12345").unwrap();
        let id = proxy.id();
        let pool = EgressPool::new(vec![proxy]);
        pool.report_blocked("google", &id);
        let assignments = ["google", "bing"]
            .map(|e| (e.into(), "query".into()))
            .to_vec();
        // Explicitly unavailable direct lane; the proxy is still viable for Bing.
        let selected = assign_egresses(&pool, assignments, &mut vec!["direct".into()]);
        assert_eq!(
            selected
                .iter()
                .map(|(engine, _, _)| engine.as_str())
                .collect::<Vec<_>>(),
            ["bing"]
        );
    }

    #[test]
    fn assignments_preserve_proxy_spreading_and_shared_direct_lane() {
        let proxies = ["http://127.0.0.1:12345", "http://127.0.0.1:12346"]
            .map(|url| crate::transport::proxy::Proxy::parse(url).unwrap())
            .to_vec();
        let pool = EgressPool::new(proxies);
        let assignments = ["bing", "yahoo", "brave", "ddg"]
            .map(|e| (e.into(), "query".into()))
            .to_vec();
        let mut used = Vec::new();
        let selected = assign_egresses(&pool, assignments, &mut used);
        assert_eq!(selected.len(), 4);
        assert_eq!(used, ["127.0.0.1:12345", "127.0.0.1:12346"]);
        assert_eq!(selected[2].2.id, "direct");
        assert_eq!(selected[3].2.id, "direct");
    }

    #[test]
    fn google_transport_health_is_separate_but_index_family_is_shared() {
        assert_eq!(engine_health_key("google@6230-05.50"), "google_http_v1");
        assert_ne!(engine_health_key("google"), "google");
        assert_ne!(
            engine_health_key("google"),
            engine_health_key("google_ghost")
        );
        assert_eq!(engine_health_key("ddg_html"), "ddg");
        assert_eq!(
            rank::engine_family("google"),
            rank::engine_family("google_ghost")
        );
    }

    #[test]
    fn engine_reports_load_legacy_cache_and_preserve_new_profile() {
        let mut report: EngineReport = serde_json::from_str(
            r#"{"engine":"google","status":"ok","hits":10,"ms":700,"egress":"direct"}"#,
        )
        .unwrap();
        assert!(report.profile.is_none());
        report.profile = Some("6230-04.44".into());
        let reloaded: EngineReport =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(reloaded.profile.as_deref(), Some("6230-04.44"));
    }

    #[test]
    fn successful_google_http_skips_browser_unless_explicitly_forced() {
        assert!(!google_ghost_wanted(false, true, 1, 3));
        assert!(google_ghost_wanted(false, false, 1, 3));
        assert!(!google_ghost_wanted(false, false, 4, 20));
        assert!(google_ghost_wanted(true, true, 4, 20));
    }

    #[test]
    fn all_engines_share_retry_eligibility() {
        for status in [
            "blocked:captcha",
            "blocked:429",
            "blocked:consent",
            "blocked:http-status",
            "empty-parse",
            "net",
            "timeout",
            "dead-proxy",
            "auth-fail",
        ] {
            assert!(retry_engine_failure(status));
            if !matches!(status, "dead-proxy" | "auth-fail") {
                assert!(is_engine_fault(status));
            }
        }
        for status in ["invalid-config", "no-results", "pacing-timeout"] {
            assert!(!retry_engine_failure(status));
            assert!(!is_engine_fault(status));
        }
    }

    #[tokio::test]
    async fn slow_retry_does_not_discard_completed_peer() {
        let fast: TaskFut = Box::pin(async {
            (
                "google".into(),
                Err(("blocked:captcha".into(), "direct".into(), true)),
            )
        });
        let slow: TaskFut = Box::pin(std::future::pending());
        let outcomes = futures_util::future::join_all(vec![
            bounded_retry(
                fast,
                "google".into(),
                "direct".into(),
                true,
                Duration::from_millis(20),
            ),
            bounded_retry(
                slow,
                "bing".into(),
                "direct".into(),
                true,
                Duration::from_millis(10),
            ),
        ])
        .await;
        assert_eq!(outcomes.len(), 2);
        assert!(matches!(&outcomes[1].1, Err((s, _, _)) if s == "retry-timeout"));
        assert_eq!(outcomes[0].0, "google");
        // A second CAPTCHA is reported, not recursively scheduled.
        assert!(matches!(&outcomes[0].1, Err((s, _, _)) if s == "blocked:captcha"));
    }

    #[test]
    fn native_google_is_available_once_in_every_intent_roster() {
        for intent in [
            intent::Intent::Web,
            intent::Intent::Code,
            intent::Intent::News,
            intent::Intent::Entity,
            intent::Intent::Paper,
        ] {
            assert_eq!(
                intent::engines_for(intent)
                    .iter()
                    .filter(|e| **e == "google")
                    .count(),
                1
            );
            assert!(!intent::engines_for(intent).contains(&"google_ghost"));
        }
    }

    // Egress/BYOK-auth noise must never look like the engine
    // misbehaving: record_outcome (quarantine) and bump_trust (the
    // ranking-weight EWMA) both gate on this predicate, and had
    // drifted out of sync before (bump_trust used to fire on
    // "dead-proxy"/"auth-fail" too, eroding trust for infra failures
    // the engine had nothing to do with).
    #[test]
    fn is_engine_fault_excludes_infra_and_no_results() {
        assert!(!is_engine_fault("dead-proxy"));
        assert!(!is_engine_fault("auth-fail"));
        assert!(!is_engine_fault("no-results"));
        assert!(!is_engine_fault("invalid-config"));
        assert!(is_engine_fault("blocked:403"));
        assert!(is_engine_fault("blocked:captcha"));
        assert!(is_engine_fault("empty-parse"));
        assert!(is_engine_fault("ghost-timeout"));
        assert!(is_engine_fault("timeout"));
        assert!(is_engine_fault("no-url"));
        assert!(is_engine_fault("net"));
    }

    #[test]
    fn google_ghost_is_its_own_family_for_ranking_math() {
        assert_eq!(rank::engine_family("google_ghost"), "google");
        assert_eq!(rank::engine_family("google"), "google");
        // Not a vertical: full RRF mass, no vertical-only penalty.
        assert!(!rank::is_vertical("google_ghost"));
    }

    #[test]
    fn family_count_dedups_shared_indexes() {
        let mut r = Merged {
            title: "t".into(),
            url: "https://a.com/".into(),
            snippet: "s".into(),
            sources: Vec::new(),
            score: 0.0,
            published: None,
        };
        r.sources = vec![
            ("bing".into(), 0),
            ("ddg".into(), 3),
            ("yahoo".into(), 5),
            ("google_ghost".into(), 2),
            ("brave".into(), 4),
        ];
        // 5 engines, 3 families (bing family dedups to one opinion).
        assert_eq!(rank::family_count(&r), 3);
        let markdown = render_compact_markdown(&outcome(vec![r]), "", None, &[]);
        assert!(markdown.contains("3 index families"), "{markdown}");
    }

    #[test]
    fn news_snippet_carries_publisher_not_bare_date() {
        let body = r#"<rss><item>
          <title>Power grid restore advances - Wire News</title>
          <link>https://example.gov/grid</link>
          <pubDate>Thu, 31 Jul 2026 07:00:00 GMT</pubDate>
        </item></rss>"#;
        let hits = verticals::parse("news", body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "Wire News · Thu, 31 Jul 2026 07:00:00 GMT");
    }

    #[test]
    fn engine_health_persists_across_restart() {
        // nextest = one process per test: DONSETCH_CACHE_DIR is
        // ours to own. Point it at a throwaway dir, write learned
        // health, and read it back like a fresh daemon would.
        let dir =
            std::env::temp_dir().join(format!("donseek-health-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        // 2024 edition marks env mutation unsafe; nextest runs each
        // test as its own process, so this is race-free here.
        unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &dir) };

        let mut trust = HashMap::new();
        trust.insert("brave".to_string(), 1.8);
        trust.insert("bing".to_string(), 0.42);
        let mut failures = HashMap::new();
        failures.insert("google".to_string(), (3, Instant::now()));
        crate::search::persist::save_health_disk(&trust, &failures);

        let (t, f) = load_health_disk();
        assert_eq!(t["brave"], 1.8, "high trust survives");
        assert_eq!(t["bing"], 0.42, "low trust survives");
        assert_eq!(f.get("google").map(|(n, _)| *n), Some(3));

        unsafe { std::env::remove_var("DONSETCH_CACHE_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "rerank")]
    #[tokio::test(flavor = "current_thread")]
    async fn blocking_ranking_keeps_the_async_executor_responsive() {
        let (tick_sender, tick_receiver) = std::sync::mpsc::channel();

        let (worker_result, ()) = tokio::join!(
            run_blocking_ranking(move || tick_receiver.recv_timeout(Duration::from_secs(1))),
            async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                tick_sender.send(()).expect("blocking worker should wait");
            }
        );

        assert!(worker_result.expect("blocking job should join").is_ok());
    }

    #[test]
    fn governor_shrinks_width_under_stress() {
        assert_eq!(width_for_stress(0.05, 4), 4);
        assert_eq!(width_for_stress(0.30, 4), 3);
        assert_eq!(width_for_stress(0.50, 4), 2);
        assert_eq!(width_for_stress(0.90, 4), 1);
        assert_eq!(width_for_stress(0.90, 0), 0);
    }

    #[test]
    fn query_validation_holds_the_line() {
        assert!(validate_query("   ").is_some(), "blank query rejected");
        assert!(validate_query("").is_some(), "empty query rejected");
        let long = "rust ".repeat(120); // 612 chars with trailing space
        assert!(validate_query(&long).is_some(), "oversized query rejected");
        assert!(validate_query("rust ownership").is_none());
        assert!(
            validate_query("glue").is_none(),
            "punctuation-only-ish query is fine"
        );
    }

    #[test]
    fn ghost_lane_gate_needs_real_thinness_or_force() {
        // A healthy merge never pays a browser render.
        assert!(!ghost_lane_wanted(5, 40));
        assert!(!ghost_lane_wanted(3, 16));
        // Thin merge on either axis fires the lane.
        assert!(ghost_lane_wanted(2, 40), "few engines ok");
        assert!(ghost_lane_wanted(4, 12), "thin hits");
        assert!(ghost_lane_wanted(0, 0));
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
        // Straddling word, last space at 16 of 20 : exactly the
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
        // COMPLETE sentence : stripping it would make a clean
        // ending look severed.
        assert_eq!(
            clip_snippet("Tokio is a runtime. Axum builds on it.", 20),
            "Tokio is a runtime.…"
        );
    }

    /// Sōseki's opening line : 3-byte chars, and no spaces at
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
        // Cut lands right after 。 (U+3002) : the same codepoint
        // in Chinese and Japanese. It ends a sentence, so it stays.
        assert_eq!(clip_snippet(JA, 17), "「吾輩は猫である。名前はまだ無い。…");
    }

    #[test]
    fn clip_keeps_chinese_terminators_and_strips_chinese_separators() {
        // ！ (U+FF01) ends a sentence : kept.
        assert_eq!(
            clip_snippet("这是一个测试。第二句话！第三句", 12),
            "这是一个测试。第二句话！…"
        );
        // 、 (U+3001) is the enumeration comma : it joins, so it
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
    fn compact_markdown_keeps_evidence_and_actionable_state_only() {
        let mut result = merged("https://tokio.rs/runtime");
        result.title = "Tokio runtime guide".into();
        result.snippet = "A focused explanation of the asynchronous runtime.".into();
        result.sources = vec![("bing".into(), 0), ("ddg".into(), 2)];
        result.score = 0.8312;
        let mut search = outcome(vec![result]);
        search.weak = true;
        search.report = vec![
            EngineReport {
                engine: "bing".into(),
                profile: None,
                status: "ok".into(),
                hits: 10,
                ms: 12,
                egress: "direct".into(),
            },
            EngineReport {
                engine: "ddg".into(),
                profile: None,
                status: "blocked:403".into(),
                hits: 0,
                ms: 20,
                egress: "proxy".into(),
            },
        ];

        let markdown = render_compact_markdown(
            &search,
            "# Search results",
            Some(&["S1".into()]),
            &[Some("· ⚠ needs browser".into())],
        );
        assert!(
            markdown.contains(
                "1. S1 · Tokio runtime guide : tokio.rs · ⚠ needs browser · 1 index family"
            ),
            "index-family count follows the route hint: {markdown}"
        );
        assert!(markdown.contains("A focused explanation"));
        assert!(markdown.contains("Weak results : low cross-index agreement."));
        assert!(markdown.contains("Degraded retrieval : 1/2 backends available."));
        for diagnostic in [
            "engines:",
            "score:",
            "results in",
            "via local",
            "fetch results by",
        ] {
            assert!(!markdown.contains(diagnostic), "{diagnostic}:\n{markdown}");
        }
    }

    #[test]
    fn compact_markdown_gives_zero_result_recovery() {
        let markdown = render_compact_markdown(&outcome(Vec::new()), "# Search results", None, &[]);
        assert!(markdown.contains("materially different formulation"));
    }

    #[test]
    fn markdown_dedupes_repeated_engines() {
        // One engine returning the same URL at two ranks must not
        // read as extra consensus : the JSON's `consensus` count
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
