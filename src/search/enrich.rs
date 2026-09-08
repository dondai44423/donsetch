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

        // Kill switch (v4 phase 2.1): with DONSETCH_NO_PREWARM=1 no
        // prewarm fetch ever goes on the wire, so servers see no
        // burst at all. The honest-off state of the layer.
        if crate::config::env_flag("DONSETCH_NO_PREWARM") {
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
                    // v4 phase 2.1: enrichment rides the shared cookie
                    // jar (browser-real). Fresh-jar enrichment + warm
                    // fetch = two devices from one IP within seconds;
                    // one store, like a browser.
                    fetcher.fetch_once_via(&url, &[], None, true, None),
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
                            return (i, None, Some(String::new()));
                        }
                        // Dead link (4xx/5xx, incl. a 200 dressed as a
                        // soft 404) -> demote.
                        if o.status >= 400 || matches!(o.verdict, Verdict::SoftNotFound) {
                            return (i, None, None);
                        }
                        // Anything else not clean content: live page we
                        // can't enrich cheaply. Neutral.
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
                        // v3 F1: keep the body for the warm handoff, even
                        // when there is no metadata to enrich with.
                        sink.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .put(&url, o.body.clone(), ct);
                        // A live page with no extractable title/desc is
                        // still alive: return the neutral marker, never
                        // the dead-link (None, None) signal.
                        if title.is_none() && desc.is_none() {
                            return (i, None, Some(String::new()));
                        }
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

    /// v4 phase 2.1: detached warm-handoff for the BYOK takeover
    /// path. Provider results already carry real titles/snippets, so
    /// the search reply does not wait for the prefetch pass: it fires
    /// in the background and parks bodies while the model reads the
    /// results. The agent's follow-up fetch of a top URL then hits
    /// RAM. Title/demotion effects of the pass are dropped here by
    /// design (the provider's own ranking stands); parking is the
    /// point. Same kill switch as the inline pass.
    pub fn spawn_prewarm(self: &std::sync::Arc<Self>, results: &[Merged]) {
        if results.is_empty() || crate::config::env_flag("DONSETCH_NO_PREWARM") {
            return;
        }
        let mut owned = results.to_vec();
        let this = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            this.enrich_results(&mut owned).await;
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::BrowserProfile;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    // == v4 phase 2.1 battery: wire truth of the prewarm layer ==
    // Every fact is observed on the real socket; nothing mocked.

    enum RigMode {
        /// 200 + Set-Cookie + a real body.
        Reach,
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
            let body = "<html><head><title>hop-ok</title></head><body>preload-tempitle text description for the wire merge of curiosity.</body></html>";
            let head = match mode {
                RigMode::Reach => format!(
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

    fn merged(url: &str, title: &str) -> Merged {
        Merged {
            title: title.to_string(),
            url: url.to_string(),
            snippet: String::new(),
            sources: Vec::new(),
            score: 1.0,
            published: None,
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
                .body
                .windows(b"hop-ok".len())
                .any(|w| w == b"hop-ok")
        );
        // The shared jar now holds the device cookie.
        let jar = searcher.fetcher.jar_snapshot("127.0.0.1");
        assert!(
            jar.iter().any(|c| c.name == "uid" && c.value == "fleet9"),
            "the enrich fetch must ride the shared jar: {:?}",
            jar
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

    /// Bot walls (200 but not ContentOk) leave the result neutral:
    /// no demotion (still valid for tier 2), no parked body.
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
}
