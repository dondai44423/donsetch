//! DonSeek : keyless multi-engine search.
//!
//! Intent → fan-out (engines across egresses + verticals
//! direct) → weighted RRF merge → ranked results with
//! honest engine reporting.

pub mod bench;
pub mod byok;
pub mod coverage;
pub mod doctor;
pub mod egress;
pub mod engines;
pub mod evidence;
pub mod families;
pub mod instant;
pub mod intent;
pub mod query;
pub mod rank;
pub mod rerank;
pub mod verticals;

mod authority;
mod contract;
mod enrich;
mod ledger;
mod locale;
mod news;
mod persist;
mod quality;
mod render;
pub(crate) use render::rank_state;
mod resolve;
mod tasks;

pub use persist::load_for_status as persist_load_for_status;
pub use persist::load_outcome_for_status as persist_load_outcome_for_status;
pub use persist::load_quality_for_status as persist_load_quality_for_status;
pub use persist::outcome_class;

pub use render::{
    SourceLine, census, render_compact_markdown, render_markdown, render_meta, source_states,
};

use enrich::PrewarmCache;
use persist::{
    OutcomeMap, QualityMap, load_cache_disk, load_health_disk, load_outcome_disk,
    load_quality_disk, save_cache_disk, save_health_disk_if_dirty, save_outcome_disk_if_dirty,
    save_quality_disk_if_dirty,
};
use tasks::{EngineResult, TaskFut, vertical_task};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::FetchError;
use crate::fetch::client::Fetcher;

use egress::EgressPool;
use intent::Intent;
use rank::Merged;

/// How long the fan-out may run before the call commits to what has answered.
/// Sources answer in 0.8–1.4 s on this network, so this is a cap on the tail,
/// not a target: a lane still running at the mark is reported `late` and
/// cancelled. A caller's `deadline_ms` shrinks it further.
const FANOUT_SOFT_MARK: Duration = Duration::from_millis(1200);

/// Never wait past this, however slow the sources are: a search with no answer
/// after four seconds is answered by `late`, not by a longer wait.
const FANOUT_HARD_MARK: Duration = Duration::from_millis(4000);
/// How long past the soft mark the call still listens for a straggler: the
/// third source is usually a few hundred milliseconds behind, not absent, and
/// waiting for it is what makes three sources agree. Past that, the answer is
/// what it is.
const FANOUT_LATE_MARK: Duration = Duration::from_millis(600);

/// The longest a call waits for a lane's own pace before deciding the lane is
/// not worth planning. A token bucket refills in seconds; a four-second wait is
/// the ledger's own statement that the source is being asked too often, and the
/// call reports the lane instead of spending that wait.
const PACE_WAIT_MAX: Duration = Duration::from_millis(1200);

/// The merge is thin below this: fewer than three answering sources, or fewer
/// items than a ranking can be built from. A thin merge gets one bounded second
/// ask (measured: it is where most of the recall lives on a bad minute).
const THIN_LANES: usize = 3;
const THIN_ITEMS: usize = 15;

/// How long a second ask may take, and only ever inside the hard mark.
const SECOND_ASK: Duration = Duration::from_millis(1500);

/// A lane's own ceiling: the mark plus a grace for the request already on the
/// wire. Dropping a lane's future does NOT stop its blocking fetch, and the
/// process waits for it at exit — measured: a 1.2 s answer with a 4.9 s wall
/// clock because a cancelled bing lane kept working. Bounding the lane is what
/// makes the mark real.
fn lane_budget() -> Duration {
    stage_budget(
        FANOUT_HARD_MARK + Duration::from_millis(400),
        Duration::from_millis(200),
    )
}

/// Delay a lane's task by the pace it owes the source, without holding up the
/// lanes beside it. A paced lane is asked, not skipped: a token bucket refill
/// is the lane's own pace, not a wall.
fn paced<'a>(pace: Duration, fut: tasks::TaskFut<'a>) -> tasks::TaskFut<'a> {
    if pace.is_zero() {
        return fut;
    }
    Box::pin(async move {
        tokio::time::sleep(pace).await;
        fut.await
    })
}

/// Contributing sources and items in what has landed so far.
fn answered(outcomes: &[(String, EngineResult)]) -> (usize, usize) {
    outcomes
        .iter()
        .fold((0usize, 0usize), |(l, n), (_, r)| match r {
            Ok(a) if a.class.contributes() => (l + 1, n + a.hits.len()),
            _ => (l, n),
        })
}

/// A status a second ask can plausibly change: the source misbehaved, rather
/// than the query being dry or the operator's config saying no.
fn worth_second_ask(status: &str) -> bool {
    // A CAPTCHA or a 429 is the source asking us to stop: the ledger rests the
    // lane, and asking again inside the same call is the rotation the design
    // refuses (search waits, it does not rotate to beat a limit).
    !matches!(
        status,
        "no-results" | "pacing-timeout" | "late" | "empty-parse"
    ) && !status.starts_with("invalid-config")
        && !status.starts_with("rate-limited")
        // A wall that asks a human, a login or a limit is the source asking us
        // to stop; a bare refusal (403) is one a different lane can clear.
        && ![
            "blocked:captcha",
            "blocked:challenge",
            "blocked:login",
            "blocked:paywall",
            "blocked:429",
        ]
        .iter()
        .any(|wall| status.starts_with(wall))
}

tokio::task_local! {
    pub(crate) static SEARCH_DEADLINE: Option<tokio::time::Instant>;
}

pub(crate) fn stage_budget(cap: Duration, reserve: Duration) -> Duration {
    SEARCH_DEADLINE
        .try_with(|deadline| {
            deadline.map_or(cap, |deadline| {
                cap.min(
                    deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .saturating_sub(reserve),
                )
            })
        })
        .unwrap_or(cap)
}

/// Chronic-failure bench time. A walled engine stops wasting a
/// fan-out slot for this long after 3 consecutive strikes.
const QUARANTINE_TTL: Duration = Duration::from_secs(600);

/// Trust at or below this means the source has proved it does not answer here.
/// It stops holding a roster slot, so a dead engine costs zero requests instead
/// of one per query (`google` and `mojeek` were failing on every search with a
/// third of the fan-out spent on them).
const TRUST_FLOOR: f64 = 0.35;

/// How long a proven-dead source sits out before it is probed once. Reusing the
/// quarantine TTL keeps one clock for "this source is not answering here".
const REPROBE_AFTER: Duration = QUARANTINE_TTL;

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

/// Normalize a query for cache and single-flight keys. Casing,
/// punctuation and stopwords collapse. These stay distinct: `+` and
/// `#` as word characters (`c++`, `c#`), a `-` that starts a token (an
/// exclusion, of a word or of a quoted phrase), a `"…"` phrase with
/// its stopwords, an uppercase `OR`, and the `site:`, `filetype:` and
/// `intitle:` operators as [`query::compile`] reads them, appended in
/// that order after the terms.
fn norm_query(q: &str) -> String {
    const STOP: &[&str] = &[
        "a", "an", "the", "is", "are", "was", "were", "of", "in", "on", "at", "to", "for", "and",
        "or", "what", "which", "how", "do", "does", "i", "you", "it",
    ];
    fn words(s: &str) -> Vec<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '+' && c != '#')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    }
    let compiled = query::compile(q);
    let mut key: Vec<String> = Vec::new();
    // Odd segments are inside double quotes; an unterminated quote
    // runs to the end of the query.
    let segments: Vec<&str> = compiled.text.split('"').collect();
    let mut exclude_phrase = false;
    for (i, segment) in segments.iter().enumerate() {
        if i % 2 == 1 {
            let phrase = words(segment);
            if !phrase.is_empty() {
                let sign = if exclude_phrase { "-" } else { "" };
                key.push(format!("{sign}\"{}\"", phrase.join(" ")));
            }
            continue;
        }
        // `-"…"`: a lone `-` right before the opening quote.
        exclude_phrase = i + 1 < segments.len()
            && segment
                .strip_suffix('-')
                .is_some_and(|head| head.is_empty() || head.ends_with(char::is_whitespace));
        for token in segment.split_whitespace() {
            if token == "OR" {
                key.push(token.to_string());
            } else if let Some(excluded) = token.strip_prefix('-') {
                let excluded = words(excluded);
                if !excluded.is_empty() {
                    key.push(format!("-{}", excluded.join("-")));
                }
            } else {
                key.extend(
                    words(token)
                        .into_iter()
                        .filter(|w| !STOP.contains(&w.as_str())),
                );
            }
        }
    }
    if let Some(site) = compiled.site {
        key.push(format!("site:{site}"));
    }
    if let Some(filetype) = compiled.filetype {
        key.push(format!("filetype:{filetype}"));
    }
    if let Some(title) = compiled.intitle {
        key.push(format!("intitle:\"{}\"", title.to_lowercase()));
    }
    key.join(" ")
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
/// (`"dead-proxy"`), a BYOK key problem (`"auth-fail"`), or simply
/// no results (`"no-results"`). Everything else (blocked, empty-parse,
/// timeout, net) erodes trust. A single predicate so quarantine and
/// trust tracking can't drift out of sync with each other again.
fn is_engine_fault(status: &str) -> bool {
    // A lane the plan never asked, the ledger rested or the scheduler cut did
    // nothing at all; counting it as a strike quarantined healthy engines the
    // budget had merely left out three times in a row. The ledger's own reading
    // of the status is the one rule for "was this the source's doing".
    if ledger::classify(status).is_none() {
        return false;
    }
    !status.starts_with("dead")
        && status != "auth-fail"
        && status != "no-results"
        && status != "invalid-config"
        && !status.starts_with("invalid-config:")
        && !status.starts_with("rate-limited")
        && status != "pacing-timeout"
        && status != "budget-skipped"
}

/// C1 roster hygiene: Mojeek answers blocked traffic with HTTP 200
/// and a parse that yields <3 hits (`empty-parse`). That is a
/// CHRONIC signal, not a one-off — burn trust harder so it drops
/// out of the default width instead of occupying a slot every
/// query. The parser stays; proxy paths still fan out to it.
fn trust_target_for(engine: &str, status: &str, ok: bool) -> f64 {
    if ok {
        return 1.2;
    }
    // A set that is *provably* another topic (quality's three-way rule: mean
    // relevance near zero, no rare term shared, and the query names an entity)
    // is the one content verdict that touches trust — and it decays gently.
    // Everything else about a source's content is our arithmetic over its own
    // items, never evidence about the source (§4). The old split between
    // "off-query" beside answering peers and "off-query alone" died with the
    // per-hit coverage test that needed it.
    if status == "off-query" {
        return 0.6;
    }
    if engine == "mojeek" && status == "empty-parse" {
        return 0.15;
    }
    0.3
}

/// What a source currently means for the next search.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceState {
    /// Answering and trusted.
    Verified,
    /// Answering badly or recently failing: still asked, and still judged on
    /// every answer.
    Suspect,
    /// Sitting out, and asked again once this long has passed.
    Benched(Duration),
}

/// One source's standing, as `search --sources` shows it.
#[derive(Debug, Clone)]
pub struct SourceHealth {
    pub name: String,
    pub trust: f64,
    pub strikes: u32,
    pub state: SourceState,
    pub family: String,
}

/// Trust below this, or any recent failure, makes a source suspect.
const VERIFIED_TRUST: f64 = 0.8;

/// The standing of a source given its trust and its failure streak (strikes and
/// time since the last one). A source is benched while a streak of three is
/// inside its window; the window lapsing makes it suspect, and it is asked once:
/// a success restores it, a failure benches it again. Nothing in that needs a
/// restart or a person.
pub(crate) fn source_state(trust: f64, failure: Option<(u32, Duration)>) -> SourceState {
    if let Some((strikes, since)) = failure
        && strikes >= 3
        && since < QUARANTINE_TTL
    {
        return SourceState::Benched(QUARANTINE_TTL - since);
    }
    if trust < VERIFIED_TRUST || failure.is_some() {
        SourceState::Suspect
    } else {
        SourceState::Verified
    }
}

/// Is the answer settled before the mark?
///
/// Only when independent families already agree on a top-3 URL: then a further
/// opinion cannot reorder the list. A plain item count is NOT enough — measured:
/// committing at two fat lanes (30 items between them) cut the contributing
/// lanes from 4.3 to 1.4 per query and doubled the misses, because consensus is
/// what the ranking is built on and two engines are one opinion plus one.
fn enough_answered(
    outcomes: &[(String, EngineResult)],
    early: bool,
    families: &families::Families,
) -> bool {
    early && outcomes.len() >= 2 && early_consensus_enough(outcomes, families)
}

/// C4: enough independent families already agree on a top-3 URL
/// that waiting for the stragglers cannot change the answer.
fn early_consensus_enough(
    outcomes: &[(String, EngineResult)],
    families: &families::Families,
) -> bool {
    use std::collections::{HashMap, HashSet};
    let mut url_families: HashMap<String, HashSet<String>> = HashMap::new();
    let mut families_ok: HashSet<String> = HashSet::new();
    for (label, result) in outcomes {
        let Ok(answer) = result else {
            continue;
        };
        let fam = rank::family_name(families, engine_name(label));
        families_ok.insert(fam.clone());
        for h in answer.hits.iter().take(3) {
            url_families
                .entry(rank::norm_key(&h.url))
                .or_default()
                .insert(fam.clone());
        }
    }
    families_ok.len() >= 2 && url_families.values().any(|fams| fams.len() >= 2)
}

