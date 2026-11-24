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
    let read = args.get("read").and_then(Value::as_u64).unwrap_or(0) as usize;
    let lang = match args.get("lang").and_then(Value::as_str) {
        Some(raw) => match crate::search::Lang::parse(raw) {
            Ok(lang) => Some(lang),
            Err(error) => return tool_error(format!("search: {error}")),
        },
        None => None,
    };
    let intent = match args.get("intent").and_then(Value::as_str) {
        Some("web") => Some(Intent::Web),
        Some("code") => Some(Intent::Code),
        Some("paper") => Some(Intent::Paper),
        Some("news") => Some(Intent::News),
        Some("entity") => Some(Intent::Entity),
        Some("image") => Some(Intent::Image),
        _ => None,
    };
    let contact_sheet = args
        .get("contact_sheet")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    search::SEARCH_DEADLINE
        .scope(deadline.map(|d| tokio::time::Instant::now() + d), async {
            if intent == Some(Intent::Image) {
                let query = &queries[0];
                return run_with_budget(
                    search_images_inner(daemon, &queries, max, lang.as_ref(), contact_sheet),
                    deadline,
                    ctx.as_mut(),
                    || search_deadline_error(query),
                )
                .await;
            }
            if queries.len() == 1 {
                let query = &queries[0];
                run_with_budget(
                    search_inner(daemon, query, max, intent, read, lang.as_ref()),
                    deadline,
                    ctx.as_mut(),
                    || search_deadline_error(query),
                )
                .await
            } else {
                let deadline_queries = queries.clone();
                run_with_budget(
                    search_batch_inner(daemon, &queries, max, intent, read, lang.as_ref()),
                    deadline,
                    ctx.as_mut(),
                    move || search_batch_deadline_error(&deadline_queries),
                )
                .await
            }
        })
        .await
}

