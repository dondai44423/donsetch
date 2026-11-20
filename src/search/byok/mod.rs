//! Bring Your Own Keys (BYOK) : provider search with key rotation.
//!
//! When the user configures API keys for external search providers
//! (Tavily, Exa, Serper.dev, SerpApi, Brave Search API, TinyFish,
//! Parallel AI, Bright Data), the entire local keyless
//! search system is bypassed. The provider handles everything:
//! search, IP, rate limiting. DonSeTch just sends the query and
//! normalizes the results.
//!
//! Chain: default provider → try each key → next provider → ...
//! → if all exhausted, fall back to local search.
//!
//! Key states persist to disk: rate-limited keys auto-recover
//! after 60s cooldown. Credit-depleted/invalid keys stay dead
//! until the user resets them.

mod bravesearch;
mod brightdata;
mod exa;
mod parallel;
pub mod plugin;
mod serpapi;
mod serpbase;
mod serper;
pub mod store;

use plugin::{PluginDef, PluginStore};

/// Doctor needs the token/zone split for the free zone probe;
/// re-export it without widening the adapter's item visibility.
pub(crate) fn brightdata_key_parts(key: &str) -> Result<(String, String), String> {
    brightdata::parse_key(key)
}
mod tavily;
mod tinyfish;

use std::time::{Duration, Instant};

use store::{ByokStore, KeyState, KeyState as KS};

use crate::search::intent::Intent;
use crate::search::rank::Merged;
use crate::search::{EngineReport, SearchOutcome};

/// A single search result from any provider.
#[derive(Debug)]
pub(crate) struct SearchHit {
    title: String,
    url: String,
    snippet: String,
    score: f32,
}

/// A successful provider result: hits, wall-clock ms, and the
/// provider's own degraded flag (plugins can report degradation).
#[derive(Debug)]
pub(crate) struct ProviderOutcome {
    pub hits: Vec<SearchHit>,
    pub ms: u64,
    pub degraded: bool,
}

type ProviderResult = Result<ProviderOutcome, KeyError>;

/// Error classification for key state management.
/// Each variant maps to a key state transition.
#[derive(Debug)]
pub(crate) enum KeyError {
    /// 401/403 : key is wrong or revoked. Permanent death.
    InvalidKey,
    /// 402 or billing message : no credits. Dead until user resets.
    CreditDepleted,
    /// 429 : too many requests. Auto-recovers after cooldown.
    RateLimited,
    /// 5xx : server problem. No state change, try next key.
    ServerError(String),
    /// Network timeout/refused. No state change, try next key.
    NetworkError,
    /// Anything else. No state change, try next key.
    UnknownError(String),
}

impl KeyError {
    fn to_key_state(&self) -> Option<KeyState> {
        match self {
            Self::InvalidKey => Some(KS::Invalid),
            Self::CreditDepleted => Some(KS::CreditDepleted),
            Self::RateLimited => Some(KS::RateLimited),
            Self::ServerError(_) | Self::NetworkError | Self::UnknownError(_) => None,
        }
    }

    /// The one place a transport failure becomes a KeyError.
    ///
    /// `reqwest::Error`'s Display includes the full request URL,
    /// query string and all. SerpApi's API takes the key as
    /// `?api_key=`, so rendering that error verbatim put the whole
    /// key into `last_error`, and from there into the MCP search
    /// error the model sees, the CLI's stderr and the DONSEEK_DEBUG
    /// log on any non-timeout transport failure (DNS, refused, TLS).
    /// The URL adds nothing here anyway: the provider name is
    /// prepended by the caller and the endpoint is a constant.
    pub(crate) fn from_transport(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::NetworkError
        } else {
            Self::UnknownError(format!("network: {}", e.without_url()))
        }
    }
}

/// Cap on how much of an upstream error body is echoed into a
/// `KeyError`. A `KeyError` reaches the model (the search error the
/// agent sees), stderr, and the DONSEEK_DEBUG log, so echoing a raw
/// multi-MB provider error page is both a context-budget hit and,
/// for a provider that carries the key in the request URL (serpapi),
/// a reflection surface: an intermediary that bounces the request
/// line into its 4xx page would otherwise leak `?api_key=...` into
/// all three sinks. 600 chars is enough to diagnose.
const ERR_BODY_CAP: usize = 600;

/// Bound an upstream error body before it lands in a `KeyError`.
/// Every provider adapter routes its `HTTP {status}: <body>` echo
/// through this so no single adapter can drift back to an uncapped
/// echo (two capped, seven did not — the drift this centralizes).
pub(super) fn err_body(text: &str) -> String {
    text.chars().take(ERR_BODY_CAP).collect()
}

/// Parse a provider response body with the diagnostics the inline
/// `parse error: {e}` used to drop (#286). Three outcomes, each
/// named: an empty body (plausible upstream behaviour for several
/// providers, which used to surface as a bare JSON "EOF" error), a
/// malformed body (the message carries the HTTP status and the byte
/// count), and a valid parse.
pub(super) fn parse_provider_json(status: u16, text: &str) -> Result<serde_json::Value, KeyError> {
    if text.trim().is_empty() {
        return Err(KeyError::UnknownError(format!(
            "empty body at HTTP {status}"
        )));
    }
    serde_json::from_str(text).map_err(|e| {
        KeyError::UnknownError(format!(
            "parse error at HTTP {status}, {} bytes: {e}",
            text.len()
        ))
    })
}

/// The error when no (provider, key) pair was usable at all. Held
/// to one clause with no comma or colon after the prefix, so
/// `compact_failure` carries it whole into the degraded line.
const NO_USABLE_KEY: &str =
    "all keys exhausted: no usable key (all invalid or depleted or cooling down)";