pub struct Searcher {
    fetcher: Fetcher,
    /// Shared with crawl + fetch (v4 A2): one health world for
    /// every proxied path. Clone of the daemon's Arc.
    pool: std::sync::Arc<EgressPool>,
    google: engines::google_wml::ProfileSelector,
    /// engine -> trust EWMA (1.0 seed; 0.2..2.0 clamp).
    /// Persisted to disk: an engine that learned "this walled me"
    /// keeps that memory across daemon restarts instead of
    /// re-paying the same failure every boot.
    trust: Mutex<HashMap<String, f64>>,
    /// B2: per-intent trust (`engine|intent_code` -> EWMA). Roster
    /// order prefers the intent sample; engine-global is the
    /// fallback and still moves on every outcome.
    trust_intent: Mutex<HashMap<String, f64>>,
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
    /// The lane ledger: per (source, lane) standing, shared across calls and
    /// processes. Loaded once, written back when a search taught it something.
    ledger: Mutex<ledger::Ledger>,
    /// Single-flight: two identical in-flight queries spend
    /// egress budget ONCE : the follower awaits the
    /// leader's result. Stampedes are an agent reality
    /// (parallel tool calls love the same query).
    inflight: Mutex<std::collections::HashSet<String>>,
    /// BYOK single-flight: a metered provider is not re-billed by a
    /// concurrent identical query (the local fan-out has `inflight`
    /// for the same reason). Keyed on the byok cache key.
    byok_inflight: Mutex<std::collections::HashSet<String>>,
    /// v3 warm handoff: enrichment bodies cached for the
    /// subsequent `web_fetch` of a top result (search → fetch
    /// is THE agent pipeline). One-shot, TTL'd, bounded.
    prewarms: std::sync::Arc<std::sync::Mutex<PrewarmCache>>,
    /// B3: learned host quality (www-stripped host -> EWMA).
    /// Filled by enrich/prefetch outcomes; nudges ranking
    /// slightly after enough samples. Kill: search.quality_prior.
    quality: Mutex<QualityMap>,
    quality_dirty: std::sync::atomic::AtomicBool,
    /// B4: agent-outcome demotes (`class|host` -> EWMA).
    /// Default off (`search.outcome_feedback`); never extra fetches.
    outcome: Mutex<OutcomeMap>,
    outcome_dirty: std::sync::atomic::AtomicBool,
    /// What each source returned for the most recent local fan-out, before the
    /// merge. Read by `search-bench` to tell a retrieval miss (the answer never
    /// arrived) from an ordering miss (it arrived and was ranked low); nothing
    /// else reads it, and a concurrent caller simply overwrites it.
    last_raw: Mutex<Vec<(String, Vec<engines::Hit>)>>,
    /// Which sources return the same results, learned from their overlap.
    families: Mutex<families::Families>,
}

#[cfg(feature = "rerank")]
/// Run synchronous ranking outside Tokio's async worker set.
///
/// Semantic reranking can enter ONNX inference and wait on the shared session
/// mutex. The ranking API stays synchronous, so the blocking pool is the narrow
/// boundary that keeps unrelated async work progressing.
async fn run_blocking_ranking<F, T>(job: F) -> Result<Option<T>, FetchError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let budget = stage_budget(Duration::from_secs(30), Duration::from_millis(500));
    if budget.is_zero() {
        return Ok(None);
    }
    match tokio::time::timeout(budget, tokio::task::spawn_blocking(job)).await {
        Ok(result) => result
            .map(Some)
            .map_err(|e| FetchError::Http(format!("search: ranking worker failed: {e}"))),
        Err(_) => Ok(None),
    }
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
    /// Outbound requests this lane spent (a feed refresh can be two).
    #[serde(default)]
    pub requests: usize,
}

#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<Merged>,
    pub intent: Intent,
    pub report: Vec<EngineReport>,
    pub cached: bool,
    pub elapsed: Duration,
    /// BYOK provider name (None = local search).
    pub provider: Option<String>,
    /// Cross-encoder reranking applied (feature on + model loaded).
    pub reranked: bool,
    /// C5: byte-derived featured snippet / instant / knowledge.
    /// Never merged into `results`. None when absent.
    pub instant: Option<instant::InstantAnswer>,
    /// Post-fan-out phase timings for the search-debug meta:
    /// (stage, ms). Empty for the cache/BYOK paths.
    pub stage_ms: Vec<(&'static str, u128)>,
}

impl Searcher {
    pub fn new(fetcher: Fetcher, pool: EgressPool) -> Self {
        Self::new_shared(fetcher, std::sync::Arc::new(pool))
    }

    /// Share one process-wide pool (daemon path).
    pub fn new_shared(fetcher: Fetcher, pool: std::sync::Arc<EgressPool>) -> Self {
        let (trust, trust_intent, failures) = load_health_disk();
        Self {
            fetcher,
            pool,
            google: engines::google_wml::ProfileSelector::from_env(),
            trust: Mutex::new(trust),
            trust_intent: Mutex::new(trust_intent),
            health_dirty: std::sync::atomic::AtomicBool::new(false),
            cache: Mutex::new(load_cache_disk()),
            failures: Mutex::new(failures),
            ledger: Mutex::new(ledger::Ledger::load()),
            inflight: Mutex::new(std::collections::HashSet::new()),
            byok_inflight: Mutex::new(std::collections::HashSet::new()),
            prewarms: std::sync::Arc::new(std::sync::Mutex::new(PrewarmCache::new())),
            quality: Mutex::new(load_quality_disk()),
            quality_dirty: std::sync::atomic::AtomicBool::new(false),
            outcome: Mutex::new(load_outcome_disk()),
            outcome_dirty: std::sync::atomic::AtomicBool::new(false),
            last_raw: Mutex::new(Vec::new()),
            families: Mutex::new(families::load()),
        }
    }

    /// The per-source results of the most recent uncached local search, in the
    /// order the sources answered. Empty when the last call was a cache hit.
    pub fn last_raw(&self) -> Vec<(String, Vec<engines::Hit>)> {
        self.last_raw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Decide whether one lane is planned: the ledger's standing first (a
    /// rested lane is a real answer, not a failure), then the request budget.
    ///
    /// Returns the wait the lane owes its own pace before the request may go
    /// out (`Some(0)` when the bucket has a token). A lane the ledger is
    /// resting, or one whose pace is longer than this call will pay, is
    /// reported rather than silently dropped.
    fn plan_lane(
        &self,
        source: &str,
        skipped: &mut Vec<(String, EngineResult)>,
    ) -> Option<Duration> {
        let key = ledger::key(lane_source(source), lane_of(source));
        let state = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ready(&key);
        match state {
            ledger::State::Ready => Some(Duration::ZERO),
            ledger::State::Paced(secs) => {
                let wait = Duration::from_secs(secs);
                if wait <= PACE_WAIT_MAX {
                    Some(wait)
                } else {
                    skipped.push((
                        source.to_string(),
                        Err((format!("paced:{secs}"), "ledger".into(), false)),
                    ));
                    None
                }
            }
            ledger::State::Rested(secs) => {
                skipped.push((
                    source.to_string(),
                    Err((format!("rested:{secs}"), "ledger".into(), false)),
                ));
                None
            }
        }
    }

    /// Tell the ledger what a lane's request amounted to.
    fn record_lane(&self, source: &str, status: &str) {
        let Some(outcome) = ledger::classify(status) else {
            return;
        };
        let retry = ledger::retry_after(status);
        let key = ledger::key(lane_source(source), lane_of(source));
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(&key, outcome, retry);
    }

    /// The shared egress pool (crawl/fetch attach the same Arc).
    pub fn egress_pool(&self) -> &std::sync::Arc<EgressPool> {
        &self.pool
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
        query::intitle_filter(query, &mut results);
        query::filetype_filter(query, &mut results);
        Some(SearchOutcome {
            results,
            intent,
            report: report.clone(),
            cached: true,
            elapsed: t0.elapsed(),
            provider,
            reranked: false,
            // BYOK providers do not surface a byte-derived SERP
            // instant layer; keep the slot honest.
            instant: None,
            stage_ms: Vec::new(),
        })
    }

    /// Issue #195: persist a fresh BYOK outcome under the BYOK key
    /// namespace, TTL'd like the local cache. Skips an empty result
    /// set (the provider path errors on empty, so this is defensive).
    /// The single-flight key for a BYOK query: the byok cache key,
    /// deliberately NOT carrying max_results (the leader publishes
    /// the provider's full top-12, so a max=2 caller and a max=10
    /// caller share one provider round-trip).
    pub fn byok_flight_key(&self, query: &str, intent: Intent) -> String {
        byok_cache_key(query, intent)
    }

    /// BYOK single-flight: a concurrent identical query JOINS the
    /// leader's provider round-trip instead of double-billing the
    /// metered provider. Followers poll the byok cache (the leader
    /// publishes there before filtering); a leader that dies or
    /// stalls past 12s computes itself, exactly like the local
    /// fan-out flight above.
    pub async fn byok_flight(
        &self,
        key: String,
        query: &str,
        intent: Intent,
        max: usize,
        compute: impl std::future::Future<Output = Result<SearchOutcome, String>>,
    ) -> Result<SearchOutcome, String> {
        let leader = {
            let mut m = self
                .byok_inflight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            m.insert(key.clone())
        };
        if !leader {
            for _ in 0..120 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Some(hit) = self.byok_cache_get(query, intent, max) {
                    return Ok(hit);
                }
            }
            // Leader died or stalled : compute ourselves.
        }
        let _guard = InflightGuard {
            map: &self.byok_inflight,
            key,
        };
        compute.await
    }

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

