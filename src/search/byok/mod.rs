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
mod serpapi;
mod serpbase;
mod serper;
pub mod store;
mod tavily;
mod tinyfish;

use std::time::{Duration, Instant};

use store::{ByokStore, KeyState, KeyState as KS};

use crate::search::intent::Intent;
use crate::search::rank::Merged;
use crate::search::{EngineReport, SearchOutcome};

/// A single search result from any provider.
struct SearchHit {
    title: String,
    url: String,
    snippet: String,
    score: f32,
}

type ProviderResult = Result<(Vec<SearchHit>, u64), KeyError>;

/// Error classification for key state management.
/// Each variant maps to a key state transition.
#[derive(Debug)]
enum KeyError {
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

/// BYOK searcher: holds the key store and HTTP client.
pub struct ByokSearcher {
    store: ByokStore,
    client: reqwest::Client,
}

impl Default for ByokSearcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ByokSearcher {
    /// Load from disk. If no keys configured, search() returns
    /// Err("not configured") and the caller falls back to local.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(2)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            store: ByokStore::new(),
            client,
        }
    }

    /// True if at least one provider has at least one key.
    pub fn is_configured(&self) -> bool {
        self.store.is_configured()
    }

    /// True if "local" is the default search method.
    pub fn is_local_default(&self) -> bool {
        self.store.is_local_default()
    }

    /// Reload config from disk (picks up CLI key changes live).
    pub fn reload(&self) {
        self.store.reload();
    }

    /// Search via the provider chain. Falls back through keys
    /// and providers. Returns Err if all keys/providers exhausted.
    pub async fn search(
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
            let (provider, key) = match self.store.pick_key_skipping(&tried) {
                Some(pk) => pk,
                None => {
                    return Err(format!("all keys exhausted: {last_error}"));
                }
            };
            tried.insert((provider.clone(), key.clone()));

            // Dispatch to the provider adapter.
            let result = dispatch(&self.client, &provider, &key, query, max, &intent).await;

            match result {
                Ok((hits, ms)) => {
                    if hits.is_empty() {
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
                    let results = to_merged(hits, &provider, max);
                    let report = vec![EngineReport {
                        engine: provider.clone(),
                        status: "ok".into(),
                        hits: results.len(),
                        ms,
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
}