/// A one-line summary of a BYOK exhaustion error for the visible
/// degraded trail (#285): "brightdata parse error at HTTP 200" on the
/// search line, where the full diagnostic would not fit. The shape is
/// built by `search()` ("all keys exhausted: <provider>: <detail>" or
/// "all providers exhausted after N attempts: ..."); anything
/// unrecognized passes through, bounded, so a future error never
/// disappears from the trail.
pub(crate) fn compact_failure(err: &str) -> String {
    let rest = err
        .strip_prefix("all keys exhausted: ")
        .or_else(|| {
            err.split_once(" attempts: ")
                .filter(|(head, _)| head.starts_with("all providers exhausted"))
                .map(|(_, rest)| rest)
        })
        .unwrap_or(err);
    let (provider, detail) = match rest.split_once(": ") {
        Some((p, d)) => (p, d),
        None => ("", rest),
    };
    // A plugin failure's detail repeats its own name ("plugin
    // <name>: exited with status 1"); drop the echo.
    let detail = detail
        .strip_prefix(&format!("plugin {provider}: "))
        .unwrap_or(detail);
    let short: String = detail
        .split([',', ';'])
        .next()
        .unwrap_or(detail)
        .trim()
        .chars()
        .take(60)
        .collect();
    if short.is_empty() {
        // An empty status is the one shape a reader cannot act on.
        return "no usable key".to_string();
    }
    if provider.is_empty() {
        short
    } else {
        format!("{provider} {short}")
    }
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidKey => write!(f, "invalid key"),
            Self::CreditDepleted => write!(f, "credit depleted"),
            Self::RateLimited => write!(f, "rate limited"),
            Self::ServerError(s) => write!(f, "server error: {s}"),
            Self::NetworkError => write!(f, "network error"),
            Self::UnknownError(s) => write!(f, "{s}"),
        }
    }
}

/// A provider failure, normalized for the caller: the typed error
/// the key-state machine acts on, plus the text a human reads.
///
/// The parked variants (`InvalidKey`, `CreditDepleted`,
/// `RateLimited`) carry no payload, which is right for a native
/// adapter (its `invalid key` came from a 401 the caller never
/// sees) and wrong for a plugin, whose whole report is the words it
/// printed, `401 Unauthorized: API key revoked`. Carrying the text
/// here is what keeps those words in `keys add plugin --test` and
/// in the search debug log, where a bare `invalid key` told the
/// user nothing about whether the credentials or the quota failed.
#[derive(Debug)]
pub(crate) struct ProviderFailure {
    pub key: KeyError,
    pub detail: String,
}

impl ProviderFailure {
    /// A failure whose text is what its variant already renders,
    /// which is exactly what the caller saw before this type
    /// existed (every native adapter path, and a plugin's own
    /// malformed-output failures).
    pub(super) fn of(key: KeyError) -> Self {
        let detail = key.to_string();
        Self { key, detail }
    }