    /// Probe configured proxies in the background and seed their RTTs.
    /// Generic failures bench lanes only after a completed batch with a
    /// successful echo; authentication failures are applied immediately.
    /// One global batch budget caps the whole run: a pool of stalled
    /// lanes cannot cost N * 6s of startup. The caller owns the returned
    /// task and must abort it on shutdown.
    pub fn preflight(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            // Per-probe liveness timeout plus one ceiling for the whole
            // batch. A probe never outlives the remaining budget, and
            // lanes the budget never reaches stay unprobed (inconclusive:
            // nothing is published for them).
            const PROBE_TIMEOUT: Duration = Duration::from_secs(6);
            const BATCH_BUDGET: Duration = Duration::from_secs(20);
            let proxies = this.pool.proxies();
            let batch_started = Instant::now();
            let mut completed = true;
            let mut echo_available = false;
            let mut dead = Vec::new();
            for proxy in proxies {
                let remaining = BATCH_BUDGET.saturating_sub(batch_started.elapsed());
                if remaining.is_zero() {
                    completed = false;
                    break;
                }
                let id = proxy.id();
                let started = Instant::now();
                let probe = this.fetcher.fetch_once_via(
                    "https://api.ipify.org/",
                    &[],
                    Some(&proxy),
                    false,
                    None,
                );
                match tokio::time::timeout(PROBE_TIMEOUT.min(remaining), probe).await {
                    Ok(Ok(o)) if o.status == 200 => {
                        echo_available = true;
                        this.pool.observe_rtt(&id, started.elapsed());
                    }
                    Ok(Err(e)) if format!("{e}").contains("CONNECT -> 407") => {
                        this.pool.report_auth_fail(&id);
                    }
                    _ => {
                        dead.push(id);
                    }
                }
            }
            // An entirely unavailable shared echo is inconclusive. Publish
            // failures only after a completed batch; temporary benches can
            // otherwise move an in-flight caller onto direct before the
            // last result. Existing dead/auth evidence is not cleared by a
            // failed probe, and a budget-cut partial batch publishes
            // nothing at all.
            if completed && echo_available {
                for id in dead {
                    this.pool.report_dead(&id);
                }
            }
        })
    }

    /// True when an engine is benched for chronic failure.
    fn quarantined(&self, engine: &str) -> bool {
        let f = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matches!(f.get(engine), Some(&(n, at)) if n >= 3 && at.elapsed() < QUARANTINE_TTL)
    }

    /// Bench a source that asked for time. Same clock and same persistence as
    /// the chronic-failure bench, but it touches neither trust nor egress
    /// health: a throttle is the source working as published.
    fn rest(&self, engine: &str) {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(engine.to_string(), (3, Instant::now()));
        self.health_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_outcome(&self, engine: &str, ok: bool) {
        // A source whose trust has already reached the floor is not on a
        // probation it can fail three times: it benches on the next failure.
        // The streak is what persists across processes (the loader keeps only
        // streaks of three or more), so this is also what makes a dead engine
        // stay out of a CLI that starts a fresh process per search.
        let at_floor = self
            .trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(engine)
            .is_some_and(|t| *t <= TRUST_FLOOR);
        let mut f = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ok {
            f.remove(engine);
        } else {
            let e = f.entry(engine.to_string()).or_insert((0, Instant::now()));
            e.0 += 1;
            if at_floor {
                e.0 = e.0.max(3);
            }
            e.1 = Instant::now();
        }
        self.health_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Drop every cached answer, in memory. Repeats inside the cache TTL are
    /// otherwise served from it; a health check must ask the sources.
    pub fn forget_cached(&self) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Ask a first-party interface directly, outside the search path: the oracle
    /// a canary is judged against. Bounded, and a failure is just a failure.
    pub async fn first_party(&self, vertical: &str, query: &str) -> Vec<engines::Hit> {
        const ORACLE_BUDGET: Duration = Duration::from_secs(8);
        tokio::time::timeout(
            ORACLE_BUDGET,
            verticals::run(&self.fetcher, vertical, query, None, None),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
    }

    /// Every source this process knows about and what it currently means for the
    /// next search: verified, suspect, or benched until a time. Read-only; it
    /// derives from the same trust and failure state the roster uses.
    pub fn health_report(&self) -> Vec<SourceHealth> {
        let trust = self
            .trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let failures = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let families = self
            .families
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut names: Vec<&String> = trust.keys().chain(failures.keys()).collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                let t = trust.get(name).copied().unwrap_or(1.0);
                let f = failures.get(name).map(|(n, at)| (*n, at.elapsed()));
                SourceHealth {
                    name: name.clone(),
                    trust: t,
                    strikes: f.map_or(0, |(n, _)| n),
                    state: source_state(t, f),
                    family: families.family_of(name),
                }
            })
            .collect()
    }

    /// Should this source sit out this search?
    ///
    /// Yes when its trust is at the floor and it is inside its bench window,
    /// which [`Searcher::record_outcome`] extends to [`REPROBE_AFTER`] once the
    /// floor is reached. After the window it is tried exactly once; a failure
    /// re-benches it, a success lifts it back into the roster. Nothing here
    /// needs a restart, a human, or a separate probe schedule.
    fn resting(&self, base_engine: &str) -> bool {
        let trust = self
            .trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(base_engine)
            .copied()
            .unwrap_or(1.0);
        if trust > TRUST_FLOOR {
            return false;
        }
        let failures = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match failures.get(base_engine) {
            Some((strikes, at)) => *strikes >= 3 && at.elapsed() < REPROBE_AFTER,
            // At the floor with no streak on record is contradictory state (a
            // hand-edited file); one request settles it either way.
            None => false,
        }
    }

    pub async fn search(
        &self,
        query: &str,
        max_results: usize,
        forced_intent: Option<Intent>,
    ) -> Result<SearchOutcome, FetchError> {
        let started = Instant::now();
        // The merge holds at most 12; asking for more is asking for what the
        // cache cannot hold, and zero results is not a search.
        let max_results = max_results.clamp(1, 12);
        if let Some(problem) = validate_query(query) {
            return Err(FetchError::Http(format!("search: {problem}")));
        }
        let normalized = query::normalize(query);
        let query = normalized.as_str();
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
                if let Some((at, cached, _total, reports)) = hit
                    && at.elapsed() < cache_ttl(intent_probe, query)
                {
                    let mut results = cached.iter().take(max_results).cloned().collect();
                    site_filter(query, &mut results);
                    query::intitle_filter(query, &mut results);
                    query::filetype_filter(query, &mut results);
                    return Ok(SearchOutcome {
                        results,
                        intent: intent_probe,
                        report: reports,
                        cached: true,
                        elapsed: started.elapsed(),
                        provider: None,
                        reranked: crate::search::rerank::loaded(),
                        // Instant is not cached: a stale featured
                        // snippet presented as fresh would lie.
                        instant: None,
                        stage_ms: Vec::new(),
                    });
                }
            }
            // Leader died or timed out : compute ourselves.
        }
        let _inflight_guard = InflightGuard {
            map: &self.inflight,
            key: sf_key,
        };
        let outcome = self
            .search_inner(query, max_results, forced_intent, started)
            .await;
        // The ledger learned something: rest a lane, or walk it back.
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .save_if_dirty();
        outcome
    }

    async fn search_inner(
        &self,
        query: &str,
        max_results: usize,
        forced_intent: Option<Intent>,
        started: Instant,
    ) -> Result<SearchOutcome, FetchError> {
        self.last_raw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        // Cache stores top-12; asking for more just
        // re-lists the same tail.
        let max_results = max_results.clamp(1, 12);
        let intent = forced_intent.unwrap_or_else(|| intent::detect(query));
        // The query's language, from its script. It goes to the sources that
        // accept one and to Wikipedia's own edition; the contract uses the same
        // script test to tell a native result from a foreign page.
        let locale = locale::detect(query);
        let cache_key = format!("{}|{}", norm_query(query), intent.code());

        if let Some((at, cached, _total, reports)) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&cache_key)
            && at.elapsed() < cache_ttl(intent, query)
        {
            let mut results = cached.iter().take(max_results).cloned().collect();
            site_filter(query, &mut results);
            query::intitle_filter(query, &mut results);
            query::filetype_filter(query, &mut results);
            return Ok(SearchOutcome {
                results,
                intent,
                report: reports.clone(),
                cached: true,
                elapsed: started.elapsed(),
                provider: None,
                reranked: crate::search::rerank::loaded(),
                // Instant is not cached: a stale featured
                // snippet presented as fresh would lie.
                instant: None,
                stage_ms: Vec::new(),
            });
        }

        // A name with an ecosystem word is answered by the registry that owns
        // it: one request, no fan-out, no ambiguity to rank around.
        if let Some(plan) = resolve::plan(query)
            && plan.ecosystem.is_some()
            && let resolve::Resolution::Hits(resolved) = resolve::run(
                &plan,
                &self.fetcher,
                stage_budget(Duration::from_millis(1500), Duration::from_millis(300)),
            )
            .await
        {
            return Ok(self.resolved_outcome(query, intent, max_results, started, resolved));
        }

        let engines = intent::engines_for(intent);
        // C2: compile operators once; each engine gets the form it
        // actually honors (DDG lite strips site: so BM25 is not
        // polluted with the literal token).
        let compiled = query::compile(query);
        let mut verticals = intent::verticals_for(intent, query);
        // site:github.com / site:stackoverflow.com / etc. join the
        // fan-out as the site's own API.
        if let Some(site) = &compiled.site
            && let Some(sv) = query::site_vertical(query::site_scope(site).0)
            && !verticals.contains(&sv)
        {
            verticals.push(sv);
        }
        let verticals = intent::trim_verticals_for_stress(verticals, self.pool.stress());

        // Fan out: engines each get their own egress
        // (spreading is the anti-rate-limit move).
        let mut futures: Vec<(String, TaskFut)> = Vec::new();
        // A lane the ledger rests, or the budget cannot afford, is reported
        // rather than silently dropped: the census says which, and why.
        let mut skipped: Vec<(String, EngineResult)> = Vec::new();
        let budget = crate::config::cfg().search.request_budget;
        let mut used_egresses: Vec<String> = Vec::new();
        let mut queries: Vec<String> = vec![query.to_string()];
        if let Some(v) = intent::variant(query) {
            queries.push(v);
        }
        // Engines get the original query; the recall variant
        // goes only to the first two engines (top trust).
        let benched =
            |e: &str| self.quarantined(engine_health_key(e)) || self.resting(engine_health_key(e));
        let mut live: Vec<&str> = engines.iter().filter(|e| !benched(e)).copied().collect();
        // Every source resting at once means the roster, not the sources, is
        // wrong: try them all rather than answering from nothing.
        if live.is_empty() {
            live = engines
                .iter()
                .filter(|e| !self.quarantined(engine_health_key(e)))
                .copied()
                .collect();
        }
        // A benched engine is named in the census as rested, never dropped
        // without a word: a source missing from the census reads as one that
        // was never part of the plan.
        for e in engines.iter().filter(|e| !live.contains(e)) {
            skipped.push((
                engine_name(e).to_string(),
                Err(("rested".into(), "bench".into(), false)),
            ));
        }
        // Rank engines by learned trust so width cuts drop
        // the weakest first. Per-intent sample wins; engine-global
        // is the fallback (B2).
        {
            let key = |e: &str| persist::trust_intent_key(engine_health_key(e), intent);
            let trust = self
                .trust
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let trust_i = self
                .trust_intent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let score = |e: &str| -> f64 {
                trust_i
                    .get(&key(e))
                    .or_else(|| trust.get(engine_health_key(e)))
                    .copied()
                    .unwrap_or(1.0)
            };
            live.sort_by(|a, b| score(b).total_cmp(&score(a)));
        }
        // ── Adaptive fan-out width: the governor. Under
        // stress the system shrinks its appetite instead of
        // burning lanes : consensus survives at width 2 by
        // construction (two independent index families).
        let width = width_for_stress(self.pool.stress(), live.len());
        for e in live.drain(width..) {
            skipped.push((
                engine_name(e).to_string(),
                Err(("not-asked".into(), "stress".into(), false)),
            ));
        }
        let mut assignments: Vec<(String, String)> = live
            .iter()
            .map(|e| (e.to_string(), query::for_engine(&compiled, e)))
            .collect();
        // Recall variants spend lanes : only when the
        // governor did NOT cut the roster (healthy pool).
        if queries.len() > 1 && self.pool.stress() < 0.15 {
            for e in live.iter().take(2) {
                assignments.push((e.to_string(), queries[1].clone()));
            }
        }
        // Authority cannot lift a source absent from the candidate pool.
        // Two scoped recalls join the initial wave, on an engine that honors
        // site:, only while the pool is healthy. Results still need ranking.
        if matches!(intent, Intent::Web | Intent::Code) && self.pool.stress() < 0.15 {
            // Yahoo is the engine that honors `site:` and answers it with the
            // scoped pages; measured against bing (ignores it) and the DDG
            // endpoints (answer nothing). The recall used to go to the others.
            let engine = live
                .iter()
                .find(|e| **e == "yahoo")
                .or_else(|| live.iter().find(|e| matches!(**e, "bing" | "ddg_html")))
                .copied()
                .or_else(|| live.contains(&"ddg").then_some("ddg_html"));
            if let Some(engine) = engine {
                for q in authority::official_queries(&compiled) {
                    assignments.push((engine.to_string(), q));
                }
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
            pool: self.pool.as_ref(),
            google: &self.google,
            locale,
        };
        for (engine, q, eg) in assign_egresses(self.pool.as_ref(), assignments, &mut used_egresses)
        {
            let name = engine_name(&engine).to_string();
            let Some(pace) = self.plan_lane(&name, &mut skipped) else {
                continue;
            };
            futures.push((
                name,
                paced(
                    pace,
                    Box::pin(tasks::engine_task_with_budget(
                        engine,
                        q,
                        eg.id,
                        eg.proxy,
                        context,
                        lane_budget(),
                        None,
                    )),
                ),
            ));
        }
        // Verticals: direct, friendly APIs.
        let verticals: Vec<&str> = verticals
            .into_iter()
            .filter(|v| !self.quarantined(v))
            .collect();
        let mut vfuts: Vec<(String, TaskFut)> = Vec::new();
        for v in verticals {
            let Some(pace) = self.plan_lane(v, &mut skipped) else {
                continue;
            };
            vfuts.push((
                engine_name(v).to_string(),
                paced(
                    pace,
                    Box::pin(vertical_task(
                        v.to_string(),
                        query.to_string(),
                        &self.fetcher,
                        None,
                        lane_budget(),
                        locale,
                    )),
                ),
            ));
        }
        if matches!(intent, Intent::Code | Intent::Paper | Intent::Entity) {
            // For a record query the vertical is the answer: plan it before the
            // engines, so the budget cannot spend its slot on a general index.
            vfuts.append(&mut futures);
            futures = vfuts;
        } else {
            futures.append(&mut vfuts);
        }

        // One item of one owner is answered by the documentation that owns it:
        // `itertools::izip` asks docs.rs's own index page for the crate, in
        // parallel with the fan-out and with no ambiguity to rank around. The
        // href is read from that page, never guessed, so an item that does not
        // exist produces nothing rather than a 404 dressed as an answer.
        if let Some(item) = resolve::item(&query::QueryFacts::new(query))
            && let Some(pace) = self.plan_lane("docs", &mut skipped)
        {
            futures.push((
                "docs".to_string(),
                paced(
                    pace,
                    Box::pin(tasks::docs_task(
                        item,
                        &self.fetcher,
                        Duration::from_millis(900),
                    )),
                ),
            ));
        }

        // A bare name is resolved alongside the fan-out: no latency cost, and
        // its records lead the list when the name is real. The lane gets a
        // shorter budget than the fan-out's soft mark so its answer lands with
        // the first ones instead of arriving late.
        if let Some(plan) = resolve::plan(query)
            && plan.ecosystem.is_none()
            && let Some(pace) = self.plan_lane("registry", &mut skipped)
        {
            futures.push((
                "registry".to_string(),
                paced(
                    pace,
                    Box::pin(tasks::registry_task(
                        plan,
                        &self.fetcher,
                        Duration::from_millis(900),
                    )),
                ),
            ));
        }

        // The news program races with the rest: the wire, the feeds and the
        // leased index, each its own lane. A news answer is dated and exact,
        // and the date contract has already dropped what is outside the query's
        // window — but the open web still runs, because the official page an
        // announcement points at is often not in the wire's index.
        if news::wants_news(query, intent) {
            if let Some(pace) = self.plan_lane("bing-news", &mut skipped) {
                // The wire is a search endpoint, not a publisher's API: it rides
                // the pool like any engine, so one direct-IP throttle does not
                // silence every news query.
                let news_proxy = self.pool.pick("bing-news", &used_egresses, true);
                let news_proxy = match news_proxy {
                    Some(egress) => {
                        used_egresses.push(egress.id.clone());
                        egress.proxy.clone()
                    }
                    None => None,
                };
                futures.push((
                    "bing-news".to_string(),
                    paced(
                        pace,
                        Box::pin(tasks::news_wire_task(
                            query.to_string(),
                            &self.fetcher,
                            news_proxy,
                            lane_budget(),
                        )),
                    ),
                ));
            }
            if let Some(pace) = self.plan_lane("feed", &mut skipped) {
                futures.push((
                    "feed".to_string(),
                    paced(
                        pace,
                        Box::pin(tasks::news_feed_task(
                            query.to_string(),
                            &self.fetcher,
                            lane_budget(),
                        )),
                    ),
                ));
            }
            if let Some(pace) = self.plan_lane("gdelt", &mut skipped) {
                futures.push((
                    "gdelt".to_string(),
                    paced(
                        pace,
                        Box::pin(tasks::news_gdelt_task(
                            query.to_string(),
                            &self.fetcher,
                            lane_budget(),
                        )),
                    ),
                ));
            }
        }

        // ── The budget ──
        //
        // A search has a request ceiling, and the lanes are chosen by class so
        // the answer is the last thing cut: up to four engines (the general
        // recall), two news lanes (the answer to "what happened"), two vertical
        // records (the answer to "what is this"), and a bare-name registry guess
        // only if there is room. Every lane over the line is reported
        // `not-asked`, never silently dropped.
        // A query about what happened lately is answered by the news program:
        // its lanes claim the budget first. Queued after the engines, they got
        // what four engines left, which was one lane, and the feed mesh was
        // never asked on a news query.
        if intent == Intent::News || contract::relative_window(query).is_some() {
            let is_news = |name: &str| matches!(name, "bing-news" | "feed" | "gdelt");
            futures.sort_by_key(|(name, _)| !is_news(name));
        }
        let mut kept: Vec<(String, TaskFut)> = Vec::with_capacity(futures.len());
        let (mut engines_kept, mut verticals_kept, mut news_kept, mut guesses_kept) =
            (0usize, 0usize, 0usize, 0usize);
        let mut spent = 0usize;
        for (name, fut) in futures.drain(..) {
            let class = if matches!(name.as_str(), "bing-news" | "feed" | "gdelt") {
                2
            } else if matches!(name.as_str(), "registry" | "registry-any") {
                3
            } else if rank::is_vertical(&name) {
                1
            } else {
                0
            };
            // bing may spend a second request on its structured fallback; the
            // budget counts the ceiling, not the hope.
            let cost = if name == "bing" { 2 } else { 1 };
            // A paper query needs its three indexes; everything else is better
            // served by another general index than by a third vertical.
            let vert_cap = if intent == Intent::Paper { 3 } else { 2 };
            let room = match class {
                0 => engines_kept < 4,
                1 => verticals_kept < vert_cap,
                2 => news_kept < 2,
                _ => guesses_kept < 1,
            } && spent + cost <= budget;
            if room {
                match class {
                    0 => engines_kept += 1,
                    1 => verticals_kept += 1,
                    2 => news_kept += 1,
                    _ => guesses_kept += 1,
                }
                spent += cost;
                kept.push((name, fut));
            } else {
                skipped.push((name, Err(("not-asked".into(), "budget".into(), false))));
            }
        }
        let futures = kept;

        // ── One budget, raced ──
        //
        // The call commits to what has answered by the soft mark and cancels the
        // rest. A source that misses it is reported `late`: never waited on, never
        // retried inside the call. Recovery is the state's job on the next call,
        // through a different lane, so one slow or walled source costs this query
        // nothing but its own absence.
        // Two marks, not one. The soft mark commits to an answer that is already
        // good; the hard mark bounds the wait when the sources are slow. A single
        // fixed mark measured on this network turned a slow minute into lost
        // answers (hit@5 0.53 against 0.68), because it cut the sources that
        // would have answered.
        let started_at = tokio::time::Instant::now();
        let soft_deadline = started_at + stage_budget(FANOUT_SOFT_MARK, Duration::from_millis(300));
        let hard_deadline = started_at + stage_budget(FANOUT_HARD_MARK, Duration::from_millis(200));
        let mut outstanding: std::collections::HashMap<String, usize> = Default::default();
        for (name, _) in &futures {
            *outstanding.entry(name.clone()).or_insert(0) += 1;
        }
        use futures_util::StreamExt as _;
        let mut futs: futures_util::stream::FuturesUnordered<_> = futures
            .into_iter()
            .map(|(name, fut)| {
                Box::pin(async move { (name, fut.await) })
                    as std::pin::Pin<
                        Box<
                            dyn std::future::Future<Output = (String, (String, EngineResult))>
                                + Send,
                        >,
                    >
            })
            .collect();
        let early = crate::config::cfg().search.search_early;
        let known_families = self
            .families
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut outcomes: Vec<(String, EngineResult)> = skipped;
        // A news query's answer is the wire: committing on consensus before the
        // news lanes report drops the record the query asked for. They get a
        // bounded head start past the soft mark; then the call commits anyway.
        let news_deadline = tokio::time::Instant::now() + Duration::from_millis(1800);
        let news_wanted = news::wants_news(query, intent);
        loop {
            let news_pending = news_wanted
                && ["bing-news", "feed", "gdelt"]
                    .iter()
                    .any(|lane| outstanding.get(*lane).copied().unwrap_or(0) > 0);
            let now = tokio::time::Instant::now();
            if enough_answered(&outcomes, early, &known_families)
                && (!news_pending || now >= news_deadline)
            {
                break;
            }
            // The soft mark only ends the wait when the answer is already
            // settled (independent families agree). A count of items is not
            // agreement: measured, stopping at two fat lanes cut contributing
            // sources from 3.5 to 1.8 per query and cost ranking quality, because
            // the third source was 200 ms behind, not absent.
            let now = tokio::time::Instant::now();
            if now >= hard_deadline {
                break;
            }
            // Until the soft mark, then a short late mark (a news query waits
            // for its wire, a call with nothing usable yet keeps the hard
            // budget). Past that, the call commits to what has answered: a
            // straggler at 2.7 s is not worth 1.5 s of everyone's wall clock.
            let until = if now < soft_deadline {
                soft_deadline
            } else {
                let (_, items) = answered(&outcomes);
                let deadline = if items == 0 {
                    hard_deadline
                } else if news_pending {
                    news_deadline.min(hard_deadline)
                } else {
                    soft_deadline + FANOUT_LATE_MARK
                };
                if now >= deadline {
                    break;
                }
                deadline
            };
            match tokio::time::timeout_at(until, futs.next()).await {
                Ok(Some((name, (label, result)))) => {
                    if let Some(n) = outstanding.get_mut(&name) {
                        *n = n.saturating_sub(1);
                    }
                    outcomes.push((label, result));
                }
                // Every lane answered.
                Ok(None) => break,
                // The mark passed with lanes still running: they are late.
                Err(_) => break,
            }
        }
        // ── One bounded second ask ──
        //
        // A thin merge gets one more attempt from the sources that misbehaved,
        // through a different lane, inside the hard mark. Never a third ask, and
        // never when the first wave already answered well: recovery is still the
        // state's job across calls, this only rescues a query where two thirds
        // of the sources were walled at once.
        let (lanes, items) = answered(&outcomes);
        if (lanes < THIN_LANES || items < THIN_ITEMS) && tokio::time::Instant::now() < hard_deadline
        {
            let mut asked: std::collections::HashSet<String> = Default::default();
            let mut retry: Vec<(String, TaskFut)> = Vec::new();
            for (label, result) in &outcomes {
                let Err((status, _, engine_lane)) = result else {
                    continue;
                };
                if !*engine_lane || !worth_second_ask(status) {
                    continue;
                }
                let engine = engine_name(label).to_string();
                if !asked.insert(engine.clone()) {
                    continue;
                }
                if outcomes
                    .iter()
                    .any(|(l, r)| engine_name(l) == engine && r.is_ok())
                {
                    continue;
                }
                let Some(eg) = self.pool.pick(&engine, &used_egresses, true) else {
                    continue;
                };
                let budget = hard_deadline.saturating_duration_since(tokio::time::Instant::now());
                retry.push((
                    engine.clone(),
                    Box::pin(tasks::engine_task_with_budget(
                        engine,
                        query.to_string(),
                        eg.id,
                        eg.proxy,
                        context,
                        budget.min(SECOND_ASK),
                        None,
                    )),
                ));
            }
            if !retry.is_empty() {
                let secs: futures_util::stream::FuturesUnordered<_> = retry
                    .into_iter()
                    .map(|(name, fut)| {
                        Box::pin(async move { (name, fut.await) })
                            as std::pin::Pin<
                                Box<
                                    dyn std::future::Future<
                                            Output = (String, (String, EngineResult)),
                                        > + Send,
                                >,
                            >
                    })
                    .collect();
                let left = hard_deadline.saturating_duration_since(tokio::time::Instant::now());
                if let Ok(done) =
                    tokio::time::timeout(left, futures_util::future::join_all(secs)).await
                {
                    for (name, outcome) in done {
                        if let Some(n) = outstanding.get_mut(&name) {
                            *n = n.saturating_sub(1);
                        }
                        outcomes.push(outcome);
                    }
                }
            }
        }

        // A call with nothing yet gives the lanes the scheduler cut one more ask
        // inside the hard mark: a straggler is slow, not absent, and an empty
        // answer is the one outcome a caller cannot use. This is not a rotation
        // around a limit — a lane that refused was rested by the ledger and is
        // not outstanding here.
        if answered(&outcomes).1 == 0 && tokio::time::Instant::now() < hard_deadline {
            let mut rescue: Vec<(String, TaskFut)> = Vec::new();
            for (name, pending) in &outstanding {
                if *pending == 0 || !engines.contains(&name.as_str()) {
                    continue;
                }
                let Some(eg) = self.pool.pick(name, &used_egresses, true) else {
                    continue;
                };
                let budget = hard_deadline.saturating_duration_since(tokio::time::Instant::now());
                rescue.push((
                    name.clone(),
                    Box::pin(tasks::engine_task_with_budget(
                        name.clone(),
                        query.to_string(),
                        eg.id,
                        eg.proxy,
                        context,
                        budget.min(SECOND_ASK),
                        None,
                    )),
                ));
            }
            if !rescue.is_empty() {
                let secs: futures_util::stream::FuturesUnordered<_> = rescue
                    .into_iter()
                    .map(|(name, fut)| {
                        Box::pin(async move { (name, fut.await) })
                            as std::pin::Pin<
                                Box<
                                    dyn std::future::Future<
                                            Output = (String, (String, EngineResult)),
                                        > + Send,
                                >,
                            >
                    })
                    .collect();
                let left = hard_deadline.saturating_duration_since(tokio::time::Instant::now());
                if let Ok(done) =
                    tokio::time::timeout(left, futures_util::future::join_all(secs)).await
                {
                    for (name, outcome) in done {
                        if let Some(n) = outstanding.get_mut(&name) {
                            *n = n.saturating_sub(1);
                        }
                        outcomes.push(outcome);
                    }
                }
            }
        }

        let mut late: Vec<String> = outstanding
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .map(|(name, _)| name)
            .collect();
        late.sort();

        let mut per_engine: Vec<(String, Vec<engines::Hit>)> = Vec::new();
        let mut report = Vec::new();
        // A provable junk set waits for the whole query: a set is another topic
        // only when the query names an entity and the set shares none of its
        // rare terms (§4). A set that answered nothing is `Empty`, never this.
        let mut off_query: Vec<(String, String)> = Vec::new();
        // C5: best instant answer across lanes (first non-empty wins
        // by engine trust order, which outcomes already respect).
        let mut instant: Option<instant::InstantAnswer> = None;
        let all: Vec<(String, EngineResult)> = outcomes;
        for (label, outcome) in all {
            let profile = label.split_once('@').map(|(_, p)| p.to_string());
            let engine = engine_name(&label).to_string();
            match outcome {
                Ok(answer) => {
                    let base = engine_health_key(&engine);
                    self.record_lane(&engine, "ok");
                    if answer.class.credits() {
                        // A real answer: the only branch that earns the source,
                        // the lane and the profile selector any credit.
                        self.record_outcome(base, true);
                        if answer.engine_lane {
                            self.pool.report_ok(&engine, &answer.egress);
                            self.bump_trust(base, intent, true);
                        }
                    } else if answer.class == engines::AnswerClass::Thin {
                        // Real items, incomplete answer: use them, learn nothing.
                        // Thinness is not evidence about the source.
                    } else if answer.class == engines::AnswerClass::Junk && answer.engine_lane {
                        // Provably another topic: its items never reach the merge.
                        off_query.push((base.to_string(), engine.clone()));
                    }
                    if instant.is_none()
                        && let Some(mut ans) = answer.instant
                    {
                        // The block is a convenience, never a hole in the URL
                        // contract: unwrap it and keep it only if it is a page.
                        ans.url = contract::canonical_url(&ans.url);
                        if contract::url_class(&ans.url) == contract::UrlClass::Page {
                            instant = Some(ans);
                        }
                    }
                    report.push(EngineReport {
                        engine: engine.clone(),
                        profile: profile.clone(),
                        status: answer.class.status().into(),
                        hits: answer.items,
                        ms: answer.ms,
                        egress: answer.egress.clone(),
                        requests: answer.requests,
                    });
                    if !answer.hits.is_empty() {
                        per_engine.push((engine, answer.hits));
                    }
                }
                Err((status, egress_id, was_engine)) => {
                    let base = engine_health_key(&engine);
                    self.record_lane(&engine, &status);
                    // Dead proxies and auth failures are egress/BYOK
                    // problems, not engine failures : don't quarantine
                    // or distrust the engine over them.
                    if is_engine_fault(&status) {
                        self.record_outcome(base, false);
                    } else if status.starts_with("rate-limited") {
                        // The source published a limit: it is rested, not
                        // distrusted, and probed again when the rest lapses.
                        self.rest(base);
                    }
                    if was_engine {
                        if status.starts_with("dead") {
                            self.pool.report_dead(&egress_id);
                        } else if status == "auth-fail" {
                            self.pool.report_auth_fail(&egress_id);
                        } else if is_engine_fault(&status) {
                            self.pool.report_blocked(&engine, &egress_id);
                        }
                    }
                    if was_engine && is_engine_fault(&status) {
                        self.bump_trust_status(base, intent, false, &status);
                    }
                    // A lane that was asked spent a request, whatever came
                    // back; a lane the plan never asked spent none.
                    let asked = !matches!(
                        status.split(':').next().unwrap_or(&status),
                        "rested" | "not-asked" | "pacing-timeout"
                    );
                    report.push(EngineReport {
                        engine,
                        profile,
                        status,
                        hits: 0,
                        ms: 0,
                        egress: egress_id,
                        requests: usize::from(asked),
                    });
                }
            }
        }

        // Every lane has answered. A set that is provably another topic decays
        // the source's trust, gently, and nothing else: it does not count as a
        // failure, and it never benches the lane (§4: trust follows transport,
        // walls and provable junk — never our own coverage arithmetic).
        for (base, _engine) in &off_query {
            self.bump_trust_status(base, intent, false, "off-query");
        }

        // A source that did not answer inside the budget is named as late, not
        // as absent: the difference is whether rephrasing could help.
        for name in late {
            // A lane that could not answer inside the budget is a slow lane:
            // the ledger rests it briefly so the next search does not wait on it.
            self.record_lane(&name, "late");
            report.push(EngineReport {
                engine: name,
                profile: None,
                status: "late".into(),
                hits: 0,
                ms: 0,
                egress: "-".into(),
                requests: 0,
            });
        }

        if per_engine.is_empty() {
            {
                let t = self
                    .trust
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let ti = self
                    .trust_intent
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let f = self
                    .failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                save_health_disk_if_dirty(self, &t, &ti, &f);
            }
            // A source that answered "nothing relevant" is an answer: the web
            // has nothing for this query, and that is not a tool failure. Only
            // a call where nothing could be asked at all is an error.
            let answered = report.iter().any(|r| {
                matches!(
                    r.status.split(':').next().unwrap_or(""),
                    "off-query" | "no-results" | "empty-parse" | "thin"
                )
            });
            if !answered {
                return Err(FetchError::Http(format!(
                    "search: all engines failed : {}",
                    report
                        .iter()
                        .map(|r| format!("{}:{}", r.engine, r.status))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
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
        if crate::config::cfg().debug.search {
            crate::diag!(
                "[search] lanes into merge: {:?}",
                per_engine
                    .iter()
                    .map(|(e, h)| (e.as_str(), h.len()))
                    .collect::<Vec<_>>()
            );
        }

        let total = rank::merged_total(&per_engine);
        // What each general source returned teaches the independence graph, and
        // this query is ranked with what it just learned.
        let families_now = {
            let lists: Vec<(String, Vec<String>)> = per_engine
                .iter()
                .filter(|(name, _)| !rank::is_vertical(name))
                .map(|(name, hits)| {
                    (
                        egress::health_key(name).to_string(),
                        hits.iter().map(|h| rank::norm_key(&h.url)).collect(),
                    )
                })
                .collect();
            let mut guard = self
                .families
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.observe(&lists) {
                families::save(&guard);
            }
            guard.clone()
        };
        *self
            .last_raw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = per_engine.clone();
        // Always merge 12 results for the cache, then trim to
        // max_results for the response. Without this, a first
        // search with max=2 caches only 2 results, and a later
        // search with max=10 returns the stale 2 from cache.
        // With semantic reranking enabled, merge includes synchronous ONNX
        // inference. Core builds keep the existing inline fast path below.
        #[cfg(feature = "rerank")]
        let (mut results, ranking_finished) = {
            let fallback =
                rank::merge_lexical(&per_engine, query, intent, &trust, &families_now, 12);
            let query = query.to_string();
            let families_job = families_now.clone();
            match run_blocking_ranking(move || {
                rank::merge(&per_engine, &query, intent, &trust, &families_job, 12)
            })
            .await?
            {
                Some(results) => (results, true),
                None => (fallback, false),
            }
        };
        #[cfg(not(feature = "rerank"))]
        let ranking_finished = true;
        #[cfg(not(feature = "rerank"))]
        let mut results = rank::merge(&per_engine, query, intent, &trust, &families_now, 12);
        // B3: learned host quality nudge. After merge (and its
        // diversity cap) so it only reorders survivors with a
        // small weight; consensus still dominates.
        {
            let quality = self
                .quality
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            persist::apply_quality_prior(&mut results, &quality);
        }
        // B4: soft demote hosts that chronically failed agent
        // outcomes (default off).
        {
            let outcome = self
                .outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            persist::apply_outcome_demote(&mut results, &outcome);
        }
        results.sort_by(|a, b| {
            a.tier
                .cmp(&b.tier)
                .then_with(|| b.score.total_cmp(&a.score))
        });

        // ── Result enrichment: prefetch top results to extract
        // real <title> and <meta description> from the actual
        // pages. Richer than SERP snippets, filters dead links.
        // The genius feature: results carry the page's own title
        // and description, not the SERP's truncated version.
        // Enrichment is one page fetch per result: opt-in (`search.enrich`),
        // never a default. It used to run on every search -- five pages, ~700 ms,
        // and a traffic amplifier for sites the agent had not chosen yet. The
        // `read: k` view (M5) is what asks for it, per call.
        let enrich_t = std::time::Instant::now();
        if crate::config::cfg().search.enrich && !results.is_empty() {
            let _ = tokio::time::timeout(
                stage_budget(Duration::from_secs(4), Duration::from_millis(500)),
                self.enrich_results(&mut results),
            )
            .await;
        }
        let enrich_ms = enrich_t.elapsed().as_millis();

        // ── site:/intitle:/filetype: post-merge enforcement:
        // engines don't strictly respect operators : some results
        // leak through. Filter them out post-merge so the agent
        // only sees what it asked for.
        let filters_t = std::time::Instant::now();
        if crate::config::cfg().debug.search {
            crate::diag!(
                "[search] pre-filter {} results; intitle={:?} titles: {:?}",
                results.len(),
                query::compile(query).intitle,
                results
                    .iter()
                    .take(6)
                    .map(|r| r.title.as_str())
                    .collect::<Vec<_>>()
            );
        }
        site_filter(query, &mut results);
        query::intitle_filter(query, &mut results);
        query::filetype_filter(query, &mut results);
        if crate::config::cfg().debug.search {
            crate::diag!("[search] post-filter {} results", results.len());
        }
        let filters_ms = filters_t.elapsed().as_millis();

        // Post-enrichment top-up: the cross-encoder now sees the
        // real page titles/descriptions on the top slice, not the
        // SERP fragments. Bounded additive nudge, then re-sort.
        // DONSEEK_NO_TOPUP is the A/B kill switch for benching.
        #[cfg(feature = "rerank")]
        let topup_ms: u128 = {
            if crate::config::cfg().search.rerank
                && crate::config::cfg().search.rerank_topup
                && !stage_budget(Duration::from_secs(1), Duration::from_millis(500)).is_zero()
            {
                let t = std::time::Instant::now();
                let q = query.to_string();
                let mut owned = results.clone();
                let r = run_blocking_ranking(move || {
                    crate::search::rerank::topup(&q, &mut owned, 8);
                    owned
                });
                if let Some(ranked) = r.await? {
                    results = ranked;
                }
                t.elapsed().as_millis()
            } else {
                0
            }
        };
        // Without the rerank feature the stage is a no-op: report a
        // flat zero so stage_ms stays schema-stable on all feature
        // halves of the build matrix.
        #[cfg(not(feature = "rerank"))]
        let topup_ms: u128 = 0;
        // Poisoning guard: a merge built while engines
        // were down must NOT persist for 30 minutes :
        // degraded-period results expire with the moment.
        let content_lanes = report.iter().filter(|r| r.status == "ok").count();
        let cacheable = ranking_finished && content_lanes >= 2 && total >= 8;
        if cacheable {
            let snapshot = {
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
                cache.clone()
                // Serialize + write with NO lock held: the save is a
                // 1-2MB JSON write, and holding the cache mutex
                // across it queued every cache hit and every
                // single-flight follower poll behind one disk write.
            };
            save_cache_disk(&snapshot);
        }

        // Persist learned engine health once per search (single
        // small write; failures inside the loop are already
        // recorded, so this snapshot is always consistent).
        {
            let t = self
                .trust
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ti = self
                .trust_intent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let f = self
                .failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            save_health_disk_if_dirty(self, &t, &ti, &f);
        }

        let stages = vec![
            ("enrich", enrich_ms),
            ("filters", filters_ms),
            ("topup", topup_ms),
        ];

        Ok(SearchOutcome {
            results: results.into_iter().take(max_results).collect(),
            intent,
            report,
            cached: false,
            elapsed: started.elapsed(),
            provider: None,
            reranked: ranking_finished && crate::search::rerank::loaded(),
            instant,
            stage_ms: stages,
        })
    }

    /// A resolved name becomes an answer without a fan-out: the registry is the
    /// first-party source, and its records enter the ordinary merge as one lane
    /// (so evidence picking, tiers and diversity apply unchanged).
    fn resolved_outcome(
        &self,
        query: &str,
        intent: Intent,
        max_results: usize,
        started: Instant,
        resolved: resolve::Resolved,
    ) -> SearchOutcome {
        let per_engine = vec![("registry".to_string(), resolved.hits.clone())];
        *self
            .last_raw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = per_engine.clone();
        let results = rank::merge(
            &per_engine,
            query,
            intent,
            &HashMap::new(),
            &families::Families::default(),
            12,
        );
        let report = vec![EngineReport {
            engine: "registry".into(),
            profile: Some(resolved.label),
            status: "ok".into(),
            hits: resolved.hits.len(),
            ms: resolved.ms,
            egress: "direct".into(),
            requests: resolved.requests,
        }];
        SearchOutcome {
            results: results.into_iter().take(max_results).collect(),
            intent,
            report,
            cached: false,
            elapsed: started.elapsed(),
            provider: None,
            reranked: false,
            instant: None,
            stage_ms: vec![("resolve", resolved.ms as u128)],
        }
    }

    fn bump_trust(&self, base_engine: &str, intent: Intent, ok: bool) {
        self.bump_trust_status(base_engine, intent, ok, "");
    }

    /// C1: Mojeek `empty-parse` (blocked:200) burns trust harder so
    /// it leaves the default width instead of occupying a slot.
    fn bump_trust_status(&self, base_engine: &str, intent: Intent, ok: bool, status: &str) {
        let target = trust_target_for(base_engine, status, ok);
        {
            let mut trust = self
                .trust
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let t = trust.entry(base_engine.to_string()).or_insert(1.0);
            *t = (*t * 0.7 + target * 0.3).clamp(0.2, 2.0);
        }
        {
            let mut trust_i = self
                .trust_intent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let key = persist::trust_intent_key(base_engine, intent);
            let t = trust_i.entry(key).or_insert(1.0);
            *t = (*t * 0.7 + target * 0.3).clamp(0.2, 2.0);
        }
        self.health_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Engine-global + per-intent trust for receipts / status.
    pub fn trust_snapshot(&self) -> (usize, usize, usize) {
        let trust = self
            .trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let trust_i = self
            .trust_intent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let failures = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let low = trust.values().filter(|&&t| t < 0.5).count()
            + trust_i.values().filter(|&&t| t < 0.5).count();
        (trust.len(), trust_i.len(), low + failures.len())
    }

    /// B3: record one enrich/prefetch outcome for a host.
    /// Clean content raises density; dead/soft-404 lowers it.
    /// No-op when the quality prior is killed.
    pub(crate) fn observe_quality(&self, host: &str, ok: bool) {
        if !crate::config::cfg().search.quality_prior {
            return;
        }
        {
            let mut quality = self
                .quality
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            persist::observe_quality(&mut quality, host, ok);
        }
        self.quality_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Persist quality map if it changed since the last save.
    pub(crate) fn save_quality_if_dirty(&self) {
        let quality = self
            .quality
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        save_quality_disk_if_dirty(self, &quality);
    }

    /// B4: record one agent-outcome miss. No-op unless
    /// `search.outcome_feedback` is on. Never fetches anything.
    pub fn observe_outcome_miss(&self, class: &str, host: &str) {
        if !crate::config::cfg().search.outcome_feedback {
            return;
        }
        {
            let mut outcome = self
                .outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            persist::observe_outcome_miss(&mut outcome, class, host);
        }
        self.outcome_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let outcome = self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        save_outcome_disk_if_dirty(self, &outcome);
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

/// The lane inside a source: bing's SERP and bing-news's wire are different
/// products with different limits, and one lesson must not rest the other.
fn lane_of(source: &str) -> &'static str {
    match source {
        "bing-news" => "wire",
        "feed" => "mesh",
        "gdelt" | "registry" | "crates" | "npm" | "pypi" | "wikipedia" | "hn" | "arxiv"
        | "openalex" | "europepmc" | "stackexchange" | "github" | "mdn" | "docs" => "api",
        _ => "serp",
    }
}

/// A lane's ledger key: the source is normalised so a bare-name registry guess
/// and a stated registry share one standing.
fn lane_source(source: &str) -> &str {
    match source {
        "registry-any" => "registry",
        other => other,
    }
}

fn engine_health_key(engine: &str) -> &str {
    let engine = engine_name(engine);
    if engine == "google" {
        return "google_http_v1";
    }
    egress::health_key(engine)
}

/// Governor: fan-out width under stress. Healthy pool →
/// all engines; stressed → shrink appetite (you can't be
/// rate-limited if you never exceed the rate); starved →
/// top engine + verticals. Consensus survives at width 2
/// by construction (two independent index families).
/// Reject a query no endpoint would answer, before any egress is spent.
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

/// How many engines a stressed pool still asks. Never fewer than two: rank
/// mass is agreement between independent families, and one engine cannot
/// agree with anything. A single engine's own failures are the ledger's to
/// rest, lane by lane.
fn width_for_stress(stress: f64, available: usize) -> usize {
    // Below the healthy mark every lane is asked. Above it the roster narrows
    // to three, never fewer: three independent families are what a ranking is
    // built from, and a two-lane merge is a coin toss presented as an answer.
    if stress < 0.15 {
        available
    } else {
        available.min(3)
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
/// `site:domain.com/path` also requires the URL path to start with
/// `/path`; a scheme in the value (`site:https://domain.com`) is
/// ignored.
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
    let Some(site) = site_domain else {
        return;
    };
    let (domain, path) = query::site_scope(&site);
    // Use raw host (not host_of which strips www.) so
    // `site:www.wikipedia.org` matches only www., while
    // `site:wikipedia.org` matches all subdomains.
    results.retain(|r| {
        let Ok(u) = url::Url::parse(&r.url) else {
            return false;
        };
        let host = u.host_str().map(|h| h.to_lowercase()).unwrap_or_default();
        (host == domain || host.ends_with(&format!(".{domain}")))
            && query::site_path_matches(&u, path)
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

    #[derive(Clone, Copy)]
    enum ProbeCase {
        Unavailable,
        PriorBan,
        Auth,
        Cancelled,
        Healthy,
    }

    async fn preflight_pair(case: ProbeCase) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut config = crate::config::DonsetchConfig::default();
        config.fetch.allow_private_egress = true;
        config.proxy.from_environment = false;
        crate::config::install(config).unwrap();
        let a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxies = [&a, &b].map(|listener| {
            crate::egress::proxy::Proxy::parse(&format!(
                "http://{}",
                listener.local_addr().unwrap()
            ))
            .unwrap()
        });
        let acceptor = if matches!(case, ProbeCase::Healthy) {
            // The mock proxy terminates the tunnel with a self-signed cert
            // for api.ipify.org; the client trusts it through SSL_CERT_FILE
            // (the engine's BoringSSL reads default verify paths from
            // SSL_CERT_FILE/SSL_CERT_DIR, same as the old transport did).
            let cert = rcgen::generate_simple_self_signed(vec!["api.ipify.org".into()]).unwrap();
            let bundle = crate::paths::cache_dir().join("owned-probe-ca.pem");
            std::fs::write(&bundle, cert.cert.pem()).unwrap();
            // Nextest gives this owned trust bundle its own process.
            unsafe { std::env::set_var("SSL_CERT_FILE", &bundle) };
            let mut config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![rustls_pki_types::CertificateDer::from(
                        cert.cert.der().to_vec(),
                    )],
                    rustls_pki_types::PrivateKeyDer::Pkcs8(
                        rustls_pki_types::PrivatePkcs8KeyDer::from(
                            cert.signing_key.serialize_der(),
                        ),
                    ),
                )
                .unwrap();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Some(Arc::new(tokio_rustls::TlsAcceptor::from(Arc::new(config))))
        } else {
            None
        };
        let pool = Arc::new(EgressPool::new(proxies.to_vec()));
        if matches!(case, ProbeCase::PriorBan) {
            pool.report_auth_fail(&proxies[0].id());
        }
        let searcher = Arc::new(Searcher::new_shared(
            Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap(),
            Arc::clone(&pool),
        ));
        let second_reached = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut servers = Vec::new();
        for (ordinal, listener) in [a, b].into_iter().enumerate() {
            let second_reached = Arc::clone(&second_reached);
            let release = Arc::clone(&release);
            let acceptor = acceptor.clone();
            servers.push(tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 16384);
                    head.push(socket.read_u8().await.unwrap());
                }
                assert!(head.starts_with(b"CONNECT api.ipify.org:443 HTTP/1.1\r\n"));
                if ordinal == 1 {
                    second_reached.notify_one();
                    if matches!(case, ProbeCase::Cancelled) {
                        assert_eq!(socket.read(&mut [0u8; 1]).await.unwrap(), 0,
                            "aborting preflight must close its pending CONNECT");
                        return;
                    }
                    release.notified().await;
                    if let Some(acceptor) = acceptor {
                        socket.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
                        let mut tls = acceptor.accept(socket).await.unwrap();
                        let mut request = Vec::new();
                        while !request.ends_with(b"\r\n\r\n") {
                            assert!(request.len() < 16384);
                            request.push(tls.read_u8().await.unwrap());
                        }
                        assert!(request.starts_with(b"GET / HTTP/1.1\r\n"));
                        tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n192.0.2.1").await.unwrap();
                        return;
                    }
                }
                let status = if ordinal == 0 && matches!(case, ProbeCase::Auth) {407} else {503};
                socket.write_all(format!("HTTP/1.1 {status} Owned probe\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                if status == 407 {
                    assert!(tokio::time::timeout(Duration::from_millis(100), listener.accept()).await.is_err(),
                        "unchanged credentials must not retry CONNECT407");
                }
            }));
        }
        let probe = searcher.preflight();
        tokio::time::timeout(Duration::from_secs(3), second_reached.notified())
            .await
            .expect("both real proxy probes must be reached");
        let ban_before_batch = matches!(case, ProbeCase::PriorBan | ProbeCase::Auth);
        let first_was_benched = pool.is_dead(&proxies[0].id());
        let selected = pool.pick_fetch("owned.example", true).unwrap().id;
        if matches!(case, ProbeCase::Cancelled) {
            probe.abort();
            assert!(probe.await.unwrap_err().is_cancelled());
        } else {
            release.notify_one();
            tokio::time::timeout(Duration::from_secs(3), probe)
                .await
                .unwrap()
                .unwrap();
        }
        for server in servers {
            tokio::time::timeout(Duration::from_secs(3), server)
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            first_was_benched, ban_before_batch,
            "generic echo failures cannot alter a lane while the batch is pending"
        );
        assert_eq!(
            selected,
            proxies[usize::from(ban_before_batch)].id(),
            "an unresolved echo batch must preserve the selected egress"
        );
        assert_eq!(
            pool.is_dead(&proxies[0].id()),
            ban_before_batch || matches!(case, ProbeCase::Healthy)
        );
        assert!(
            !pool.is_dead(&proxies[1].id()),
            "an unavailable echo must not bench B, including after a real A407"
        );
        let reloaded = EgressPool::new(proxies.to_vec());
        assert_eq!(
            reloaded.is_dead(&proxies[0].id()),
            pool.is_dead(&proxies[0].id()),
            "completed health evidence must survive actual persistence"
        );
    }

    #[tokio::test]
    async fn stealth_v3_preflight_does_not_bench_a_lane_before_the_probe_batch_finishes() {
        preflight_pair(ProbeCase::Unavailable).await;
    }

    #[tokio::test]
    async fn stealth_v3_preflight_preserves_prior_bans_when_echo_is_unavailable() {
        preflight_pair(ProbeCase::PriorBan).await;
    }

    #[tokio::test]
    async fn stealth_v3_preflight_applies_auth_failure_without_benching_inconclusive_peer() {
        preflight_pair(ProbeCase::Auth).await;
    }

    #[tokio::test]
    async fn stealth_v3_preflight_cancellation_closes_connect_without_publishing_partial_batch() {
        preflight_pair(ProbeCase::Cancelled).await;
    }

    #[tokio::test]
    async fn stealth_v3_preflight_completed_healthy_tls_echo_benches_failed_peer() {
        preflight_pair(ProbeCase::Healthy).await;
    }

    // The batch must obey one global ceiling: a pool of stalled lanes
    // used to cost N * 6s of startup (8 lanes = 48s serial). With the
    // ceiling the run stops at ~20s, the partial batch publishes
    // nothing, and the unprobed lanes stay unbenched.
    #[tokio::test]
    async fn stealth_v3_preflight_batch_obeys_a_global_ceiling() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut config = crate::config::DonsetchConfig::default();
        config.fetch.allow_private_egress = true;
        config.proxy.from_environment = false;
        crate::config::install(config).unwrap();
        let mut listeners = Vec::new();
        let mut proxies = Vec::new();
        for _ in 0..8 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            proxies.push(
                crate::egress::proxy::Proxy::parse(&format!(
                    "http://{}",
                    listener.local_addr().unwrap()
                ))
                .unwrap(),
            );
            listeners.push(listener);
        }
        let pool = Arc::new(EgressPool::new(proxies.clone()));
        let searcher = Arc::new(Searcher::new_shared(
            Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap(),
            Arc::clone(&pool),
        ));
        let mut servers = Vec::new();
        for listener in listeners {
            servers.push(tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        while !head.ends_with(b"\r\n\r\n") {
                            assert!(head.len() < 16384);
                            let mut buf = [0u8; 1024];
                            let Ok(n) = socket.read(&mut buf).await else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..n]);
                        }
                        // Accept the tunnel, then stall the handshake so
                        // the probe dies on its own timeout.
                        if socket
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .await
                            .is_ok()
                        {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                    });
                }
            }));
        }
        let started = Instant::now();
        searcher.preflight().await.unwrap();
        let elapsed = started.elapsed();
        for server in servers {
            server.abort();
        }
        assert!(
            elapsed < Duration::from_secs(30),
            "preflight must obey the global ceiling; 8 stalled lanes took {elapsed:?} \
             (serial N * 6s = 48s)"
        );
        assert!(
            proxies.iter().all(|p| !pool.is_dead(&p.id())),
            "a budget-cut partial batch must publish nothing"
        );
    }

    /// A source that serves bot-pages beside answering peers leaves the roster
    /// within three searches, and returns by itself, with no person and no
    /// restart, once its window lapses and it answers again.
    // Depends on nextest's process-per-test isolation (DONSETCH_CACHE_DIR).
    #[test]
    fn a_decayed_source_is_benched_within_five_calls_and_returns_unattended() {
        let s = test_searcher();
        let mut calls = 0;
        while !s.quarantined("bing") && !s.resting("bing") {
            // What one search does when a source answers off-query beside peers.
            s.record_outcome("bing", false);
            s.bump_trust_status("bing", Intent::Web, false, "off-query");
            calls += 1;
            assert!(calls <= 5, "still in the roster after {calls} calls");
        }
        assert_eq!(calls, 3, "three strikes bench it");
        let state = |s: &Searcher| {
            s.health_report()
                .into_iter()
                .find(|h| h.name == "bing")
                .map(|h| h.state)
        };
        assert!(matches!(state(&s), Some(SourceState::Benched(_))));

        // The window lapses (no clock to wind, so age the streak): it is asked again.
        let aged = Instant::now()
            .checked_sub(QUARANTINE_TTL + Duration::from_secs(1))
            .expect("monotonic clock is past the window");
        s.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert("bing".into(), (3, aged));
        assert!(!s.quarantined("bing") && !s.resting("bing"));
        assert_eq!(state(&s), Some(SourceState::Suspect));

        // It answers; trust climbs back and the streak clears.
        for _ in 0..4 {
            s.record_outcome("bing", true);
            s.bump_trust("bing", Intent::Web, true);
        }
        assert_eq!(state(&s), Some(SourceState::Verified));
    }

    #[test]
    fn a_source_state_follows_trust_and_the_failure_streak() {
        let long = Duration::from_secs(3600);
        assert_eq!(source_state(1.0, None), SourceState::Verified);
        assert_eq!(source_state(0.7, None), SourceState::Suspect);
        assert_eq!(source_state(1.1, Some((1, long))), SourceState::Suspect);
        assert_eq!(source_state(1.1, Some((3, long))), SourceState::Suspect);
        assert_eq!(
            source_state(0.3, Some((3, Duration::from_secs(60)))),
            SourceState::Benched(QUARANTINE_TTL - Duration::from_secs(60))
        );
    }

    // Depends on nextest's process-per-test isolation: test_searcher sets the
    // process-wide DONSETCH_CACHE_DIR.
    #[test]
    fn a_throttled_source_is_rested_without_losing_trust() {
        let s = test_searcher();
        let trust_before = s
            .trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("stackexchange")
            .copied();
        assert!(!s.quarantined("stackexchange"));
        s.rest("stackexchange");
        assert!(
            s.quarantined("stackexchange"),
            "a throttled source sits out"
        );
        assert!(!s.quarantined("github"), "only the throttled source");
        let trust_after = s
            .trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("stackexchange")
            .copied();
        assert_eq!(
            trust_before, trust_after,
            "a published limit is not distrust"
        );
    }

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
                families: 0,
                from_page: false,
                tier: 0,
            })
            .collect::<Vec<_>>();
        let report = vec![EngineReport {
            engine: provider.to_string(),
            profile: None,
            status: "ok".into(),
            hits: results.len(),
            ms: 12,
            egress: "byok".into(),
            requests: 0,
        }];
        SearchOutcome {
            results,
            intent: Intent::Web,
            report,
            cached: false,
            elapsed: Duration::ZERO,
            provider: Some(provider.to_string()),
            reranked: false,
            instant: None,
            stage_ms: Vec::new(),
        }
    }

    // Issue #195: a repeat BYOK query must be served from the cache
    // (cached: true, provider preserved), never re-billing the
    // provider. The store/serve roundtrip is the mechanism the wiring
    // in search_tool relies on.
    #[tokio::test]
    async fn wave450_optional_search_stages_preserve_time_for_results() {
        let cap = Duration::from_secs(30);
        let reserve = Duration::from_secs(5);
        assert_eq!(stage_budget(cap, reserve), cap);
        SEARCH_DEADLINE
            .scope(
                Some(tokio::time::Instant::now() + Duration::from_millis(200)),
                async {
                    assert_eq!(stage_budget(cap, reserve), Duration::ZERO);
                },
            )
            .await;
        SEARCH_DEADLINE
            .scope(
                Some(tokio::time::Instant::now() + Duration::from_secs(8)),
                async {
                    let budget = stage_budget(cap, reserve);
                    assert!(
                        budget > Duration::from_secs(2) && budget <= Duration::from_secs(3),
                        "{budget:?}"
                    );
                },
            )
            .await;
        assert_eq!(
            stage_budget(cap, reserve),
            cap,
            "deadline must not escape the call"
        );
    }

    #[test]
    fn byok_results_roundtrip_the_cache_with_provider_and_cached_flag() {
        let s = test_searcher();
        // Cold: nothing cached, so a caller must go bill the provider.
        assert!(s.byok_cache_get("quic test 42", Intent::Web, 3).is_none());

        let mut out = byok_outcome("tinyfish", &["https://a.test/", "https://b.test/"]);
        out.results[0].title = "QUIC test 42 transport evidence".into();
        s.byok_cache_put("quic test 42", &out);

        // Warm: served from cache, marked cached, provider intact.
        let hit = s
            .byok_cache_get("quic test 42", Intent::Web, 3)
            .expect("a fresh BYOK entry must hit");
        assert!(hit.cached, "a served BYOK cache entry must report cached");
        assert_eq!(hit.provider.as_deref(), Some("tinyfish"));
        assert_eq!(hit.results.len(), 2);
        assert_eq!(hit.results[0].url, "https://a.test/");
        s.byok_cache_put("qwertyuiop unrelated gibberish", &out);
        let unrelated = s
            .byok_cache_get("qwertyuiop unrelated gibberish", Intent::Web, 3)
            .unwrap();
        assert_eq!(
            unrelated.results[0].url, "https://a.test/",
            "the BYOK cache keys on the query, not on a relevance judgement"
        );
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

    const JAGUAR_REVIEW: &str = "https://cars.example/jaguar-f-type-review";

    /// Bench every engine and vertical the queries would fan out to,
    /// so a search the cache does not answer fails at once with
    /// "all engines failed" instead of reaching the network.
    fn bench_every_source(s: &Searcher, queries: &[&str]) {
        let mut failures = s.failures.lock().unwrap();
        let engines = intent::engines_for(Intent::Web).iter().copied();
        for e in engines.map(engine_health_key) {
            failures.insert(e.to_string(), (3, Instant::now()));
        }
        for q in queries {
            for v in intent::verticals_for(Intent::Web, q) {
                failures.insert(v.to_string(), (3, Instant::now()));
            }
        }
    }

    /// Store a local-path entry the way `search_inner` keys it.
    fn seed_local_cache(s: &Searcher, query: &str, url: &str) {
        let key = format!("{}|{}", norm_query(query), Intent::Web.code());
        s.cache
            .lock()
            .unwrap()
            .insert(key, (Instant::now(), vec![merged(url)], 12, Vec::new()));
    }

    #[tokio::test]
    async fn an_exclusion_query_is_not_served_the_included_results() {
        let s = test_searcher();
        bench_every_source(&s, &["jaguar car", "jaguar -car"]);
        seed_local_cache(&s, "jaguar car", JAGUAR_REVIEW);
        // Negative: a repeat of the cached query is still a hit.
        let repeat = s
            .search("jaguar car", 5, Some(Intent::Web))
            .await
            .expect("a repeat query is served from the cache");
        assert!(repeat.cached);
        assert_eq!(repeat.results[0].url, JAGUAR_REVIEW);
        let excluded = s.search("jaguar -car", 5, Some(Intent::Web)).await;
        assert!(
            matches!(&excluded, Err(e) if e.to_string().contains("all engines failed")),
            "\"jaguar -car\" must miss the \"jaguar car\" entry, got {:?}",
            excluded.map(|out| (out.cached, out.results))
        );
    }

    #[tokio::test]
    async fn an_exclusion_query_does_not_join_the_included_flight() {
        let s = test_searcher();
        bench_every_source(&s, &["jaguar car", "jaguar -car"]);
        // A "jaguar car" leader is in flight. A follower polls for its
        // cache write for up to 12 s before running on its own.
        let leader = format!("{}|{}", norm_query("jaguar car"), Intent::Web.code());
        s.inflight.lock().unwrap().insert(leader);
        // Negative: an identical query joins the flight and is served
        // what the leader publishes.
        let (joined, ()) = tokio::join!(s.search("jaguar car", 5, Some(Intent::Web)), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            seed_local_cache(&s, "jaguar car", JAGUAR_REVIEW);
        });
        let joined = joined.expect("a follower is served the leader's results");
        assert!(joined.cached);
        assert_eq!(joined.results[0].url, JAGUAR_REVIEW);
        // Drop the published entry: only the flight can hold the next
        // query back now.
        s.cache.lock().unwrap().clear();
        let excluded = tokio::time::timeout(
            Duration::from_secs(3),
            s.search("jaguar -car", 5, Some(Intent::Web)),
        )
        .await;
        assert!(
            matches!(&excluded, Ok(Err(_))),
            "\"jaguar -car\" waited on the \"jaguar car\" flight: {excluded:?}"
        );
    }

    #[test]
    fn a_byok_exclusion_query_misses_the_included_entry() {
        let s = test_searcher();
        s.byok_cache_put("jaguar car", &byok_outcome("serper", &[JAGUAR_REVIEW]));
        // Negative: a repeat of the cached query is still a hit.
        let repeat = s
            .byok_cache_get("jaguar car", Intent::Web, 5)
            .expect("a repeat query is served from the cache");
        assert!(repeat.cached);
        assert_eq!(repeat.results[0].url, JAGUAR_REVIEW);
        let excluded = s.byok_cache_get("jaguar -car", Intent::Web, 5);
        assert!(
            excluded.is_none(),
            "\"jaguar -car\" was served the provider's \"jaguar car\" answer: {:?}",
            excluded.map(|out| out.results)
        );
    }

    #[test]
    fn regression_unavailable_google_does_not_stop_other_assignments() {
        let proxy = crate::egress::proxy::Proxy::parse("http://127.0.0.1:12345").unwrap();
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
            .map(|url| crate::egress::proxy::Proxy::parse(url).unwrap())
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
    fn google_transport_health_key_is_transport_specific() {
        assert_eq!(engine_health_key("google@6230-05.50"), "google_http_v1");
        assert_ne!(engine_health_key("google"), "google");
        assert_eq!(engine_health_key("ddg_html"), "ddg");
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
    fn a_slow_minute_is_not_answered_with_nothing() {
        // Two lanes with real items is the fast path; a single lane, however
        // fat, is not, and neither is a pile of failures. This is what keeps the
        // soft mark from cutting the sources that would have answered.
        let lane = |hits: usize| {
            let one = |i: usize| crate::search::engines::Hit {
                title: format!("result {i}"),
                url: format!("https://example.com/{i}"),
                snippet: String::new(),
                rank: i,
                published: None,
            };
            (
                "brave".to_string(),
                Ok(crate::search::tasks::LaneAnswer {
                    hits: (0..hits).map(one).collect(),
                    items: hits,
                    ms: 1,
                    egress: "direct".into(),
                    engine_lane: true,
                    instant: None,
                    class: crate::search::engines::AnswerClass::Answered,
                    requests: 1,
                }),
            )
        };
        let dead = |name: &str| {
            (
                name.to_string(),
                Err(("blocked:429".to_string(), "direct".to_string(), true)),
            )
        };
        // What counts toward the commit: contributing lanes and their items.
        assert_eq!(answered(&[lane(10), lane(3)]), (2, 13));
        assert_eq!(answered(&[lane(40)]), (1, 40));
        // Failures and off-query answers contribute nothing.
        assert_eq!(answered(&[dead("bing"), dead("ddg"), lane(2)]), (1, 2));
        // A thin merge earns one second ask; a fat one does not.
        assert!(answered(&[lane(4), lane(3)]).1 < THIN_ITEMS);
        assert!(answered(&[lane(10), lane(3)]).0 < THIN_LANES);
    }

    #[test]
    fn the_answer_is_settled_by_two_real_lanes_not_by_waiting() {
        let lane = |hits: usize| {
            let one = |i: usize| crate::search::engines::Hit {
                title: format!("result {i}"),
                url: format!("https://example.com/{i}"),
                snippet: String::new(),
                rank: i,
                published: None,
            };
            (
                "brave".to_string(),
                Ok(crate::search::tasks::LaneAnswer {
                    hits: (0..hits).map(one).collect(),
                    items: hits,
                    ms: 1,
                    egress: "direct".into(),
                    engine_lane: true,
                    instant: None,
                    class: crate::search::engines::AnswerClass::Answered,
                    requests: 1,
                }),
            )
        };
        // Two lanes with no shared URL between them: no agreement, keep waiting.
        let fam = families::Families::default();
        assert!(!enough_answered(&[lane(20), lane(20)], true, &fam));
        // A plain item count is not agreement either.
        assert!(!enough_answered(&[lane(40)], true, &fam));
        // Early consensus stays off when the config turns it off.
        assert!(!enough_answered(&[lane(2)], false, &fam));
    }

    #[test]
    fn a_failed_source_is_a_fault_only_when_it_misbehaved() {
        for status in [
            "blocked:captcha",
            "blocked:429",
            "blocked:consent",
            "blocked:http-status",
            "empty-parse",
            "net",
            "timeout",
            "off-query",
        ] {
            assert!(is_engine_fault(status), "{status} is the source's fault");
        }
        // Infra and config problems, and a query that legitimately found
        // nothing, never erode the source's record.
        for status in [
            "invalid-config",
            "invalid-config: proxy: unsupported scheme",
            "no-results",
            "pacing-timeout",
            "dead-proxy",
            "auth-fail",
            // Scheduling outcomes: the source was never asked.
            "not-asked",
            "rested",
            "rested:45",
            "late",
            "skipped",
        ] {
            assert!(
                !is_engine_fault(status),
                "{status} is not the source's fault"
            );
        }
    }

    /// The budget leaving a healthy source out is not the source failing: before
    /// this held, three budget cuts in a row quarantined yahoo for ten minutes
    /// and it vanished from every later search's census.
    // Depends on nextest's process-per-test isolation (DONSETCH_CACHE_DIR).
    #[test]
    fn a_source_the_plan_left_out_is_never_benched_for_it() {
        let s = test_searcher();
        for _ in 0..5 {
            for status in ["not-asked", "rested:30", "late"] {
                if is_engine_fault(status) {
                    s.record_outcome("yahoo", false);
                }
            }
        }
        assert!(!s.quarantined("yahoo") && !s.resting("yahoo"));
    }

    #[test]
    fn every_intent_roster_names_each_engine_once() {
        // google and mojeek are not on this egress (blocked every query; an HTTP
        // 200 captcha) and no longer hold a slot. What must hold is that a
        // roster names no engine twice and keeps enough lanes for a ranking.
        for intent in [
            intent::Intent::Web,
            intent::Intent::Code,
            intent::Intent::News,
            intent::Intent::Entity,
            intent::Intent::Paper,
        ] {
            let roster = intent::engines_for(intent);
            // A paper query's answer is its three indexes: two general lanes
            // beside them are enough, and the verticals carry the recall.
            let floor = if matches!(intent, intent::Intent::Paper) {
                2
            } else {
                3
            };
            assert!(roster.len() >= floor, "{intent:?}: {roster:?}");
            let mut seen = std::collections::HashSet::new();
            for engine in roster {
                assert!(seen.insert(*engine), "{intent:?} names {engine} twice");
            }
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
        assert!(!is_engine_fault("rate-limited:900"));
        assert!(!worth_second_ask("rate-limited:900"));
        assert!(!worth_second_ask("blocked:captcha"));
        assert!(!worth_second_ask("blocked:429"));
        assert!(worth_second_ask("blocked:403"));
        assert!(worth_second_ask("timeout"));
        assert!(is_engine_fault("blocked:403"));
        assert!(is_engine_fault("blocked:captcha"));
        assert!(is_engine_fault("empty-parse"));
        assert!(is_engine_fault("timeout"));
        assert!(is_engine_fault("no-url"));
        assert!(is_engine_fault("net"));
    }

    #[test]
    fn mojeek_empty_parse_burns_trust_harder() {
        assert_eq!(trust_target_for("mojeek", "empty-parse", false), 0.15);
        assert_eq!(trust_target_for("bing", "empty-parse", false), 0.3);
        assert_eq!(trust_target_for("mojeek", "blocked:429", false), 0.3);
        assert_eq!(trust_target_for("mojeek", "ok", true), 1.2);
        // Provable junk is the one content verdict that touches trust, and it
        // decays gently: the source was reachable and answered, it just
        // answered another question.
        assert_eq!(trust_target_for("bing", "off-query", false), 0.6);
        assert_eq!(trust_target_for("brave", "off-query", false), 0.6);
    }

    #[test]
    fn early_consensus_needs_two_families_on_a_shared_top3() {
        use crate::search::engines::Hit;
        let hit = |url: &str, rank: usize| Hit {
            title: "t".into(),
            url: url.into(),
            snippet: "s".into(),
            rank,
            published: None,
        };
        let ok = |engine: &str, urls: &[&str]| {
            (
                engine.to_string(),
                Ok(crate::search::tasks::LaneAnswer {
                    hits: urls
                        .iter()
                        .enumerate()
                        .map(|(i, u)| hit(u, i + 1))
                        .collect::<Vec<_>>(),
                    items: urls.len(),
                    ms: 10,
                    egress: "direct".to_string(),
                    engine_lane: true,
                    instant: None,
                    class: crate::search::engines::AnswerClass::Answered,
                    requests: 1,
                }),
            )
        };
        // Two families that share a top-3 URL: agreement, commit.
        let two = vec![
            ok("brave", &["https://a.com/x", "https://b.com/y"]),
            ok("bing", &["https://a.com/x", "https://b.com/y"]),
        ];
        let fam = families::Families::default();
        assert!(early_consensus_enough(&two, &fam));
        // Three families but no shared top-3 URL: no agreement, keep waiting.
        let disjoint = vec![
            ok("brave", &["https://a.com/x"]),
            ok("bing", &["https://b.com/y"]),
            ok("mojeek", &["https://c.com/z"]),
        ];
        assert!(!early_consensus_enough(&disjoint, &fam));
        // One family alone is never enough, however many lanes it fills.
        let one = vec![ok("brave", &["https://a.com/x"])];
        assert!(!early_consensus_enough(&one, &fam));
    }

    // The family count a result carries is the one measured when it was ranked,
    // not a table lookup: the same five sources are three opinions once the
    // graph has seen bing, ddg and yahoo return the same results, and five
    // before it has.
    #[test]
    fn family_count_is_the_measured_one() {
        use crate::search::engines::Hit;
        let hit = |url: &str, rank: usize| Hit {
            title: "t".into(),
            url: url.into(),
            snippet: "s".into(),
            rank,
            published: None,
        };
        let same: Vec<Hit> = (0..6)
            .map(|i| hit(&format!("https://x.com/{i}"), i))
            .collect();
        let per: Vec<(String, Vec<Hit>)> = ["bing", "ddg", "yahoo", "google", "brave"]
            .iter()
            .map(|e| (e.to_string(), same.clone()))
            .collect();
        let trust = HashMap::new();

        let unmeasured = rank::merge(
            &per,
            "x",
            Intent::Web,
            &trust,
            &families::Families::default(),
            10,
        );
        assert_eq!(rank::family_count(&unmeasured[0]), 5);

        let mut learned = families::Families::default();
        let lists = |names: &[&str], urls: &[&str]| -> Vec<(String, Vec<String>)> {
            names
                .iter()
                .map(|n| (n.to_string(), urls.iter().map(|u| u.to_string()).collect()))
                .collect()
        };
        for _ in 0..4 {
            learned.observe(&lists(
                &["bing", "ddg", "yahoo"],
                &["a/1", "a/2", "a/3", "a/4", "a/5"],
            ));
        }
        let measured = rank::merge(&per, "x", Intent::Web, &trust, &learned, 10);
        // bing, ddg and yahoo are one opinion; google and brave stand alone.
        assert_eq!(rank::family_count(&measured[0]), 3);
        let markdown = render_compact_markdown(&outcome(vec![measured[0].clone()]), "", None, &[]);
        assert!(markdown.contains(" · 3 indexes"), "{markdown}");
    }

    #[test]
    fn a_source_at_the_trust_floor_loses_its_slot_and_earns_it_back() {
        let s = test_searcher();
        let key = engine_health_key("google");
        assert!(!s.resting(key), "an unproven source is never rested");
        // Eight failures is where the EWMA reaches the floor.
        for _ in 0..8 {
            s.bump_trust_status(key, Intent::Web, false, "blocked:http-status");
            s.record_outcome(key, false);
        }
        assert!(
            s.resting(key),
            "a dead source must stop costing a request on every query"
        );
        assert!(
            !s.resting(engine_health_key("brave")),
            "a healthy source keeps its slot"
        );
        // One answer and it is back: no restart, no human, no probe schedule.
        s.record_outcome(key, true);
        assert!(!s.resting(key), "a success removes the bench");
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
        let trust_intent = HashMap::new();
        let mut failures = HashMap::new();
        failures.insert("google".to_string(), (3, Instant::now()));
        crate::search::persist::save_health_disk(&trust, &trust_intent, &failures);

        let (t, ti, f) = load_health_disk();
        assert_eq!(t["brave"], 1.8, "high trust survives");
        assert_eq!(t["bing"], 0.42, "low trust survives");
        assert_eq!(f.get("google").map(|(n, _)| *n), Some(3));
        // v1-shaped write (no trust_intent) still loads; migration
        // seeds every intent from the engine-global EWMA.
        assert!(
            ti.contains_key(&super::persist::trust_intent_key("brave", Intent::Web)),
            "v1 trust must migrate into per-intent keys"
        );

        unsafe { std::env::remove_var("DONSETCH_CACHE_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B2 discriminator: a News-intent demotion of an engine must
    /// not crush the same engine's Web-intent trust (and vice versa).
    #[test]
    fn per_intent_trust_is_scoped_not_global() {
        let dir = std::env::temp_dir().join(format!(
            "donseek-intent-trust-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &dir) };

        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let searcher = Searcher::new(fetcher, EgressPool::new(Vec::new()));
        for _ in 0..8 {
            searcher.bump_trust("bing", Intent::News, false);
            searcher.bump_trust("bing", Intent::Web, true);
        }
        {
            let ti = searcher
                .trust_intent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let news = ti[&super::persist::trust_intent_key("bing", Intent::News)];
            let web = ti[&super::persist::trust_intent_key("bing", Intent::Web)];
            assert!(
                news < 0.6 && web > 1.0,
                "news demotion must stay scoped: news={news} web={web}"
            );
        }
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

        assert!(
            worker_result
                .expect("blocking job should join")
                .expect("job completed within budget")
                .is_ok()
        );
    }

    #[cfg(feature = "rerank")]
    #[tokio::test(flavor = "current_thread")]
    async fn report_audit_semantic_deadline_retains_lexical_evidence() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
        let result = SEARCH_DEADLINE
            .scope(Some(deadline), async {
                let (ranked, started) = tokio::join!(
                    run_blocking_ranking(move || {
                        started_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                        "semantic evidence"
                    }),
                    started_rx,
                );
                started.unwrap();
                ranked.unwrap().unwrap_or("retrieved lexical evidence")
            })
            .await;
        release_tx.send(()).unwrap();
        assert_eq!(result, "retrieved lexical evidence");
        assert!(tokio::time::Instant::now() < deadline);
    }

    #[test]
    fn governor_shrinks_width_under_stress() {
        assert_eq!(width_for_stress(0.05, 4), 4);
        assert_eq!(width_for_stress(0.30, 4), 3);
        assert_eq!(width_for_stress(0.50, 4), 3);
        assert_eq!(
            width_for_stress(0.90, 4),
            3,
            "three families are what a ranking is built from"
        );
        assert_eq!(width_for_stress(0.90, 1), 1);
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
    fn norm_query_collapses_variants() {
        assert_eq!(
            norm_query("Rust Async: the Runtime, comparison!"),
            norm_query("rust async runtime comparison")
        );
        assert_ne!(norm_query("kafka vs nats"), norm_query("kafka"));
        // A plain query's key is its lowercased words minus stopwords.
        assert_eq!(
            norm_query("Rust Async: the Runtime, comparison!"),
            "rust async runtime comparison"
        );
        // Punctuation that is not an operator still collapses.
        for (a, b) in [
            ("x-ray machine", "x ray machine"),
            ("jaguar - car", "jaguar car"),
            ("rust or go", "rust go"),
            ("rust site:github.com", "rust site:GitHub.com/"),
        ] {
            assert_eq!(norm_query(a), norm_query(b), "{a:?} vs {b:?}");
        }
        // An operator keeps each pair apart.
        for (a, b) in [
            ("jaguar car", "jaguar -car"),
            ("rust async", "\"rust async\""),
            ("c tutorial", "c# tutorial"),
            ("rust go", "rust OR go"),
            ("jaguar \"the car\"", "jaguar -\"the car\""),
            ("rust site github com", "rust site:github.com"),
            ("rust filetype pdf", "rust filetype:pdf"),
            ("intitle rust", "intitle:rust"),
        ] {
            assert_ne!(norm_query(a), norm_query(b), "{a:?} vs {b:?}");
        }
    }

    /// A restarted searcher must count the time no process ran: an
    /// entry fresh at save and three days old on disk is not served.
    /// Reaches search-cache.json through the per-process test cache
    /// root, so it relies on nextest's process per test.
    #[tokio::test]
    async fn persisted_cache_entry_counts_time_spent_stopped() {
        let query = "tokio runtime overview";
        let key = format!("{}|{}", norm_query(query), Intent::Web.code());
        let mut map = persist::CacheMap::new();
        map.insert(
            key.clone(),
            (
                Instant::now(),
                vec![merged("https://tokio.rs/")],
                12,
                Vec::new(),
            ),
        );
        persist::save_cache_disk(&map);
        let path = crate::paths::cache_dir().join("search-cache.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        // The daemon was stopped for three days after that save: move
        // the entry's stored time back by three days. The file mtime is
        // not read by the loader, so backdating it would simulate nothing.
        // Only a Unix write time can be moved back: an age-at-save
        // field is 0 here and would stay 0.
        let mut doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let stored = doc[0][1].as_u64().unwrap();
        assert!(
            stored > 3 * 86_400,
            "stored field {stored} is not a Unix write time, so it cannot be moved 3 days back; on disk: {}",
            &raw[..raw.len().min(160)]
        );
        doc[0][1] = (stored - 3 * 86_400).into();
        std::fs::write(&path, doc.to_string()).unwrap();
        let s = Searcher::new(
            Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap(),
            EgressPool::new(Vec::new()),
        );
        // Checked on the searcher's own cache: a search() call would
        // send the query to live engines.
        assert!(
            !s.cache.lock().unwrap().contains_key(&key),
            "an entry saved 3 days ago (Web TTL 30 min) was loaded as fresh; on disk: {}",
            &raw[..raw.len().min(160)]
        );
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
            intent: Intent::Web,
            report: Vec::new(),
            cached: false,
            elapsed: Duration::from_millis(10),
            provider: None,
            reranked: false,
            instant: None,
            stage_ms: Vec::new(),
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
        search.report = vec![
            EngineReport {
                engine: "bing".into(),
                profile: None,
                status: "ok".into(),
                hits: 10,
                ms: 12,
                egress: "direct".into(),
                requests: 0,
            },
            EngineReport {
                engine: "ddg".into(),
                profile: None,
                status: "blocked:403".into(),
                hits: 0,
                ms: 20,
                egress: "proxy".into(),
                requests: 0,
            },
        ];

        let markdown = render_compact_markdown(
            &search,
            "# Search results",
            Some(&["S1".into()]),
            &[Some("· ⚠ needs browser".into())],
        );
        assert!(
            markdown.contains("1. S1 · Tokio runtime guide : tokio.rs · ⚠ needs browser"),
            "the route hint follows the host: {markdown}"
        );
        assert!(markdown.contains("A focused explanation"));
        // The census names who answered and who did not; there is no
        // "weak" verdict and no degraded ratio to read.
        assert!(
            markdown.contains("sources: bing 10 · ddg — failed"),
            "census line missing: {markdown}"
        );
        assert!(!markdown.contains("Weak results"));
        assert!(!markdown.contains("Degraded retrieval"));
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
            families: 0,
            from_page: false,
            tier: 0,
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

    #[test]
    fn site_filter_with_a_path_keeps_only_that_subtree() {
        let mut results = vec![
            merged("https://github.com/tokio-rs/tokio"),
            merged("https://github.com/rust-lang/rust"),
            merged("https://example.com/tokio-rs/tokio"),
            merged("https://github.com/"),
            merged("https://github.com/Tokio-RS/mio"),
        ];
        site_filter("runtime site:github.com/tokio-rs/", &mut results);
        let kept: Vec<&str> = results.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            kept,
            vec![
                "https://github.com/tokio-rs/tokio",
                "https://github.com/Tokio-RS/mio"
            ],
            "on-host hits outside /tokio-rs and off-host hits on the same path drop"
        );
    }

    #[test]
    fn site_filter_ignores_a_scheme_in_the_value() {
        let mut results = vec![
            merged("https://docs.rs/tokio/latest/tokio/"),
            merged("http://docs.rs/serde"),
            merged("https://crates.io/crates/tokio"),
        ];
        site_filter("tokio site:https://docs.rs", &mut results);
        assert_eq!(results.len(), 2, "docs.rs on any scheme, crates.io drops");
        assert!(results.iter().all(|r| r.url.contains("://docs.rs/")));
    }

    #[test]
    fn site_operator_with_a_path_keeps_on_site_results() {
        let mut results = vec![
            merged("https://github.com/tokio-rs/tokio"),
            merged("https://github.com/tokio-rs/axum"),
            merged("https://example.com/tokio"),
        ];
        site_filter("runtime site:github.com/tokio-rs", &mut results);
        let kept: Vec<&str> = results.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            kept,
            vec![
                "https://github.com/tokio-rs/tokio",
                "https://github.com/tokio-rs/axum"
            ],
            "site:github.com/tokio-rs must keep the github.com/tokio-rs results"
        );
    }
}
