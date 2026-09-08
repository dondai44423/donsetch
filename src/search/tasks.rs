//! The fan-out task runners: one future per engine/vertical/lane,
//! all returning `(engine-id, EngineResult)` so the merge loop can
//! do uniform bookkeeping (trust, quarantine, pool health, reports)
//! over every lane shape.

use std::time::Instant;

use super::ENGINE_TIMEOUT;
use super::egress::EgressPool;
use super::engines;
use super::verticals;
use crate::detect::walls::Verdict;
use crate::error::FetchError;
use crate::fetch::client::Fetcher;

pub(super) type EngineResult =
    Result<(Vec<engines::Hit>, u64, String, bool), (String, String, bool)>;

pub(super) type TaskFut<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = (String, EngineResult)> + Send + 'a>>;

#[derive(Clone, Copy)]
pub(super) struct EngineContext<'a> {
    pub fetcher: &'a Fetcher,
    pub pool: &'a EgressPool,
    pub google: &'a engines::google_wml::ProfileSelector,
}

pub(super) async fn engine_task(
    engine: String,
    query: String,
    egress_id: String,
    proxy: Option<crate::transport::proxy::Proxy>,
    context: EngineContext<'_>,
) -> (String, EngineResult) {
    engine_task_with_budget(
        engine,
        query,
        egress_id,
        proxy,
        context,
        ENGINE_TIMEOUT,
        None,
    )
    .await
}

/// One admission and HTTP attempt under a single deadline, including pacing.
pub(super) async fn engine_task_with_budget(
    engine: String,
    query: String,
    egress_id: String,
    proxy: Option<crate::transport::proxy::Proxy>,
    context: EngineContext<'_>,
    budget: std::time::Duration,
    previous: Option<(&str, &str)>,
) -> (String, EngineResult) {
    let EngineContext {
        fetcher,
        pool,
        google,
    } = context;
    let mut label = engine.clone();
    let deadline = tokio::time::Instant::now() + budget;
    let started = Instant::now();
    if tokio::time::timeout_at(deadline, pool.pace(&engine, &egress_id))
        .await
        .is_err()
    {
        return (label, Err(("pacing-timeout".into(), egress_id, true)));
    }
    let lease = if engine == "google" {
        match google.select(&egress_id, previous) {
            Ok(lease) => {
                label = lease.label();
                Some(lease)
            }
            Err(status) => return (label, Err((status.into(), egress_id, true))),
        }
    } else {
        None
    };
    let google_ua = lease.as_ref().map(|lease| lease.user_agent());
    let Some(url) = engines::serp_url(&engine, &query) else {
        return (label, Err(("no-url".into(), egress_id, true)));
    };
    let out = match tokio::time::timeout_at(deadline, async {
        if let Some(ua) = google_ua {
            fetcher
                .fetch_once_via_user_agent(&url, proxy.as_ref(), ua)
                .await
        } else {
            fetcher
                .fetch_once_via(&url, &[], proxy.as_ref(), false, None)
                .await
        }
    })
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
    let html = crate::extract::charset::decode(
        &out.body,
        out.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or(""),
    );
    if engine == "google"
        && let Some(status) = engines::google_wml::response_error(out.status, &out.headers, &html)
    {
        if let Some(lease) = &lease {
            google.finish(lease, status);
        }
        return (label, Err((status.into(), egress_id, true)));
    }
    if out.status == 429 || !matches!(out.verdict, Verdict::ContentOk) {
        return (
            label,
            Err((format!("blocked:{}", out.status), egress_id, true)),
        );
    }
    let hits = engines::parse(&engine, &html);
    if hits.len() < 3 {
        // Honest "no results" is NOT an engine failure :
        // don't burn trust/lanes for a dry query.
        let lower = html.to_lowercase();
        let dry = lower.contains("no results")
            || lower.contains("did not match any")
            || lower.contains("no good results")
            || lower.contains("nothing found");
        let status = if dry { "no-results" } else { "empty-parse" };
        return (label, Err((status.into(), egress_id, true)));
    }
    if let Some(lease) = &lease {
        google.finish(lease, "ok");
    }
    (label, Ok((hits, ms, egress_id, true)))
}

/// The browser-render SERP lane. Runs the SERP URL through the
/// shared ghost hook (render cache shortcut included), parses
/// with the desktop parser (separate from the WML HTTP layout), and
/// reports honestly: "google_ghost" on the engine list, egress
/// "ghost". Health is transport-specific; ranking still counts only
/// one Google index family across HTTP and browser results.
pub(super) async fn ghost_engine_task(
    engine: String,
    query: String,
    hook: crate::crawl::GhostHook,
) -> (String, EngineResult) {
    let started = Instant::now();
    let Some(url) = engines::serp_url("google_ghost", &query) else {
        return (engine, Err(("no-url".into(), "ghost".into(), true)));
    };
    // The hook runs acquire + render + one retry internally,
    // so the budget here covers a completed first attempt plus
    // most of the retry: cutting mid-retry is fine, the first
    // render usually lands inside 15s.
    let rendered = match tokio::time::timeout(std::time::Duration::from_secs(30), hook(url)).await {
        Err(_) => return (engine, Err(("ghost-timeout".into(), "ghost".into(), true))),
        Ok(Err(e)) => {
            let status = if e.contains("captcha") {
                "blocked:captcha"
            } else {
                "ghost-render"
            };
            return (engine, Err((status.into(), "ghost".into(), true)));
        }
        Ok(Ok(r)) => r.html,
    };
    let hits = engines::parse("google_ghost", &rendered);
    let ms = started.elapsed().as_millis() as u64;
    if hits.len() < 3 {
        // 200-but-no-results 2026 Google = bot wall or an AI-mode
        // shell: either way the lane produced nothing usable.
        return (
            engine,
            Err(("blocked:captcha".into(), "ghost".into(), true)),
        );
    }
    (engine, Ok((hits, ms, "ghost".into(), true)))
}

