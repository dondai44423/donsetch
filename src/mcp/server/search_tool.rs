//! The search tool handler: dispatch, query parsing (incl. the
//! 2-variant batch), single + batch search flows, handle/URL
//! binding, model/debug meta surfaces and the search error contract.

use serde_json::{Value, json};

use super::errors::batch_failure_kind;
use super::fetch_tool::{bind_search_handles, bind_search_urls, route_hints};
use super::*;
pub(super) async fn search_tool(
    daemon: &Arc<Daemon>,
    args: &Value,
    mut ctx: Option<ToolCtx>,
) -> Value {
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
        Ok(out) => render_search_outcome(daemon, &out, max).await,
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
                    crate::diag!("[byok] all providers exhausted, falling back to local: {e}");
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
                    crate::diag!("[byok] local search failed, trying BYOK fallback: {e}");
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

    #[tokio::test]
    async fn cli_search_uses_urls_while_mcp_handles_stay_in_session() {
        use super::render_search_outcome;
        use crate::mcp::server::Daemon;
        use crate::mcp::server::fetch_tool::resolve_fetch_url;
        use std::sync::Arc;
        // Nextest gives this test its own process and config/state root.
        let mut daemon = Daemon::new().await.unwrap();
        daemon.search_handles = false;
        let cli = Arc::new(daemon);
        let output = SearchOutcome {
            results: vec![Merged {
                title: "Answer".into(),
                url: "https://example.com/answer".into(),
                snippet: "Evidence".into(),
                sources: vec![("bing".into(), 0)],
                score: 0.9,
                published: None,
            }],
            weak: false,
            intent: Intent::Web,
            report: Vec::new(),
            cached: false,
            elapsed: Duration::ZERO,
            provider: None,
            reranked: false,
            instant: None,
            stage_ms: Vec::new(),
        };
        let result = render_search_outcome(&cli, &output, 1).await;
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("1. https://example.com/answer")
        );
        assert!(
            result["structuredContent"]["results"][0]
                .get("handle")
                .is_none()
        );
        let second = Arc::new(Daemon::new().await.unwrap());
        let url = result["structuredContent"]["results"][0]["url"]
            .as_str()
            .unwrap();
        assert_eq!(resolve_fetch_url(&second, url).await.unwrap(), url);
        let mcp = render_search_outcome(&second, &output, 1).await;
        let handle = mcp["structuredContent"]["results"][0]["handle"]
            .as_str()
            .unwrap();
        assert_eq!(resolve_fetch_url(&second, handle).await.unwrap(), url);
        assert_eq!(
            resolve_fetch_url(&second, &format!("@{handle}"))
                .await
                .unwrap(),
            url
        );
        // Never persist S-handles to fix the CLI: another session cannot resolve them.
        assert!(resolve_fetch_url(&cli, handle).await.is_err());
        assert!(
            resolve_fetch_url(&second, "@https://example.com/")
                .await
                .is_err()
        );
        cli.shutdown().await;
        second.shutdown().await;
    }

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
