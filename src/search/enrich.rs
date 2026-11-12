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

/// B3 quality observation from one enrich prefetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum QualityObs {
    /// Real content page (parked body, usable metadata).
    Clean,
    /// Dead link / soft-404.
    Dead,
    /// Wall, timeout, or live-but-not-enrichable. Never a quality fact.
    Neutral,
}

/// v3 F1: search→fetch warm handoff store.
pub struct PrewarmCache {
    entries: HashMap<String, PrewarmEntry>,
}

pub struct PrewarmEntry {
    pub outcome: crate::fetch::client::FetchOutcome,
    pub at: Instant,
}

/// The most pages one search reads, for `read` and for snippet repair together:
/// more is the agent fetching pages through search.
const READ_MAX: usize = 3;
const PREWARM_CAP: usize = 10;
const PREWARM_BODY_MAX: usize = 1_500_000;
const PREWARM_TTL: Duration = Duration::from_secs(600);

impl PrewarmCache {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub(super) fn put(&mut self, url: &str, outcome: crate::fetch::client::FetchOutcome) {
        if outcome.body.len() > PREWARM_BODY_MAX {
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
                outcome,
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
    /// Overlap one procedural evidence read with the remaining index requests.
    /// The final ranking is unchanged; only a retained URL may use the excerpt.
    pub(super) async fn with_early_evidence<F>(
        &self,
        query: &str,
        enabled: bool,
        started: Instant,
        work: F,
    ) -> Result<(super::SearchOutcome, Vec<String>), FetchError>
    where
        F: std::future::Future<Output = Result<super::SearchOutcome, FetchError>>,
    {
        if !enabled || super::query::QueryFacts::new(query).shape() != super::query::Shape::HowTo {
            return work.await.map(|out| (out, Vec::new()));
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(sender)));
        let read = async {
            let hit: super::engines::Hit = receiver.await.ok()?;
            let remaining = Duration::from_millis(2200)
                .saturating_sub(started.elapsed())
                .min(Duration::from_millis(1500));
            if remaining.is_zero() {
                return None;
            }
            let mut candidate = vec![Merged {
                title: hit.title,
                url: hit.url,
                snippet: hit.snippet,
                sources: Vec::new(),
                score: 0.0,
                published: hit.published,
                families: 0,
                from_page: false,
                tier: 1,
            }];
            self.read_at(&mut candidate, query, vec![0], remaining)
                .await;
            candidate.pop()
        };
        let (out, preview) = tokio::join!(super::EARLY_PAGE.scope(slot, work), read);
        let mut out = out?;
        let mut attempted = Vec::new();
        if let Some(preview) = preview {
            attempted.push(preview.url.clone());
            if preview.from_page
                && let Some(result) = out
                    .results
                    .iter_mut()
                    .find(|r| super::rank::norm_key(&r.url) == super::rank::norm_key(&preview.url))
            {
                // The read already retained its source's displayed date. Do
                // not prepend a different index's date to that same excerpt.
                result.snippet = preview.snippet;
                result.from_page = true;
            }
        }
        Ok((out, attempted))
    }