    /// A failure that carries text of its own, because the variant
    /// cannot hold it.
    pub(super) fn new(key: KeyError, detail: String) -> Self {
        Self { key, detail }
    }
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Max attempts before giving up: prevents infinite loops if
/// every key is transient-failing (server errors). Each attempt
/// either consumes a key (marks it dead) or is transient (same
/// key retried). We cap total attempts to avoid hammering a
/// downed provider.
const MAX_ATTEMPTS: usize = 20;

/// BYOK searcher: holds the key store, plugin store and HTTP client.
pub struct ByokSearcher {
    store: ByokStore,
    plugins: PluginStore,
    client: reqwest::Client,
}

impl Default for ByokSearcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ByokSearcher {
    /// Load from disk. If no keys or plugins are configured,
    /// search() returns Err("not configured") and the caller
    /// falls back to local.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(2)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            store: ByokStore::new(),
            plugins: PluginStore::new(),
            client,
        }
    }

    /// True if at least one keyed provider or plugin exists.
    pub fn is_configured(&self) -> bool {
        // Search-scoped: a store holding only fetch-side keys
        // (unlocker) is not a configured search (#284).
        self.store.has_search_providers() || self.plugins.is_configured()
    }

    /// True if "local" is the default search method.
    pub fn is_local_default(&self) -> bool {
        self.store.is_local_default()
    }

    /// Reload config from disk (picks up CLI key changes live).
    pub fn reload(&self) {
        self.store.reload();
        self.plugins.reload();
    }

    /// Search via the provider chain. Falls back through keys
    /// and providers. Returns Err if all keys/providers exhausted.
    pub(crate) async fn search(
        &self,
        query: &str,
        max_results: usize,
        forced_intent: Option<Intent>,
    ) -> Result<SearchOutcome, String> {
        // Reload to pick up any CLI key changes since daemon start.
        self.reload();

        if !self.is_configured() {
            return Err("no BYOK keys configured".to_string());
        }

        let intent = forced_intent.unwrap_or_else(|| crate::search::intent::detect(query));
        let max = max_results.clamp(1, 12);
        let started = Instant::now();

        let mut attempts = 0;
        let mut last_error = String::new();
        // Track keys we've already tried this call. Transient
        // errors (5xx, network) don't change key state, so
        // pick_key() would return the same key again : infinite
        // loop without this set.
        let mut tried: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        // One lane per attempted provider, in walk order, so `_meta`
        // answers "which providers were tried, and how did each one
        // end" rather than only "who answered". A caller could not
        // tell "Tavily is the default" from "Serper was asked first
        // and came back empty", and a monitor built on the report
        // paged a provider change that never happened (#253).
        let mut walk: Vec<EngineReport> = Vec::new();

        loop {
            attempts += 1;
            if attempts > MAX_ATTEMPTS {
                return Err(format!(
                    "all providers exhausted after {MAX_ATTEMPTS} attempts: {last_error}"
                ));
            }

            // Pick the next usable (provider, key) pair,
            // skipping any we've already tried this call.
            // Plugins participate with a synthetic key equal to
            // their name: one attempt per plugin per call, like
            // every transient-failing provider.
            let (provider, key, plugin_def) = match self.pick_any_skipping(&tried) {
                Some(pk) => pk,
                None => {
                    // Nothing was pickable on the first pass: every
                    // key is invalid, depleted or cooling down and
                    // every plugin is striking. `last_error` is still
                    // empty here, and the bare prefix rendered as an
                    // empty engine status ("byok: ") in the degraded
                    // trail, the one case #285 exists for.
                    if last_error.is_empty() {
                        return Err(NO_USABLE_KEY.to_string());
                    }
                    return Err(format!("all keys exhausted: {last_error}"));
                }
            };
            tried.insert((provider.clone(), key.clone()));
            let is_plugin = plugin_def.is_some();

            // Dispatch to the provider adapter. Both arms
            // normalize to `ProviderFailure`: a plugin brings its own
            // words, a native adapter's text is what its variant
            // already renders. Timed here rather than inside the
            // adapter so a walked-past lane reports the wall time it
            // cost this call, whether it failed or came back empty.
            let attempt_started = Instant::now();
            let result: Result<ProviderOutcome, ProviderFailure> = match plugin_def {
                Some(def) => plugin::run_plugin(&provider, &def, query, max, &intent).await,
                None => dispatch(&self.client, &provider, &key, query, max, &intent)
                    .await
                    .map_err(ProviderFailure::of),
            };
            let attempt_ms = attempt_started.elapsed().as_millis() as u64;

            match result {
                Ok(outcome) => {
                    if outcome.hits.is_empty() {
                        // Provider returned 0 results : don't
                        // return an empty list to the agent.
                        // Try the next provider, and if all are
                        // empty, fall back to local search.
                        last_error = format!("{provider}: empty results");
                        if crate::config::cfg().debug.search {
                            crate::diag!("[byok] {provider} returned 0 results, trying next");
                        }
                        walk.push(EngineReport {
                            cache_age_ms: None,
                            engine: provider.clone(),
                            profile: None,
                            status: "empty".into(),
                            hits: 0,
                            ms: attempt_ms,
                            egress: "byok".into(),
                            requests: 0,
                        });
                        continue;
                    }
                    let mut hits = outcome.hits;
                    resolve_redirect_hits(&mut hits).await;
                    let results = to_merged(hits, &provider, max);
                    let mut report = std::mem::take(&mut walk);
                    report.push(EngineReport {
                        cache_age_ms: None,
                        engine: provider.clone(),
                        profile: None,
                        status: if outcome.degraded {
                            "degraded".into()
                        } else {
                            "ok".into()
                        },
                        hits: results.len(),
                        ms: outcome.ms,
                        egress: "byok".into(),
                        requests: 0,
                    });
                    let quality =
                        super::quality::assess(query, &results, &report, &Default::default());
                    return Ok(SearchOutcome {
                        results,
                        intent,
                        report,
                        cached: false,
                        quality,
                        cache_age: None,
                        elapsed: started.elapsed(),
                        provider: Some(provider),
                        // Provider-ranked, not cross-encoder-ranked.
                        reranked: false,
                        // Providers do not expose a byte-derived
                        // SERP instant layer.
                        instant: None,
                        requests: None,
                        stage_ms: Vec::new(),
                    });
                }
                Err(failure) => {
                    let shown = &failure.detail;
                    // Log the error for debugging.
                    if crate::config::cfg().debug.search {
                        crate::diag!(
                            "[byok] {provider} key={}... {shown}",
                            key.chars().take(8).collect::<String>(),
                        );
                    }

                    last_error = format!("{provider}: {shown}");

                    // The lane this attempt occupied, named by how it
                    // actually ended: a rate-limited key or plugin is
                    // the transient case, everything else here is an
                    // error the caller can see in `_meta`.
                    walk.push(EngineReport {
                        cache_age_ms: None,
                        engine: provider.clone(),
                        profile: None,
                        status: match failure.key.to_key_state() {
                            Some(KS::RateLimited) => "rate_limited",
                            _ => "error",
                        }
                        .into(),
                        hits: 0,
                        ms: attempt_ms,
                        egress: "byok".into(),
                        requests: 0,
                    });

                    // Update key state if this is a key-level error.
                    // A plugin's credentials live inside the plugin,
                    // so its state is recorded against the plugin
                    // rather than against a key we hold.
                    if let Some(new_state) = failure.key.to_key_state() {
                        if is_plugin {
                            self.plugins.mark_state(&provider, new_state);
                        } else {
                            self.store.update_key_state(&provider, &key, new_state);
                        }
                    }

                    // Transient errors (server, network) don't mark
                    // the key dead : but we still try the next key
                    // to avoid getting stuck on a flaky provider.
                    // The loop continues to pick_key().
                }
            }
        }
    }

    /// Combine the two lookups: Try a plugin named as default
    /// first (they live outside the keyed provider chain), then
    /// fall back to keyed providers (default-first), then
    /// remaining plugins in registration order. A plugin that
    /// reported itself invalid, out of credit or rate-limited is
    /// skipped, the same way pick_key_skipping skips such a key.
    fn pick_any_skipping(
        &self,
        tried: &std::collections::HashSet<(String, String)>,
    ) -> Option<(String, String, Option<PluginDef>)> {
        let snap = self.plugins.snapshot();
        let default = self.store.current_default();
        if !default.is_empty() && default != "local" {
            let pair = (default.clone(), default.clone());
            if let Some(def) = snap.plugins.get(&default).cloned()
                && !tried.contains(&pair)
                && self.plugins.is_usable(&default)
            {
                return Some((default.clone(), default, Some(def)));
            }
        }
        if let Some((provider, key)) = self.store.pick_key_skipping(tried) {
            return Some((provider, key, None));
        }
        for name in snap.names() {
            if *name == default {
                continue;
            }
            let pair = (name.clone(), name.clone());
            if tried.contains(&pair) {
                continue;
            }
            if let Some(def) = snap.plugins.get(name).cloned()
                && self.plugins.is_usable(name)
            {
                return Some((name.clone(), name.clone(), Some(def)));
            }
        }
        None
    }
}

