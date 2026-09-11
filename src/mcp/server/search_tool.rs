//! The search tool handler: dispatch, query parsing (incl. the
//! 2-variant batch), single + batch search flows, handle/URL
//! binding, model/debug meta surfaces, the ghost pre-solve hook,
//! and the search error contract.

use serde_json::{Value, json};

use super::errors::batch_failure_kind;
use super::fetch_tool::{bind_search_handles, bind_search_urls, route_hints};
use super::*;
pub(super) async fn search_tool(
    daemon: &Arc<Daemon>,
    args: &Value,
    mut ctx: Option<ToolCtx>,
) -> Value {
    daemon.refresh_vault().await;
    let deadline = args
        .get("deadline_ms")
        .and_then(Value::as_u64)
        .map(|ms| std::time::Duration::from_millis(ms.clamp(500, 600_000)));
    let queries = match parse_search_queries(args) {
        Ok(queries) => queries,
        Err(message) => return tool_error(message),
    };
    let max = args.get("max_results").and_then(Value::as_u64).unwrap_or(7) as usize;
    let intent = match args.get("intent").and_then(Value::as_str) {
        Some("web") => Some(Intent::Web),
        Some("code") => Some(Intent::Code),
        Some("paper") => Some(Intent::Paper),
        Some("news") => Some(Intent::News),
        Some("entity") => Some(Intent::Entity),
        _ => None,
    };

    if queries.len() == 1 {
        let query = &queries[0];
        run_with_budget(
            search_inner(daemon, query, max, intent),
            deadline,
            ctx.as_mut(),
            || search_deadline_error(query),
        )
        .await
    } else {
        let deadline_queries = queries.clone();
        run_with_budget(
            search_batch_inner(daemon, &queries, max, intent),
            deadline,
            ctx.as_mut(),
            move || search_batch_deadline_error(&deadline_queries),
        )
        .await
    }
}

/// Parse the required base query and at most two explicit alternate
/// formulations. DonSeTch never invents variants: the calling agent has the
/// task context and can express ambiguity without a local language model.
pub(super) fn parse_search_queries(args: &Value) -> Result<Vec<String>, String> {
    let base = args
        .get("query")
        .and_then(Value::as_str)
        .filter(|query| !query.trim().is_empty())
        .ok_or_else(|| "search: query required".to_string())?;
    // Preserve the original base query exactly. This keeps the established
    // single-query path and cache key behavior unchanged.
    let mut queries = vec![base.to_string()];

    let Some(variants) = args.get("query_variants") else {
        return Ok(queries);
    };
    let variants = variants
        .as_array()
        .ok_or_else(|| "search: query_variants must be an array of strings".to_string())?;
    if variants.len() > 2 {
        return Err("search: query_variants accepts at most 2 entries".to_string());
    }
    for variant in variants {
        let variant = variant
            .as_str()
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .ok_or_else(|| {
                "search: every query_variants entry must be a non-empty string".to_string()
            })?;
        if !queries
            .iter()
            .any(|existing| existing.trim().eq_ignore_ascii_case(variant))
        {
            queries.push(variant.to_string());
        }
    }
    Ok(queries)
}

/// Honest deadline error for search (v3 D1).
pub(super) fn make_ghost_hook(
    ghost_mgr: std::sync::Arc<GhostManager>,
    profile: BrowserProfile,
    fetcher: std::sync::Arc<Fetcher>,
    state: Arc<tokio::sync::Mutex<GhostState>>,
    skip_cache_read: bool,
) -> crate::crawl::GhostHook {
    std::sync::Arc::new(move |url: String| {
        let ghost_mgr = std::sync::Arc::clone(&ghost_mgr);
        let profile = profile.clone();
        let fetcher = std::sync::Arc::clone(&fetcher);
        let state = Arc::clone(&state);
        async move {
            // Render cache shortcut (crawl only).
            if !skip_cache_read {
                let s = state.lock().await;
                if let Some(rc) = s.render_for(&url) {
                    return Ok(crate::crawl::GhostRender {
                        html: rc.html.clone(),
                    });
                }
            }
            let g_host = crate::search::rank::host_of(&url);
            let mut g = match ghost_mgr.acquire_for(&profile, Some(g_host.as_str())).await {
                Ok(g) => g,
                Err(e) => return Err(format!("browser launch: {e}")),
            };
            let page = match ops::ghost_fetch(&mut g, &url, std::time::Duration::from_secs(20))
                .await
            {
                Ok(p) => p,
                Err(first) => {
                    // Retry once on transient timeout.
                    match ops::ghost_fetch(&mut g, &url, std::time::Duration::from_secs(20)).await {
                        Ok(p) => p,
                        Err(second) => {
                            return Err(format!("render: {first}; retry: {second}"));
                        }
                    }
                }
            };
            if page.captcha {
                return Err("interactive captcha (unsolvable by design)".to_string());
            }
            if !page.cookies.is_empty() {
                fetcher.import_cookies(&page.cookies).await;
                crate::ghost::cache::store_session_cookies(&page.cookies);
            }
            {
                let mut s = state.lock().await;
                s.record_render(&url, &page.html);
            }
            Ok(crate::crawl::GhostRender { html: page.html })
        }
        .boxed()
    })
}

