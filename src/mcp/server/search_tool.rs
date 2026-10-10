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

    search::SEARCH_DEADLINE
        .scope(deadline.map(|d| tokio::time::Instant::now() + d), async {
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
        })
        .await
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

/// Render a selected browser request within its caller's absolute deadline.
pub(super) fn make_ghost_hook(
    ghost_mgr: std::sync::Arc<GhostManager>,
    profile: BrowserProfile,
    fetcher: std::sync::Arc<Fetcher>,
    state: Arc<tokio::sync::Mutex<GhostState>>,
    skip_cache_read: bool,
) -> crate::crawl::GhostHook {
    std::sync::Arc::new(move |request: crate::crawl::GhostRequest| {
        let ghost_mgr = Arc::clone(&ghost_mgr);
        let profile = profile.clone();
        let fetcher = Arc::clone(&fetcher);
        let state = Arc::clone(&state);
        async move {
            let deadline = request.deadline;
            if deadline <= std::time::Instant::now() {
                return Err("browser handoff deadline exceeded".into());
            }
            tokio::time::timeout_at(deadline.into(), async move {
                let url = request.url;
                let g_host = crate::search::rank::host_of(&url);
                let (mut wire, persona) = {
                    let s = state.lock().await;
                    let persona = s
                        .personas
                        .get(&g_host)
                        .filter(|p| p.quarantine_reason.is_none());
                    (
                        persona.map(|p| p.ghost_wire()).unwrap_or_default(),
                        persona.map(|p| (p.generation, p.entropy_seed)),
                    )
                };
                wire.route = Some(request.route);
                let context = crate::ghost::cache::render_context(&profile, &wire, persona);
                // Legacy URL-only records cannot establish this route/persona.
                if !skip_cache_read {
                    let s = state.lock().await;
                    if let Some(rc) = s.render_for_context(&url, &context)
                        && let Some(document) = rc.document.as_ref()
                        && document.url == url
                        && crate::detect::walls::detect_dom_smart(rc.html.as_bytes())
                            == crate::detect::walls::Verdict::ContentOk
                    {
                        return Ok(crate::crawl::GhostRender {
                            html: rc.html.clone(),
                            document: document.clone(),
                        });
                    }
                }
                let g = ghost_mgr
                    .acquire_for_wire(&profile, Some(g_host.as_str()), wire)
                    .await
                    .map_err(|error| format!("browser launch: {error}"))?;
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let read = ghost_mgr
                    .read_document(
                        g,
                        &profile,
                        &url,
                        remaining.min(std::time::Duration::from_secs(20)),
                    )
                    .await
                    .map_err(|error| format!("render: {error}"))?;
                let _guard = read.guard;
                let page = read.page;
                if page.outcome != ops::BrowserOutcome::Content {
                    return Err(format!("browser content unavailable: {:?}", page.outcome));
                }
                let verdict = crate::detect::walls::detect_dom_smart(page.html.as_bytes());
                if verdict != crate::detect::walls::Verdict::ContentOk {
                    return Err(format!("rendered access gate: {verdict:?}"));
                }
                if !page.cookies.is_empty() {
                    // A pooled browser can predate a logout for one of
                    // its domains: a dead session must not ride back
                    // into the jar or the vault.
                    let cookies = crate::ghost::cache::session_cookies_since(
                        &page.cookies,
                        _guard.launched_epoch,
                    );
                    fetcher.import_vault_cookies(&cookies).await;
                    crate::ghost::cache::store_session_cookies(&cookies);
                }
                state
                    .lock()
                    .await
                    .record_render_document(&page.document, &page.html, &context);
                Ok(crate::crawl::GhostRender {
                    html: page.html,
                    document: page.document,
                })
            })
            .await
            .unwrap_or_else(|_| Err("browser handoff deadline exceeded".into()))
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
            let top = out.results.first().map(|r| r.url.as_str());
            maybe_pre_solve(daemon, top);
            render_search_outcome(daemon, &out, max).await
        }
        Err(failure) => search_error(query, &failure.cause, failure.byok_tried, failure.kind),
    }
}

pub(super) async fn render_search_outcome(
    daemon: &Arc<Daemon>,
    out: &crate::search::SearchOutcome,
    requested: usize,
) -> Value {
    let hs = bind_search_handles(daemon, out).await;
    let hints = route_hints(daemon, out).await;
    let md = search::render_compact_markdown(out, "# Search results", Some(&hs), &hints);
    let mut model = search_model_meta(out, &hs);
    model["requested_results"] = json!(requested.clamp(1, 12));
    model["returned_results"] = json!(out.results.len());
    if out.results.len() < requested.clamp(1, 12) {
        model["underfilled"] = json!(true);
    }
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
            // Every hit is listed exactly as the search returned it, even
            // when a local `deny` rule covers its host. A hit that matches
            // a deny rule could carry a marker so the agent does not try
            // to fetch it and spend a round trip on the refusal; v1 leaves
            // that out on purpose, because the display contract (where the
            // marker goes, its token cost, whether an annotated hit still
            // takes a result slot) has to be settled first. Dropping such
            // hits is ruled out: the title and any DOI are how the agent
            // finds a mirror.
            let mut item = json!({
                "rank": index + 1,
                "url": result.url,
                "source_type": crate::search::rank::source_type(&result.url),
            });
            if let Some(handle) = handles.get(index) {
                item["handle"] = json!(handle);
            }
            item
        })
        .collect::<Vec<_>>();
    let mut item = json!({
        "ok": true,
        "weak": out.weak,
        "results": results,
        "provider": out.provider.as_deref().unwrap_or("keyless"),
    });
    let (failed, _) = out.engine_health();
    let failed = failed.len();
    if failed > 0 {
        item["degraded"] = json!(true);
        item["failed_engines"] = json!(failed);
        if out.results.is_empty() {
            item["next_action"] = json!(
                "retrieval was incomplete; check provider/engine health with doctor --deep or retry another source before concluding that no answer exists"
            );
        }
    }
    // Instant answers are decision aids, not ranked evidence: they
    // ride structuredContent (not the organic list) with their source.
    if let Some(ans) = &out.instant {
        item["instant"] = json!({
            "kind": ans.kind,
            "text": ans.text,
            "url": ans.url,
            "engine": ans.engine,
        });
    }
    item
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
                model["requested_results"] = json!(max.clamp(1, 12));
                model["returned_results"] = json!(out.results.len());
                if out.results.len() < max.clamp(1, 12) {
                    model["underfilled"] = json!(true);
                }
                model["query"] = json!(query);
                searches.push(model);
                let mut debug = search_debug_meta(out);
                debug["query"] = json!(query);
                diagnostics.push(debug);
            }
            Err(failure) => {
                markdown.push_str(&format!("{heading}\nFailed : {}", failure.cause));
                searches.push(json!({
                    "ok": false,
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
            "ok": true,
            "ok_count": ok,
            "query_count": queries.len(),
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
    let mut byok_fail: Option<String> = None;
    if byok_configured && !local_first {
        match byok_search_cached(daemon, query, max, intent).await {
            Ok(out) => return Ok(out),
            Err(e) => {
                if crate::config::cfg().debug.search {
                    eprintln!("[byok] all providers exhausted, falling back to local: {e}");
                }
                // #285: keep the failure so the fallback result can
                // say a provider was tried and why it failed; the
                // result was otherwise indistinguishable from a run
                // with no BYOK keys at all.
                byok_fail = Some(e);
            }
        }
    }

    // Local search (primary in local-first mode, fallback in BYOK-first).
    match daemon.searcher.search(query, max, intent).await {
        Ok(mut out) => {
            // #285: fold the BYOK failure into the visible engines
            // trail (the same `degraded:` field local engine
            // failures use).
            if let Some(e) = byok_fail {
                out.report.insert(
                    0,
                    crate::search::EngineReport {
                        engine: "byok".into(),
                        profile: None,
                        status: crate::search::byok::compact_failure(&e),
                        hits: 0,
                        ms: 0,
                        egress: "byok".into(),
                    },
                );
            }
            Ok(out)
        }
        Err(e) => {
            // Local failed : if BYOK is configured and we're in
            // local-first mode, try BYOK as a last resort.
            if byok_configured && local_first {
                if crate::config::cfg().debug.search {
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
    // Single-flight on the byok cache key: two concurrent identical
    // queries share one provider round-trip instead of both missing
    // the cache and both billing the metered provider.
    let key = daemon.searcher.byok_flight_key(query, resolved);
    let d2 = Arc::clone(daemon);
    let q2 = query.to_string();
    daemon
        .searcher
        .byok_flight(key, query, resolved, max, async move {
            let daemon = d2;
            let query = q2.as_str();
            // Fetch the provider's FULL top slice (12) regardless of
            // the caller's ask, cache that, then truncate: caching
            // the truncated outcome meant a first search at max=2
            // served every later larger request a 2-row slice as
            // cached: true (the local path fixed this same bug).
            let mut out = daemon.byok.search(query, 12, intent).await?;
            // Store the provider's own top slice before site:
            // filtering, so a later hit filters on serve exactly as
            // the miss path does.
            daemon.searcher.byok_cache_put(query, &out);
            // Issue #190: site: queries reach BYOK results too, and
            // prewarm only the rows that survive the filter (law 5:
            // zero added latency).
            crate::search::site_filter(query, &mut out.results);
            out.results.truncate(max.clamp(1, 12));
            out.weak |= crate::search::rank::relevance_is_weak(&out.results, query);
            daemon.searcher.spawn_prewarm(&out.results);
            Ok(out)
        })
        .await
}

/// Predict-prefetch the walledest top result while the agent reads
/// results: when the top URL's domain is known-walled (skip-to-solve
/// route), start ONE background solve NOW. The agent's fetch a few
/// seconds later rides warm. Bounded: one in flight daemon-wide, top
/// result only, no extraction, and every failure feeds the same
/// cooldown memory the fetch path uses.
pub(crate) fn maybe_pre_solve(daemon: &Arc<Daemon>, top_url: Option<&str>) {
    let Some(url) = top_url else { return };
    let Ok(parsed) = crate::fetch::guards::validate_url_basic(url) else {
        return;
    };
    let Some(host) = parsed.host_str() else {
        return;
    };
    let d = daemon.clone();
    let host_str = host.to_string();
    let path = parsed.path().to_string();
    let url_str = url.to_string();
    // One pre-solve at a time: the flag is taken BEFORE the spawn, so
    // the daemon's slot always holds the live handle (owned, never
    // detached) and a lost race skips the win without burning a task.
    if daemon
        .pre_solve_busy
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let handle = tokio::spawn(async move {
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
        if crate::config::cfg().debug.ghost {
            eprintln!(
                "[pre-solve] kicking background solve for {} ({})",
                host_str, url_str
            );
        }
        let t0 = std::time::Instant::now();
        // v4 E2: pre-solve on the host's persona wire, like the fetch
        // paths — a default-wire pre-solve would render incoherently
        // with the persona and thrash the pool slot against a
        // persona-wire fetch.
        let route = d.fetcher.route_for_fetch(&url_str);
        let (mut wire, persona_al) = {
            let s = d.state.lock().await;
            let persona = s
                .personas
                .get(&host_str)
                .filter(|p| p.quarantine_reason.is_none());
            let language = persona
                .map(|p| crate::profile::accept_language_with_persona(&host_str, &path, &p.locale));
            (
                persona.map(|p| p.ghost_wire()).unwrap_or_default(),
                language,
            )
        };
        wire.route = Some(route.clone());
        let Ok(g) = d
            .ghost_mgr
            .acquire_for_wire(&d.profile, Some(host_str.as_str()), wire)
            .await
        else {
            return;
        };
        let Ok(read) = d
            .ghost_mgr
            .read_document(g, &d.profile, &url_str, std::time::Duration::from_secs(20))
            .await
        else {
            return;
        };
        let _guard = read.guard;
        let page = read.page;
        if page.outcome.is_wall()
            || matches!(
                crate::detect::walls::detect_dom_smart(page.html.as_bytes()),
                crate::detect::walls::Verdict::Challenge(_)
                    | crate::detect::walls::Verdict::Blocked
            )
        {
            d.state.lock().await.record_wall_failed(&host_str);
            return;
        }
        if page.outcome != ops::BrowserOutcome::Content
            || crate::detect::walls::detect_dom_smart(page.html.as_bytes())
                != crate::detect::walls::Verdict::ContentOk
        {
            return;
        }
        if !page.cookies.is_empty() {
            // A pooled browser can predate a logout for one of its
            // domains: a dead session must not ride back into the
            // jar or the vault.
            let cookies =
                crate::ghost::cache::session_cookies_since(&page.cookies, _guard.launched_epoch);
            d.fetcher.import_vault_cookies(&cookies).await;
            crate::ghost::cache::store_session_cookies(&cookies);
            // Honest replay_ok: only verified tier-1 replay earns
            // warm routing.
            let replay_ok = matches!(
                d.fetcher.fetch_persona_on_route(&url_str, persona_al.as_deref(), Some(&route)).await,
                Ok(o) if o.verdict == crate::detect::walls::Verdict::ContentOk
            );
            d.state.lock().await.record_solved(
                &host_str,
                &page.cookies,
                page.vendor.as_deref(),
                replay_ok,
            );
        }
        if crate::config::cfg().debug.ghost {
            eprintln!(
                "[pre-solve] done for {} in {}ms",
                host_str,
                t0.elapsed().as_millis()
            );
        }
    });
    if let Ok(mut slot) = daemon.pre_solve_task.lock() {
        *slot = Some(handle);
    }
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
mod pre_solve_tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Native Chromium, owned proxies and nextest's per-process state only.
    #[test]
    #[ignore = "requires native Chromium"]
    fn stealth_v3_pre_solve_replay_keeps_the_browser_route_after_concurrent_429() {
        pre_solve_native_fixture("http://example.com/owned-solve");
    }

    #[test]
    #[ignore = "requires native Chromium"]
    fn stealth_v3_pre_solve_uses_the_host_persona_on_a_nondefault_port() {
        pre_solve_native_fixture("http://example.com:8443/owned-solve");
    }

    fn pre_solve_native_fixture(document_url: &'static str) {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let mut config = crate::config::DonsetchConfig::default();
                        config.browser.backend = crate::config::BrowserBackend::Headless;
                        config.browser.cloak_auto_download = false;
                        config.browser.route_probes = false;
                        config.proxy.from_environment = false;
                        config.proxy.fetch_rotate = true;
                        config.fetch.allow_private_egress = true;
                        crate::config::install(config).unwrap();
                        let a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let proxies = [&a, &b].map(|listener| {
                            crate::transport::proxy::Proxy::parse(&format!(
                                "http://{}", listener.local_addr().unwrap()
                            )).unwrap()
                        });
                        let pool = Arc::new(EgressPool::new(proxies.to_vec()));
                        pool.observe_rtt(&proxies[0].id(), Duration::from_millis(1));
                        pool.observe_rtt(&proxies[1].id(), Duration::from_millis(2));
                        crate::search::egress::install_global(Arc::clone(&pool));
                        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
                        let first_document = Arc::new(tokio::sync::Notify::new());
                        let release_document = Arc::new(tokio::sync::Notify::new());
                        let (stop, stopped) = tokio::sync::watch::channel(false);
                        let mut servers = Vec::new();
                        for (lane, listener) in [("A", a), ("B", b)] {
                            let events = Arc::clone(&events);
                            let first_document = Arc::clone(&first_document);
                            let release_document = Arc::clone(&release_document);
                            let mut stopped = stopped.clone();
                            servers.push(tokio::spawn(async move {
                                let mut handlers = tokio::task::JoinSet::new();
                                loop {
                                    tokio::select! {
                                        _ = stopped.changed() => break,
                                        accepted = listener.accept(), if handlers.len() < 16 => {
                                            let (mut socket, _) = accepted.unwrap();
                                            let events = Arc::clone(&events);
                                            let first_document = Arc::clone(&first_document);
                                            let release_document = Arc::clone(&release_document);
                                            handlers.spawn(async move {
                                                let mut head = Vec::new();
                                                while !head.ends_with(b"\r\n\r\n") {
                                                    assert!(head.len() < 16384);
                                                    match tokio::time::timeout(Duration::from_secs(3), socket.read_u8()).await {
                                                        Ok(Ok(byte)) => head.push(byte),
                                                        _ if head.is_empty() => return,
                                                        other => panic!("partial owned proxy request: {other:?}"),
                                                    }
                                                }
                                                let request = String::from_utf8(head).unwrap();
                                                let target = request.split_whitespace().nth(1).unwrap().to_string();
                                                let document = target.ends_with("/owned-solve");
                                                let burn = target.ends_with("/owned-burn");
                                                let language_echo = url::Url::parse(&target).is_ok_and(|url| url.path() == "/owned-language");
                                                let first = if document || burn || language_echo {
                                                    let mut events = events.lock().unwrap();
                                                    let first = document && !events.iter().any(|(_, url, _, _)| url == document_url);
                                                    let cookie = request.lines().find(|line| line.to_ascii_lowercase().starts_with("cookie:")).unwrap_or("").to_string();
                                                    let language = request.lines().find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("accept-language")).map(|(_, value)| value.trim().to_string())).unwrap_or_default();
                                                    events.push((lane, target, cookie, language));
                                                    first
                                                } else { false };
                                                if first {
                                                    first_document.notify_one();
                                                    release_document.notified().await;
                                                }
                                                let (status, body, cookie) = if document {
                                                    (200, format!("<article><h1>Owned pre-solve</h1><p>{}</p></article>",
                                                        "The browser and verified cookie replay must retain the original route through a concurrent rate limit. ".repeat(40)) + r#"<script>
const report = (where, value) => fetch('/owned-language?where='+where+'&value='+encodeURIComponent(JSON.stringify(value)));
const language = () => ({language:navigator.language,languages:navigator.languages,intl:Intl.DateTimeFormat().resolvedOptions().locale});
report('main', language());
const worker = new Worker(URL.createObjectURL(new Blob(['postMessage(({language:navigator.language,languages:navigator.languages,intl:Intl.DateTimeFormat().resolvedOptions().locale}))'], {type:'text/javascript'})));
worker.onmessage = event => {report('worker', event.data);worker.terminate();};
</script>"#,
                                                        "Set-Cookie: cf_clearance=owned-clearance; Path=/; HttpOnly\r\n")
                                                } else if language_echo {
                                                    (204, String::new(), "")
                                                } else if burn {
                                                    (429, "<h1>Too many requests</h1>".into(), "")
                                                } else {
                                                    (502, String::new(), "")
                                                };
                                                let response = format!("HTTP/1.1 {status} Owned\r\nContent-Type: text/html\r\n{cookie}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                                                let _ = socket.write_all(response.as_bytes()).await;
                                            });
                                        }
                                        done = handlers.join_next(), if !handlers.is_empty() => { done.unwrap().unwrap(); }
                                    }
                                }
                            }));
                        }
                        let mut daemon = Daemon::new().await.unwrap();
                        daemon.fetcher = Arc::new(Fetcher::new(daemon.profile.clone()).unwrap().with_egress(Arc::clone(&pool)));
                        daemon.state.lock().await.profiles.insert("example.com".into(), crate::ghost::cache::DomainProfile {
                            needs_tier2: true,
                            ..Default::default()
                        });
                        let mut persona = crate::persona::Persona::mint(
                            "example.com", &crate::persona::PersonaCaps::from_profile(&daemon.profile),
                            1, crate::ghost::cache::now());
                        persona.locale = "fr-FR".into();
                        daemon.state.lock().await.personas.insert("example.com".into(), persona);
                        let daemon = Arc::new(daemon);
                        maybe_pre_solve(&daemon, Some(document_url));
                        tokio::time::timeout(Duration::from_secs(15), first_document.notified()).await.expect("known wall and persona must be found by host independently of the port");
                        let burn = daemon.fetcher.fetch("http://example.com/owned-burn").await.unwrap();
                        assert_eq!(burn.status, 429, "the concurrent request must really rate-limit");
                        assert_eq!(pool.pick_fetch("example.com", true).unwrap().id, proxies[1].id(), "the actual 429 must burn A for the next independent call");
                        release_document.notify_one();
                        tokio::time::timeout(Duration::from_secs(15), async {
                            while daemon.pre_solve_busy.load(std::sync::atomic::Ordering::SeqCst) {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                        }).await.unwrap();
                        tokio::time::timeout(Duration::from_secs(3), async {
                            while events.lock().unwrap().iter().filter(|(_, url, _, _)| url::Url::parse(url).is_ok_and(|url| url.path() == "/owned-language")).count() < 2 {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                        }).await.unwrap();
                        daemon.ghost_mgr.shutdown().await;
                        stop.send(true).unwrap();
                        for server in servers { server.await.unwrap(); }
                        assert!(daemon.state.lock().await.profiles["example.com"].replay_ok);
                        let events = events.lock().unwrap();
                        let documents: Vec<_> = events.iter().filter(|(_, url, _, _)| url.ends_with("/owned-solve")).collect();
                        assert_eq!(documents.len(), 2, "one native read and one verified HTTP replay: {events:?}");
                        assert!(documents.iter().all(|(_, url, _, _)| url == document_url), "original authority and port survive browser and replay: {events:?}");
                        assert!(documents.iter().all(|(lane, _, _, _)| *lane == "A"), "replay must keep the browser's selected route: {events:?}");
                        assert!(documents[0].2.is_empty(), "fresh native profile has no fixture cookie");
                        assert!(documents[1].2.contains("cf_clearance=owned-clearance"), "HTTP replay must actually carry the exported cookie: {events:?}");
                        assert!(documents[0].3.starts_with("fr-FR"), "native read must use the persona locale: {events:?}");
                        assert_eq!(documents[0].3, documents[1].3, "browser and replay must keep the full preference list: {events:?}");
                        assert_eq!(documents[1].3, crate::profile::accept_language_with_persona("example.com", "/owned-solve", "fr-FR"), "replay must retain the persona language: {events:?}");
                        let echoes: Vec<_> = events.iter().filter(|(_, url, _, _)| url::Url::parse(url).is_ok_and(|url| url.path() == "/owned-language")).map(|(lane, target, _, _)| {
                            assert_eq!(*lane, "A", "native language beacons retain the document route");
                            let url = url::Url::parse(target).unwrap();
                            let payload = url.query_pairs().find(|(name, _)| name == "value").unwrap().1.into_owned();
                            serde_json::from_str::<Value>(&payload).unwrap()
                        }).collect();
                        assert_eq!(echoes.len(), 2, "actual main-frame and worker observations");
                        for echo in &echoes {
                            assert_eq!(echo["language"], "fr-FR", "{echoes:?}");
                            assert_eq!(echo["languages"][0], "fr-FR", "{echoes:?}");
                            // Chromium's French UI/ICU bundle is `fr`; it
                            // need not retain the regional preference tag.
                            assert_eq!(echo["intl"].as_str().unwrap().split('-').next(), Some("fr"), "{echoes:?}");
                        }
                    });
            }).unwrap().join().unwrap();
    }
}

#[cfg(test)]
mod crawl_route_tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn stealth_v3_expired_browser_handoff_cannot_return_cached_content() {
        let profile = BrowserProfile::host_default();
        let route = crate::transport::request_route::RequestRoute::direct();
        let wire = crate::ghost::GhostWire {
            route: Some(route.clone()),
            ..Default::default()
        };
        let context = crate::ghost::cache::render_context(&profile, &wire, None);
        let url = "https://owned.example/expired";
        let mut state = GhostState::default();
        state.record_render_document(
            &crate::ghost::document::Document {
                url: url.into(),
                generation: 7,
                status: Some(201),
                ..Default::default()
            },
            "<article><h1>Owned cached article</h1><p>Useful cached content.</p></article>",
            &context,
        );
        assert!(state.render_for_context(url, &context).is_some());
        let manager = GhostManager::new().await;
        let hook = make_ghost_hook(
            Arc::clone(&manager),
            profile.clone(),
            Arc::new(Fetcher::new(profile).unwrap()),
            Arc::new(tokio::sync::Mutex::new(state)),
            false,
        );
        let result = hook(crate::crawl::GhostRequest {
            url: url.into(),
            route,
            deadline: std::time::Instant::now() - Duration::from_secs(1),
        })
        .await;
        manager.shutdown().await;
        assert!(matches!(result, Err(ref reason) if reason == "browser handoff deadline exceeded"));
    }

    #[test]
    #[ignore = "requires native Chromium"]
    fn stealth_v3_native_crawl_keeps_its_selected_http_lane() {
        native_crawl_fixture(false, Duration::from_secs(45));
    }

    #[test]
    #[ignore = "requires native Chromium"]
    fn stealth_v3_native_crawl_recovers_a_wall_with_a_short_budget() {
        native_crawl_fixture(false, Duration::from_secs(5));
    }

    #[test]
    #[ignore = "requires native Chromium"]
    fn stealth_v3_native_crawl_recovers_a_thin_shell_with_a_short_budget() {
        native_crawl_fixture(true, Duration::from_secs(5));
    }

    fn native_crawl_fixture(thin: bool, deadline: Duration) {
        std::thread::Builder::new().stack_size(8 * 1024 * 1024).spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let mut config = crate::config::DonsetchConfig::default();
                config.browser.backend = crate::config::BrowserBackend::Headless;
                config.browser.cloak_auto_download = false;
                config.browser.route_probes = false;
                config.proxy.from_environment = false;
                config.proxy.fetch_rotate = false;
                config.fetch.allow_private_egress = true;
                config.fetch.shadow_fetch = crate::config::ShadowFetch::Never;
                crate::config::install(config).unwrap();
                let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}/owned-crawl", origin.local_addr().unwrap());
                let proxy = (crate::transport::proxy::Proxy::parse(&format!("http://{}",proxy.local_addr().unwrap())).unwrap(), proxy);
                let pool = Arc::new(EgressPool::new(vec![proxy.0.clone()]));
                let events = Arc::new(std::sync::Mutex::new(Vec::new()));
                let (stop, stopped) = tokio::sync::watch::channel(false);
                let mut servers = Vec::new();
                for (lane, listener) in [("DIRECT", origin), ("A", proxy.1)] {
                    let events = Arc::clone(&events);
                    let mut stopped = stopped.clone();
                    servers.push(tokio::spawn(async move {
                        let mut handlers = tokio::task::JoinSet::new();
                        loop {
                            tokio::select! {
                                _ = stopped.changed() => break,
                                accepted = listener.accept(), if handlers.len() < 16 => {
                                    let (mut socket, _) = accepted.unwrap();
                                    let events = Arc::clone(&events);
                                    handlers.spawn(async move {
                                        let mut head = Vec::new();
                                        while !head.ends_with(b"\r\n\r\n") {
                                            assert!(head.len() < 16384);
                                            match tokio::time::timeout(Duration::from_secs(3),socket.read_u8()).await {
                                                Ok(Ok(byte)) => head.push(byte),
                                                _ if head.is_empty() => return,
                                                other => panic!("partial owned crawl request: {other:?}"),
                                            }
                                        }
                                        let head = String::from_utf8(head).unwrap();
                                        let target = head.split_whitespace().nth(1).unwrap();
                                        let document = target.ends_with("/owned-crawl");
                                        let first = if document {
                                            let mut events = events.lock().unwrap();
                                            let first = events.is_empty();
                                            events.push((lane, target.to_string()));
                                            first
                                        } else { false };
                                        let (status, body) = if document && first && lane == "A" && thin {
                                            (200, format!("<html><body><div id='app'></div><script>{}</script></body></html>", "/* owned shell */".repeat(400)))
                                        } else if document && first && lane == "A" {
                                            (403, "<html><body><h1>Access denied</h1></body></html>".into())
                                        } else if document {
                                            (201, format!("<article><h1>Owned crawl route {lane}</h1><p>{}</p></article>",
                                                "Useful research content from the selected route must survive the actual HTTP to native browser handoff. ".repeat(45)))
                                        } else { (502, String::new()) };
                                        let response = format!("HTTP/1.1 {status} Owned\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                                        let _ = socket.write_all(response.as_bytes()).await;
                                    });
                                }
                                done = handlers.join_next(), if !handlers.is_empty() => { done.unwrap().unwrap(); }
                            }
                        }
                    }));
                }
                let daemon = Daemon::new().await.unwrap();
                let (crawler, governor) = crate::crawl::real::build(
                    Arc::clone(&daemon.fetcher),
                    pool,
                    Some(Arc::clone(&daemon.state)),
                );
                // Only the owned A lane is currently ready; fetch's pool opt-in
                // remains off. The crawl's independent lane choice must travel.
                for _ in 0..8 { governor.on_error("127.0.0.1", "direct"); }
                assert_eq!(governor.best_lane("127.0.0.1").unwrap().id,proxy.0.id());
                let crawler = crawler.with_ghost(make_ghost_hook(
                    Arc::clone(&daemon.ghost_mgr),daemon.profile.clone(),Arc::clone(&daemon.fetcher),Arc::clone(&daemon.state),true));
                let started = std::time::Instant::now();
                let result = crawler.crawl(&url,crate::crawl::CrawlOptions {
                    mode:crate::crawl::CrawlMode::Content,respect_robots:false,max_pages:1,max_depth:0,
                    deadline,..Default::default()
                },None).await.unwrap();
                assert!(started.elapsed() < deadline, "useful native content must arrive inside the original budget");
                daemon.ghost_mgr.shutdown().await;
                stop.send(true).unwrap();
                for server in servers { server.await.unwrap(); }
                let events = events.lock().unwrap();
                assert_eq!(result.pages.len(),1,"real HTTP denial must recover useful native content: {:?}",result.skipped);
                assert_eq!(events.len(),2,"one actual HTTP read and one native read: {events:?}");
                assert!(events.iter().all(|(lane,_)| *lane=="A"),"native recovery must retain the crawl's selected lane: {events:?}");
                assert!(result.pages[0].markdown.contains("Owned crawl route A"));
            });
        }).unwrap().join().unwrap();
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
    fn v471_search_health_counts_backends_not_topup_attempts() {
        use crate::search::{EngineReport, render_compact_markdown, render_markdown};
        let mut output = SearchOutcome {
            results: Vec::new(),
            weak: true,
            intent: Intent::Web,
            report: Vec::new(),
            cached: false,
            elapsed: Duration::ZERO,
            provider: None,
            reranked: false,
            instant: None,
            stage_ms: Vec::new(),
        };
        let report = |engine: &str, status: &str| EngineReport {
            engine: engine.into(),
            status: status.into(),
            profile: None,
            hits: 0,
            ms: 0,
            egress: "direct".into(),
        };
        output.report = vec![
            report("bing", "ok"),
            report("bing", "no-results"),
            report("bing", "pacing-timeout"),
            report("ddg", "blocked:403"),
        ];
        assert_eq!(search_model_meta(&output, &[])["failed_engines"], 1);
        let compact = render_compact_markdown(&output, "Results", None, &[]);
        assert!(compact.contains("1/2 backends available"), "{compact}");
        let full = render_markdown(&output, "query", None, &[]);
        assert!(full.contains("1/2 engines ok (ddg: blocked:403)"), "{full}");
        // A successful recovery or a cached provider remains available.
        output.report = vec![report("bing", "blocked:429"), report("bing", "cached")];
        assert!(search_model_meta(&output, &[]).get("degraded").is_none());
        assert!(!render_compact_markdown(&output, "Results", None, &[]).contains("Degraded"));
        assert!(!render_markdown(&output, "query", None, &[]).contains("degraded"));
        output.report = vec![
            report("bing", "blocked:429"),
            report("bing", "pacing-timeout"),
        ];
        assert_eq!(search_model_meta(&output, &[])["failed_engines"], 1);
        assert!(
            render_compact_markdown(&output, "Results", None, &[])
                .contains("0/1 backends available")
        );
    }

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
            instant: None,
            stage_ms: Vec::new(),
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
