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

pub(super) async fn engine_task(
    engine: String,
    query: String,
    egress_id: String,
    proxy: Option<crate::transport::proxy::Proxy>,
    fetcher: &Fetcher,
    pool: &EgressPool,
) -> (String, EngineResult) {
    let label = engine.clone();
    pool.pace(&engine, &egress_id).await;
    let started = Instant::now();
    let Some(url) = engines::serp_url(&engine, &query) else {
        return (label, Err(("no-url".into(), egress_id, true)));
    };
    let out = match tokio::time::timeout(
        ENGINE_TIMEOUT,
        fetcher.fetch_once_via(&url, &[], proxy.as_ref(), false, None),
    )
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
    if out.status == 429 || !matches!(out.verdict, Verdict::ContentOk) {
        return (
            label,
            Err((format!("blocked:{}", out.status), egress_id, true)),
        );
    }
    let html = crate::extract::charset::decode(
        &out.body,
        out.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or(""),
    );
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
    (label, Ok((hits, ms, egress_id, true)))
}

/// The browser-render SERP lane. Runs the SERP URL through the
/// shared ghost hook (render cache shortcut included), parses
/// with the same layered parser as the plain-HTTP engine, and
/// reports honestly: "google_ghost" on the engine list, egress
/// "ghost". Engine id shares the "google" base for trust +
/// quarantine so repeated cascades learn.
pub(super) async fn ghost_engine_task(
    engine: String,
    query: String,
    hook: crate::crawl::GhostHook,
) -> (String, EngineResult) {
    let started = Instant::now();
    let Some(url) = engines::serp_url("google", &query) else {
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
    let hits = engines::parse("google", &rendered);
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
        Ok(Ok(hits)) => {
            let ms = started.elapsed().as_millis() as u64;
            (vertical, Ok((hits, ms, "direct".into(), false)))
        }
    }
}
