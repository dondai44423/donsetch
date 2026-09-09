//! Bring Your Own Keys (BYOK) : provider search with key rotation.
//!
//! When the user configures API keys for external search providers
//! (Tavily, Exa, Serper.dev, SerpApi, Brave Search API, TinyFish,
//! Parallel AI, Bright Data), the entire local 5-engine
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
        self.store.is_configured() || self.plugins.is_configured()
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
                    return Err(format!("all keys exhausted: {last_error}"));
                }
            };
            tried.insert((provider.clone(), key.clone()));

            // Dispatch to the provider adapter.
            let result = match plugin_def {
                Some(def) => plugin::run_plugin(&provider, &def, query, max, &intent).await,
                None => dispatch(&self.client, &provider, &key, query, max, &intent).await,
            };

            match result {
                Ok(outcome) => {
                    if outcome.hits.is_empty() {
                        // Provider returned 0 results : don't
                        // return an empty list to the agent.
                        // Try the next provider, and if all are
                        // empty, fall back to local search.
                        last_error = format!("{provider}: empty results");
                        if std::env::var_os("DONSEEK_DEBUG").is_some() {
                            eprintln!("[byok] {provider} returned 0 results, trying next");
                        }
                        continue;
                    }
                    let results = to_merged(outcome.hits, &provider, max);
                    let report = vec![EngineReport {
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
                    }];
                    return Ok(SearchOutcome {
                        results,
                        weak: false,
                        intent,
                        report,
                        cached: false,
                        elapsed: started.elapsed(),
                        provider: Some(provider),
                        // Provider-ranked, not cross-encoder-ranked.
                        reranked: false,
                    });
                }
                Err(key_error) => {
                    // Log the error for debugging.
                    if std::env::var_os("DONSEEK_DEBUG").is_some() {
                        eprintln!(
                            "[byok] {provider} key={}... {}",
                            key.chars().take(8).collect::<String>(),
                            key_error
                        );
                    }

                    last_error = format!("{provider}: {key_error}");

                    // Update key state if this is a key-level error.
                    if let Some(new_state) = key_error.to_key_state() {
                        self.store.update_key_state(&provider, &key, new_state);
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
    /// remaining plugins in registration order.
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
            if let Some(def) = snap.plugins.get(name).cloned() {
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

/// Convert provider hits to the Merged format used by the
/// local search pipeline. Each hit gets a single source
/// (the provider name) with its score. Deduplicates by
/// normalized URL and filters empty titles/URLs.
fn to_merged(hits: Vec<SearchHit>, provider: &str, max: usize) -> Vec<Merged> {
    use crate::search::rank::norm_key;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    hits.into_iter()
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
}