async fn search_images_inner(
    daemon: &Arc<Daemon>,
    queries: &[String],
    max: usize,
    lang: Option<&crate::search::Lang>,
    contact_sheet: bool,
) -> Value {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

    for q in queries {
        if let Some(problem) = search::validate_query(q) {
            return tool_error_structured(
                format!("search: {problem}"),
                "permanent",
                Some(json!({
                    "query": q,
                    "next_action": "fix the query and search again",
                })),
            );
        }
    }

    let requested = max.clamp(1, 12);
    let out =
        crate::search::images::search_images_multi(queries, requested, lang, contact_sheet).await;
    let urls: Vec<String> = out.results.iter().map(|h| h.image_url.clone()).collect();
    let handles = bind_search_urls(daemon, &urls).await;
    let handles_opt = (!handles.is_empty()).then_some(handles.as_slice());
    let md = crate::search::images::render_image_markdown_with_handles(&out, handles_opt);

    let mut content = Vec::new();
    if let Some(png) = &out.sheet_png {
        content.push(json!({
            "type": "image",
            "data": BASE64_STANDARD.encode(png),
            "mimeType": "image/png"
        }));
    }
    content.push(json!({ "type": "text", "text": md }));

    let results = out
        .results
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let mut item = json!({
                "rank": i + 1,
                "title": h.title,
                "image_url": h.image_url,
                "source": h.source,
            });
            if let Some(handle) = handles.get(i) {
                item["handle"] = json!(handle);
            }
            if let Some(t) = &h.thumb_url {
                item["thumb_url"] = json!(t);
            }
            if let Some(p) = &h.page_url {
                item["page_url"] = json!(p);
            }
            if let Some(w) = h.width {
                item["width"] = json!(w);
            }
            if let Some(ht) = h.height {
                item["height"] = json!(ht);
            }
            item
        })
        .collect::<Vec<_>>();

    let answered_sources = out.sources.iter().filter(|s| s.state == "answered").count();
    let degraded = answered_sources == 0 || out.results.is_empty();
    let elapsed_ms = out.elapsed.as_millis() as u64;

    let mut structured = json!({
        "ok": true,
        "intent": "image",
        "query": out.query,
        "provider": "keyless-images",
        "elapsed_ms": elapsed_ms,
        "filled": !out.results.is_empty(),
        "requested_results": requested,
        "returned_results": out.results.len(),
        "underfilled": out.results.len() < requested,
        "answered_sources": answered_sources,
        "degraded": degraded,
        "contact_sheet": out.sheet_png.is_some(),
        "results": results,
        "sources": out.sources,
    });
    if contact_sheet {
        structured["sheet_tiles"] = json!(out.sheet_tiles);
        structured["sheet_attempted"] = json!(out.sheet_attempted);
    }
    if let Some(note) = &out.sheet_note {
        structured["sheet_note"] = json!(note);
    }
    if out.results.is_empty() {
        structured["next_action"] =
            json!("no image source returned matching items; rephrase with visual nouns or retry");
    } else if out.results.len() < requested.min(3) {
        structured["next_action"] =
            json!("thin image answer; broaden terms or add a query_variant");
    }

    json!({
        "content": content,
        "structuredContent": structured,
        "isError": false,
    })
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
    read: usize,
    lang: Option<&crate::search::Lang>,
) -> Value {
    match search_read(daemon, query, max, intent, read, lang).await {
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
    let model = search_model_meta(out, &hs, requested);
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
pub(super) fn search_model_meta(
    out: &crate::search::SearchOutcome,
    handles: &[String],
    requested: usize,
) -> Value {
    let requested = requested.clamp(1, 12);
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
                "url": crate::search::evidence_url(&result.url),
                "source_type": crate::search::rank::source_type(&result.url),
            });
            if let Some(handle) = handles.get(index) {
                item["handle"] = json!(handle);
            }
            item
        })
        .collect::<Vec<_>>();
    // The retrieval state, as facts: who answered, with how many items, and
    // what every source that did not answer said. There is no verdict flag to
    // read and no degraded ratio to interpret -- the census is the state.
    let sources = crate::search::source_states(&out.report)
        .into_iter()
        .map(|s| {
            let age = out
                .report
                .iter()
                .filter(|report| report.engine == s.id)
                .filter_map(|report| report.cache_age_ms)
                .max();
            let mut source =
                json!({"id": s.id, "items": s.items, "status": s.status, "state": s.state});
            if let Some(age) = age {
                source["cache_age_ms"] = json!(age);
            }
            source
        })
        .collect::<Vec<_>>();
    let mut item = json!({
        "ok": true,
        "results": results,
        "sources": sources,
        "provider": out.provider.as_deref().unwrap_or("keyless"),
    });
    // The answer's standing, as fields a caller can branch on without reading
    // the census: a list to choose from, a thin list, or nothing.
    let answered = out
        .report
        .iter()
        .filter(|r| matches!(r.status.as_str(), "ok" | "cached" | "thin"))
        .count();
    item["filled"] = json!(!out.results.is_empty());
    item["requested_results"] = json!(requested);
    item["returned_results"] = json!(out.results.len());
    item["underfilled"] = json!(out.results.len() < requested);
    item["degraded"] = json!(out.quality.degraded());
    item["confidence"] = json!(out.quality.confidence);
    if let Some(age) = out.cache_age {
        item["cache_age_ms"] = json!(age.as_millis() as u64);
    }
    item["answered_sources"] = json!(answered);
    if out.results.is_empty() && !out.report.is_empty() {
        item["next_action"] = json!(
            "no source returned matching items; rephrase or retry another source before concluding that no answer exists"
        );
    } else if out.results.len() < requested.min(3) {
        item["next_action"] = json!("thin answer: a retry usually adds the sources that were late");
    } else if out.quality.degraded() {
        item["next_action"] = json!("limited independent evidence; retry for confirmation");
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
    read: usize,
    lang: Option<&crate::search::Lang>,
) -> Value {
    let started = std::time::Instant::now();
    let futures = queries
        .iter()
        .map(|query| search_read(daemon, query, max, intent, read, lang));
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
                let mut model = search_model_meta(out, query_handles.unwrap_or(&[]), max);
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

/// The search pipeline, then `read`: the top `read` results' pages are fetched
/// inside the same deadline and their passages replace the teasers, and a stub
/// snippet in the top three is repaired the same way. Applies to a provider's
/// results as much as to the local fan-out.
async fn search_read(
    daemon: &Arc<Daemon>,
    query: &str,
    max: usize,
    intent: Option<Intent>,
    read: usize,
    lang: Option<&crate::search::Lang>,
) -> Result<crate::search::SearchOutcome, SearchFailure> {
    let mut out = search_outcome(daemon, query, max, intent, read, lang).await?;
    if out.provider.is_none() {
        return Ok(out);
    }
    if read > 0 {
        daemon
            .searcher
            .read_results(&mut out.results, query, read)
            .await;
    }
    // A stub snippet in the top three is read from its page, inside what
    // `read` left of the same budget.
    daemon
        .searcher
        .repair_snippets(&mut out.results, query, read)
        .await;
    Ok(out)
}

/// Acquisition and bounded evidence reads without presentation, so multi-query
/// mode can share one deadline and one final handle table.
pub(super) async fn search_outcome(
    daemon: &Arc<Daemon>,
    query: &str,
    max: usize,
    intent: Option<Intent>,
    read: usize,
    lang: Option<&crate::search::Lang>,
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

    // BYOK-first mode: try providers, fall back to local. An explicit `lang`
    // keeps the local language-aware path: a provider that does not accept the
    // tag must not silently answer in its own locale.
    let byok_skipped_lang = byok_configured && !local_first && lang.is_some();
    let mut byok_fail: Option<String> = None;
    if byok_configured && !local_first && lang.is_none() {
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
    match daemon
        .searcher
        .search_with_read(query, max, intent, read, lang)
        .await
    {
        Ok(mut out) => {
            // #285 / #472: fold a BYOK failure or explicit-lang bypass into the
            // source census so a configured provider that did not answer is
            // named beside the local sources instead of vanishing.
            fold_byok_census(&mut out.report, byok_fail.as_deref(), byok_skipped_lang);
            Ok(out)
        }
        Err(e) => {
            // Local failed : if BYOK is configured and we're in local-first
            // mode, try BYOK as a last resort — unless an explicit `lang` was
            // requested (#472: never let a locale-unaware provider answer in
            // its own language).
            if byok_fallback_allowed(byok_configured, local_first, lang.is_some()) {
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
                let cause = if byok_configured && local_first && lang.is_some() {
                    format!("{e}; byok skipped: explicit lang")
                } else {
                    e.to_string()
                };
                Err(SearchFailure {
                    cause,
                    byok_tried: false,
                    kind: "transient",
                })
            }
        }
    }
}

/// Record a configured BYOK provider that did not answer — either because it
/// failed (#285) or because an explicit `lang` bypassed it (#472) — at the
/// front of `report` so the receipt is distinguishable from an unconfigured
/// keyless run.
fn fold_byok_census(
    report: &mut Vec<crate::search::EngineReport>,
    byok_fail: Option<&str>,
    byok_skipped_lang: bool,
) {
    let status = if let Some(e) = byok_fail {
        crate::search::byok::compact_failure(e)
    } else if byok_skipped_lang {
        "skipped: explicit lang".into()
    } else {
        return;
    };
    report.insert(
        0,
        crate::search::EngineReport {
            cache_age_ms: None,
            engine: "byok".into(),
            profile: None,
            status,
            hits: 0,
            ms: 0,
            egress: "byok".into(),
            requests: 0,
        },
    );
}

/// Last-resort BYOK fallback after a local failure is only permitted when
/// `local_first` is active, a provider is configured, and no explicit `lang`
/// constrains the answer language (#472).
fn byok_fallback_allowed(
    byok_configured: bool,
    local_first: bool,
    has_explicit_lang: bool,
) -> bool {
    byok_configured && local_first && !has_explicit_lang
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
                families: 0,
                from_page: false,
                tier: 0,
            }],
            intent: Intent::Web,
            report: Vec::new(),
            cached: false,
            quality: Default::default(),
            cache_age: None,
            elapsed: Duration::ZERO,
            provider: None,
            reranked: false,
            instant: None,
            requests: None,
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
            intent: Intent::Web,
            report: Vec::new(),
            cached: false,
            quality: Default::default(),
            cache_age: None,
            elapsed: Duration::ZERO,
            provider: None,
            reranked: false,
            instant: None,
            requests: None,
            stage_ms: Vec::new(),
        };
        let report = |engine: &str, status: &str| EngineReport {
            cache_age_ms: None,
            engine: engine.into(),
            status: status.into(),
            profile: None,
            hits: 0,
            ms: 0,
            egress: "direct".into(),
            requests: 0,
        };
        output.report = vec![
            report("bing", "ok"),
            report("bing", "no-results"),
            report("bing", "pacing-timeout"),
            report("ddg", "blocked:403"),
        ];
        // One state per source, whatever its retries: bing answered (a
        // successful attempt outlives its retries), ddg did not.
        // `status` is the raw outcome a 5.1 parser reads; `state` is the
        // census word beside it.
        let sources = search_model_meta(&output, &[], 5)["sources"].clone();
        assert_eq!(sources[0]["id"], "bing");
        assert_eq!(sources[0]["status"], "ok");
        assert_eq!(sources[0]["state"], "answered");
        assert_eq!(sources[1]["id"], "ddg");
        assert_eq!(sources[1]["status"], "403");
        assert_eq!(sources[1]["state"], "failed");
        let compact = render_compact_markdown(&output, "Results", None, &[]);
        assert!(
            compact.contains("sources: bing 0 · ddg — failed"),
            "{compact}"
        );
        let full = render_markdown(&output, "query", None, &[]);
        assert!(full.contains("sources: bing 0 · ddg — failed"), "{full}");
        // A cached provider answer is still an answer, not a failure.
        output.report = vec![report("bing", "blocked:429"), report("bing", "cached")];
        let cached = search_model_meta(&output, &[], 5)["sources"][0].clone();
        assert_eq!(cached["status"], "cached", "a cached answer is an answer");
        assert_eq!(cached["state"], "answered", "a cached answer is an answer");
        output.report = vec![
            report("bing", "blocked:429"),
            report("bing", "pacing-timeout"),
        ];
        let refused = search_model_meta(&output, &[], 5)["sources"][0].clone();
        assert_eq!(refused["status"], "429");
        assert_eq!(refused["state"], "failed");
        // A published rate limit rests the source; it did not fail.
        output.report = vec![report("stackexchange", "rate-limited:3219")];
        let rested = search_model_meta(&output, &[], 5)["sources"][0].clone();
        assert_eq!(rested["status"], "rate-limited:3219");
        assert_eq!(rested["state"], "rested");
        // A source the plan never asked is in the machine census but not in the
        // footer the model reads; resting sources are counted there, not named.
        output.report = vec![
            report("bing", "ok"),
            report("google", "not-asked"),
            report("mojeek", "rested:120"),
            report("yahoo", "rested"),
        ];
        let sources = search_model_meta(&output, &[], 5)["sources"].clone();
        assert_eq!(sources[1]["state"], "not asked");
        assert_eq!(sources[2]["state"], "rested");
        let compact = render_compact_markdown(&output, "Results", None, &[]);
        assert!(compact.contains("sources: bing 0 · 2 resting"), "{compact}");
        assert!(!compact.contains("google"), "{compact}");
        assert!(!compact.contains("mojeek"), "{compact}");
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
                families: 0,
                from_page: false,
                tier: 0,
            }],
            intent: Intent::Web,
            report: Vec::new(),
            cached: false,
            quality: Default::default(),
            cache_age: None,
            elapsed: Duration::from_millis(10),
            provider: None,
            reranked: true,
            instant: None,
            requests: None,
            stage_ms: Vec::new(),
        };
        let state = search_model_meta(&output, &["S1".into()], 5);
        assert_eq!(state["underfilled"], true);
        let single = search_model_meta(&output, &[], 1);
        assert_eq!(single["underfilled"], false);
        assert_eq!(single["requested_results"], 1);
        assert!(
            !single["next_action"]
                .as_str()
                .unwrap_or_default()
                .contains("thin answer")
        );
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
        // #472: model metadata and debug metadata agree on `provider`.
        assert_eq!(state["provider"], "keyless");
        assert_eq!(debug["provider"], "keyless");
    }

    #[test]
    fn issue_472_explicit_lang_byok_bypass_is_named_and_fallback_respects_lang() {
        use super::{byok_fallback_allowed, fold_byok_census};

        let mut report = vec![crate::search::EngineReport {
            cache_age_ms: None,
            engine: "bing".into(),
            profile: None,
            status: "ok".into(),
            hits: 5,
            ms: 400,
            egress: "direct".into(),
            requests: 1,
        }];
        // Regression: BYOK-first skipped because of explicit `lang` surfaces in
        // both the JSON `sources` census (`state: "not asked"`) and the
        // Markdown census line.
        fold_byok_census(&mut report, None, true);
        let states = crate::search::source_states(&report);
        let byok = states
            .iter()
            .find(|s| s.id == "byok")
            .expect("byok entry must appear in sources");
        assert_eq!(byok.status, "skipped: explicit lang");
        assert_eq!(byok.state, "not asked");
        let md_census = crate::search::census(&report);
        assert!(
            md_census.contains("byok — skipped: explicit lang"),
            "markdown census must disclose bypass: {md_census}"
        );

        // Negative: unconfigured BYOK emits no `byok` entry.
        let mut unconfigured = vec![report[1].clone()];
        fold_byok_census(&mut unconfigured, None, false);
        assert!(!unconfigured.iter().any(|r| r.engine == "byok"));

        // Regression + negative for local-first last-resort BYOK fallback:
        // allowed without explicit lang, forbidden when explicit lang is set.
        assert!(byok_fallback_allowed(true, true, false));
        assert!(!byok_fallback_allowed(true, true, true));
        assert!(!byok_fallback_allowed(false, true, false));
        assert!(!byok_fallback_allowed(true, false, false));
    }
}