/// Dispatch to the right provider adapter.
async fn dispatch(
    client: &reqwest::Client,
    provider: &str,
    key: &str,
    query: &str,
    max: usize,
    intent: &Intent,
) -> ProviderResult {
    match provider {
        "tavily" => tavily::search(client, key, query, max, intent).await,
        "exa" => exa::search(client, key, query, max, intent).await,
        "serper" => serper::search(client, key, query, max, intent).await,
        "serpapi" => serpapi::search(client, key, query, max, intent).await,
        "serpbase" => serpbase::search(client, key, query, max, intent).await,
        "bravesearch" => bravesearch::search(client, key, query, max, intent).await,
        "tinyfish" => tinyfish::search(client, key, query, max, intent).await,
        "parallel" => parallel::search(client, key, query, max, intent).await,
        "brightdata" => brightdata::search(client, key, query, max, intent).await,
        _ => Err(KeyError::UnknownError(format!(
            "unknown provider: {provider}"
        ))),
    }
}

/// Max concurrent cookie-less `/goto` 302 probes per BYOK search.
const GOTO_CONCURRENCY: usize = 4;
/// Per-request timeout for one `/goto` redirect probe.
const GOTO_REQ_TIMEOUT: Duration = Duration::from_millis(2500);
/// Whole-batch budget for resolving opaque `/goto` links in one search.
const GOTO_BATCH_BUDGET: Duration = Duration::from_secs(4);

/// Build a cookie-less, redirect-disabled HTTP client for resolving one
/// opaque `/goto` link (honors DonSeTch's proxy configuration).
fn goto_client_for(url: &str) -> Option<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(GOTO_REQ_TIMEOUT)
        .no_proxy();
    match crate::egress::proxy::from_env_for(url) {
        Ok(Some(proxy)) => {
            if proxy.scheme == crate::egress::proxy::ProxyScheme::Socks5 {
                return None;
            }
            builder = builder.proxy(reqwest::Proxy::all(proxy.wire_url()).ok()?);
        }
        Ok(None) => {}
        Err(_) => return None,
    }
    builder.build().ok()
}

/// Outcome of one cookie-less `/goto` 302 lookup.
enum GotoOutcome {
    /// Clean 3xx `Location` pointing at a fetchable page URL.
    Resolved(String),
    /// Upstream refused/throttled (403/429/503) — trip batch stop.
    Refused,
    /// Timeout, transport error, non-3xx, or unusable `Location`.
    Unresolved,
}

/// Resolve one opaque redirect URL with a single cookie-less, no-Referer,
/// non-following GET and validate the returned `Location` target.
async fn resolve_one_goto(client: &reqwest::Client, raw: &str) -> GotoOutcome {
    let resp = match client.get(raw).send().await {
        Ok(r) => r,
        Err(_) => return GotoOutcome::Unresolved,
    };
    let status = resp.status().as_u16();
    if matches!(status, 403 | 429 | 503) {
        return GotoOutcome::Refused;
    }
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return GotoOutcome::Unresolved;
    }
    let Some(location) = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
    else {
        return GotoOutcome::Unresolved;
    };
    let Ok(base) = url::Url::parse(raw) else {
        return GotoOutcome::Unresolved;
    };
    let Ok(resolved) = base.join(location.trim()) else {
        return GotoOutcome::Unresolved;
    };
    let candidate = crate::search::contract::decode_redirect(resolved.as_str())
        .unwrap_or_else(|| resolved.to_string());
    if crate::search::contract::url_class(&candidate) != crate::search::contract::UrlClass::Page {
        return GotoOutcome::Unresolved;
    }
    GotoOutcome::Resolved(candidate)
}

/// Unwrap local redirect wrappers and resolve opaque Google `/goto?url=…`
/// links returned by signed-out Google SERP providers (e.g. Bright Data, #456).
///
/// Only runs network requests when at least one hit is a Google `/goto` link.
/// Every request is cookie-less, sends no `Referer`, does not follow the
/// redirect, respects `[rules.url]`, runs with bounded concurrency and a tight
/// deadline, and stops the batch immediately on HTTP 403/429/503.
async fn resolve_redirect_hits(hits: &mut [SearchHit]) {
    resolve_redirect_hits_with(hits, None, crate::search::contract::is_google_goto).await;
}

