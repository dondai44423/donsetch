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
pub mod explain;
pub mod families;
pub mod fuse;
pub mod instant;
pub mod intent;
pub mod ltr;
pub mod query;
pub mod rank;
pub mod rank_weights;
pub mod verticals;

mod authority;
mod contract;
mod cse;
mod enrich;
mod lane_cache;
pub(crate) mod ledger;
mod locale;
mod news;
mod persist;
pub mod quality;
mod render;
mod schedule;
pub(crate) use render::rank_state;
mod resolve;
mod tasks;

pub use persist::load_for_status as persist_load_for_status;
pub use persist::load_outcome_for_status as persist_load_outcome_for_status;
pub use persist::load_quality_for_status as persist_load_quality_for_status;
pub use persist::outcome_class;

pub use render::{
    SourceLine, census, evidence_url, render_compact_markdown, render_markdown, render_meta,
    source_states,
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

tokio::task_local! {
    pub(crate) static SEARCH_DEADLINE: Option<tokio::time::Instant>;
    static API_LEDGER: Arc<Mutex<ledger::Ledger>>;
    static EARLY_PAGE: Arc<Mutex<Option<tokio::sync::oneshot::Sender<engines::Hit>>>>;
    static EARLY_ALLOWANCE: Arc<tasks::RequestAllowance>;
}

fn offer_early_page(source: &str, result: &EngineResult) {
    if rank::is_vertical(engine_name(source)) {
        return;
    }
    let Ok(answer) = result else {
        return;
    };
    if !answer.class.credits() {
        return;
    }
    let Some(hit) = answer.hits.first() else {
        return;
    };
    let _ = EARLY_PAGE.try_with(|slot| {
        let mut slot = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some()
            && EARLY_ALLOWANCE
                .try_with(|allowance| allowance.take_optional())
                .unwrap_or(false)
            && let Some(sender) = slot.take()
        {
            let _ = sender.send(hit.clone());
        }
    });
}

// A successful API response may publish a wait in its JSON body. Keep the
// current answer, but apply that wait before another source request can start.
pub(crate) fn record_api_rest(raw: &str, wait: Duration) {
    let _ = API_LEDGER.try_with(|ledger| {
        let mut ledger = ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Ok(url) = url::Url::parse(raw) {
            ledger.record_origin(
                &url.origin().ascii_serialization(),
                ledger::Outcome::Throttled,
                Some(wait.as_secs().max(1)),
            );
        }
    });
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
    calendar_year(days)
}

fn calendar_year(days: i64) -> i64 {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    era * 400 + yoe + i64::from(mp >= 10)
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
    if query::QueryFacts::new(q).is_identifier() {
        return format!(
            "identifier:{}",
            q.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }
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

fn fresh_family_lists(
    per_engine: &[(String, Vec<engines::Hit>)],
    reports: &[EngineReport],
    comparable: &std::collections::HashSet<String>,
) -> Vec<(String, Vec<String>)> {
    per_engine
        .iter()
        .filter(|(name, _)| {
            !rank::is_vertical(name)
                && comparable.contains(name)
                && reports.iter().any(|report| {
                    report.engine == *name
                        && report.cache_age_ms.is_none()
                        && matches!(report.status.as_str(), "ok" | "thin")
                })
        })
        .map(|(name, hits)| {
            (
                egress::health_key(name).to_string(),
                hits.iter().map(|hit| rank::norm_key(&hit.url)).collect(),
            )
        })
        .collect()
}

/// Is the answer settled before the mark?
///
/// Independent families must already agree on a top-3 URL. A later opinion can
/// still change the order; the paired quality gate measures that tradeoff.
/// A plain item count is not enough — measured:
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

/// Whether two sources supply measured independent opinions.
fn independent_pair(a: &str, b: &str, families: &families::Families) -> bool {
    let a = egress::health_key(engine_name(a));
    let b = egress::health_key(engine_name(b));
    rank::family_name(families, a) != rank::family_name(families, b)
        && (rank::is_vertical(a) || rank::is_vertical(b) || families.overlap(a, b).is_some())
}

/// C4: full, measured independent answers share a top-three URL.
fn early_consensus_enough(
    outcomes: &[(String, EngineResult)],
    families: &families::Families,
) -> bool {
    let mut url_sources: HashMap<String, Vec<&str>> = HashMap::new();
    for (label, result) in outcomes {
        let Ok(answer) = result else {
            continue;
        };
        if !answer.class.credits() {
            continue;
        }
        for h in answer.hits.iter().take(3) {
            url_sources
                .entry(rank::norm_key(&h.url))
                .or_default()
                .push(label);
        }
    }
    url_sources.values().any(|sources| {
        sources.iter().enumerate().any(|(i, a)| {
            sources[i + 1..]
                .iter()
                .any(|b| independent_pair(a, b, families))
        })
    })
}

pub struct Searcher {
    fetcher: Fetcher,
    lane_cache: Mutex<lane_cache::LaneCache>,
    last_requests: std::sync::atomic::AtomicUsize,
    /// Shared with crawl + fetch (v4 A2): one health world for
    /// every proxied path. Clone of the daemon's Arc.
    pool: std::sync::Arc<EgressPool>,
    google: engines::google_wml::ProfileSelector,
    cse: cse::Client,
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
    cache: Mutex<persist::CacheMap>,
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
    ledger: Arc<Mutex<ledger::Ledger>>,
    /// Single-flight: two identical in-flight queries spend
    /// egress budget ONCE : the follower awaits the
    /// leader's result. Stampedes are an agent reality
    /// (parallel tool calls love the same query).
    inflight: Mutex<HashMap<String, std::sync::Arc<LocalFlight>>>,
    /// Identical complete calls share evidence reads as well as retrieval.
    pipelines: Mutex<HashMap<String, std::sync::Arc<LocalFlight>>>,
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
    /// The inputs of the most recent local merge: what each source returned,
    /// and the trust, families and depth it was ranked with. Read by
    /// `search-bench` (a retrieval miss or an ordering miss) and by
    /// `search --explain`; a concurrent caller simply overwrites it.
    last_merge: Mutex<Option<rank::MergeInput>>,
    /// Which sources return the same results, learned from their overlap.
    families: Mutex<families::Families>,
}

// v3 F1: search→fetch warm handoff store : filled by enrichment, drained
// by the fetch tool. Implementation + the enrichment pass live in
// `search::enrich`.

/// Per-engine outcome for honest reporting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EngineReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_age_ms: Option<u64>,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub status: String,
    pub hits: usize,
    pub ms: u64,
    /// Which lane carried it (observability for the
    /// governor's routing decisions).
    pub egress: String,
    /// Physical exchanges this call started for the lane; reused data costs zero.
    #[serde(default)]
    pub requests: usize,
}

#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<Merged>,
    pub intent: Intent,
    pub report: Vec<EngineReport>,
    pub cached: bool,
    pub quality: quality::Assessment,
    pub cache_age: Option<Duration>,
    pub elapsed: Duration,
    /// BYOK provider name (None = local search).
    pub provider: Option<String>,
    /// Deprecated and always false: search has no cross-encoder, no ONNX and no
    /// model in its path (§8). The field stays in the envelope for one release
    /// so a caller that reads it does not break.
    pub reranked: bool,
    /// C5: byte-derived featured snippet / instant / knowledge.
    /// Never merged into `results`. None when absent.
    pub instant: Option<instant::InstantAnswer>,
    /// Physical HTTP exchanges for this local call, including page reads.
    /// None when a provider path has no complete measurement.
    pub requests: Option<usize>,
    /// Post-fan-out phase timings for the search-debug meta:
    /// (stage, ms). Empty for the cache/BYOK paths.
    pub stage_ms: Vec<(&'static str, u128)>,
}

pub(crate) fn origin_policy(
    ledger: Arc<Mutex<ledger::Ledger>>,
) -> (
    crate::fetch::layer::OriginGuard,
    donsetch_fetch::ResponseHead,
) {
    let gate = Arc::clone(&ledger);
    let guard = Arc::new(move |raw: &str| {
        let origin = url::Url::parse(raw).ok()?.origin().ascii_serialization();
        gate.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .origin_rest(&origin)
    });
    let observe = Arc::new(
        move |status: u16, raw: &str, headers: &[(String, String)]| {
            let Ok(url) = url::Url::parse(raw) else {
                return;
            };
            let exhausted = status == 200
                && headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("x-ratelimit-remaining") && value.trim() == "0"
                });
            let wait = verticals::throttle("", if exhausted { 403 } else { status }, headers, &[]);
            let outcome = if wait.is_some() {
                ledger::Outcome::Throttled
            } else if matches!(status, 401 | 403) {
                ledger::Outcome::Blocked
            } else {
                return;
            };
            ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record_origin(
                    &url.origin().ascii_serialization(),
                    outcome,
                    wait.map(|d| d.as_secs()),
                );
        },
    );
    (guard, observe)
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
            cse: cse::Client::load(),
            trust: Mutex::new(trust),
            trust_intent: Mutex::new(trust_intent),
            health_dirty: std::sync::atomic::AtomicBool::new(false),
            cache: Mutex::new(load_cache_disk()),
            lane_cache: Mutex::new(lane_cache::LaneCache::load()),
            last_requests: std::sync::atomic::AtomicUsize::new(0),
            failures: Mutex::new(failures),
            ledger: Arc::new(Mutex::new(ledger::Ledger::load())),
            inflight: Mutex::new(HashMap::new()),
            pipelines: Mutex::new(HashMap::new()),
            byok_inflight: Mutex::new(std::collections::HashSet::new()),
            prewarms: std::sync::Arc::new(std::sync::Mutex::new(PrewarmCache::new())),
            quality: Mutex::new(load_quality_disk()),
            quality_dirty: std::sync::atomic::AtomicBool::new(false),
            outcome: Mutex::new(load_outcome_disk()),
            outcome_dirty: std::sync::atomic::AtomicBool::new(false),
            last_merge: Mutex::new(None),
            families: Mutex::new(families::load()),
        }
    }

    /// The per-source results of the most recent uncached local search, in the
    /// order the sources answered. Empty when the last call was a cache hit.
    pub fn last_raw(&self) -> Vec<(String, Vec<engines::Hit>)> {
        self.last_merge()
            .map(|input| input.per_engine)
            .unwrap_or_default()
    }

    /// The inputs of the most recent uncached local merge; `None` after a cache
    /// hit or a BYOK answer.
    pub fn last_merge(&self) -> Option<rank::MergeInput> {
        self.last_merge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Drop the cached answer for a query, so the next call fans out and ranks
    /// afresh. `search --explain` uses it: a replayed answer has no merge to
    /// explain.
    pub fn forget(&self, query: &str) {
        let key = norm_query(&query::normalize(query));
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|k, _| k.rsplit_once('|').map(|(q, _)| q) != Some(key.as_str()));
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
        let (at, cached, _total, report, quality) = cache.get(&key)?;
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
            quality: *quality,
            cache_age: Some(at.elapsed()),
            elapsed: t0.elapsed(),
            provider,
            reranked: false,
            // BYOK providers do not surface a byte-derived SERP
            // instant layer; keep the slot honest.
            instant: None,
            requests: None,
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
                .max_by_key(|(_, (at, _, _, _, _))| at.elapsed())
                .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
        let results: Vec<Merged> = out.results.iter().take(12).cloned().collect();
        let total = results.len();
        cache.insert(
            key,
            (
                Instant::now(),
                results,
                total,
                out.report.clone(),
                out.quality,
            ),
        );
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
        self.lane_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Ask a first-party interface directly, outside the search path: the oracle
    /// a canary is judged against. Bounded, and a failure is just a failure.
    pub async fn first_party(&self, vertical: &str, query: &str) -> Vec<engines::Hit> {
        const ORACLE_BUDGET: Duration = Duration::from_secs(8);
        let (guard, observe) = origin_policy(Arc::clone(&self.ledger));
        let work = tokio::time::timeout(
            ORACLE_BUDGET,
            verticals::run(&self.fetcher, vertical, query, None, None),
        );
        let result = API_LEDGER
            .scope(
                Arc::clone(&self.ledger),
                crate::fetch::layer::REQUEST_ORIGIN_GUARD.scope(
                    guard,
                    crate::fetch::layer::REQUEST_RESPONSE.scope(observe, work),
                ),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .save_if_dirty();
        result
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
        let mut names: Vec<String> = trust.keys().chain(failures.keys()).cloned().collect();
        for intent in [
            Intent::Web,
            Intent::Code,
            Intent::News,
            Intent::Paper,
            Intent::Entity,
        ] {
            names.extend(
                intent::engines_for(intent)
                    .iter()
                    .map(|name| name.to_string()),
            );
        }
        names.extend(
            [
                "registry",
                "docs",
                "wikipedia",
                "github",
                "stackexchange",
                "mdn",
                "hn",
                "arxiv",
                "openalex",
                "europepmc",
                "bing-news",
                "feed",
                "gdelt",
            ]
            .map(str::to_string),
        );
        names.sort();
        names.dedup();
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        names
            .into_iter()
            .map(|name| {
                let health_key = engine_health_key(&name);
                let t = trust.get(health_key).copied().unwrap_or(1.0);
                let f = failures.get(health_key).map(|(n, at)| (*n, at.elapsed()));
                let state = match ledger.ready(&ledger::key(lane_source(&name), lane_of(&name))) {
                    ledger::State::Rested(secs) => SourceState::Benched(Duration::from_secs(secs)),
                    _ if !trust.contains_key(health_key) && f.is_none() => SourceState::Suspect,
                    _ => source_state(t, f),
                };
                SourceHealth {
                    family: rank::family_name(&families, &name),
                    name,
                    trust: t,
                    strikes: f.map_or(0, |(n, _)| n),
                    state,
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
        self.search_with_read(query, max_results, forced_intent, 0)
            .await
    }

    /// Search and its bounded evidence reads share accounting and origin rests.
    /// Default snippet repair uses the remaining search allowance; an explicit
    /// read grants at most three additional physical exchanges.
    pub async fn search_with_read(
        &self,
        query: &str,
        max_results: usize,
        forced_intent: Option<Intent>,
        read: usize,
    ) -> Result<SearchOutcome, FetchError> {
        let started = Instant::now();
        let read = read.min(3);
        let max_results = max_results.clamp(1, 12);
        if let Some(problem) = validate_query(query) {
            self.last_requests
                .store(0, std::sync::atomic::Ordering::Relaxed);
            return Err(FetchError::Http(format!("search: {problem}")));
        }
        // Acquisition still shares across result/read limits. The complete
        // pipeline shares only equivalent requests, so a one-result caller
        // never pays for page reads requested by a twelve-result caller.
        let key = format!(
            "{}|{}|read={read}|max={max_results}",
            norm_query(&query::normalize(query)),
            forced_intent
                .unwrap_or_else(|| intent::detect(query))
                .code()
        );
        let flight = loop {
            let (leader, flight) = {
                let mut map = self
                    .pipelines
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match map.entry(key.clone()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let flight = Arc::new(LocalFlight::default());
                        entry.insert(Arc::clone(&flight));
                        (true, flight)
                    }
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        (false, Arc::clone(entry.get()))
                    }
                }
            };
            if leader {
                break flight;
            }
            loop {
                if let Some(mut answer) = flight
                    .answer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                {
                    answer.requests = Some(0); // this caller started no exchange
                    for report in &mut answer.report {
                        report.requests = 0;
                        report.ms = 0;
                    }
                    answer.elapsed = started.elapsed();
                    self.last_requests
                        .store(0, std::sync::atomic::Ordering::Relaxed);
                    return Ok(answer);
                }
                if flight.done.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                if started.elapsed() >= Duration::from_secs(12) {
                    self.last_requests
                        .store(0, std::sync::atomic::Ordering::Relaxed);
                    return Err(FetchError::Timeout);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let _pipeline_guard = LocalFlightGuard {
            map: &self.pipelines,
            key,
            flight: Arc::clone(&flight),
        };
        let limit = crate::config::cfg().search.request_budget;
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (guard, observe) = origin_policy(Arc::clone(&self.ledger));
        let pipeline = async {
            let (mut out, attempted) = self
                .with_early_evidence(
                    query,
                    read == 0,
                    started,
                    crate::fetch::layer::REQUEST_LIMIT.scope(
                        limit,
                        self.search_counted(query, max_results, forced_intent),
                    ),
                )
                .await?;
            if read > 0 {
                self.read_results(&mut out.results, query, read).await;
            }
            // Cold documents took 0.8–1.3 s in native witnesses; 450 ms spent
            // requests without yielding evidence. Use the remaining 2.2 s call
            // envelope, not an extra wait after an already-slow search.
            let repair_time = Duration::from_millis(2200)
                .saturating_sub(started.elapsed())
                .min(Duration::from_millis(1500));
            self.repair_default_snippets(&mut out.results, query, read, repair_time, &attempted)
                .await;
            self.ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .save_if_dirty();
            out.elapsed = started.elapsed();
            Ok::<_, FetchError>(out)
        };
        let work = crate::fetch::layer::REQUEST_COUNT.scope(
            Arc::clone(&count),
            crate::fetch::layer::REQUEST_LIMIT.scope(limit.saturating_add(read), pipeline),
        );
        let mut outcome = crate::fetch::layer::REQUEST_ORIGIN_GUARD
            .scope(
                guard,
                crate::fetch::layer::REQUEST_RESPONSE
                    .scope(observe, API_LEDGER.scope(Arc::clone(&self.ledger), work)),
            )
            .await;
        let requests = count.load(std::sync::atomic::Ordering::Relaxed);
        self.last_requests
            .store(requests, std::sync::atomic::Ordering::Relaxed);
        if let Ok(out) = &mut outcome {
            out.requests = Some(requests);
            *flight
                .answer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(out.clone());
        }
        outcome
    }

    /// Physical HTTP exchanges started by the last completed local call, including
    /// failures and cancelled lanes. Concurrent calls may replace this receipt.
    pub fn last_request_count(&self) -> usize {
        self.last_requests
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn search_counted(
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
        let flight = loop {
            let (leader, flight) = {
                let mut map = self
                    .inflight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match map.entry(sf_key.clone()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let flight = std::sync::Arc::new(LocalFlight::default());
                        entry.insert(std::sync::Arc::clone(&flight));
                        (true, flight)
                    }
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        (false, std::sync::Arc::clone(entry.get()))
                    }
                }
            };
            if leader {
                break flight;
            }
            // Share this call's fresh answer, even when its confidence forbids
            // caching. Only callers already waiting on this flight can see it.
            loop {
                if let Some(mut answer) = flight
                    .answer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                {
                    answer.results.truncate(max_results);
                    for report in &mut answer.report {
                        report.requests = 0;
                        report.ms = 0;
                    }
                    answer.elapsed = started.elapsed();
                    return Ok(answer);
                }
                if flight.done.load(std::sync::atomic::Ordering::Acquire) {
                    break; // Failed/cancelled leader: elect one replacement.
                }
                if started.elapsed() >= Duration::from_secs(12) {
                    return Err(FetchError::Timeout);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let _inflight_guard = LocalFlightGuard {
            map: &self.inflight,
            key: sf_key,
            flight: std::sync::Arc::clone(&flight),
        };
        let mut outcome = self.search_inner(query, 12, forced_intent, started).await;
        if let Ok(answer) = &mut outcome {
            *flight
                .answer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(answer.clone());
            answer.results.truncate(max_results);
        }
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
        *self
            .last_merge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        // Cache stores top-12; asking for more just
        // re-lists the same tail.
        let max_results = max_results.clamp(1, 12);
        let intent = forced_intent.unwrap_or_else(|| intent::detect(query));
        // The query's language, from its script. It goes to the sources that
        // accept one and to Wikipedia's own edition; the contract uses the same
        // script test to tell a native result from a foreign page.
        let locale = locale::detect(query);
        let cache_key = format!("{}|{}", norm_query(query), intent.code());

        if let Some((at, cached, _total, reports, quality)) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&cache_key)
            && at.elapsed() < cache_ttl(intent, query)
            && quality.confidence == quality::Confidence::High
        {
            let mut results = cached.iter().take(max_results).cloned().collect();
            site_filter(query, &mut results);
            query::intitle_filter(query, &mut results);
            query::filetype_filter(query, &mut results);
            return Ok(SearchOutcome {
                results,
                intent,
                report: reports
                    .iter()
                    .cloned()
                    .map(|mut report| {
                        report.requests = 0;
                        report.ms = 0;
                        if matches!(report.status.as_str(), "ok" | "cached" | "thin") {
                            report.status = "cached".into();
                            report.cache_age_ms = Some(
                                report
                                    .cache_age_ms
                                    .unwrap_or(0)
                                    .saturating_add(at.elapsed().as_millis() as u64),
                            );
                        }
                        report
                    })
                    .collect(),
                cached: true,
                quality: *quality,
                cache_age: Some(at.elapsed()),
                elapsed: started.elapsed(),
                provider: None,
                reranked: false,
                // Instant is not cached: a stale featured
                // snippet presented as fresh would lie.
                instant: None,
                requests: None,
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
        let query_facts = query::QueryFacts::new(query);
        let engine_allowance = Arc::new(tasks::RequestAllowance::new(0));
        let mut futures: Vec<(String, TaskFut)> = Vec::new();
        let mut request_costs = HashMap::new();
        // A lane the ledger rests, or the budget cannot afford, is reported
        // rather than silently dropped: the census says which, and why.
        let mut skipped: Vec<(String, EngineResult)> = Vec::new();
        let budget = crate::config::cfg().search.request_budget;
        let mut used_egresses: Vec<String> = Vec::new();
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
        let mut priority_sources = vec!["google-cse".to_string()];
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
            if let Some(engine) = engine
                && let Some(scoped) = authority::official_queries(&compiled).into_iter().next()
                && let Some(assignment) = assignments.iter_mut().find(|(name, _)| name == engine)
            {
                assignment.1 = scoped;
                priority_sources.push(engine.to_string());
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
        // An official recall asks a different question. It cannot teach index
        // independence against a peer's broad query; neither can a cache replay.
        let comparable: std::collections::HashSet<String> = if compiled.has_operators() {
            Default::default()
        } else {
            assignments
                .iter()
                .filter(|(engine, q)| *q == query::for_engine(&compiled, engine))
                .map(|(engine, _)| engine_name(engine).to_string())
                .collect()
        };
        let context = tasks::EngineContext {
            fetcher: &self.fetcher,
            pool: self.pool.as_ref(),
            google: &self.google,
            cse: &self.cse,
            locale,
            requests: &engine_allowance,
            facts: Some(&query_facts),
        };
        for (engine, q, eg) in assign_egresses(self.pool.as_ref(), assignments, &mut used_egresses)
        {
            let name = engine_name(&engine).to_string();
            let cached = self
                .lane_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&name, &q, intent);
            let pace = if cached {
                Some(Duration::ZERO)
            } else {
                self.plan_lane(&name, &mut skipped)
            };
            let Some(pace) = pace else {
                continue;
            };
            request_costs.insert(
                name.clone(),
                if cached {
                    0
                } else if name == "google-cse" {
                    self.cse.request_cost(eg.proxy.as_ref())
                } else {
                    1
                },
            );
            futures.push((
                name.clone(),
                Box::pin(lane_cache::run(
                    &self.lane_cache,
                    name.clone(),
                    q.clone(),
                    intent,
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
                )),
            ));
        }
        // Verticals: direct, friendly APIs.
        let verticals: Vec<&str> = verticals
            .into_iter()
            .filter(|v| !self.quarantined(v))
            .collect();
        let mut vfuts: Vec<(String, TaskFut)> = Vec::new();
        for v in verticals {
            if verticals::endpoint_in(v, query, locale).is_none() {
                skipped.push((
                    v.to_string(),
                    Err(("not-asked".into(), "no-interface".into(), false)),
                ));
                continue;
            }
            let pace = if self
                .lane_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(v, query, intent)
            {
                Some(Duration::ZERO)
            } else {
                self.plan_lane(v, &mut skipped)
            };
            let Some(pace) = pace else {
                continue;
            };
            vfuts.push((
                engine_name(v).to_string(),
                Box::pin(lane_cache::run(
                    &self.lane_cache,
                    v.to_string(),
                    query.to_string(),
                    intent,
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
                )),
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
                        query.to_string(),
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
        if query.split_whitespace().count() == 1
            && futures.iter().any(|(name, _)| name == "registry")
        {
            futures.sort_by_key(|(name, _)| name != "registry");
            priority_sources.push("registry".into());
        }
        if futures.iter().any(|(name, _)| name == "docs") {
            futures.sort_by_key(|(name, _)| name != "docs");
            priority_sources.push("docs".into());
        }
        let mut kept: Vec<(String, TaskFut)> = Vec::with_capacity(futures.len());
        let (mut engines_kept, mut verticals_kept, mut news_kept, mut guesses_kept) =
            (0usize, 0usize, 0usize, 0usize);
        let mut spent = 0usize;
        let mut first_party_reserved = 0usize;
        let mut initial_requests = Vec::new();
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
            // Reserve mandatory first exchanges, including Google's cold
            // bootstrap. Optional retries cannot consume another lane's start.
            let cost = match name.as_str() {
                "feed" | "arxiv" | "docs" => 2,
                "registry" | "registry-any" => 3,
                _ => request_costs.get(&name).copied().unwrap_or(1),
            };
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
                if class != 0 {
                    first_party_reserved += cost;
                } else {
                    initial_requests.push((name.clone(), cost));
                }
                kept.push((name, fut));
            } else {
                skipped.push((name, Err(("not-asked".into(), "budget".into(), false))));
            }
        }
        let futures: Vec<(String, TaskFut<'_>)> = kept
            .into_iter()
            .map(|(name, future)| {
                let counter = engine_allowance.counter(&name);
                let allowance = &engine_allowance;
                let source = name.clone();
                let measured: TaskFut<'_> = Box::pin(async move {
                    let (label, mut result) = crate::fetch::layer::REQUEST_SOURCE_COUNT
                        .scope(std::sync::Arc::clone(&counter), future)
                        .await;
                    allowance.release(&source);
                    if let Ok(answer) = &mut result {
                        answer.requests = counter.load(std::sync::atomic::Ordering::Relaxed);
                    }
                    (label, result)
                });
                (name, measured)
            })
            .collect();
        engine_allowance.plan(
            budget.saturating_sub(first_party_reserved),
            initial_requests,
        );

        let known_families = self
            .families
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (outcomes, late) = EARLY_ALLOWANCE
            .scope(
                Arc::clone(&engine_allowance),
                schedule::run(
                    futures,
                    skipped,
                    &known_families,
                    crate::config::cfg().search.search_early,
                    news::wants_news(query, intent),
                    &priority_sources,
                ),
            )
            .await;

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
                    if answer.cache_age.is_none() {
                        self.record_lane(&engine, "ok");
                    }
                    if answer.class.credits() && answer.cache_age.is_none() {
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
                        if ans.permitted(&query_facts) {
                            instant = Some(ans);
                        }
                    }
                    report.push(EngineReport {
                        cache_age_ms: answer.cache_age.map(|age| age.as_millis() as u64),
                        engine: engine.clone(),
                        profile: profile.clone(),
                        status: if answer.cache_age.is_some() {
                            "cached"
                        } else {
                            answer.class.status()
                        }
                        .into(),
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
                    }
                    // The ledger above owns Retry-After. A throttle must not
                    // also create a fixed ten-minute transport quarantine.
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
                        "rested" | "not-asked" | "pacing-timeout" | "budget-skipped"
                    );
                    report.push(EngineReport {
                        cache_age_ms: None,
                        requests: if was_engine {
                            engine_allowance.spent(&engine)
                        } else {
                            usize::from(asked)
                        },
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
                cache_age_ms: None,
                requests: engine_allowance.spent(&name),
                engine: name,
                profile: None,
                status: "late".into(),
                hits: 0,
                ms: 0,
                egress: "-".into(),
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

        let native_lang = locale::evidence_language(query);
        retain_native_script(&mut per_engine, native_lang);

        let total = rank::merged_total(&per_engine);
        // What each general source returned teaches the independence graph, and
        // this query is ranked with what it just learned.
        let families_now = {
            let lists = fresh_family_lists(&per_engine, &report, &comparable);
            let mut guard = self
                .families
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.observe(&lists) {
                families::save(&guard);
            }
            guard.clone()
        };
        // Always merge 12 results for the cache, then trim to
        // max_results for the response. Without this, a first
        // search with max=2 caches only 2 results, and a later
        // search with max=10 returns the stale 2 from cache.
        // A query with an operator filters its own answer after the merge, so it
        // needs a deeper pool: the twelve a plain query needs can be emptied by a
        // title filter (measured: `intitle:"Attention Is All You Need" arxiv` came
        // back empty while three lanes had answered — the paper sat beyond slot 12).
        let merge_depth = if compiled.has_operators() { 40 } else { 12 };
        let mut input = rank::MergeInput {
            per_engine,
            query: query.to_string(),
            intent,
            trust,
            families: families_now,
            depth: merge_depth,
            post_adjustments: HashMap::new(),
        };
        let mut results = input.merge();
        let base_scores: HashMap<_, _> = results
            .iter()
            .map(|r| (rank::norm_key(&r.url), r.score))
            .collect();
        // Compatibility hook: historical host quality no longer changes the
        // evidence-ranked order (5.3.0 §14).
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
        results.sort_by(|a, b| {
            a.tier
                .cmp(&b.tier)
                .then_with(|| b.score.total_cmp(&a.score))
        });
        input.post_adjustments = results
            .iter()
            .filter_map(|r| {
                let key = rank::norm_key(&r.url);
                let delta = r.score - base_scores.get(&key).copied().unwrap_or(r.score);
                (delta != 0.0).then_some((key, delta))
            })
            .collect();
        *self
            .last_merge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(input);

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

        // No cross-encoder stage: search carries no model (5.3.0 §8). The
        // stage is reported flat so `stage_ms` stays schema-stable.
        let topup_ms: u128 = 0;
        // Poisoning guard: a merge built while engines
        // were down must NOT persist for 30 minutes :
        // degraded-period results expire with the moment.
        let quality = quality::assess(
            query,
            &results,
            &report,
            &self
                .families
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let cacheable = quality.confidence == quality::Confidence::High;
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
                        .max_by_key(|(_, (at, _, _, _, _))| at.elapsed())
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
                        quality,
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

        self.lane_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .save();

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
            quality,
            cache_age: None,
            elapsed: started.elapsed(),
            provider: None,
            reranked: false,
            instant,
            requests: None,
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
        let input = rank::MergeInput {
            per_engine: vec![("registry".to_string(), resolved.hits.clone())],
            query: query.to_string(),
            intent,
            trust: HashMap::new(),
            families: families::Families::default(),
            depth: 12,
            post_adjustments: HashMap::new(),
        };
        let results = input.merge();
        *self
            .last_merge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(input);
        let report = vec![EngineReport {
            cache_age_ms: resolved.cache_age.map(|age| age.as_millis() as u64),
            engine: "registry".into(),
            profile: Some(resolved.label),
            status: if resolved.cache_age.is_some() {
                "cached"
            } else {
                "ok"
            }
            .into(),
            hits: resolved.hits.len(),
            ms: resolved.ms,
            egress: if resolved.cache_age.is_some() {
                "cache"
            } else {
                "direct"
            }
            .into(),
            requests: crate::fetch::layer::REQUEST_COUNT
                .try_with(|count| count.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(resolved.requests),
        }];
        let quality = quality::assess(query, &results, &report, &families::Families::default());
        SearchOutcome {
            results: results.into_iter().take(max_results).collect(),
            intent,
            report,
            cached: false,
            quality,
            cache_age: None,
            elapsed: started.elapsed(),
            provider: None,
            reranked: false,
            instant: None,
            requests: None,
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

    /// Local health facts without a search, network request or model loader.
    pub fn diagnostics(&self) -> serde_json::Value {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let high = cache
            .values()
            .filter(|(_, _, _, _, q)| q.confidence == quality::Confidence::High)
            .count();
        let (lanes, fresh) = self
            .lane_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .counts();
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let states: Vec<_> = intent::engines_for(Intent::Web).iter().map(|source| {
            serde_json::json!({"source": source, "standing": format!("{:?}", ledger.ready(&ledger::key(source, "serp")))})
        }).collect();
        serde_json::json!({"runtime_model": false, "model_download": false,
            "cache": {"merged": cache.len(), "high": high, "lanes": lanes, "fresh_lanes": fresh},
            "ledger": states, "egress": self.pool.lane_summary().len()})
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

/// Healthy pools retain the available roster; stressed pools keep at most
/// three endpoints. Confidence measures independence separately from that count.
fn width_for_stress(stress: f64, available: usize) -> usize {
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

fn retain_native_script(per_engine: &mut Vec<(String, Vec<engines::Hit>)>, lang: Option<&str>) {
    let Some(lang) = lang else { return };
    if !per_engine
        .iter()
        .flat_map(|(_, hits)| hits)
        .any(|h| locale::native(lang, &h.title, &h.snippet))
    {
        return;
    }
    for (_, hits) in per_engine.iter_mut() {
        hits.retain(|h| locale::native(lang, &h.title, &h.snippet));
    }
    per_engine.retain(|(_, hits)| !hits.is_empty());
}

/// A fresh answer shared only with callers waiting on the same request.
#[derive(Default)]
struct LocalFlight {
    answer: Mutex<Option<SearchOutcome>>,
    done: std::sync::atomic::AtomicBool,
}

struct LocalFlightGuard<'a> {
    map: &'a Mutex<HashMap<String, std::sync::Arc<LocalFlight>>>,
    key: String,
    flight: std::sync::Arc<LocalFlight>,
}

impl Drop for LocalFlightGuard<'_> {
    fn drop(&mut self) {
        let mut map = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map
            .get(&self.key)
            .is_some_and(|entry| std::sync::Arc::ptr_eq(entry, &self.flight))
        {
            map.remove(&self.key);
        }
        self.flight
            .done
            .store(true, std::sync::atomic::Ordering::Release);
    }
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

    // nextest isolates the installed process configuration and native cache.
    #[tokio::test]
    async fn default_repair_and_explicit_reads_report_and_obey_their_physical_budget() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.fetch.allow_private_egress = true;
        config.state.no_disk_state = true;
        config.search.request_budget = 1;
        crate::config::install(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let log = Arc::clone(&seen);
        let slow = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pause = Arc::clone(&slow);
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                let pause = Arc::clone(&pause);
                tokio::spawn(async move {
                    let mut head = [0u8; 8192];
                    if stream.read(&mut head).await.unwrap_or(0) == 0 {
                        return;
                    }
                    log.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if pause.load(std::sync::atomic::Ordering::Acquire) {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    let body = format!("<html><head><title>Orion calibration</title></head><body><article><p>{}</p></article></body></html>", "Orion calibration establishes the sensor value as 42 before the measurement begins. ".repeat(8));
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        let searcher = Searcher::new(
            Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap(),
            EgressPool::new(Vec::new()),
        );
        let query = "Orion calibration";
        let results = (0..2)
            .map(|i| {
                let mut r = merged(&format!("http://{address}/page-{i}"));
                if i == 0 {
                    r.snippet = "This guide explains Orion calibration and how to establish a sensor value before measurement.".into();
                } else {
                    r.snippet = "Orion calibration measures sensor response before the observation begins.".into();
                }
                r
            })
            .collect();
        searcher.cache.lock().unwrap().insert(
            format!("{}|{}", norm_query(query), Intent::Web.code()),
            (
                Instant::now(),
                results,
                2,
                vec![EngineReport {
                    engine: "bing".into(),
                    profile: None,
                    status: "ok".into(),
                    hits: 2,
                    ms: 10,
                    egress: "direct".into(),
                    cache_age_ms: None,
                    requests: 1,
                }],
                quality::Assessment {
                    confidence: quality::Confidence::High,
                    families: 2,
                    agreement: 2,
                    junk_ratio: 0.0,
                    coverage: 1.0,
                },
            ),
        );
        let default = searcher.search(query, 2, Some(Intent::Web)).await.unwrap();
        assert_eq!(default.requests, Some(1));
        assert_eq!(
            default.report[0].requests, 0,
            "cached provenance is not a new engine request"
        );
        assert_eq!(default.report[0].status, "cached");
        assert!(default.report[0].cache_age_ms.is_some());
        assert_eq!(default.results.iter().filter(|r| r.from_page).count(), 1);
        assert!(default.results[0].from_page && default.results[0].snippet.contains("42"));
        assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 1);
        let explicit = searcher
            .search_with_read(query, 2, Some(Intent::Web), 2)
            .await
            .unwrap();
        assert_eq!(explicit.requests, Some(2));
        assert!(
            explicit
                .results
                .iter()
                .all(|r| r.from_page && r.snippet.contains("42"))
        );
        assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 3);
        let (a, b) = tokio::join!(
            searcher.search(query, 2, Some(Intent::Web)),
            searcher.search(query, 2, Some(Intent::Web))
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(
            a.requests.unwrap() + b.requests.unwrap(),
            1,
            "identical callers share the repair request"
        );
        assert!(a.results[0].from_page && b.results[0].from_page);
        assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 4);
        let (a, b) = tokio::join!(
            searcher.search_with_read(query, 2, Some(Intent::Web), 2),
            searcher.search_with_read(query, 2, Some(Intent::Web), 2)
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(
            a.requests.unwrap() + b.requests.unwrap(),
            2,
            "explicit page reads are shared too"
        );
        assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 6);
        let (a, b) = tokio::join!(
            searcher.search(query, 2, Some(Intent::Web)),
            searcher.search_with_read(query, 2, Some(Intent::Web), 1)
        );
        assert_eq!(
            a.unwrap().requests.unwrap() + b.unwrap().requests.unwrap(),
            2,
            "different evidence requests keep their own page reads"
        );
        let (a, b) = tokio::join!(
            searcher.search(query, 1, Some(Intent::Web)),
            searcher.search(query, 2, Some(Intent::Web))
        );
        assert_eq!((a.unwrap().results.len(), b.unwrap().results.len()), (1, 2));
        slow.store(true, std::sync::atomic::Ordering::Release);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(40),
                searcher.search(query, 2, Some(Intent::Web))
            )
            .await
            .is_err()
        );
        assert!(
            searcher.pipelines.lock().unwrap().is_empty(),
            "cancelling a leader releases its complete-call flight"
        );
        assert!(searcher.inflight.lock().unwrap().is_empty());
        slow.store(false, std::sync::atomic::Ordering::Release);
        let recovered = searcher.search(query, 2, Some(Intent::Web)).await.unwrap();
        assert!(recovered.results[0].from_page && recovered.results[0].snippet.contains("42"));
        let expired = SEARCH_DEADLINE
            .scope(
                Some(tokio::time::Instant::now()),
                searcher.search(query, 2, Some(Intent::Web)),
            )
            .await
            .unwrap();
        assert_eq!(
            expired.requests,
            Some(0),
            "an exhausted evidence deadline starts no exchange"
        );
        assert!(expired.results.iter().all(|r| !r.from_page));

        // The read starts while acquisition still waits on another index,
        // shares its physical ceiling, and changes no final ranking decision.
        slow.store(true, std::sync::atomic::Ordering::Release);
        let mut planned = default.clone();
        planned.results[0].from_page = false;
        planned.results[0].snippet =
            "2 days ago - Learn how to perform Orion calibration in this guide.".into();
        planned.results[0].score = 777.0;
        let first = engines::Hit {
            title: planned.results[0].title.clone(),
            url: planned.results[0].url.clone(),
            snippet: "3 days ago - Learn how to perform Orion calibration in this guide.".into(),
            rank: 0,
            published: None,
        };
        let make_answer = |class| {
            Ok(tasks::LaneAnswer {
                hits: vec![first.clone()],
                items: 1,
                ms: 0,
                egress: "direct".into(),
                engine_lane: true,
                instant: None,
                class,
                requests: 0,
                cache_age: None,
            })
        };
        let before = seen.load(std::sync::atomic::Ordering::Relaxed);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let work = async {
            offer_early_page("bing", &make_answer(engines::AnswerClass::Thin));
            assert!(
                EARLY_PAGE.with(|slot| slot.lock().unwrap().is_some()),
                "a thin list cannot start the read"
            );
            offer_early_page("bing", &make_answer(engines::AnswerClass::Answered));
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                seen.load(std::sync::atomic::Ordering::Relaxed),
                before + 1,
                "page read overlaps acquisition"
            );
            Ok(planned.clone())
        };
        let (overlapped, attempted) = crate::fetch::layer::REQUEST_COUNT
            .scope(
                Arc::clone(&count),
                crate::fetch::layer::REQUEST_LIMIT.scope(
                    1,
                    searcher.with_early_evidence(
                        "how to perform Orion calibration",
                        true,
                        Instant::now(),
                        EARLY_ALLOWANCE.scope(Arc::new(tasks::RequestAllowance::new(1)), work),
                    ),
                ),
            )
            .await
            .unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(attempted, vec![first.url.clone()]);
        assert!(overlapped.results[0].from_page && overlapped.results[0].snippet.contains("42"));
        assert!(overlapped.results[0].snippet.starts_with("3 days ago - "));
        assert_eq!(overlapped.results[0].snippet.matches("days ago").count(), 1);
        assert_eq!(overlapped.results[0].score, 777.0);
        slow.store(false, std::sync::atomic::Ordering::Release);
        let mut after = overlapped.results.clone();
        searcher
            .repair_default_snippets(
                &mut after,
                "how to perform Orion calibration",
                0,
                Duration::from_millis(200),
                &attempted,
            )
            .await;
        assert_eq!(
            seen.load(std::sync::atomic::Ordering::Relaxed),
            before + 1,
            "no duplicate read after an overlapping success"
        );
        let no_hint = tokio::time::timeout(
            Duration::from_millis(200),
            searcher.with_early_evidence(
                "how to perform Orion calibration",
                true,
                Instant::now(),
                async { Ok(planned.clone()) },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            no_hint.1.is_empty(),
            "closing acquisition also closes an unused hint"
        );
        let reserved = Arc::new(tasks::RequestAllowance::new(0));
        reserved.plan(1, vec![("ddg".into(), 1)]);
        let before_blocked = seen.load(std::sync::atomic::Ordering::Relaxed);
        let blocked = searcher
            .with_early_evidence(
                "how to perform Orion calibration",
                true,
                Instant::now(),
                EARLY_ALLOWANCE.scope(Arc::clone(&reserved), async {
                    offer_early_page("bing", &make_answer(engines::AnswerClass::Answered));
                    Ok(planned.clone())
                }),
            )
            .await
            .unwrap();
        assert!(
            blocked.1.is_empty(),
            "a page cannot use another source's reserved start"
        );
        assert_eq!(
            seen.load(std::sync::atomic::Ordering::Relaxed),
            before_blocked
        );
        assert!(reserved.take("ddg"));
        let missing = searcher
            .with_early_evidence(
                "how to perform Orion calibration",
                true,
                Instant::now(),
                EARLY_ALLOWANCE.scope(Arc::new(tasks::RequestAllowance::new(1)), async {
                    offer_early_page("bing", &make_answer(engines::AnswerClass::Answered));
                    let mut out = planned.clone();
                    out.results.clear();
                    Ok(out)
                }),
            )
            .await
            .unwrap();
        assert!(
            missing.0.results.is_empty(),
            "a preview cannot insert a result the final ranking discarded"
        );
        server.abort();
    }

    #[test]
    fn native_evidence_takes_precedence_without_erasing_a_useful_foreign_fallback() {
        let hit = |title: &str| engines::Hit {
            title: title.into(),
            url: "https://example.com/page".into(),
            snippet: String::new(),
            rank: 0,
            published: None,
        };
        let mut lists = vec![
            ("one".into(), vec![hit("Tokyo weather"), hit("東京の天気")]),
            ("two".into(), vec![hit("English-only title")]),
        ];
        let unchanged = lists.clone();
        retain_native_script(&mut lists, None);
        assert_eq!(lists.len(), unchanged.len());
        retain_native_script(&mut lists, Some("ja"));
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].1.len(), 1);
        assert_eq!(lists[0].1[0].title, "東京の天気");
        retain_native_script(&mut lists, Some("ko"));
        assert_eq!(
            lists.len(),
            1,
            "a missing native source must not erase the only available page"
        );
    }

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
    fn source_health_includes_unmeasured_sources_and_the_ledgers_rest() {
        let s = test_searcher();
        let initial = s.health_report();
        assert!(
            initial
                .iter()
                .any(|row| row.name == "google-cse" && row.state == SourceState::Suspect)
        );
        s.record_lane("google-cse", "rate-limited:120");
        assert!(s.health_report().iter().any(|row| row.name == "google-cse" && matches!(row.state, SourceState::Benched(_))));
        assert!(
            s.health_report()
                .iter()
                .any(|row| row.name == "yahoo" && row.state == SourceState::Suspect)
        );
    }

    #[test]
    fn the_calendar_year_changes_in_january_not_march() {
        assert_eq!(calendar_year(0), 1970);
        assert_eq!(calendar_year(20_758), 2026); // 2026-11-01
        assert_eq!(calendar_year(10_956), 1999); // 1999-12-31
        assert_eq!(calendar_year(10_957), 2000); // 2000-01-01
        assert_eq!(calendar_year(11_017), 2000); // 2000-03-01
    }

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
    #[tokio::test]
    async fn a_throttled_source_is_rested_without_losing_trust() {
        let s = test_searcher();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        crate::fetch::layer::REQUEST_COUNT
            .scope(
                Arc::clone(&count),
                API_LEDGER.scope(Arc::clone(&s.ledger), async {
                    record_api_rest(
                        "https://api.stackexchange.com/2.3/search/advanced",
                        Duration::from_secs(120),
                    );
                    // An oracle/canary uses the same wait and must not spend a request.
                    assert!(s.first_party("stackexchange", "nginx").await.is_empty());
                }),
            )
            .await;
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(
            ledger::Ledger::load()
                .origin_rest("https://api.stackexchange.com")
                .is_some()
        );
        let trust_before = s
            .trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("stackexchange")
            .copied();
        assert!(!s.quarantined("stackexchange"));
        s.record_lane("stackexchange", "rate-limited:120");
        let mut skipped = Vec::new();
        assert!(s.plan_lane("stackexchange", &mut skipped).is_none());
        assert!(
            !s.quarantined("stackexchange"),
            "the published wait is not a ten-minute transport failure"
        );
        let state = s
            .ledger
            .lock()
            .unwrap()
            .ready(&ledger::key("stackexchange", "api"));
        assert!(
            matches!(state, ledger::State::Rested(119..=120)),
            "{state:?}"
        );
        assert!(
            s.plan_lane("github", &mut skipped).is_some(),
            "only the throttled source rests"
        );
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
            cache_age_ms: None,
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
            quality: Default::default(),
            cache_age: None,
            elapsed: Duration::ZERO,
            provider: Some(provider.to_string()),
            reranked: false,
            instant: None,
            requests: None,
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
        s.cache.lock().unwrap().insert(
            key,
            (
                Instant::now(),
                vec![merged(url)],
                12,
                Vec::new(),
                quality::Assessment {
                    confidence: quality::Confidence::High,
                    ..Default::default()
                },
            ),
        );
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
        let flight = std::sync::Arc::new(LocalFlight::default());
        s.inflight
            .lock()
            .unwrap()
            .insert(leader, std::sync::Arc::clone(&flight));
        // Negative: an identical query joins the flight and is served
        // what the leader publishes.
        let (joined, ()) = tokio::join!(s.search("jaguar car", 5, Some(Intent::Web)), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            seed_local_cache(&s, "jaguar car", JAGUAR_REVIEW);
            let mut answer = s
                .search_inner("jaguar car", 12, Some(Intent::Web), Instant::now())
                .await
                .unwrap();
            // Concurrent callers may share a fresh weak answer, while later
            // callers must not read it from the merged cache.
            answer.cached = false;
            answer.quality.confidence = quality::Confidence::Low;
            answer.results[0].snippet =
                "A detailed jaguar car review with independent road test results.".into();
            answer.report = vec![EngineReport {
                engine: "bing".into(),
                profile: None,
                status: "ok".into(),
                hits: 1,
                ms: 10,
                egress: "direct".into(),
                cache_age_ms: None,
                requests: 1,
            }];
            *flight.answer.lock().unwrap() = Some(answer);
        });
        let joined = joined.expect("a follower is served the leader's results");
        assert!(!joined.cached);
        assert_eq!(joined.quality.confidence, quality::Confidence::Low);
        assert_eq!(joined.results[0].url, JAGUAR_REVIEW);
        assert_eq!(joined.requests, Some(0));
        assert_eq!(
            joined.report[0].requests, 0,
            "shared provenance is not another exchange"
        );
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
                    cache_age: None,
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
        assert_eq!(answered(&[lane(4), lane(3)]).1, 7);
        assert_eq!(answered(&[lane(10), lane(3)]).0, 2);
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
                    cache_age: None,
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
                    cache_age: None,
                }),
            )
        };
        // Two names sharing a top-3 URL are not yet independent evidence.
        let two = vec![
            ok("brave", &["https://a.com/x", "https://b.com/y"]),
            ok("bing", &["https://a.com/x", "https://b.com/y"]),
        ];
        let mut fam = families::Families::default();
        assert!(!early_consensus_enough(&two, &fam));
        let lists: Vec<_> = ["brave", "bing"]
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    (0..5)
                        .map(|i| format!("https://{name}.example/{i}"))
                        .collect(),
                )
            })
            .collect();
        for _ in 0..3 {
            fam.observe(&lists);
        }
        assert!(early_consensus_enough(&two, &fam));
        let duplicate = vec![
            ok("bing", &["https://a.com/x"]),
            ok("bing@recall", &["https://a.com/x"]),
        ];
        assert!(!early_consensus_enough(&duplicate, &fam));
        let owned = vec![
            ok("bing", &["https://a.com/x"]),
            ok("stackexchange", &["https://a.com/x"]),
        ];
        assert!(early_consensus_enough(&owned, &Default::default()));
        let mut thin = vec![
            ok("brave", &["https://a.com/x", "https://b.com/y"]),
            ok("bing", &["https://a.com/x", "https://b.com/y"]),
        ];
        for (_, answer) in &mut thin {
            answer.as_mut().unwrap().class = engines::AnswerClass::Thin;
        }
        assert!(
            !early_consensus_enough(&thin, &fam),
            "thin fallback agreement must keep listening for a full answer"
        );
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

    #[test]
    fn cached_or_scoped_lists_cannot_manufacture_independence_samples() {
        let hits: Vec<_> = (0..5)
            .map(|i| engines::Hit {
                title: "A page".into(),
                url: format!("https://example.com/{i}"),
                snippet: "An answer".into(),
                rank: i,
                published: None,
            })
            .collect();
        let sources = vec![("bing".into(), hits.clone()), ("yahoo".into(), hits)];
        let mut reports: Vec<_> = ["bing", "yahoo"]
            .into_iter()
            .map(|engine| EngineReport {
                engine: engine.into(),
                status: "ok".into(),
                hits: 5,
                ms: 1,
                egress: "direct".into(),
                requests: 1,
                profile: None,
                cache_age_ms: None,
            })
            .collect();
        let comparable = ["bing".into(), "yahoo".into()].into();
        let mut families = families::Families::default();
        for _ in 0..2 {
            families.observe(&fresh_family_lists(&sources, &reports, &comparable));
        }
        reports[1].cache_age_ms = Some(1000);
        reports[1].status = "cached".into();
        families.observe(&fresh_family_lists(&sources, &reports, &comparable));
        assert!(families.overlap("bing", "yahoo").is_none());
        reports[1].cache_age_ms = None;
        reports[1].status = "ok".into();
        families.observe(&fresh_family_lists(
            &sources,
            &reports,
            &["bing".into()].into(),
        ));
        assert!(
            families.overlap("bing", "yahoo").is_none(),
            "scoped recall is not a matching query"
        );
        families.observe(&fresh_family_lists(&sources, &reports, &comparable));
        assert_eq!(families.count(["bing", "yahoo"]), 1);
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
        // Operators and code syntax keep different questions apart.
        for (a, b) in [
            ("select!", "select"),
            ("std::mem::take", "std mem take"),
            ("camelCase", "camelcase"),
            ("foo_bar", "foo bar"),
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
                quality::Assessment {
                    confidence: quality::Confidence::High,
                    ..Default::default()
                },
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
    // is identical at every snippet budget (`evidence::budget`).

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
            quality: Default::default(),
            cache_age: None,
            elapsed: Duration::from_millis(10),
            provider: None,
            reranked: false,
            instant: None,
            requests: None,
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
                cache_age_ms: None,
                engine: "bing".into(),
                profile: None,
                status: "ok".into(),
                hits: 10,
                ms: 12,
                egress: "direct".into(),
                requests: 0,
            },
            EngineReport {
                cache_age_ms: None,
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