#[derive(Debug)]
pub(super) struct SearchFailure {
    pub(super) cause: String,
    pub(super) byok_tried: bool,
    /// "permanent" for bad input that no retry or fallback fixes
    /// (validate_query rejected it before any engine was contacted);
    /// "transient" for exhausted engines/providers.
    pub(super) kind: &'static str,
}

/// The search pipeline: BYOK providers (if configured) with
/// local-engine fallback, or local-first when keys say so.
/// No deadline/cancel logic here : the wrapper owns the clock.
pub(super) async fn search_inner(
    daemon: &Arc<Daemon>,
    query: &str,
    max: usize,
    intent: Option<Intent>,
) -> Value {
    match search_outcome(daemon, query, max, intent).await {
        Ok(out) => {
            memory_ingest_outcome(&out);
            let top = out.results.first().map(|r| r.url.as_str());
            maybe_pre_solve(daemon, top);
            render_search_outcome(daemon, &out).await
        }
        Err(failure) => search_error(query, &failure.cause, failure.byok_tried, failure.kind),
    }
}

/// Remember snippets of the top search hits before rendering. The
/// memory's body cap truncates us so this stays small, and the
/// kill switch short-circuits everything.
#[cfg_attr(not(feature = "rerank"), allow(unused_variables))]
fn memory_ingest_outcome(out: &crate::search::SearchOutcome) {
    #[cfg(feature = "rerank")]
    if !crate::memory::kill_switch() {
        let rows: Vec<(String, String, String)> = out
            .results
            .iter()
            .map(|r| (r.url.clone(), r.title.clone(), r.snippet.clone()))
            .collect();
        crate::memory::ingest_async(rows);
    }
}

pub(super) async fn render_search_outcome(
    daemon: &Arc<Daemon>,
    out: &crate::search::SearchOutcome,
) -> Value {
    let hs = bind_search_handles(daemon, out).await;
    let hints = route_hints(daemon, out).await;
    let md = search::render_compact_markdown(out, "# Search results", Some(&hs), &hints);
    let model = search_model_meta(out, &hs);
    let debug = search_debug_meta(out);
    json!({
        "content": [{ "type": "text", "text": md }],
        "structuredContent": model,
        "_meta": {"com.donsetch/search-debug": debug},
    })
}

/// Machine state needed to route a subsequent fetch. Titles and snippets are
/// already present on the linear evidence surface; ranking and engine
/// telemetry remain in client-only metadata.
pub(super) fn search_model_meta(out: &crate::search::SearchOutcome, handles: &[String]) -> Value {
    let results = out
        .results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let mut item = json!({
                "rank": index + 1,
                "url": result.url,
            });
            if let Some(handle) = handles.get(index) {
                item["handle"] = json!(handle);
            }
            item
        })
        .collect::<Vec<_>>();
    json!({"weak": out.weak, "results": results})
}

pub(super) fn search_debug_meta(out: &crate::search::SearchOutcome) -> Value {
    // Full machine view: per-result title/url/snippet/score plus
    // the engines report. This is the client-only namespace (the
    // model never sees _meta), so the detail costs CLI/pipeline
    // consumers nothing and no model tokens. The compact-contract
    // PR pruned search-debug down to telemetry only, which broke
    // every machine consumer reading meta.results[].snippet (the
    // in-repo bench went 0/30 silently; live-found, restored).
    search::render_meta(out)
}