pub(super) async fn vertical_task(
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
        Ok(Ok(hits)) => vertical_success(vertical, hits, started.elapsed().as_millis() as u64),
    }
}

/// Pure outcome mapping for a vertical fetch (#164 S2).
///
/// An empty vertical is a "no results" outcome, not a healthy engine:
/// counting it as Ok would inflate ok_engines and relax the
/// retry/cache gates, labeling degraded searches healthy exactly when
/// retrieval is worst. "no-results" is excluded from retries, trust
/// penalties, and egress fault reporting (is_engine_fault), so this
/// stays an honest, penalty-free report.
fn vertical_success(vertical: String, hits: Vec<engines::Hit>, ms: u64) -> (String, EngineResult) {
    if hits.is_empty() {
        return (vertical, Err(("no-results".into(), "direct".into(), false)));
    }
    (vertical, Ok((hits, ms, "direct".into(), false)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn engine_budget_includes_pacing_without_starting_network() {
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let pool = EgressPool::new(Vec::new());
        let google = engines::google_wml::ProfileSelector::from_env();
        let context = EngineContext {
            fetcher: &fetcher,
            pool: &pool,
            google: &google,
        };
        pool.pace("bing", "direct").await;
        let (_, outcome) = engine_task_with_budget(
            "bing".into(),
            "unused".into(),
            "direct".into(),
            None,
            context,
            std::time::Duration::from_millis(1),
            None,
        )
        .await;
        let (status, _, _) = outcome.unwrap_err();
        assert_eq!(status, "pacing-timeout");
        assert!(!super::super::is_engine_fault(&status));
    }

    /// Explicit live test of the actual fan-out task, not a second HTTP client.
    /// Never runs in the ordinary offline test suite.
    #[tokio::test]
    #[ignore = "makes three paced, direct requests to Google; no browser or paid API"]
    async fn google_wml_live() {
        let fetcher = Fetcher::new(crate::profile::BrowserProfile::host_default()).unwrap();
        let pool = EgressPool::new(Vec::new());
        let google = engines::google_wml::ProfileSelector::from_env();
        let context = EngineContext {
            fetcher: &fetcher,
            pool: &pool,
            google: &google,
        };
        for query in [
            "rust programming language",
            "PostgreSQL documentation",
            "musei di Roma",
        ] {
            let (engine, outcome) = engine_task(
                "google".into(),
                query.into(),
                "direct".into(),
                None,
                context,
            )
            .await;
            let (hits, ms, egress, was_engine) =
                outcome.expect("Google HTTP lane must return usable results");
            assert!(engine.starts_with("google@"));
            assert_eq!(egress, "direct");
            assert!(was_engine);
            assert!(hits.len() >= 3);
            assert!(
                hits.iter()
                    .all(|h| !h.title.is_empty() && url::Url::parse(&h.url).is_ok())
            );
            assert!(hits.iter().any(|h| !h.snippet.is_empty()));
            eprintln!(
                "{query:?}: {} hits, {ms} ms, first={}",
                hits.len(),
                hits[0].url
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    fn hit(title: &str, url: &str) -> engines::Hit {
        engines::Hit {
            title: title.into(),
            url: url.into(),
            snippet: String::new(),
            rank: 1,
            published: None,
        }
    }

    #[test]
    fn vertical_empty_hits_is_no_results_not_success() {
        // #164: an empty vertical used to count as a healthy engine,
        // inflating ok_engines and relaxing retry/cache gates exactly
        // when retrieval was worst. It must report no-results instead.
        let (name, result) = vertical_success("github".into(), Vec::new(), 12);
        assert_eq!(name, "github");
        let (status, egress, was_engine) = result.expect_err("empty vertical must not be Ok");
        assert_eq!(status, "no-results");
        assert_eq!(egress, "direct");
        assert!(!was_engine, "vertical outcomes are not engine faults");
    }

    #[test]
    fn vertical_no_results_is_neither_retried_nor_a_fault() {
        let (_, result) = vertical_success("wikipedia".into(), Vec::new(), 5);
        let (status, _, _) = result.expect_err("empty vertical must be Err");
        assert!(
            !crate::search::is_engine_fault(&status),
            "no-results must stay out of trust/quarantine penalties"
        );
    }

    #[test]
    fn vertical_with_hits_is_success() {
        let (name, result) = vertical_success("github".into(), vec![hit("A", "https://a.com")], 42);
        assert_eq!(name, "github");
        let (hits, ms, egress, was_engine) = result.expect("hits => success");
        assert_eq!(hits.len(), 1);
        assert_eq!(ms, 42);
        assert_eq!(egress, "direct");
        assert!(!was_engine);
    }
}