    /// Enrich top results by prefetching destination pages.
    ///
    /// Extracts real <title> and <meta name="description">
    /// from the actual page HTML : richer than any SERP
    /// snippet. Dead links (404/timeout) get demoted 50%.
    /// Pages behind bot walls are left untouched (still
    /// valid results, no demotion).
    ///
    /// This is what makes our search better than any
    /// individual engine: results carry the page's own
    /// title and description, not the SERP's truncated
    /// version. Works even when SERP parsers return empty
    /// snippets. Dead links that rank well are demoted.
    pub(super) async fn enrich_results(&self, results: &mut [Merged]) {
        const ENRICH_TOP: usize = 5;
        const ENRICH_TIMEOUT: Duration = Duration::from_secs(4);

        // Kill switch: with prewarm disabled ([fetch]
        // prewarm = false; historically DONSETCH_NO_PREWARM) no prewarm
        // fetch ever goes on the wire, so servers see no burst at all.
        // The honest-off state of the layer.
        if !crate::config::cfg().fetch.prewarm {
            return;
        }
        let n = results.len().min(ENRICH_TOP);
        if n == 0 {
            return;
        }

        // Spawn parallel fetches for top N results.
        let fetcher = &self.fetcher;
        type EnrichFut<'a> = std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = (usize, Option<String>, Option<String>, QualityObs),
                    > + Send
                    + 'a,
            >,
        >;
        let prewarms = self.prewarms.clone();
        let mut futures: Vec<EnrichFut> = Vec::new();
        for (i, r) in results.iter().take(n).enumerate() {
            let url = r.url.clone();
            let sink = prewarms.clone();
            futures.push(Box::pin(async move {
                // The prewarm rides the ambient proxy like every other
                // request path. Behind a forward proxy that is the only
                // route out, a direct dial here failed and the result was
                // scored QualityObs::Dead: live search results got demoted
                // as dead links because of the egress, not the page.
                let route = crate::egress::request_route::RequestRoute::configured();
                let env_proxy = match route.proxy_for(&url) {
                    Ok(proxy) => proxy,
                    // An unusable client route is not evidence that the page died.
                    Err(_) => return (i, None, Some(String::new()), QualityObs::Neutral),
                };
                let out = tokio::time::timeout(
                    ENRICH_TIMEOUT,
                    // enrichment rides the shared cookie
                    // jar (browser-real). Fresh-jar enrichment + warm
                    // fetch = two devices from one IP within seconds;
                    // one store, like a browser.
                    fetcher.fetch_once_via(&url, &[], env_proxy.as_ref(), true, None),
                )
                .await;
                match out {
                    // Outer timeout / transport timeout = a slow but
                    // alive page. Demoting it as dead would punish
                    // anything slow, so stay neutral.
                    Err(_) => (i, None, Some(String::new()), QualityObs::Neutral),
                    Ok(Err(e)) if neutral_failure(&e) => {
                        (i, None, Some(String::new()), QualityObs::Neutral)
                    }
                    // A local rule refused the host: the same shape a bot
                    // wall gets. The score stays, no quality is recorded
                    // and the hit keeps its SERP title and snippet. A deny
                    // is a policy fact about the operator, not a fact
                    // about the page, so it must neither halve the score
                    // nor teach the persisted host-quality map that the
                    // host is bad (that would outlive the rule). A later
                    // version may add a per-rule knob that lets a deny
                    // demote search hits; v1 never lets a rule affect
                    // search ranking.
                    Ok(Err(crate::error::FetchError::Denied { .. })) => {
                        (i, None, Some(String::new()), QualityObs::Neutral)
                    }
                    // A proxy the route needs could not be used (an
                    // unreadable proxy URL, or a hop the route proxies
                    // from a read that left direct): a routing fact, not
                    // a page fact, so the hit keeps its score and no
                    // quality is recorded, as for a deny.
                    Ok(Err(crate::error::FetchError::ProxyConfig(_))) => {
                        (i, None, Some(String::new()), QualityObs::Neutral)
                    }
                    // Refused / DNS-dead / nothing recovered = dead.
                    Ok(Err(_)) => (i, None, None, QualityObs::Dead),
                    Ok(Ok(mut o)) => {
                        o.route = route;
                        // Wall-family verdicts first: a gated page is
                        // alive, never a dead link. Challenge walls
                        // answer 403/429 (cf-mitigated), so the
                        // status>=400 dead-link gate must not run ahead
                        // of this or every walled result gets wrongly
                        // demoted (the v4 2.1 enrich wire battery caught
                        // it). No enrich, no demote: a wall is a routing
                        // fact, not a relevance fact.
                        if matches!(
                            o.verdict,
                            Verdict::Challenge(_)
                                | Verdict::AuthWall
                                | Verdict::Paywall
                                | Verdict::Blocked
                        ) {
                            return (i, None, Some(String::new()), QualityObs::Neutral);
                        }
                        // Dead link (4xx/5xx, incl. a 200 dressed as a
                        // soft 404) -> demote.
                        if o.status >= 400 || matches!(o.verdict, Verdict::SoftNotFound) {
                            return (i, None, None, QualityObs::Dead);
                        }
                        // Anything else not clean content: live page we
                        // can't enrich cheaply. Neutral.
                        if !matches!(o.verdict, Verdict::ContentOk) {
                            return (i, None, Some(String::new()), QualityObs::Neutral);
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
                        // v3 F1: keep the body for the warm handoff, even
                        // when there is no metadata to enrich with.
                        sink.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .put(&url, o);
                        // A live page with no extractable title/desc is
                        // still alive: return the neutral marker, never
                        // the dead-link (None, None) signal.
                        if title.is_none() && desc.is_none() {
                            return (i, None, Some(String::new()), QualityObs::Clean);
                        }
                        (i, title, desc, QualityObs::Clean)
                    }
                }
            }));
        }

        // Batch deadline: the enrich pass never holds the SERP
        // hostage past its budget. Pages that finish inside the
        // window enrich and score; stragglers are dropped and the
        // SERP snippet stays as the fallback. Without this one slow
        // top page inflated every cold search by up to its whole
        // 4s per-item timeout while every other result was ready.
        const ENRICH_BATCH_DEADLINE_MS: u64 = 700;
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(ENRICH_BATCH_DEADLINE_MS);
        let mut set: futures_util::stream::FuturesUnordered<_> = futures.into_iter().collect();
        let mut enriched: Vec<(usize, Option<String>, Option<String>, QualityObs)> = Vec::new();
        while let Ok(Some(out)) =
            tokio::time::timeout_at(deadline, futures_util::StreamExt::next(&mut set)).await
        {
            enriched.push(out);
        }

        for (i, title, desc, obs) in enriched {
            if i >= results.len() {
                continue;
            }
            // B3: density from real enrich outcomes. Walls and
            // timeouts never enter the map.
            match obs {
                QualityObs::Clean => {
                    let host = super::rank::host_of(&results[i].url);
                    self.observe_quality(&host, true);
                }
                QualityObs::Dead => {
                    let host = super::rank::host_of(&results[i].url);
                    self.observe_quality(&host, false);
                }
                QualityObs::Neutral => {}
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

        // Persist quality only when something was recorded.
        self.save_quality_if_dirty();

        // Re-sort after enrichment (dead links demoted).
        results.sort_by(|a, b| {
            a.tier
                .cmp(&b.tier)
                .then_with(|| b.score.total_cmp(&a.score))
        });
    }

    /// `read: k`: fetch the top `k` results in parallel inside the call's
    /// budget and replace each teaser with the passage of the page that answers
    /// the query (`evidence::passage`). A page that cannot be read (a wall, a
    /// timeout, a non-document, nothing that speaks to the query) keeps its
    /// teaser: reading never demotes, reorders or removes a result. Bodies are
    /// parked for the follow-up `web_fetch` unless prewarm is switched off.
    pub async fn read_results(&self, results: &mut [Merged], query: &str, k: usize) {
        let n = results.len().min(k.min(READ_MAX));
        self.read_at(results, query, (0..n).collect(), Duration::from_secs(4))
            .await;
    }

    /// Repair stubs and a procedural/promising result. An unreadable guide may
    /// fall back to one independent publisher within the same shared budgets.
    pub(super) async fn repair_default_snippets(
        &self,
        results: &mut [Merged],
        query: &str,
        already: usize,
        timeout: Duration,
        attempted: &[String],
    ) {
        let remaining_pages = READ_MAX.saturating_sub(already + attempted.len());
        if remaining_pages == 0 {
            return;
        }
        let allow_guide = already == 0 && !results.iter().take(READ_MAX).any(|r| r.from_page);
        let howto = allow_guide
            && super::query::QueryFacts::new(query).shape() == super::query::Shape::HowTo;
        let available = |i: usize| {
            !attempted
                .iter()
                .any(|url| same_publisher(url, &results[i].url))
        };
        let mut indices: Vec<usize> = (already..results.len().min(READ_MAX))
            .filter(|&i| {
                available(i)
                    && !results[i].from_page
                    && super::evidence::is_stub(&results[i].snippet)
            })
            .take(remaining_pages)
            .collect();
        let guide = (already..results.len().min(READ_MAX)).find(|&i| {
            allow_guide
                && indices.len() < remaining_pages
                && available(i)
                && !results[i].from_page
                && !indices.contains(&i)
                && (howto || super::evidence::is_teaser(&results[i].snippet))
        });
        if let Some(i) = guide {
            indices.push(i);
        }
        indices.sort_unstable();
        let deadline = tokio::time::Instant::now() + timeout;
        self.read_at(results, query, indices.clone(), timeout).await;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if guide.is_none()
            || remaining.is_zero()
            || indices.len() >= remaining_pages
            || indices.iter().any(|&i| results[i].from_page)
        {
            return;
        }
        // A blocked publisher must not monopolize the evidence attempt. Try one
        // other ranked publisher, never an alternate endpoint of the same site,
        // inside the original deadline and the remaining page/HTTP allowance.
        let tried: Vec<_> = indices
            .iter()
            .map(|&i| results[i].url.as_str())
            .chain(attempted.iter().map(String::as_str))
            .collect();
        if let Some(i) = (already..results.len().min(READ_MAX)).find(|&i| {
            let host = super::rank::host_of(&results[i].url);
            !indices.contains(&i)
                && !results[i].from_page
                && !host.is_empty()
                && !tried.iter().any(|url| same_publisher(url, &results[i].url))
        }) {
            self.read_at(results, query, vec![i], remaining).await;
        }
    }

    /// Repair provider stubs inside the explicit read's remaining page budget.
    pub async fn repair_snippets(&self, results: &mut [Merged], query: &str, already: usize) {
        let stubs = (already..results.len().min(READ_MAX))
            .filter(|&i| !results[i].from_page && super::evidence::is_stub(&results[i].snippet))
            .collect();
        self.read_at(results, query, stubs, Duration::from_secs(4))
            .await;
    }

    /// Fetch the results at `indices` in parallel inside the call's budget and
    /// replace each teaser with the page's passage that answers the query.
    async fn read_at(
        &self,
        results: &mut [Merged],
        query: &str,
        indices: Vec<usize>,
        timeout: Duration,
    ) {
        /// Share of the query's topic words a passage must hold to be shown.
        const READ_MIN_COVER: f64 = 0.4;
        if indices.is_empty() {
            return;
        }
        let park = crate::config::cfg().fetch.prewarm;
        let budget = super::stage_budget(timeout, Duration::from_millis(100));
        if budget.is_zero() {
            return;
        }
        let facts = super::query::QueryFacts::new(query);
        let fetcher = &self.fetcher;
        let sink = self.prewarms.clone();
        let mut set: futures_util::stream::FuturesUnordered<_> = indices
            .into_iter()
            .filter_map(|i| results.get(i).map(|r| (i, r)))
            .map(|(i, r)| {
                let url = r.url.clone();
                let title = r.title.clone();
                let facts = &facts;
                let sink = sink.clone();
                let query = query.to_string();
                async move {
                    let route = crate::egress::request_route::RequestRoute::configured();
                    let proxy = route.proxy_for(&url).ok()?;
                    let mut o = fetcher
                        .fetch_once_via(&url, &[], proxy.as_ref(), true, None)
                        .await
                        .ok()?;
                    o.route = route;
                    if o.status >= 400 || !matches!(o.verdict, Verdict::ContentOk) {
                        return None;
                    }
                    if !super::query::permits(facts.compiled(), &title, &o.url) {
                        return None;
                    }
                    let ct = o
                        .headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    // Documents only: a PDF or a binary is the fetch tool's job.
                    if !(ct.is_empty() || ct.contains("html") || ct.starts_with("text/")) {
                        return None;
                    }
                    let body = o.body.clone();
                    let final_url = o.url.clone();
                    let page_url = final_url.clone();
                    let q = query.clone();
                    let passage = tokio::task::spawn_blocking(move || {
                        let opts = crate::extract::ExtractOptions {
                            // Selection sees deeper instructions too; only the
                            // chosen passage reaches the 320-character reply.
                            // Do not pre-filter by query words: Ingredients or
                            // Commands sections often do not repeat the topic.
                            max_chars: Some(64_000),
                            ..Default::default()
                        };
                        let ex = crate::extract::extract(&body, &ct, &page_url, &opts).ok()?;
                        super::evidence::passage(&q, &ex.markdown, super::evidence::PASSAGE_CHARS)
                    })
                    .await
                    .ok()
                    .flatten();
                    if park {
                        sink.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .put(&url, o);
                    }
                    passage
                        .filter(|p| {
                            super::contract::veto(
                                facts,
                                &super::engines::Hit {
                                    title: title.clone(),
                                    url: final_url.clone(),
                                    snippet: p.clone(),
                                    rank: i,
                                    published: None,
                                },
                            )
                            .is_none()
                        })
                        .map(|p| (i, p))
                }
            })
            .collect();
        let deadline = tokio::time::Instant::now() + budget;
        while let Ok(Some(done)) =
            tokio::time::timeout_at(deadline, futures_util::StreamExt::next(&mut set)).await
        {
            if let Some((i, passage)) = done
                && let Some(r) = results.get_mut(i)
            {
                // Instructions need not repeat all the words in an introduction.
                // A procedural code/list passage can replace it with less
                // overlap; every replacement still keeps the coverage floor.
                let after = super::evidence::coverage(query, &passage);
                let instructions =
                    facts.shape() == super::query::Shape::HowTo && passage.contains('\n');
                if after >= READ_MIN_COVER
                    && (super::evidence::is_stub(&r.snippet)
                        || super::evidence::is_teaser(&r.snippet)
                        || instructions
                        || after >= super::evidence::coverage(query, &r.snippet))
                {
                    r.snippet = super::evidence::dated_passage(&r.snippet, &passage);
                    r.from_page = true;
                }
            }
        }
    }

    /// detached warm-handoff for the BYOK takeover
    /// path. Provider results already carry real titles/snippets, so
    /// the search reply does not wait for the prefetch pass: it fires
    /// in the background and parks bodies while the model reads the
    /// results. The agent's follow-up fetch of a top URL then hits
    /// RAM. Title/demotion effects of the pass are dropped here by
    /// design (the provider's own ranking stands); parking is the
    /// point. Same kill switch as the inline pass.
    pub fn spawn_prewarm(self: &std::sync::Arc<Self>, results: &[Merged]) {
        if results.is_empty() || !crate::config::cfg().fetch.prewarm {
            return;
        }
        let mut owned = results.to_vec();
        let this = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            this.enrich_results(&mut owned).await;
        });
    }
}

fn same_publisher(a: &str, b: &str) -> bool {
    let a = super::rank::host_of(a);
    let b = super::rank::host_of(b);
    !a.is_empty()
        && !b.is_empty()
        && (a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}")))
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

/// A transport error that says "slow", not "dead": the connect timed
/// out, or the resolver did not answer in time. #248 split the
/// resolver timeout out of `Timeout` into `DnsTimeout`, and this leg
/// still matched `Timeout` alone, so a 5s resolver blip during enrich
/// demoted a live result 50% and marked its host's quality down.
fn neutral_failure(e: &FetchError) -> bool {
    matches!(
        e,
        FetchError::Timeout | FetchError::SlowTransfer(_) | FetchError::DnsTimeout(_)
    ) || matches!(e, FetchError::Http(status) if status.starts_with("budget:") || status.starts_with("rested:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::BrowserProfile;

    #[test]
    fn a_resolver_timeout_is_neutral_failure() {
        assert!(neutral_failure(&FetchError::Timeout));
        assert!(neutral_failure(&FetchError::DnsTimeout(
            "the resolver did not answer within 5s".into()
        )));
        assert!(neutral_failure(&FetchError::Http(
            "budget: request ceiling reached".into()
        )));
        assert!(neutral_failure(&FetchError::Http("rested:900".into())));
        assert!(!neutral_failure(&FetchError::Dns("no such host".into())));
        assert!(!neutral_failure(&FetchError::Http(
            "connection refused".into()
        )));
    }
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    // == battery: wire truth of the prewarm layer ==
    // Every fact is observed on the real socket; nothing mocked.

    enum RigMode {
        /// 200 + Set-Cookie + a real body.
        Reach,
        /// Actual instructions follow more than 2,000 bytes of topical prose.
        Instructions,
        /// 403 + cf-mitigated: challenge (a wall).
        Wall,
        /// 404 (a dead link).
        Dead,
    }

    fn serve(mode: RigMode) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = sock.set_read_timeout(Some(Duration::from_secs(6)));
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut raw = String::new();
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if line == "\r\n" {
                    break;
                }
                raw.push_str(&line);
            }
            let body = if matches!(&mode, RigMode::Instructions) {
                let background = (0..40).map(|i| format!("<p>Orion sensor calibration background {i} describes the apparatus and why accurate measurements matter before proceeding to the actual installation procedure.</p>")).collect::<String>();
                format!(
                    "<html><head><title>How to calibrate Orion sensor</title></head><body><article><h1>How to calibrate Orion sensor</h1><p>Learn how to calibrate the Orion sensor in this guide.</p>{background}<h2>Commands</h2><pre><code>toolctl --zero 42\ntoolctl --verify</code></pre></article></body></html>"
                )
            } else {
                "<html><head><title>hop-ok</title></head><body>preload-tempitle text description for the wire merge of curiosity.</body></html>".to_string()
            };
            let head = match mode {
                RigMode::Reach | RigMode::Instructions => format!(
                    "HTTP/1.1 200 OK\r\nSet-Cookie: uid=fleet9; Path=/; Max-Age=86400\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ),
                RigMode::Wall => "HTTP/1.1 403 Forbidden\r\ncf-mitigated: challenge\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                RigMode::Dead => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
            };
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body.as_bytes());
            let _ = tx.send(raw);
        });
        (port, rx)
    }