/// Retrying a fully-failed batch only makes sense if at least one
/// variant failed for a transient (engine/provider) reason; if every
/// variant was rejected by validate_query ("permanent"), no engine
/// was ever contacted and retrying the same queries won't help.
/// Pulled out as a pure function so this logic is testable without a
/// live `Daemon`.
pub(super) async fn search_batch_inner(
    daemon: &Arc<Daemon>,
    queries: &[String],
    max: usize,
    intent: Option<Intent>,
) -> Value {
    let started = std::time::Instant::now();
    let futures = queries
        .iter()
        .map(|query| search_outcome(daemon, query, max, intent));
    let outcomes = futures_util::future::join_all(futures).await;
    for out in outcomes.iter().flatten() {
        memory_ingest_outcome(out);
    }
    if let Some(Ok(first)) = outcomes.first() {
        let top = first.results.first().map(|r| r.url.as_str());
        maybe_pre_solve(daemon, top);
    }
    let ok = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    if ok == 0 {
        let errors = queries
            .iter()
            .zip(outcomes.iter())
            .filter_map(|(query, outcome)| match outcome {
                Ok(_) => None,
                Err(failure) => Some(json!({"query": query, "error": failure.cause})),
            })
            .collect::<Vec<_>>();
        let kind = batch_failure_kind(outcomes.iter().filter_map(|outcome| match outcome {
            Ok(_) => None,
            Err(f) => Some(f.kind),
        }));
        let next_action = if kind == "permanent" {
            "fix the queries and search again"
        } else {
            "retry once, then reduce to the strongest single query"
        };
        return tool_error_structured(
            format!("search: all {} query variants failed", queries.len()),
            kind,
            Some(json!({
                "queries": queries,
                "errors": errors,
                "next_action": next_action,
            })),
        );
    }

    // Mint one global set of handles so every S-handle in every section keeps
    // resolving after the batch completes. Binding each sub-search separately
    // would leave clients with ambiguous per-section numbering/state.
    let urls = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().ok())
        .flat_map(|out| out.results.iter().map(|result| result.url.clone()))
        .collect::<Vec<_>>();
    let handles = bind_search_urls(daemon, &urls).await;
    let mut handle_offset = 0usize;
    let mut markdown = format!("# Search results : {} formulations", queries.len());
    let mut searches = Vec::with_capacity(queries.len());
    let mut diagnostics = Vec::with_capacity(queries.len());

    for (query, outcome) in queries.iter().zip(outcomes.iter()) {
        markdown.push_str("\n\n");
        let role = if searches.is_empty() {
            "primary"
        } else {
            "variant"
        };
        let heading = format!("## q{} {role} : {query}", searches.len());
        match outcome {
            Ok(out) => {
                let count = out.results.len();
                let query_handles = if handles.is_empty() {
                    None
                } else {
                    Some(&handles[handle_offset..handle_offset + count])
                };
                handle_offset += count;
                let hints = route_hints(daemon, out).await;
                markdown.push_str(&search::render_compact_markdown(
                    out,
                    &heading,
                    query_handles,
                    &hints,
                ));
                let mut model = search_model_meta(out, query_handles.unwrap_or(&[]));
                model["query"] = json!(query);
                searches.push(model);
                let mut debug = search_debug_meta(out);
                debug["query"] = json!(query);
                diagnostics.push(debug);
            }
            Err(failure) => {
                markdown.push_str(&format!("{heading}\nFailed : {}", failure.cause));
                searches.push(json!({
                    "query": query,
                    "error": failure.cause,
                    "results": [],
                }));
                diagnostics.push(json!({"query": query, "error": failure.cause}));
            }
        }
    }

    json!({
        "content": [{ "type": "text", "text": markdown }],
        "structuredContent": {
            "query_count": queries.len(),
            "ok": ok,
            "errors": queries.len() - ok,
            "searches": searches,
        },
        "_meta": {"com.donsetch/search-debug": {
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "searches": diagnostics,
        }},
    })
}