async fn resolve_redirect_hits_with<F>(
    hits: &mut [SearchHit],
    override_client: Option<reqwest::Client>,
    should_resolve: F,
) where
    F: Fn(&str) -> bool,
{
    for hit in hits.iter_mut() {
        if let Some(decoded) = crate::search::contract::decode_redirect(&hit.url) {
            hit.url = decoded;
        }
    }
    let check_url_policy = override_client.is_none();
    let targets: Vec<(usize, String)> = hits
        .iter()
        .enumerate()
        .filter(|(_, h)| should_resolve(&h.url))
        .filter(|(_, h)| {
            !check_url_policy || crate::fetch::guards::validate_url_basic(&h.url).is_ok()
        })
        .map(|(i, h)| (i, h.url.clone()))
        .collect();
    if targets.is_empty() {
        return;
    }
    let client = match override_client {
        Some(c) => c,
        None => match goto_client_for(&targets[0].1) {
            Some(c) => c,
            None => return,
        },
    };
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(GOTO_CONCURRENCY));
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut join = tokio::task::JoinSet::new();
    for (idx, url) in targets {
        let client = client.clone();
        let sem = std::sync::Arc::clone(&sem);
        let stopped = std::sync::Arc::clone(&stopped);
        join.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return (idx, GotoOutcome::Unresolved);
            };
            if stopped.load(std::sync::atomic::Ordering::Acquire) {
                return (idx, GotoOutcome::Unresolved);
            }
            let outcome = resolve_one_goto(&client, &url).await;
            if matches!(outcome, GotoOutcome::Refused) {
                stopped.store(true, std::sync::atomic::Ordering::Release);
            }
            (idx, outcome)
        });
    }
    let deadline = tokio::time::Instant::now() + GOTO_BATCH_BUDGET;
    while let Ok(Some(res)) = tokio::time::timeout_at(deadline, join.join_next()).await {
        if let Ok((idx, GotoOutcome::Resolved(url))) = res {
            hits[idx].url = url;
        }
    }
    join.abort_all();
}