    fn searcher() -> crate::search::Searcher {
        crate::search::Searcher::new(
            crate::fetch::client::Fetcher::new(BrowserProfile::host_default()).unwrap(),
            crate::search::egress::EgressPool::new(vec![]),
        )
    }

    /// A result whose teaser is "serp", served by the rig on `port`.
    fn teased(port: u16) -> Vec<Merged> {
        let mut m = merged(&format!("http://127.0.0.1:{port}/p"), "t");
        m.snippet = "serp".into();
        vec![m]
    }

    fn merged(url: &str, title: &str) -> Merged {
        Merged {
            title: title.to_string(),
            url: url.to_string(),
            snippet: String::new(),
            sources: Vec::new(),
            score: 1.0,
            published: None,
            families: 0,
            from_page: false,
            tier: 0,
        }
    }

    /// The prewarm burst rides the shared cookie jar (browser-real)
    /// and park bodies in the store for the agent's next fetch:
    /// the enrich fetch learns a device cookie; a SECOND
    /// enrichment fetch to the SAME HOST (fresh listener) replays
    /// it, exactly like a returning browser.
    #[tokio::test]
    async fn prewarm_rides_the_jar_and_serves_the_body() {
        unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
        let first = serve(RigMode::Reach);
        let rig1 = format!("http://127.0.0.1:{}/hop", first.0);
        let searcher = searcher();
        let mut hits = vec![merged(
            &rig1,
            "Search title that is long enough to be replaced",
        )];
        searcher.enrich_results(&mut hits).await;
        first
            .1
            .recv_timeout(Duration::from_secs(5))
            .expect("first leg hit the rig");
        //Ram handoff: the body PARKS for one hop.
        let entry = searcher
            .prewarms()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(&rig1);
        assert!(entry.is_some(), "body parked for one hop");
        assert!(
            entry
                .unwrap()
                .outcome
                .body
                .windows(b"hop-ok".len())
                .any(|w| w == b"hop-ok")
        );
        // The shared jar now holds the device cookie.
        let jar = searcher.fetcher.cookie_names("127.0.0.1");
        assert!(
            jar.iter().any(|name| name == "uid"),
            "the enrich fetch must ride the shared jar: {jar:?}"
        );
        // Second leg: a NEW listener on an ephemeral port; the cookie
        // keys on the host (127.0.0.1), not the port: it must ride.
        let (port2, rx2) = serve(RigMode::Reach);
        let rig2 = format!("http://127.0.0.1:{port2}/hop");
        let mut hits2 = vec![merged(&rig2, "Wire ride probe")];
        searcher.enrich_results(&mut hits2).await;
        let raw = rx2
            .recv_timeout(Duration::from_secs(4))
            .expect("second leg reached");
        // The enrich leg must replay the cookie leg 1 set. Our h1
        // transport emits lowercase header names, so match
        // case-insensitively.
        let raw_lc = raw.to_ascii_lowercase();
        assert!(
            raw_lc.contains("cookie: uid=fleet9"),
            "the enrich fetch must replay the device cookie: {raw}"
        );
    }