/// The search pipeline without presentation. Keeping acquisition separate lets
/// multi-query mode share one deadline and one final handle table while the
/// single-query response stays byte-for-byte compatible.
pub(super) async fn search_outcome(
    daemon: &Arc<Daemon>,
    query: &str,
    max: usize,
    intent: Option<Intent>,
) -> Result<crate::search::SearchOutcome, SearchFailure> {
    // Input hygiene first: a bad query is a permanent-shaped failure
    // whether the fanout would have been BYOK or local.
    if let Some(problem) = search::validate_query(query) {
        return Err(SearchFailure {
            cause: problem,
            byok_tried: false,
            kind: "permanent",
        });
    }
    // Reload from disk first : picks up keys added/removed
    // via CLI while the daemon was running.
    daemon.byok.reload();
    let byok_configured = daemon.byok.is_configured();
    let local_first = daemon.byok.is_local_default();

    // BYOK-first mode: try providers, fall back to local.
    if byok_configured && !local_first {
        match byok_search_cached(daemon, query, max, intent).await {
            Ok(out) => return Ok(out),
            Err(e) => {
                if std::env::var_os("DONSEEK_DEBUG").is_some() {
                    eprintln!("[byok] all providers exhausted, falling back to local: {e}");
                }
                // Fall through to local search.
            }
        }
    }

    // Local search (primary in local-first mode, fallback in BYOK-first).
    match daemon.searcher.search(query, max, intent).await {
        Ok(out) => Ok(out),
        Err(e) => {
            // Local failed : if BYOK is configured and we're in
            // local-first mode, try BYOK as a last resort.
            if byok_configured && local_first {
                if std::env::var_os("DONSEEK_DEBUG").is_some() {
                    eprintln!("[byok] local search failed, trying BYOK fallback: {e}");
                }
                match byok_search_cached(daemon, query, max, intent).await {
                    Ok(out) => Ok(out),
                    Err(e2) => Err(SearchFailure {
                        cause: format!("local ({e}); byok ({e2})"),
                        byok_tried: true,
                        kind: "transient",
                    }),
                }
            } else {
                Err(SearchFailure {
                    cause: e.to_string(),
                    byok_tried: false,
                    kind: "transient",
                })
            }
        }
    }
}

/// One BYOK acquisition with the shared search cache wrapped around
/// it (issue #195). A repeat query inside the TTL is served from the
/// cache with `cached: true` and never re-bills the provider; a fresh
/// result is stored before it is filtered/prewarmed, so the cache
/// holds the provider's full top slice exactly like the local path.
/// Used by both BYOK entry points (BYOK-first and the local-first
/// fallback) so caching cannot drift between them.
async fn byok_search_cached(
    daemon: &Arc<Daemon>,
    query: &str,
    max: usize,
    intent: Option<Intent>,
) -> Result<crate::search::SearchOutcome, String> {
    let resolved = intent.unwrap_or_else(|| crate::search::intent::detect(query));
    if let Some(hit) = daemon.searcher.byok_cache_get(query, resolved, max) {
        return Ok(hit);
    }
    let mut out = daemon.byok.search(query, max, intent).await?;
    // Store the provider's own top slice before site: filtering, so a
    // later hit filters on serve exactly as the miss path does.
    daemon.searcher.byok_cache_put(query, &out);
    // Issue #190: site: queries reach BYOK results too, and prewarm
    // only the rows that survive the filter (law 5: zero added latency).
    crate::search::site_filter(query, &mut out.results);
    daemon.searcher.spawn_prewarm(&out.results);
    Ok(out)
}

/// Search failure → structured error: every engine (and BYOK if
/// tried) failed. The agent needs to know retrying is safe and
/// what the levers are (BYOK keys, intent, simpler query).
/// Predict-prefetch the walledest top result while the agent reads
/// results: when the top URL's domain is known-walled (skip-to-solve
/// route), start ONE background solve NOW. The agent's fetch a few
/// seconds later rides warm. Bounded: one in flight daemon-wide, top
/// result only, no extraction, and every failure feeds the same
/// cooldown memory the fetch path uses.
pub(crate) fn maybe_pre_solve(daemon: &Arc<Daemon>, top_url: Option<&str>) {
    let Some(url) = top_url else { return };
    if !url.starts_with("http") {
        return;
    }
    let Some(host) = url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .filter(|h| !h.is_empty() && h.contains('.'))
    else {
        return;
    };
    let d = daemon.clone();
    let host_str = host.to_string();
    let url_str = url.to_string();
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        if d.pre_solve_busy.swap(true, Ordering::SeqCst) {
            return; // one pre-solve at a time
        }
        let _guard = PreSolveGuard(&d);
        {
            let state = d.state.lock().await;
            if !matches!(
                state.route_for(&host_str),
                RouteDecision::SkipToSolve | RouteDecision::RecheckCold
            ) {
                return; // not a known wall: the search prewarm covers it
            }
        }
        if std::env::var_os("DONGHOST_DEBUG").is_some() {
            eprintln!(
                "[pre-solve] kicking background solve for {} ({})",
                host_str, url_str
            );
        }
        let t0 = std::time::Instant::now();
        let Ok(mut g) = d
            .ghost_mgr
            .acquire_for(&d.profile, Some(host_str.as_str()))
            .await
        else {
            return;
        };
        let page =
            match ops::ghost_fetch(&mut g, &url_str, std::time::Duration::from_secs(20)).await {
                Ok(p) => p,
                Err(_) => return,
            };
        if page.captcha
            || matches!(
                crate::detect::walls::detect_dom_smart(page.html.as_bytes()),
                crate::detect::walls::Verdict::Challenge(_)
                    | crate::detect::walls::Verdict::Blocked
            )
        {
            d.state.lock().await.record_wall_failed(&host_str);
            return;
        }
        if !page.cookies.is_empty() {
            d.fetcher.import_cookies(&page.cookies).await;
            crate::ghost::cache::store_session_cookies(&page.cookies);
            // Honest replay_ok: only verified tier-1 replay earns
            // warm routing.
            let replay_ok = matches!(
                d.fetcher.fetch(&url_str).await,
                Ok(o) if o.verdict == crate::detect::walls::Verdict::ContentOk
            );
            d.state.lock().await.record_solved(
                &host_str,
                &page.cookies,
                page.vendor.as_deref(),
                replay_ok,
            );
        }
        if std::env::var_os("DONGHOST_DEBUG").is_some() {
            eprintln!(
                "[pre-solve] done for {} in {}ms",
                host_str,
                t0.elapsed().as_millis()
            );
        }
    });
}

