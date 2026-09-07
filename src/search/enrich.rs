//! Result enrichment: the search→fetch warm handoff. Prefetches the
//! top results to replace SERP snippets with the pages' own title and
//! meta description, demotes dead links, and parks bodies in the
//! `PrewarmCache` so the agent's subsequent `web_fetch` of a top
//! result is served from RAM in one hop.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use scraper::Selector;

use super::Searcher;
use super::rank::Merged;
use crate::detect::walls::Verdict;
use crate::error::FetchError;

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
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub(super) fn put(&mut self, url: &str, body: Vec<u8>, content_type: String) {
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

    /// One-shot: a served prewarm is consumed : the second
    /// fetch of the same URL goes to the network for freshness.
    pub fn take(&mut self, url: &str) -> Option<PrewarmEntry> {
        let e = self.entries.remove(url)?;
        (e.at.elapsed() < PREWARM_TTL).then_some(e)
    }
}

impl Searcher {
    /// Enrich top results by prefetching destination pages.
    ///
    /// Extracts real <title> and <meta name="description">
    /// from the actual page HTML : richer than any SERP
    /// snippet. Dead links (404/timeout) get demoted 50%.
    /// Pages behind bot walls are left untouched (still
    /// valid results, agent fetches via tier 2).
    ///
    /// This is what makes our search better than any
    /// individual engine: results carry the page's own
    /// title and description, not the SERP's truncated
    /// version. Works even when SERP parsers return empty
    /// snippets. Dead links that rank well are demoted.
    pub(super) async fn enrich_results(&self, results: &mut [Merged]) {
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
                    // Outer timeout / transport timeout = a slow but
                    // alive page. Demoting it as dead would punish
                    // anything slow, so stay neutral.
                    Err(_) | Ok(Err(FetchError::Timeout)) => (i, None, Some(String::new())),
                    // Refused / DNS-dead / nothing recovered = dead.
                    Ok(Err(_)) => (i, None, None),
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
                        sink.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .put(&url, o.body.clone(), ct);
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
                    // Dead link : demote 50%.
                    r.score *= 0.5;
                }
                (None, Some(d)) if d.is_empty() => {
                    // Bot wall : leave untouched.
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