    /// The kill switch keeps the wire silent: DONSETCH_NO_PREWARM=1
    /// means the prewarm layer makes ZERO requests, and the merged
    /// scores are untouched.
    #[tokio::test]
    async fn kill_switch_keeps_the_wire_silent() {
        unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
        unsafe { std::env::set_var("DONSETCH_NO_PREWARM", "1") };
        let (port, rx) = serve(RigMode::Reach);
        let searcher = searcher();
        let mut hits = vec![merged(&format!("http://127.0.0.1:{port}/hop"), "serp")];
        searcher.enrich_results(&mut hits).await;
        // Zero requests reached the server.
        assert!(
            rx.recv_timeout(Duration::from_millis(900)).is_err(),
            "no prewarm fetch may go on the wire under the kill switch"
        );
        // Score untouched: the layer did not demote or promote.
        assert_eq!(hits[0].score, 1.0);
        // Nothing parked by a killed layer.
        assert!(
            searcher
                .prewarms()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(&hits[0].url)
                .is_none(),
            "a killed prewarm cannot leave an entry behind"
        );
        unsafe { std::env::remove_var("DONSETCH_NO_PREWARM") };
    }

    /// `read` replaces the teaser with the page's own passage, marks it, parks the
    /// body for the follow-up fetch, and leaves score and order alone.
    #[tokio::test]
    async fn read_replaces_the_teaser_with_the_pages_passage() {
        unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
        let (port, rx) = serve(RigMode::Reach);
        let searcher = searcher();
        let mut hits = vec![merged(&format!("http://127.0.0.1:{port}/page"), "serp")];
        searcher
            .read_results(&mut hits, "wire merge curiosity", 1)
            .await;
        rx.recv_timeout(Duration::from_secs(4))
            .expect("the page was fetched");
        assert!(hits[0].from_page, "the snippet is marked as the page's");
        assert!(
            hits[0].snippet.contains("wire merge of curiosity"),
            "{}",
            hits[0].snippet
        );
        assert_eq!(hits[0].score, 1.0, "reading never moves a result");
        assert!(
            searcher
                .prewarms()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(&hits[0].url)
                .is_some(),
            "the body is parked for the follow-up fetch"
        );
        let (port, _) = serve(RigMode::Instructions);
        let mut deep = vec![merged(
            &format!("http://127.0.0.1:{port}/instructions"),
            "serp",
        )];
        deep[0].snippet = "Calibrate Orion sensor safely remotely using proven principles and background information.".into();
        searcher
            .read_results(
                &mut deep,
                "how to calibrate Orion sensor safely remotely",
                1,
            )
            .await;
        assert!(
            deep[0].from_page && deep[0].snippet.contains("--zero 42\ntoolctl --verify"),
            "{}",
            deep[0].snippet
        );
    }