/// RAII reset: the pre-solve flag clears when the task ends no
/// matter how it exits.
pub(super) struct PreSolveGuard<'a>(&'a Daemon);
impl Drop for PreSolveGuard<'_> {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.0.pre_solve_busy.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod search_variant_tests {
    use super::parse_search_queries;
    use serde_json::json;

    #[test]
    pub(super) fn single_query_contract_is_unchanged() {
        assert_eq!(
            parse_search_queries(&json!({"query": "  rust ownership  "})).unwrap(),
            vec!["  rust ownership  "]
        );
    }

    #[test]
    pub(super) fn variants_are_trimmed_and_case_insensitive_duplicates_are_removed() {
        assert_eq!(
            parse_search_queries(&json!({
                "query": "  rust async trait patterns  ",
                "query_variants": [
                    "async fn in trait rust",
                    "RUST ASYNC TRAIT PATTERNS"
                ]
            }))
            .unwrap(),
            vec!["  rust async trait patterns  ", "async fn in trait rust"]
        );
    }

    #[test]
    pub(super) fn variants_are_bounded_and_strictly_typed() {
        assert!(
            parse_search_queries(&json!({
                "query": "base",
                "query_variants": ["one", "two", "three"]
            }))
            .unwrap_err()
            .contains("at most 2")
        );
        assert!(
            parse_search_queries(&json!({"query": "base", "query_variants": "one"}))
                .unwrap_err()
                .contains("array of strings")
        );
        assert!(
            parse_search_queries(&json!({"query": "base", "query_variants": [""]}))
                .unwrap_err()
                .contains("non-empty string")
        );
    }
}

#[cfg(test)]
mod search_output_contract_tests {
    use super::{search_debug_meta, search_model_meta};
    use crate::search::SearchOutcome;
    use crate::search::intent::Intent;
    use crate::search::rank::Merged;
    use std::time::Duration;

    #[test]
    pub(super) fn search_structure_routes_without_repeating_ranked_evidence() {
        let output = SearchOutcome {
            results: vec![Merged {
                title: "Visible in markdown".into(),
                url: "https://example.com/answer".into(),
                snippet: "Evidence belongs to text".into(),
                sources: vec![("bing".into(), 0)],
                score: 0.9,
                published: None,
            }],
            weak: false,
            intent: Intent::Web,
            report: Vec::new(),
            cached: false,
            elapsed: Duration::from_millis(10),
            provider: None,
            reranked: true,
        };
        let state = search_model_meta(&output, &["S1".into()]);
        assert_eq!(state["results"][0]["rank"], 1);
        assert_eq!(state["results"][0]["handle"], "S1");
        // The markdown shows title, host and the S-handle : never the
        // raw URL. This field is the model's only source of citable
        // URLs once the compat fold merges the surfaces (issue #27).
        assert_eq!(state["results"][0]["url"], "https://example.com/answer");
        for absent in ["title", "snippet", "score", "engines"] {
            assert!(state["results"][0].get(absent).is_none());
        }
        let debug = search_debug_meta(&output);
        assert_eq!(debug["results"][0]["score"], 0.9);
        // The machine channel (client-only _meta) carries the full
        // per-result view: scripts, the bench, and pipelines read
        // meta.results[].snippet through the CLI --json re-materializer.
        assert_eq!(debug["results"][0]["title"], "Visible in markdown");
        assert_eq!(debug["results"][0]["url"], "https://example.com/answer");
        assert_eq!(debug["results"][0]["snippet"], "Evidence belongs to text");
    }
}