/// Convert provider hits to the Merged format used by the
/// local search pipeline. Each hit gets a single source
/// (the provider name) with its score. Unwraps local
/// redirectors, deduplicates by normalized URL, and filters
/// empty titles/URLs.
fn to_merged(hits: Vec<SearchHit>, provider: &str, max: usize) -> Vec<Merged> {
    use crate::search::rank::norm_key;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    hits.into_iter()
        .map(|mut h| {
            if let Some(decoded) = crate::search::contract::decode_redirect(&h.url) {
                h.url = decoded;
            }
            h
        })
        .filter(|h| !h.url.is_empty() && !h.title.trim().is_empty())
        .filter(|h| seen.insert(norm_key(&h.url)))
        .take(max)
        .enumerate()
        .map(|(i, h)| Merged {
            title: h.title,
            url: h.url,
            snippet: h.snippet,
            score: h.score as f64,
            sources: vec![(provider.to_string(), i + 1)],
            published: None,
            families: 0,
            from_page: false,
            tier: 3,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_error_to_state_mapping() {
        assert_eq!(KeyError::InvalidKey.to_key_state(), Some(KS::Invalid));
        assert_eq!(
            KeyError::CreditDepleted.to_key_state(),
            Some(KS::CreditDepleted)
        );
        assert_eq!(KeyError::RateLimited.to_key_state(), Some(KS::RateLimited));
        assert_eq!(KeyError::ServerError("500".into()).to_key_state(), None);
        assert_eq!(KeyError::NetworkError.to_key_state(), None);
        assert_eq!(KeyError::UnknownError("x".into()).to_key_state(), None);
    }

    // A real transport error from a refused loopback connect, with the
    // key where SerpApi's API puts it (the query string). The raw
    // reqwest error renders the full URL, so this is exactly the path
    // that used to put the key into the model-visible search error.
    #[tokio::test]
    async fn transport_errors_never_carry_the_request_url() {
        const KEY: &str = "SUPERSECRETKEY123";
        // no_proxy: a reachable HTTP_PROXY in the environment would
        // turn the refused connect into a 502 *response*.
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        let err = client
            .get("http://127.0.0.1:1/search")
            .query(&[("q", "hello"), ("api_key", KEY)])
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert!(!err.is_timeout(), "connection refused, not a timeout");
        // Sanity: the unredacted error really does carry the key,
        // otherwise this test proves nothing.
        assert!(err.to_string().contains(KEY));
        let mapped = KeyError::from_transport(err);
        assert!(
            !mapped.to_string().contains(KEY),
            "key leaked into KeyError: {mapped}"
        );
        assert!(matches!(mapped, KeyError::UnknownError(_)));
    }

    // Every adapter echoes an upstream error body through err_body,
    // which caps it at 600 chars. Without the cap a burned proxy's
    // multi-MB HTML error page landed verbatim in the model context,
    // stderr and the debug log (a context DoS), and for serpapi
    // specifically it could carry the URL-borne api_key.
    #[test]
    fn err_body_caps_the_echoed_error() {
        let huge = "x".repeat(10_000);
        let capped = err_body(&huge);
        assert_eq!(capped.chars().count(), ERR_BODY_CAP);
        // Multibyte input is capped on CHARS, never mid-codepoint.
        let cjk = "東".repeat(10_000);
        let out = err_body(&cjk);
        assert_eq!(out.chars().count(), ERR_BODY_CAP);
        assert!(out.chars().all(|c| c == '東'));
        // A short body is returned intact.
        assert_eq!(err_body("HTTP 429 slow down"), "HTTP 429 slow down");
    }

    // serpapi puts the key in the request URL, so a reflecting
    // intermediary can bounce it into the error body. The adapter
    // scrubs the exact key substring before it is echoed; this pins
    // the scrub-then-cap the adapter relies on.
    #[test]
    fn serpapi_style_key_scrub_removes_the_reflected_key() {
        const KEY: &str = "sk-serpapi-SECRET-9f8e7d";
        let reflected =
            format!("<html>Bad request to /search?q=x&api_key={KEY} : forbidden</html>");
        let echoed = err_body(&reflected).replace(KEY, "<redacted>");
        assert!(!echoed.contains(KEY), "the key must not survive: {echoed}");
        assert!(echoed.contains("<redacted>"));
    }

    #[test]
    fn to_merged_preserves_order_and_scores() {
        let hits = vec![
            SearchHit {
                title: "A".into(),
                url: "https://a.com".into(),
                snippet: "sa".into(),
                score: 0.9,
            },
            SearchHit {
                title: "B".into(),
                url: "https://b.com".into(),
                snippet: "sb".into(),
                score: 0.5,
            },
        ];
        let merged = to_merged(hits, "tavily", 10);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].title, "A");
        assert!((merged[0].score - 0.9).abs() < 0.01);
        assert_eq!(merged[0].sources.len(), 1);
        assert_eq!(merged[0].sources[0].0, "tavily");
    }

    #[test]
    fn to_merged_deduplicates_urls() {
        let hits = vec![
            SearchHit {
                title: "A".into(),
                url: "https://a.com".into(),
                snippet: "s".into(),
                score: 0.9,
            },
            SearchHit {
                title: "A2".into(),
                url: "https://www.a.com/".into(),
                snippet: "s".into(),
                score: 0.8,
            },
            SearchHit {
                title: "B".into(),
                url: "https://b.com".into(),
                snippet: "s".into(),
                score: 0.5,
            },
        ];
        // a.com and www.a.com/ normalize to the same key
        let merged = to_merged(hits, "tavily", 10);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].title, "A");
        assert_eq!(merged[1].title, "B");
    }

    #[test]
    fn to_merged_filters_empty_titles() {
        let hits = vec![
            SearchHit {
                title: "".into(),
                url: "https://a.com".into(),
                snippet: "s".into(),
                score: 0.9,
            },
            SearchHit {
                title: "  \n".into(),
                url: "https://b.com".into(),
                snippet: "s".into(),
                score: 0.8,
            },
            SearchHit {
                title: "Real".into(),
                url: "https://c.com".into(),
                snippet: "s".into(),
                score: 0.5,
            },
        ];
        let merged = to_merged(hits, "exa", 10);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title, "Real");
    }

    #[test]
    fn to_merged_respects_max() {
        let hits: Vec<SearchHit> = (0..5)
            .map(|i| SearchHit {
                title: format!("T{i}"),
                url: format!("https://t{i}.com"),
                snippet: "s".into(),
                score: 1.0 - i as f32 * 0.1,
            })
            .collect();
        let merged = to_merged(hits, "exa", 3);
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn to_merged_caps_after_dedup() {
        // #164: duplicate-heavy provider output must still deliver the
        // requested number of unique results: dedup runs BEFORE the
        // cap, so duplicates cannot eat the max budget. (The old
        // parse-time truncate let plugins return fewer unique hits
        // than requested.)
        let hits = vec![
            SearchHit {
                title: "a".into(),
                url: "https://a.com".into(),
                snippet: "s".into(),
                score: 1.0,
            },
            SearchHit {
                title: "dup of a".into(),
                url: "https://a.com/".into(), // trailing slash = same norm_key
                snippet: "s".into(),
                score: 0.9,
            },
            SearchHit {
                title: "b".into(),
                url: "https://b.com".into(),
                snippet: "s".into(),
                score: 0.8,
            },
        ];
        let merged = to_merged(hits, "exa", 2);
        assert_eq!(merged.len(), 2, "dedup must not shrink the max budget");
        assert_eq!(merged[0].title, "a");
        assert_eq!(
            merged[1].title, "b",
            "second slot goes to the next unique hit"
        );
    }

    // ── plugin state, end to end ───────────────────────────────
    // The unit tests cover the state machine and the parser. These
    // cover the wiring: a plugin that reports itself dead must stop
    // being spawned, and must still be skipped after a restart, since
    // the reload at the start of every search is what the whole
    // design leans on.

    #[cfg(unix)]
    static CACHE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// A throwaway cache dir with `DONSETCH_CACHE_DIR` pointed at it.
    /// `cache_dir()` reads the env per call on purpose, and nextest
    /// runs one process per test, so this is race-free here.
    #[cfg(unix)]
    fn throwaway_cache(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "donsetch-byok-{tag}-{}-{}",
            std::process::id(),
            CACHE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("DONSETCH_CACHE_DIR", &dir) };
        dir
    }

    #[cfg(unix)]
    fn spawn_count(counter: &std::path::Path) -> usize {
        std::fs::read_to_string(counter)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Register one `/bin/sh` plugin that logs every spawn and answers
    /// the given envelope on stdout with a non-zero exit.
    #[cfg(unix)]
    fn register_counter_plugin(dir: &std::path::Path, envelope: &str) -> std::path::PathBuf {
        let counter = dir.join("spawns.log");
        let mut cfg = plugin::PluginConfig::empty();
        cfg.add(
            "counted",
            vec![
                "/bin/sh".into(),
                "-c".into(),
                format!(
                    "echo x >> {}; echo '{}'; exit 1",
                    counter.display(),
                    envelope
                ),
            ],
            10_000,
            &std::collections::HashSet::new(),
        )
        .unwrap();
        cfg.save();
        counter
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_parked_plugin_is_not_spawned_again() {
        let dir = throwaway_cache("park");
        let counter = register_counter_plugin(
            &dir,
            r#"{"format":1,"error":"401 Unauthorized: API key revoked","error_kind":"invalid_key"}"#,
        );

        let first = ByokSearcher::new();
        let _ = first.search("q", 5, Some(Intent::Web)).await;
        assert_eq!(spawn_count(&counter), 1, "the first search spawns it once");

        // A fresh searcher is the daemon-restart path: the parked state
        // has to come back off disk, so this also proves the search
        // itself reads it rather than trusting one process's memory.
        let second = ByokSearcher::new();
        let _ = second.search("q", 5, Some(Intent::Web)).await;
        assert_eq!(
            spawn_count(&counter),
            1,
            "a plugin that reported invalid_key must not be spawned again"
        );

        let on_disk = std::fs::read_to_string(dir.join("plugins.json")).unwrap();
        assert!(
            on_disk.contains("\"invalid\""),
            "the state must persist: {on_disk}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_legacy_plugin_is_still_spawned_on_every_search() {
        // The compatibility half: an envelope with no error_kind
        // records no state, so a plugin written before this contract
        // keeps behaving exactly as it did.
        let dir = throwaway_cache("legacy");
        let counter = register_counter_plugin(
            &dir,
            r#"{"format":1,"error":"upstream is down","retryable":true}"#,
        );

        let searcher = ByokSearcher::new();
        for _ in 0..3 {
            let _ = searcher.search("q", 5, Some(Intent::Web)).await;
        }
        assert_eq!(
            spawn_count(&counter),
            3,
            "a legacy plugin is tried again on every search"
        );
        let on_disk = std::fs::read_to_string(dir.join("plugins.json")).unwrap();
        assert!(
            on_disk.contains("\"active\""),
            "a legacy envelope must record no state: {on_disk}"
        );
    }

    /// Register plugins from `(name, sh -c script)` pairs, in order.
    #[cfg(unix)]
    fn register_plugins(plugins: &[(&str, &str)]) {
        let mut cfg = plugin::PluginConfig::empty();
        for (name, script) in plugins {
            cfg.add(
                name,
                vec!["/bin/sh".into(), "-c".into(), (*script).to_string()],
                10_000,
                &std::collections::HashSet::new(),
            )
            .unwrap();
        }
        cfg.save();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_report_names_every_provider_walked_past() {
        // #253: `_meta` listed only the provider that answered, so a
        // caller could not tell "second is the default" from "first
        // was asked first and came back empty", and a monitor built
        // on the report paged a provider change that never happened.
        let _dir = throwaway_cache("walk-empty");
        register_plugins(&[
            ("first", r#"echo '{"format":1,"results":[]}'"#),
            (
                "second",
                r#"echo '{"format":1,"results":[{"title":"t","url":"https://example.com/a"}]}'"#,
            ),
        ]);

        let outcome = ByokSearcher::new()
            .search("q", 5, Some(Intent::Web))
            .await
            .expect("the second provider answers");

        assert_eq!(outcome.provider.as_deref(), Some("second"));
        assert_eq!(outcome.report.len(), 2, "both attempts are lanes");
        assert_eq!(outcome.report[0].engine, "first");
        assert_eq!(outcome.report[0].status, "empty");
        assert_eq!(outcome.report[0].hits, 0);
        assert_eq!(outcome.report[0].egress, "byok");
        assert_eq!(outcome.report[1].engine, "second");
        assert_eq!(outcome.report[1].status, "ok");
        assert_eq!(outcome.report[1].hits, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_walked_past_provider_is_named_by_how_it_failed() {
        let _dir = throwaway_cache("walk-error");
        register_plugins(&[
            (
                "throttled",
                r#"echo '{"format":1,"error":"429 too many requests","error_kind":"rate_limited"}'; exit 1"#,
            ),
            (
                "second",
                r#"echo '{"format":1,"results":[{"title":"t","url":"https://example.com/b"}]}'"#,
            ),
        ]);

        let outcome = ByokSearcher::new()
            .search("q", 5, Some(Intent::Web))
            .await
            .expect("the second provider answers");

        assert_eq!(outcome.report.len(), 2);
        assert_eq!(outcome.report[0].engine, "throttled");
        assert_eq!(
            outcome.report[0].status, "rate_limited",
            "a throttled provider is the transient case, not a generic error"
        );
        assert_eq!(outcome.report[0].hits, 0);
        assert_eq!(outcome.report[1].status, "ok");
    }

    // #286: the helpers keep what the inline code dropped.
    #[test]
    fn parse_provider_json_names_empty_and_malformed_bodies() {
        let v = parse_provider_json(200, "{\"ok\":1}").expect("valid json");
        assert_eq!(v["ok"], 1);
        let e = parse_provider_json(200, "").unwrap_err();
        assert!(e.to_string().contains("empty body at HTTP 200"), "{e}");
        let e = parse_provider_json(200, "<html>nope</html>").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("parse error at HTTP 200"), "{msg}");
        assert!(msg.contains("17 bytes"), "{msg}");
    }

    // #285: the degraded line reads "<provider> <reason>".
    #[test]
    fn compact_failure_shapes_one_line() {
        assert_eq!(
            compact_failure("all keys exhausted: brightdata: parse error"),
            "brightdata parse error"
        );
        assert_eq!(
            compact_failure(
                "all keys exhausted: serper: parse error at HTTP 200, 0 bytes: EOF while parsing a value"
            ),
            "serper parse error at HTTP 200"
        );
        assert_eq!(
            compact_failure(
                "all providers exhausted after 20 attempts: tavily: HTTP 500: upstream"
            ),
            "tavily HTTP 500: upstream"
        );
        assert_eq!(
            compact_failure("all keys exhausted: serper: empty results"),
            "serper empty results"
        );
        // A plugin detail that echoes its own name is trimmed, and
        // an inner "status: N" survives (only commas/semicolons
        // bound the clause).
        assert_eq!(
            compact_failure(
                "all keys exhausted: badplugin: plugin badplugin: exited with status exit status: 1"
            ),
            "badplugin exited with status exit status: 1"
        );
    }

    // A store whose every key is invalid, depleted or cooling down
    // reaches the exhausted arm before any attempt, so there is no
    // last error to quote. The line still has to say why: an empty
    // "byok: " status is the only report status that carries no word.
    #[test]
    fn an_exhausted_store_with_no_attempt_still_names_the_reason() {
        assert_eq!(
            compact_failure(NO_USABLE_KEY),
            "no usable key (all invalid or depleted or cooling down)"
        );
        // The bare prefix (the pre-fix shape) never yields an empty status.
        assert_eq!(compact_failure("all keys exhausted: "), "no usable key");
        assert_eq!(compact_failure(""), "no usable key");
    }

    // #456: Bright Data / Google SERP opaque `/goto` 302 links resolve
    // cookie-lessly without following the redirect, while invalid/non-page
    // targets and rate-limited replies keep the original URL cleanly.
    #[tokio::test]
    async fn goto_redirect_resolution_unwraps_302_without_cookies_or_referer() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let mut reader = BufReader::new(socket);
                let mut first_line = String::new();
                if reader.read_line(&mut first_line).await.unwrap_or(0) == 0 {
                    continue;
                }
                let mut headers = Vec::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line.trim().is_empty()
                    {
                        break;
                    }
                    headers.push(line.to_ascii_lowercase());
                }
                assert!(
                    !headers.iter().any(|h| h.starts_with("cookie:")),
                    "must never send cookies on /goto: {headers:?}"
                );
                assert!(
                    !headers.iter().any(|h| h.starts_with("referer:")),
                    "must never send referer on /goto: {headers:?}"
                );
                let reply = if first_line.contains("url=ok1") {
                    "HTTP/1.1 302 Found\r\nLocation: https://docs.rs/tokio/latest/tokio/macro.select.html\r\nSet-Cookie: NID=tracking; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if first_line.contains("url=dup1") {
                    "HTTP/1.1 302 Found\r\nLocation: https://docs.rs/tokio/latest/tokio/macro.select.html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if first_line.contains("url=badloc") {
                    "HTTP/1.1 302 Found\r\nLocation: https://www.google.com/goto?url=loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = reader.get_mut().write_all(reply.as_bytes()).await;
            }
        });

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .unwrap();
        let mut hits = vec![
            SearchHit {
                title: "Tokio select!".into(),
                url: format!("http://{addr}/goto?url=ok1"),
                snippet: "macro".into(),
                score: 1.0,
            },
            SearchHit {
                title: "Duplicate via goto".into(),
                url: format!("http://{addr}/goto?url=dup1"),
                snippet: "dup".into(),
                score: 0.5,
            },
            SearchHit {
                title: "Nested redirector".into(),
                url: format!("http://{addr}/goto?url=badloc"),
                snippet: "loop".into(),
                score: 0.33,
            },
            SearchHit {
                title: "Wrapped /url?q=".into(),
                url: "https://www.google.com/url?q=https%3A%2F%2Fusers.rust-lang.org%2Ft%2F1"
                    .into(),
                snippet: "local decode".into(),
                score: 0.2,
            },
        ];

        resolve_redirect_hits_with(&mut hits, Some(client.clone()), |u| {
            u.contains("/goto?url=")
        })
        .await;

        assert_eq!(
            hits[0].url,
            "https://docs.rs/tokio/latest/tokio/macro.select.html"
        );
        assert_eq!(
            hits[1].url,
            "https://docs.rs/tokio/latest/tokio/macro.select.html"
        );
        assert_eq!(
            hits[2].url,
            format!("http://{addr}/goto?url=badloc"),
            "nested redirector Location must be refused"
        );
        assert_eq!(
            hits[3].url, "https://users.rust-lang.org/t/1",
            "local /url?q= wrappers must unwrap without network"
        );

        let merged = to_merged(hits, "brightdata", 5);
        assert_eq!(
            merged.len(),
            3,
            "duplicate resolved /goto hits must collapse in to_merged"
        );
        assert_eq!(
            merged[0].url,
            "https://docs.rs/tokio/latest/tokio/macro.select.html"
        );
        assert_eq!(merged[2].url, "https://users.rust-lang.org/t/1");

        // Rate-limited batch preserves the original /goto URLs cleanly.
        let mut rl_hits = vec![SearchHit {
            title: "Rate limited".into(),
            url: format!("http://{addr}/goto?url=rl"),
            snippet: "rl".into(),
            score: 1.0,
        }];
        resolve_redirect_hits_with(&mut rl_hits, Some(client), |u| u.contains("/goto?url=")).await;
        assert_eq!(rl_hits[0].url, format!("http://{addr}/goto?url=rl"));

        server.abort();
    }

    #[tokio::test]
    async fn goto_resolution_respects_url_deny_rules() {
        let mut cfg = crate::config::DonsetchConfig::default();
        cfg.rules.url.insert(
            "google.com".into(),
            crate::rules::UrlRule {
                action: crate::rules::RuleAction::Deny,
                message: Some("no google".into()),
                ..Default::default()
            },
        );
        crate::config::install(cfg).unwrap();

        let original = "https://www.google.com/goto?url=CAES_DENIED".to_string();
        let mut hits = vec![SearchHit {
            title: "Denied".into(),
            url: original.clone(),
            snippet: "s".into(),
            score: 1.0,
        }];
        // Would fail/hang if it attempted a live network call against a dead client;
        // validate_url_basic rejects google.com before any HTTP client is built.
        resolve_redirect_hits(&mut hits).await;
        assert_eq!(hits[0].url, original);
    }
}