    /// A page that does not speak to the query, a wall, a dead link and `k` = 0
    /// all leave the teaser exactly as the search returned it.
    #[tokio::test]
    async fn read_leaves_the_teaser_when_there_is_nothing_to_quote() {
        unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
        let searcher = searcher();
        let (port, _rx) = serve(RigMode::Reach);
        let mut off = teased(port);
        searcher
            .read_results(&mut off, "kubernetes ingress controller", 1)
            .await;
        assert_eq!((off[0].snippet.as_str(), off[0].from_page), ("serp", false));

        let (port, _rx) = serve(RigMode::Reach);
        let mut excluded = teased(port);
        searcher
            .read_results(&mut excluded, "wire merge -curiosity", 1)
            .await;
        assert_eq!(
            (excluded[0].snippet.as_str(), excluded[0].from_page),
            ("serp", false),
            "a page read must not introduce an excluded term"
        );

        for mode in [RigMode::Wall, RigMode::Dead] {
            let (port, _rx) = serve(mode);
            let mut hits = teased(port);
            searcher
                .read_results(&mut hits, "wire merge curiosity", 1)
                .await;
            assert_eq!((hits[0].snippet.as_str(), hits[0].score), ("serp", 1.0));
            assert!(!hits[0].from_page);
        }

        let (port, rx) = serve(RigMode::Reach);
        let mut none = teased(port);
        searcher
            .read_results(&mut none, "wire merge curiosity", 0)
            .await;
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "read 0 makes no request"
        );
        for independent_publisher in [false, true] {
            let (blocked, _) = serve(RigMode::Wall);
            let (alternative, received) = serve(RigMode::Reach);
            let mut first = merged(&format!("http://127.0.0.1:{blocked}/guide"), "First guide");
            first.snippet = "Learn how to wire merge curiosity in this guide.".into();
            let host = if independent_publisher {
                "localhost"
            } else {
                "127.0.0.1"
            };
            let mut second = merged(
                &format!("http://{host}:{alternative}/article"),
                "Other article",
            );
            second.snippet = "A separate page with a useful description of the technique.".into();
            let mut results = vec![first, second];
            searcher
                .repair_default_snippets(
                    &mut results,
                    "wire merge curiosity",
                    0,
                    Duration::from_secs(2),
                    &[],
                )
                .await;
            assert!(!results[0].from_page);
            assert_eq!(
                results[1].from_page, independent_publisher,
                "only another publisher may supply fallback evidence"
            );
            assert_eq!(
                received.try_recv().is_ok(),
                independent_publisher,
                "a refusal never triggers another endpoint on that publisher"
            );
        }
    }

    /// Dead links (404) are honestly demoted 50%, and nothing gets
    /// parked for them.
    #[tokio::test]
    async fn dead_link_demotes_and_does_not_park() {
        unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
        let (port, rx) = serve(RigMode::Dead);
        let searcher = searcher();
        let url = format!("http://127.0.0.1:{}/dead", port);
        let mut hits = vec![merged(&url, "dead")];
        searcher.enrich_results(&mut hits).await;
        rx.recv_timeout(Duration::from_secs(4))
            .expect("dead leg was reached");
        assert!(
            (hits[0].score - 0.5).abs() < f64::EPSILON,
            "dead link demotes 50%: got {}",
            hits[0].score
        );
        assert!(
            searcher
                .prewarms()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(&url)
                .is_none(),
            "a 404 never parks a body"
        );
    }

    /// A hit whose page redirects to a host the configured route sends
    /// through a proxy, from a read that left direct, is refused with
    /// ProxyConfig. That is a routing fact: the hit keeps its score and no
    /// host quality is recorded. Sets process-wide proxy variables, so it
    /// relies on nextest's process per test.
    #[tokio::test]
    async fn a_redirect_the_route_refuses_is_neither_dead_nor_recorded() {
        unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
        // Nothing listens here: a direct dial of the hop is refused.
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let read_head = |sock: &std::net::TcpStream| -> String {
            let _ = sock.set_read_timeout(Some(Duration::from_secs(4)));
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if line == "\r\n" {
                    break;
                }
            }
            first.trim_end().to_string()
        };
        // The proxy answers every request with a page and logs its request line.
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        let through = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        {
            let through = std::sync::Arc::clone(&through);
            std::thread::spawn(move || {
                for sock in proxy.incoming() {
                    let Ok(mut sock) = sock else { break };
                    let head = read_head(&sock);
                    through.lock().unwrap().push(head);
                    let body = "<html><head><title>Through the proxy: the real page title of this search result</title><meta name=\"description\" content=\"A description long enough to be taken as the snippet of this search result, from the proxy.\"></head><body><p>Readable text of the page.</p></body></html>";
                    let _ = sock.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                }
            });
        }
        // The hit's page: a redirect to localhost, which NO_PROXY does not name.
        let page = TcpListener::bind("127.0.0.1:0").unwrap();
        let page_port = page.local_addr().unwrap().port();
        let page_heads = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        {
            let page_heads = std::sync::Arc::clone(&page_heads);
            std::thread::spawn(move || {
                for sock in page.incoming() {
                    let Ok(mut sock) = sock else { break };
                    let head = read_head(&sock);
                    page_heads.lock().unwrap().push(head);
                    let _ = sock.write_all(
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: http://localhost:{dead}/page\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                }
            });
        }
        unsafe {
            for name in [
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
                "all_proxy",
                "http_proxy",
                "no_proxy",
                "REQUEST_METHOD",
            ] {
                std::env::remove_var(name);
            }
            std::env::set_var("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}"));
            std::env::set_var("NO_PROXY", "127.0.0.1");
        }
        let searcher = searcher();
        let url = format!("http://127.0.0.1:{page_port}/start");
        let mut hits = vec![merged(&url, "search title long enough to be replaced")];
        let before = hits[0].score;
        searcher.enrich_results(&mut hits).await;
        std::thread::sleep(Duration::from_millis(200));
        let quality = searcher
            .quality
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&crate::search::persist::quality_host_key("127.0.0.1"))
            .copied();
        let parked = searcher
            .prewarms()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(&url)
            .is_some();
        assert!(
            (hits[0].score - before).abs() < f64::EPSILON,
            "a routing refusal must not demote the hit: {} -> {}",
            before,
            hits[0].score
        );
        assert!(
            quality.is_none(),
            "a routing refusal must not record host quality"
        );
        assert!(!parked, "nothing was read, so nothing is parked");
        assert!(
            through.lock().unwrap().is_empty(),
            "the refused hop must not reach the proxy: {:?}",
            through.lock().unwrap()
        );
        assert_eq!(
            page_heads.lock().unwrap().len(),
            1,
            "the hit's own page was read once"
        );
    }

    /// The prewarm burst must ride the ambient proxy like every other
    /// request path. Behind a forward proxy that is the only route out, a
    /// direct dial here failed and the result was scored
    /// QualityObs::Dead: live search results were demoted to dead links
    /// because of the egress, not because of the page.
    #[tokio::test]
    async fn prewarm_rides_the_env_proxy() {
        unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
        // A port with nothing behind it stands in for "no direct route":
        // bind and drop, so the number is free but the dial is refused.
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        // The rig plays the proxy: a plaintext http proxy hop arrives as an
        // absolute-form GET, which is what it records.
        let (port, rx) = serve(RigMode::Reach);
        unsafe {
            std::env::set_var("HTTP_PROXY", format!("http://127.0.0.1:{port}"));
            std::env::set_var("ALL_PROXY", format!("http://127.0.0.1:{port}"));
            std::env::set_var("NO_PROXY", "");
            std::env::set_var("no_proxy", "");
        }
        let searcher = searcher();
        let url = format!("http://127.0.0.1:{dead}/page");
        let mut hits = vec![merged(&url, "search title long enough to be replaced")];
        searcher.enrich_results(&mut hits).await;
        let raw = rx
            .recv_timeout(Duration::from_secs(6))
            .expect("the prewarm must reach the proxy instead of dialing direct");
        assert!(
            raw.starts_with(&format!("GET http://127.0.0.1:{dead}/page")),
            "the prewarm must go out in absolute form through the proxy: {raw}"
        );
        // And the leg is a real body fetch, not a Dead verdict.
        assert!(
            searcher
                .prewarms()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(&url)
                .is_some(),
            "the body must still park for the agent's next fetch"
        );
        unsafe {
            std::env::remove_var("HTTP_PROXY");
            std::env::remove_var("ALL_PROXY");
        }
    }

    /// Bot walls (200 but not ContentOk) leave the result neutral:
    /// no demotion (still a valid result), no parked body.
    #[tokio::test]
    async fn wall_leg_neutral_no_park() {
        unsafe { std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1") };
        let (port, rx) = serve(RigMode::Wall);
        let searcher = searcher();
        let url = format!("http://127.0.0.1:{}/wall", port);
        let mut hits = vec![merged(&url, "wall")];
        searcher.enrich_results(&mut hits).await;
        rx.recv_timeout(Duration::from_secs(4))
            .expect("wall leg was reached");
        assert_eq!(
            hits[0].score, 1.0,
            "walls stay neutral: no demote, no promote"
        );
        assert!(
            searcher
                .prewarms()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(&url)
                .is_none(),
            "a wall page never enters the prewarm store"
        );
    }

    /// A config whose rules hold one deny rule keyed `key`.
    fn install_deny_rule(key: &str) {
        let mut config = crate::config::DonsetchConfig::default();
        config.fetch.allow_private_egress = true;
        config.rules.url.insert(
            key.to_string(),
            crate::rules::UrlRule {
                action: crate::rules::RuleAction::Deny,
                message: Some("use another source".to_string()),
                ..Default::default()
            },
        );
        crate::config::install(config).unwrap();
    }

    /// A host a rule denies is never contacted by the prefetch, and the
    /// denial is not a quality fact: the hit keeps its score and its place,
    /// and the learned-quality map records nothing for its host.
    ///
    /// Needs nextest: it installs a process-global config, which a second
    /// test in the same process could not do.
    #[tokio::test]
    async fn denied_hit_keeps_score_and_records_no_quality() {
        install_deny_rule("127.0.0.1");
        let (port, rx) = serve(RigMode::Reach);
        let searcher = searcher();
        let url = format!("http://127.0.0.1:{port}/hop");
        let mut hits = vec![merged(&url, "denied"), merged("http://other.invalid/", "x")];
        hits[1].score = 0.4;
        searcher.enrich_results(&mut hits).await;
        assert!(
            rx.recv_timeout(Duration::from_millis(900)).is_err(),
            "a denied host must not be contacted by the prefetch"
        );
        assert_eq!(hits[0].url, url, "the denied hit keeps its position");
        assert_eq!(hits[0].score, 1.0, "a denial never demotes");
        assert!(
            searcher
                .quality
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&crate::search::persist::quality_host_key("127.0.0.1"))
                .is_none(),
            "a denial must not teach the quality map anything about the host"
        );
        assert!(
            searcher
                .prewarms()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(&url)
                .is_none(),
            "nothing is parked for a denied host"
        );
    }

    /// The negative case: a rule for another host leaves the prefetch of
    /// this one untouched, and its clean fetch is still recorded.
    ///
    /// Needs nextest: it installs a process-global config.
    #[tokio::test]
    async fn rule_for_another_host_leaves_the_prefetch_alone() {
        install_deny_rule("denied.invalid");
        let (port, rx) = serve(RigMode::Reach);
        let searcher = searcher();
        let url = format!("http://127.0.0.1:{port}/hop");
        let mut hits = vec![merged(
            &url,
            "Search title that is long enough to be replaced",
        )];
        searcher.enrich_results(&mut hits).await;
        rx.recv_timeout(Duration::from_secs(5))
            .expect("an allowed host is still prefetched");
        assert!(
            searcher
                .quality
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&crate::search::persist::quality_host_key("127.0.0.1"))
                .is_some(),
            "a clean fetch of an allowed host is still recorded"
        );
    }
}
