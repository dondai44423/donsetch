//! The crawl tool handler: dispatch, map/content mode, and the
//! CrawlResult rendering + next-action guidance for the model.

use serde_json::{Value, json};
use std::borrow::Cow;

use super::*;
#[allow(clippy::field_reassign_with_default)]
pub(super) async fn crawl_tool(daemon: &Arc<Daemon>, args: &Value, ctx: Option<ToolCtx>) -> Value {
    daemon.refresh_vault().await;
    // Resume can work without a url (the seed is stored in the
    // resume state). If url is missing AND no resume token, error.
    let url = match args.get("url").and_then(Value::as_str) {
        Some(u) if u.starts_with("http://") || u.starts_with("https://") => u.to_string(),
        // Empty string (the CLI's explicit resume-only positional) and
        // a missing key are the same case: the seed is loaded from
        // the resume state.
        None | Some("") => {
            if args.get("resume").and_then(Value::as_str).is_none() {
                return tool_error("crawl: url required (or provide resume token to continue)");
            }
            String::new()
        }
        Some(u) => return tool_error(format!("crawl: url must be http(s), got: {u}")),
    };
    let mut opts = CrawlOptions::default();
    opts.focus = args.get("focus").and_then(Value::as_str).map(String::from);
    opts.mode = match args.get("mode").and_then(Value::as_str).unwrap_or("full") {
        "map" => CrawlMode::Map,
        "content" => CrawlMode::Content,
        _ => CrawlMode::Full,
    };
    if let Some(n) = args.get("max_pages").and_then(Value::as_u64) {
        opts.max_pages = n.clamp(1, 200) as usize;
    }
    if let Some(n) = args.get("max_depth").and_then(Value::as_u64) {
        opts.max_depth = n.clamp(0, 8) as u32;
    }
    if let Some(n) = args.get("max_total_chars").and_then(Value::as_u64) {
        opts.max_total_chars = (n as usize).clamp(4_000, 500_000);
    }
    if let Some(n) = args.get("per_page_max").and_then(Value::as_u64) {
        opts.per_page_max = (n as usize).clamp(400, 40_000);
    }
    if let Some(a) = args.get("include_paths").and_then(Value::as_array) {
        opts.include_paths = a
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
    }
    if let Some(a) = args.get("exclude_paths").and_then(Value::as_array) {
        opts.exclude_paths = a
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
    }
    if let Some(b) = args.get("same_host").and_then(Value::as_bool) {
        opts.same_host = b;
    }
    if let Some(b) = args.get("respect_robots").and_then(Value::as_bool) {
        opts.respect_robots = b;
    }
    if let Some(n) = args.get("deadline_s").and_then(Value::as_u64) {
        opts.deadline = std::time::Duration::from_secs(n.clamp(5, 600));
    }
    if let Some(q) = args.get("min_quality").and_then(Value::as_f64) {
        opts.min_quality = q.clamp(0.0, 1.0) as f32;
    }
    // v4 phase 3: dataset mode is output-format only.
    if args
        .get("dataset")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        opts.dataset = true;
    }
    let resume = args.get("resume").and_then(Value::as_str).map(String::from);

    // v4 phase 3 delta crawl: pages are re-checked and their fresh
    // fingerprint compared with page history; only changed or new
    // pages land in the results. Recording happens for every fetched
    // page below, so crawls keep feeding the same memory fetches do.
    if args
        .get("since_last")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let hist = Arc::clone(&daemon.history);
        opts.delta_unchanged = Some(Arc::new(move |url: &str, fp: &str| {
            hist.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .matches_fingerprint(url, fp)
        }));
    }
    {
        let hist = Arc::clone(&daemon.history);
        opts.on_page = Some(Arc::new(
            move |url: &str, fp: Option<&str>, md: &str, title: Option<&str>| {
                if let Some(fp) = fp {
                    let mut h = hist
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    h.record(url, fp, md.len(), title, md);
                }
            },
        ));
    }

    // v3: cancellation + progress. The crawl stops its workers
    // gracefully on cancel (the stop-flag mechanism) and persists
    // its resume token : partial progress is never lost.
    if let Some(c) = &ctx {
        opts.cancel = Some(c.cancel_receiver());
        let parts = c.progress_parts();
        let last_emit = Arc::new(std::sync::atomic::AtomicU64::new(0));
        opts.progress = Some(Arc::new(move |done, queued| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            // Throttle: first pages + one beat every 2s.
            if done <= 2
                || now.saturating_sub(last_emit.load(std::sync::atomic::Ordering::Relaxed)) > 2_000
            {
                last_emit.store(now, std::sync::atomic::Ordering::Relaxed);
                emit_progress(
                    &parts,
                    done as u64,
                    None,
                    &format!("{done} pages, {queued} queued"),
                );
            }
        }));
    }

    // Centralized guard (SSRF and local rules) on the seed. A url-less
    // resume guards the seed stored in its token: the peek reads the
    // token without consuming it, so a seed refused here leaves the
    // token usable once the operator changes the rule. An unreadable
    // token is not this check's to report: crawl() takes it below and
    // returns its own resume-token error.
    let guarded_seed = if url.is_empty() {
        resume
            .as_deref()
            .and_then(|tok| crate::crawl::resume_store_peek(tok).ok())
    } else {
        Some(url.clone())
    };
    if let Some(seed) = &guarded_seed
        && let Some(refusal) = seed_guard_error(seed)
    {
        return refusal;
    }

    // Ghost-warm: if this host was tier-2 solved recently, the
    // clearance cookies ride tier 1 from page one.
    if let Some(host) = url::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
    {
        let route = daemon.state.lock().await.route_for(&host);
        if let RouteDecision::Warm(cookies) = route {
            daemon.fetcher.import_cookies(&cookies).await;
        }
    }

    // Map mode: thin static inventories get one bounded rendered read
    // of the seed through the same ghost path fetch/screenshot own;
    // any failure keeps the static map (the renderer returns None).
    if opts.mode == CrawlMode::Map {
        let daemon = Arc::clone(daemon);
        opts.render_html = Some(std::sync::Arc::new(move |url: String| {
            let daemon = Arc::clone(&daemon);
            Box::pin(async move { rendered_seed_html(&daemon, &url).await })
        }));
    }

    let requested_mode = opts.mode;
    let dataset = opts.dataset;
    let crawl_t0 = std::time::Instant::now();
    let result = match daemon.crawler.crawl(&url, opts, resume.as_deref()).await {
        Ok(r) => {
            // Batch-flush the fingerprints the crawl just recorded.
            daemon
                .history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .flush();
            r
        }
        Err(e) => {
            // Crawl failures are input errors (bad seed / expired
            // resume token) : permanent, not worth a blind retry.
            // Classify honestly so the agent doesn't burn calls.
            let msg = e.to_ascii_lowercase();
            let (kind, hint) = if msg.contains("resume token") {
                (
                    "permanent",
                    "the resume token is expired or unknown : start a fresh crawl (omit resume)",
                )
            } else if msg.contains("bad seed") || msg.contains("must have a host") {
                (
                    "permanent",
                    "check the seed URL format (full scheme + host, e.g. https://example.com/docs/)",
                )
            } else {
                (
                    "transient",
                    "retry may work; if repeated, check network/access or use another source",
                )
            };
            let mut trace = Trace::default();
            trace.step("crawl", "crawl", "error", crawl_t0.elapsed().as_millis());
            return tool_error_structured(
                format!("crawl: {e}"),
                kind,
                Some(json!({
                    "url": url,
                    "escalation": trace.value(),
                    "next_action": hint,
                })),
            );
        }
    };

    if let Some(failure) = &result.seed_failure {
        let mut trace = Trace::default();
        trace.step(
            "crawl",
            "crawl",
            "seed_failed",
            crawl_t0.elapsed().as_millis(),
        );
        return seed_failure_error(failure, Some(trace.value()));
    }

    let mut rendered = render_crawl_result(&result, requested_mode, dataset);
    if !url.is_empty() && result.seed != url {
        rendered["structuredContent"]["requested_seed"] = json!(url);
        rendered["structuredContent"]["resolved_seed"] = json!(result.seed);
    }
    rendered
}

/// One rendered read of the seed for a thin map inventory. Returns
/// (rendered HTML, final URL); None on any failure, and for a read that
/// is not the seed's content (`rendered_seed_usable`) : the map keeps its
/// static inventory and never dies because the browser cannot start.
async fn rendered_seed_html(daemon: &Arc<Daemon>, url: &str) -> Option<(String, String)> {
    let target = crate::fetch::guards::validate_url_basic(url).ok()?;
    if crate::fetch::guards::ensure_url_safe(target.as_str())
        .await
        .is_err()
    {
        return None;
    }
    let host = target.host_str()?.to_string();
    let wire = {
        let mut state = daemon.state.lock().await;
        let caps = crate::persona::PersonaCaps::from_profile(daemon.fetcher.profile());
        state.ensure_persona(&host, &caps);
        state.ensure_persona_egress(&host);
        let mut wire = state
            .personas
            .get(&host)
            .filter(|p| p.quarantine_reason.is_none())
            .map(|p| p.ghost_wire())
            .unwrap_or_default();
        let route = daemon.fetcher.route_for_fetch(target.as_str());
        wire.route = Some(route);
        wire
    };
    let ghost = daemon
        .ghost_mgr
        .acquire_for_wire(&daemon.profile, Some(&host), wire)
        .await
        .ok()?;
    let read = daemon
        .ghost_mgr
        .read_document(
            ghost,
            &daemon.profile,
            target.as_str(),
            std::time::Duration::from_secs(20),
        )
        .await
        .ok()?;
    // The static harvest reads the buffered seed only when it is
    // ContentOk and 2xx; the rendered one takes the same gate, or a
    // dead seed's 404 page (site header and footer links) would come
    // back as a complete map.
    if !rendered_seed_usable(read.page.outcome, read.page.document.status) {
        return None;
    }
    let ghost = read.guard;
    let html = ghost.outer_html().await.ok()?;
    let final_url = ghost
        .current_url()
        .await
        .unwrap_or_else(|_| url.to_string());
    Some((html, final_url))
}

/// True when a rendered seed read counts as the seed's content for the
/// map harvest: the browser outcome is `Content` or `Incomplete`, and
/// the document's HTTP status, when known, is 2xx. A 404, a wall, a
/// login or a paywall page is never harvested.
fn rendered_seed_usable(outcome: ops::BrowserOutcome, status: Option<u16>) -> bool {
    // Incomplete is a page whose content oracle did not settle in time,
    // not a refusal; today's harvest reads it and keeps doing so. The
    // status check is needed beside the outcome: a 404 page with a full
    // site header classifies as Content by its visible text alone.
    matches!(
        outcome,
        ops::BrowserOutcome::Content | ops::BrowserOutcome::Incomplete
    ) && status.is_none_or(|s| (200..300).contains(&s))
}

/// The refusal for a seed the guard rejects, or None when it passes.
/// A rule denial, of the seed or of its adapter rewrite, becomes the
/// policy error; any other guard error keeps its plain text.
fn seed_guard_error(seed: &str) -> Option<Value> {
    match crate::fetch::guards::validate_url_basic(seed) {
        Err(e @ FetchError::Denied { .. }) => Some(policy_error_value(&e, seed, None)),
        Err(e) => Some(tool_error(format!("{e}"))),
        Ok(parsed) => {
            // The crawl fetcher asks the wire for the adapter rewrite
            // (crawl/real.rs), so a seed whose rewrite lands on a denied
            // host is refused like a directly denied one.
            let (rewritten, _via) = crate::adapters::rewrite(&parsed)?;
            let denial = crate::rules::rules().denial_for_str(&rewritten)?;
            Some(policy_error_value(&denial.into_error(), seed, None))
        }
    }
}

/// The structured tool error for a crawl whose seed failed with nothing
/// else to fetch. A rule refusal (the seed redirected into a denied
/// host) is the policy error plus `requested_url` and `landing_url`;
/// every other cause carries the code `web_fetch` gives the same failure
/// (`content.notfound`, `wall.*`, `network.*`; `crawl.robots_disallow`,
/// `crawl.robots_unreachable` or `crawl.seed_failed` where it has
/// none), the seed's `status`,
/// `verdict`, `error` and `robots` evidence, a `discovery` note and a
/// `next_action` and kind for that cause.
fn seed_failure_error(failure: &crate::crawl::SeedFailure, escalation: Option<Value>) -> Value {
    use crate::crawl::RobotsRefusal;

    let landing = failure
        .landing
        .as_deref()
        .filter(|landing| *landing != failure.requested);
    if let Some(denial) = &failure.denial {
        let mut value = policy_error_value(
            &denial.clone().into_error(),
            landing.unwrap_or(&failure.requested),
            escalation,
        );
        value["structuredContent"]["requested_url"] = json!(failure.requested);
        if let Some(landing) = landing {
            value["structuredContent"]["landing_url"] = json!(landing);
        }
        return value;
    }

    let status = failure.status;
    let verdict = failure.verdict.as_deref().unwrap_or("");
    let is_wall = verdict.starts_with("Challenge")
        || matches!(verdict, "Blocked" | "AuthWall" | "Paywall")
        || matches!(status, 401 | 403);
    // The code is the one web_fetch gives the same failure, so a 404 seed
    // also gets `read_status: "notfound"` and a `suggested_query` from
    // tool_error_structured. The crawl codes cover only causes web_fetch
    // has no code for; `crawl.seed` stays the malformed-seed input error.
    const FALLBACK: &str = "crawl.seed_failed";
    let (kind, code, cause, next_action): (&str, Cow<'static, str>, String, &str) = match failure
        .robots
    {
        Some(RobotsRefusal::Disallow) => (
            "walled",
            Cow::Borrowed("crawl.robots_disallow"),
            "the origin's robots.txt disallows the seed".to_string(),
            "use another source, or pass respect_robots=false (the explicit bypass) if ignoring this site's robots.txt is acceptable",
        ),
        // No bypass hint: switching robots off during an outage would
        // undo the unreachable-means-disallow rule (#351).
        Some(RobotsRefusal::Unreachable) => (
            "transient",
            Cow::Borrowed("crawl.robots_unreachable"),
            "the origin's robots.txt could not be read, which counts as disallow-all".to_string(),
            "retry later, once the site's robots.txt is reachable again",
        ),
        None if matches!(status, 404 | 410) || verdict == "SoftNotFound" => (
            "permanent",
            Cow::Borrowed("content.notfound"),
            format!("the seed returned {status} (not found)"),
            "check the seed URL (typo? deleted page?), or web_search the page title to find the moved copy",
        ),
        // A fetch error reaches the crawl as status 0 with a Blocked
        // verdict (crawl/real.rs), so status 0 is tested before the walls.
        // Its error survives only as text, so the text classifier names
        // it; its default (`content.extract`) is no network code, and the
        // crawl fallback takes over.
        None if status == 0 => (
            "transient",
            failure
                .error
                .as_deref()
                .map(|error| error_code(error, None))
                .filter(|code| {
                    ["network.", "tls.", "proxy.", "deadline.", "guard."]
                        .iter()
                        .any(|family| code.starts_with(family))
                })
                .unwrap_or(Cow::Borrowed(FALLBACK)),
            match &failure.error {
                Some(error) => format!("the seed fetch got no response ({error})"),
                None => "the seed fetch got no response".to_string(),
            },
            "retry once; if it repeats, check that the seed host is reachable, or use another source",
        ),
        // Ahead of the walls: a 429 or 503 carries a Blocked verdict, and
        // web_fetch reads Blocked on those statuses as transient
        // (`verdict_kind`), not as a wall.
        None if status == 429 || status >= 500 => (
            "transient",
            Cow::Borrowed(if status == 429 {
                "network.ratelimit"
            } else {
                FALLBACK
            }),
            format!("the seed host answered {status}"),
            "wait a few minutes and retry the crawl",
        ),
        None if is_wall => (
            "walled",
            Some(error_code("", Some(&json!({ "verdict": verdict }))))
                .filter(|code| code.starts_with("wall."))
                .unwrap_or(Cow::Borrowed(if status == 401 {
                    "wall.auth"
                } else {
                    "wall.blocked"
                })),
            format!("the site walled the seed fetch ({status})"),
            "fetch the seed URL with web_fetch, which can escalate to a browser, or use another source",
        ),
        None => (
            if (400..500).contains(&status) {
                "permanent"
            } else {
                "transient"
            },
            Cow::Borrowed(FALLBACK),
            format!("the seed returned no readable page ({status})"),
            "check the seed URL, or use a different seed URL",
        ),
    };
    let discovery = if failure.has_sitemap_phase {
        "discovery found no sitemap entries to crawl instead"
    } else {
        "this mode has no sitemap phase"
    };
    let mut text = format!("crawl: seed {} failed", failure.requested);
    if let Some(landing) = landing {
        text.push_str(&format!(" (redirected to {landing})"));
    }
    text.push_str(&format!(": {cause}; {discovery}"));
    let mut structured = json!({
        "code": code,
        "url": failure.requested,
        "requested_url": failure.requested,
        "status": status,
        "discovery": discovery,
        "next_action": next_action,
    });
    if let Some(landing) = landing {
        structured["landing_url"] = json!(landing);
    }
    if let Some(verdict) = &failure.verdict {
        structured["verdict"] = json!(verdict);
    }
    if let Some(error) = &failure.error {
        structured["error"] = json!(error);
    }
    if let Some(robots) = failure.robots {
        structured["robots"] = json!(match robots {
            RobotsRefusal::Disallow => "disallow",
            RobotsRefusal::Unreachable => "unreachable",
        });
    }
    if let Some(escalation) = escalation {
        structured["escalation"] = escalation;
    }
    tool_error_structured(text, kind, Some(structured))
}

/// Queued URLs shown in the debug block. The queue itself holds up to
/// `crawl::frontier::MAX_QUEUE`; the response shows the head and says
/// how many there are.
pub(super) const QUEUED_PREVIEW: usize = 100;

fn queued_preview(queued: &[String]) -> Value {
    json!(queued.iter().take(QUEUED_PREVIEW).collect::<Vec<_>>())
}

pub(super) fn render_crawl_result(
    result: &crate::crawl::CrawlResult,
    requested_mode: CrawlMode,
    dataset: bool,
) -> Value {
    if dataset {
        return render_crawl_dataset(result, requested_mode);
    }
    // One linear evidence document: page identity and body appear exactly once.
    let mut text = String::new();
    text.push_str(&format!("# Crawl\n{}\n\n", result.seed));
    // The count sits at the head as well as in the trailing section, so
    // it survives a client that truncates a long response.
    if !result.denied.is_empty() {
        text.push_str(&format!(
            "Denied by local DonSeTch rules: {}, listed at the end.\n\n",
            url_count(result.denied.total())
        ));
    }
    if requested_mode == CrawlMode::Map {
        text.push_str("## Discovered URLs\n");
        for u in &result.map {
            text.push_str(&format!("- {u}\n"));
        }
        text.push('\n');
    }
    if requested_mode != CrawlMode::Map {
        for (index, page) in result
            .pages
            .iter()
            .filter(|page| !page.duplicate)
            .enumerate()
        {
            text.push_str(&format!("## [{}]", index + 1));
            if !page.title.is_empty() {
                text.push_str(&format!(" {}", page.title));
            }
            text.push('\n');
            text.push_str(&page.url);
            let body = strip_source_frontmatter(
                &page.markdown,
                &page.url,
                (!page.title.is_empty()).then_some(page.title.as_str()),
            );
            if !body.is_empty() {
                text.push_str("\n\n");
                text.push_str(&body);
            }
            text.push_str("\n\n---\n\n");
        }
        if requested_mode == CrawlMode::Full {
            let rendered = result
                .pages
                .iter()
                .filter(|page| !page.duplicate)
                .map(|page| page.url.as_str())
                .collect::<std::collections::HashSet<_>>();
            let remaining = result
                .map
                .iter()
                .filter(|url| !rendered.contains(url.as_str()))
                .collect::<Vec<_>>();
            if !remaining.is_empty() {
                text.push_str(&format!("## Discovered URLs not fetched\n{} URLs; inventory is in structuredContent.map.\n", remaining.len()));
            }
        }
    }
    if !result.denied.is_empty() {
        if !text.ends_with("\n\n") {
            text.push('\n');
        }
        text.push_str(&denied_section(&result.denied));
    }

    let next_action = compute_crawl_next_action(result);
    let mut structured = json!({
        "ok": true,
        "seed": result.seed,
        "complete": crawl_complete(result, requested_mode),
        "pages": result.pages.iter().filter(|p| !p.duplicate).map(|p| json!({
            "url": p.url,
            "lastmod": p.lastmod,
            "content_complete": p.next_offset.is_none() && p.partial.is_none(),
            "partial": p.partial,
            "next_offset": p.next_offset,
        })).collect::<Vec<_>>(),
        "stop": format!("{:?}", result.stop),
    });
    if requested_mode != CrawlMode::Content {
        structured["map"] = json!(result.map);
    }
    if requested_mode == CrawlMode::Map {
        structured.as_object_mut().unwrap().remove("pages");
        structured["mode"] = json!("map");
        structured["discovered"] = json!(result.map.len());
        if crawl_complete(result, requested_mode) {
            structured["stop"] = json!("InventoryComplete");
        }
    }
    if let Some(resume) = &result.resume {
        structured["resume"] = json!(resume);
    }
    if !next_action.is_empty() {
        structured["next_action"] = json!(next_action);
    }
    if !result.denied.is_empty() {
        structured["denied_by_local_rules"] = denied_structured(&result.denied);
    }
    let debug = json!({
        "mode": format!("{:?}", requested_mode),
        "map": result.map,
        "queued": queued_preview(&result.queued),
        "queued_total": result.queued.len(),
        "filtered_out": result.filtered_out,
        "skipped": result.skipped.iter().map(|(u, w)| json!({"url": u, "reason": w})).collect::<Vec<_>>(),
        "pages": result.pages.iter().map(|p| json!({
            "url": p.url,
            "title": p.title,
            "kind": format!("{:?}", p.kind),
            "chars": p.chars,
            "next_offset": p.next_offset,
            "quality": p.quality,
            "duplicate": p.duplicate,
            "parent": p.parent,
            "score": (p.score * 100.0).round() / 100.0,
            "lastmod": p.lastmod,
        })).collect::<Vec<_>>(),
        "crawl_delay": result.crawl_delay,
        "elapsed_s": result.elapsed.as_secs_f64(),
    });
    json!({
        "content": [{"type": "text", "text": text.trim_end()}],
        "structuredContent": structured,
        "_meta": {"com.donsetch/crawl-debug": debug},
    })
}

/// Dataset mode (v4 phase 3): one JSON object per fetched page,
/// JSON Lines. Rows are sorted by URL for deterministic output
/// (delta-friendly diffs across recrawls). Duplicate content pages
/// are dropped: a dataset wants one row per page. serde_json does
/// the escaping, so every row is valid JSON by construction.
pub(super) fn render_crawl_dataset(
    result: &crate::crawl::CrawlResult,
    requested_mode: CrawlMode,
) -> Value {
    let mut rows: Vec<Value> = result
        .pages
        .iter()
        .filter(|p| !p.duplicate)
        .map(|p| {
            json!({
                "url": p.url,
                "title": p.title,
                "kind": format!("{:?}", p.kind),
                "markdown": p.markdown,
                "chars": p.chars,
                "fetched_at": p.fetched_at,
                "lastmod": p.lastmod,
                "parent": p.parent,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a["url"]
            .as_str()
            .unwrap_or("")
            .cmp(b["url"].as_str().unwrap_or(""))
    });

    let mut text = String::new();
    for row in &rows {
        text.push_str(&serde_json::to_string(row).unwrap_or_default());
        text.push('\n');
    }

    let next_action = compute_crawl_next_action(result);
    let mut structured = json!({
        "ok": true,
        "seed": result.seed,
        "dataset": true,
        // Schema marker (v4 F). Row shape: url/title/kind/markdown/
        // chars/fetched_at/lastmod/parent. Bump on any row-field change
        // so downstream parsers can fail closed.
        "dataset_version": 1,
        "rows": rows.len(),
        // Summed row chars so CLI/MCP summaries report real volume in
        // dataset mode (the JSON mode's pages[] is absent here).
        "chars": rows
            .iter()
            .filter_map(|r| r.get("chars").and_then(|c| c.as_u64()))
            .sum::<u64>(),
        "complete": crawl_complete(result, requested_mode),
        "stop": format!("{:?}", result.stop),
    });
    let partial_pages = result
        .pages
        .iter()
        .filter(|p| !p.duplicate && (p.next_offset.is_some() || p.partial.is_some()))
        .map(|p| json!({"url": p.url, "next_offset": p.next_offset, "partial": p.partial}))
        .collect::<Vec<_>>();
    if !partial_pages.is_empty() {
        structured["partial_pages"] = json!(partial_pages);
    }
    if requested_mode != CrawlMode::Content {
        structured["map"] = json!(result.map);
    }
    if let Some(resume) = &result.resume {
        structured["resume"] = json!(resume);
    }
    if !next_action.is_empty() {
        structured["next_action"] = json!(next_action);
    }
    // JSON Lines leave no room for a text section: the denials live
    // only here (and in the [meta] fold, see compat::shape_result).
    if !result.denied.is_empty() {
        structured["denied_by_local_rules"] = denied_structured(&result.denied);
    }
    let debug = json!({
        "mode": format!("{:?}", requested_mode),
        "rows": rows.len(),
        "queued": queued_preview(&result.queued),
        "queued_total": result.queued.len(),
        "filtered_out": result.filtered_out,
        "skipped": result
            .skipped
            .iter()
            .map(|(u, w)| json!({ "url": u, "reason": w }))
            .collect::<Vec<_>>(),
        "crawl_delay": result.crawl_delay,
        "elapsed_s": result.elapsed.as_secs_f64(),
    });
    json!({
        "content": [{ "type": "text", "text": text.trim_end() }],
        "structuredContent": structured,
        "_meta": { "com.donsetch/crawl-debug": debug },
    })
}

fn url_count(n: usize) -> String {
    if n == 1 {
        "1 URL".into()
    } else {
        format!("{n} URLs")
    }
}

/// The trailing "Denied by local DonSeTch rules" section: one group per
/// rule with its kind phrase, the operator's message as `Next action:`
/// and the group's capped URLs. The rule key never appears in it.
fn denied_section(denied: &crate::crawl::DeniedLog) -> String {
    let mut out = String::from(
        "## Denied by local DonSeTch rules (not fetched; web_fetch would refuse them too)\n",
    );
    for group in &denied.groups {
        out.push('\n');
        out.push_str(crate::rules::kind_phrase(&group.kind));
        out.push('\n');
        out.push_str(&format!("Next action: {}\n", group.message));
        for url in &group.urls {
            out.push_str(&format!("- {url}\n"));
        }
        let rest = group.count.saturating_sub(group.urls.len());
        if group.urls.is_empty() {
            out.push_str(&format!("{}\n", url_count(group.count)));
        } else if rest > 0 {
            out.push_str(&format!("…and {rest} more\n"));
        }
    }
    out
}

/// `denied_by_local_rules` for `structuredContent`: the total and one
/// object per rule with `rule`, `errorKind`, `next_action`, `count` and
/// the capped `urls`.
fn denied_structured(denied: &crate::crawl::DeniedLog) -> Value {
    let groups = denied
        .groups
        .iter()
        .map(|group| {
            // `errorKind` is the one camelCase key among snake_case
            // siblings on purpose: it is the existing public name the
            // web_fetch deny error and batch rows use (copied from the
            // result's top-level `errorKind`, named after MCP's
            // `isError`), so an agent meets one name for one value
            // wherever a denial appears. Do not respell it error_kind.
            json!({
                "rule": group.rule,
                "errorKind": group.kind,
                "next_action": group.message,
                "count": group.count,
                "urls": group.urls,
            })
        })
        .collect::<Vec<_>>();
    json!({ "count": denied.total(), "groups": groups })
}

/// True for a skip reason the crawl's rules check wrote (a URL that
/// redirected into a denied host, or one refused at fetch time): it
/// starts with the policy code `policy.denied.`.
fn is_policy_skip(reason: &str) -> bool {
    // The operator's rule key follows the code in such a reason, so
    // callers test this before any substring match on skip reasons.
    reason.starts_with("policy.denied.")
}

/// Compute actionable guidance for the agent based on crawl
/// results. Returns an empty string when the crawl succeeded
/// normally (no guidance needed).
pub(super) fn compute_crawl_next_action(result: &crate::crawl::CrawlResult) -> String {
    use crate::crawl::StopReason;

    // Resume available : always suggest it first.
    if let Some(tok) = &result.resume {
        return format!(
            "resume={tok} to continue crawling (stopped: {:?}).",
            result.stop
        );
    }

    // 0 pages : diagnose why.
    if result.pages.is_empty() && result.map.is_empty() {
        // Policy rows first, and out of the substring tests below: their
        // reason carries the operator's rule key, and a key such as
        // `wallpaper.example` matches "wall", `404news.example` matches "404".
        let (policy_rows, skip_reasons): (Vec<&str>, Vec<&str>) = result
            .skipped
            .iter()
            .map(|(_, w)| w.as_str())
            .partition(|r| is_policy_skip(r));
        if skip_reasons.is_empty() && (!policy_rows.is_empty() || !result.denied.is_empty()) {
            return "every URL this crawl reached was refused by a local DonSeTch rule: do not retry them through DonSeTch; use another source, following the rule's Next action where the denied list gives one.".into();
        }
        let all_scope = !skip_reasons.is_empty()
            && skip_reasons
                .iter()
                .all(|r| r.contains("out of scope") || r.contains("filtered"));
        let all_blocked = !skip_reasons.is_empty()
            && skip_reasons
                .iter()
                .all(|r| r.contains("Challenge") || r.contains("Blocked") || r.contains("wall"));
        let all_404 = !skip_reasons.is_empty()
            && skip_reasons
                .iter()
                .all(|r| r.contains("404") || r.contains("NotFound"));
        let has_sitemap = !result.map.is_empty();

        if all_404 {
            return "seed URL returned 404 : check the URL is correct.".into();
        }
        if all_blocked {
            return "the site blocked the crawler. Fetch the seed URL directly to check access, or retry from another permitted network.".into();
        }
        if all_scope && result.filtered_out > 0 {
            return "all discovered URLs were outside the seed's path scope. Try broader include_paths; same_host=false permits other hosts.".into();
        }
        if !has_sitemap && result.map.is_empty() && result.filtered_out == 0 {
            return "no URL inventory or readable pages were found. Check seed accessibility and scope; verify that the seed exposes readable links, or broaden include_paths.".into();
        }
        return "crawl returned 0 readable pages. Check seed access, broaden include_paths, or use a different seed URL.".into();
    }

    // Pages found but stopped early.
    match result.stop {
        StopReason::MaxPages => {
            "crawl hit the page budget. Increase max_pages or use resume to continue.".into()
        }
        StopReason::CharBudget => {
            "crawl hit the character budget. Increase max_total_chars or use resume to continue."
                .into()
        }
        StopReason::Deadline => {
            "crawl hit the time deadline. Increase deadline_s or use resume to continue.".into()
        }
        StopReason::Cancelled => {
            "crawl cancelled : resume with the token above to continue where it stopped.".into()
        }
        StopReason::ThrottledOut => {
            "the host throttled the crawler. Wait a few minutes and resume.".into()
        }
        StopReason::DepthLimit => {
            "crawl hit the depth limit. Increase max_depth to discover more pages.".into()
        }
        StopReason::FrontierEmpty => String::new(), // normal completion
    }
}

fn crawl_complete(result: &crate::crawl::CrawlResult, mode: CrawlMode) -> bool {
    matches!(result.stop, crate::crawl::StopReason::FrontierEmpty)
        && if mode == CrawlMode::Map {
            !result.map.is_empty()
        } else {
            (!result.pages.is_empty() || !result.skipped.is_empty())
                && result
                    .pages
                    .iter()
                    .all(|p| p.next_offset.is_none() && p.partial.is_none())
                // A policy row's reason carries the operator's rule key,
                // which may itself contain "duplicate"; reject it first.
                && result.skipped.iter().all(|(_, why)| {
                    !is_policy_skip(why)
                        && (why == "unchanged since last crawl" || why.contains("duplicate"))
                })
                // A denied URL is an excluded page: the pages behind it
                // were not crawled.
                && result.denied.is_empty()
        }
}

#[cfg(test)]
mod crawl_output_contract_tests {
    #[test]
    fn report_audit_inventory_does_not_bloat_content_and_partial_pages_are_honest() {
        let mut result = dataset_fixture();
        result.map = (0..1000)
            .map(|i| format!("https://example.com/page/{i}"))
            .collect();
        result.stop = StopReason::FrontierEmpty;
        result.pages[0].next_offset = Some(400);
        let content = render_crawl_result(&result, CrawlMode::Full, false);
        assert!(content["content"][0]["text"].as_str().unwrap().len() < 1000);
        assert_eq!(content["structuredContent"]["complete"], false);
        assert_eq!(content["structuredContent"]["pages"][0]["next_offset"], 400);
        let dataset = render_crawl_result(&result, CrawlMode::Full, true);
        assert_eq!(
            dataset["structuredContent"]["partial_pages"][0]["next_offset"],
            400
        );
        result.pages[0].next_offset = None;
        result.pages[0].partial = Some(crate::extract::PartialContent {
            reason: "source returned a subset",
            items_found: 3,
            items_total: Some(20),
        });
        let subset = render_crawl_result(&result, CrawlMode::Full, false);
        assert_eq!(subset["structuredContent"]["complete"], false);
        assert_eq!(
            subset["structuredContent"]["pages"][0]["content_complete"],
            false
        );
        assert_eq!(
            subset["structuredContent"]["pages"][0]["partial"]["items_total"],
            20
        );
        let subset_data = render_crawl_result(&result, CrawlMode::Full, true);
        assert_eq!(
            subset_data["structuredContent"]["partial_pages"][0]["partial"]["items_found"],
            3
        );
        let map = render_crawl_result(&result, CrawlMode::Map, false);
        assert!(map["structuredContent"].get("pages").is_none());
        assert_eq!(map["structuredContent"]["discovered"], 1000);
        assert_eq!(map["structuredContent"]["stop"], "InventoryComplete");
    }

    use super::render_crawl_result;
    use crate::crawl::{CrawlMode, CrawlPage, CrawlResult, StopReason};
    use crate::extract::ContentKind;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn wave450_map_inventory_is_model_state_without_false_404() {
        let mut result = CrawlResult {
            seed: "https://example.com/docs/".into(),
            pages: vec![],
            queued: vec![],
            filtered_out: 0,
            skipped: vec![],
            stop: StopReason::FrontierEmpty,
            elapsed: Duration::ZERO,
            map: vec!["https://example.com/docs/a".into()],
            crawl_delay: None,
            resume: None,
            denied: Default::default(),
            seed_failure: None,
        };
        let rendered = render_crawl_result(&result, CrawlMode::Map, false);
        assert_eq!(rendered["structuredContent"]["map"], json!(result.map));
        assert!(rendered["structuredContent"].get("next_action").is_none());
        result.map.clear();
        let empty = render_crawl_result(&result, CrawlMode::Content, false);
        assert_eq!(empty["structuredContent"]["complete"], false);
        assert!(
            !empty["structuredContent"]["next_action"]
                .as_str()
                .unwrap()
                .contains("404")
        );
    }

    #[test]
    pub(super) fn crawl_renders_page_identity_once_and_keeps_resume_as_state() {
        let page = CrawlPage {
            url: "https://example.com/docs/page".into(),
            title: "Evidence page".into(),
            kind: ContentKind::Article,
            markdown: "# Evidence page\nhttps://example.com/docs/page\n\nUseful evidence.".into(),
            next_offset: None,
            partial: None,
            chars: 16,
            quality: 0.93,
            duplicate: false,
            parent: Some("https://example.com/docs/".into()),
            score: 0.88,
            lastmod: Some("2026-09-04".into()),
            fetched_at: 1_770_000_000,
        };
        let result = CrawlResult {
            seed: "https://example.com/docs/".into(),
            pages: vec![page],
            queued: vec![],
            filtered_out: 0,
            skipped: vec![],
            stop: StopReason::MaxPages,
            elapsed: Duration::from_millis(42),
            map: vec![],
            crawl_delay: None,
            resume: Some("opaque-resume".into()),
            denied: Default::default(),
            seed_failure: None,
        };
        let output = render_crawl_result(&result, CrawlMode::Full, false);
        let text = output["content"][0]["text"].as_str().unwrap();
        assert_eq!(text.matches("Evidence page").count(), 1);
        assert_eq!(text.matches("https://example.com/docs/page").count(), 1);
        assert!(!text.contains("quality="));
        assert!(!text.contains("opaque-resume"));
        assert_eq!(output["structuredContent"]["resume"], "opaque-resume");
        assert!(
            output["structuredContent"]["pages"][0]
                .get("quality")
                .is_none()
        );
        assert_eq!(
            output["_meta"]["com.donsetch/crawl-debug"]["pages"][0]["quality"],
            json!(0.93_f32)
        );
    }

    // The debug block carried every queued URL: a wide crawl put
    // thousands of them into each response. It carries a preview and
    // the count.
    #[test]
    fn the_queued_list_in_the_response_is_a_bounded_preview() {
        let result = CrawlResult {
            seed: "https://example.com/".into(),
            pages: vec![],
            queued: (0..5_000)
                .map(|i| format!("https://example.com/p/{i}"))
                .collect(),
            filtered_out: 0,
            skipped: vec![],
            stop: StopReason::MaxPages,
            elapsed: Duration::from_millis(1),
            map: vec![],
            crawl_delay: None,
            resume: Some("tok".into()),
            denied: Default::default(),
            seed_failure: None,
        };
        for dataset in [false, true] {
            let output = render_crawl_result(&result, CrawlMode::Full, dataset);
            let debug = &output["_meta"]["com.donsetch/crawl-debug"];
            let shown = debug["queued"].as_array().unwrap().len();
            assert!(
                shown <= super::QUEUED_PREVIEW,
                "dataset={dataset}: {shown} queued URLs in the response"
            );
            assert_eq!(debug["queued_total"], json!(5_000), "dataset={dataset}");
            assert_eq!(debug["queued"][0], "https://example.com/p/0");
        }
    }

    fn dataset_fixture() -> crate::crawl::CrawlResult {
        let page = |url: &str, title: &str, md: &str, dup: bool| CrawlPage {
            url: url.into(),
            title: title.into(),
            kind: crate::extract::ContentKind::Article,
            markdown: md.into(),
            next_offset: None,
            partial: None,
            chars: md.len(),
            quality: 0.9,
            duplicate: dup,
            parent: None,
            score: 1.0,
            lastmod: None,
            fetched_at: 1_770_000_000,
        };
        crate::crawl::CrawlResult {
            seed: "https://example.com/docs/".into(),
            pages: vec![
                page(
                    "https://example.com/docs/b",
                    "B",
                    "line with \"quotes\" and\nnewlines",
                    false,
                ),
                page("https://example.com/docs/a", "A", "alpha body", false),
                page(
                    "https://example.com/docs/b?x=1",
                    "B dup",
                    "alpha body",
                    true,
                ),
            ],
            queued: vec![],
            filtered_out: 0,
            skipped: vec![(
                "https://example.com/docs/walled".into(),
                "wall.challenge".into(),
            )],
            stop: crate::crawl::StopReason::FrontierEmpty,
            elapsed: std::time::Duration::from_millis(7),
            map: vec![],
            crawl_delay: None,
            resume: None,
            denied: Default::default(),
            seed_failure: None,
        }
    }

    // Dataset mode must emit one VALID JSON object per non-duplicate
    // page, sorted by URL, with the page's own markdown verbatim
    // (escaping included). A markdown document or unsorted/duplicate
    // rows fail this test.
    #[test]
    fn dataset_mode_emits_valid_sorted_jsonl() {
        let out = render_crawl_result(&dataset_fixture(), crate::crawl::CrawlMode::Full, true);
        let text = out["content"][0]["text"].as_str().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "duplicates dropped, one row per page");
        let row0: serde_json::Value = serde_json::from_str(lines[0]).expect("row 0 valid JSON");
        let row1: serde_json::Value = serde_json::from_str(lines[1]).expect("row 1 valid JSON");
        assert_eq!(row0["url"], "https://example.com/docs/a");
        assert_eq!(row1["url"], "https://example.com/docs/b");
        // Verbatim markdown with hostile characters survives the round trip.
        assert_eq!(row1["markdown"], "line with \"quotes\" and\nnewlines");
        assert_eq!(row0["fetched_at"], 1_770_000_000u64);
        assert_eq!(out["structuredContent"]["rows"], 2);
        assert_eq!(out["structuredContent"]["dataset"], true);
        assert_eq!(out["structuredContent"]["dataset_version"], 1);
        assert_eq!(out["structuredContent"]["complete"], false);
        // Skipped pages surface in debug, never as rows.
        assert_eq!(
            out["_meta"]["com.donsetch/crawl-debug"]["skipped"][0]["reason"],
            "wall.challenge"
        );
    }

    #[test]
    fn dataset_mode_still_reports_resume_and_budget_stops() {
        let mut r = dataset_fixture();
        r.stop = crate::crawl::StopReason::MaxPages;
        r.resume = Some("tok-1".into());
        let out = render_crawl_result(&r, crate::crawl::CrawlMode::Full, true);
        assert_eq!(out["structuredContent"]["resume"], "tok-1");
        assert_eq!(out["structuredContent"]["complete"], false);
        let hint = out["structuredContent"]["next_action"].as_str().unwrap();
        assert!(hint.contains("resume"), "resume guidance preserved");
    }

    #[test]
    fn markdown_mode_unchanged_by_dataset_flag_absence() {
        let out = render_crawl_result(&dataset_fixture(), crate::crawl::CrawlMode::Full, false);
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("# Crawl"));
        assert!(text.contains("## [1]"));
    }

    fn denied_fixture() -> crate::crawl::DeniedLog {
        crate::crawl::DeniedLog {
            groups: vec![
                crate::crawl::DeniedGroup {
                    rule: "banned.example".into(),
                    kind: "walled".into(),
                    message: "use BladeBrowser for this site".into(),
                    count: 16,
                    urls: vec![
                        "https://www.banned.example/publication/1".into(),
                        "https://www.banned.example/publication/2".into(),
                    ],
                },
                crate::crawl::DeniedGroup {
                    rule: "gone-mirror.example".into(),
                    kind: "permanent".into(),
                    message: "this mirror is gone, do not look for it".into(),
                    count: 3,
                    urls: vec![],
                },
            ],
        }
    }

    fn empty_result() -> CrawlResult {
        CrawlResult {
            seed: "https://example.com/docs/".into(),
            pages: vec![],
            queued: vec![],
            filtered_out: 0,
            skipped: vec![],
            stop: StopReason::FrontierEmpty,
            elapsed: Duration::ZERO,
            map: vec![],
            crawl_delay: None,
            resume: None,
            denied: Default::default(),
            seed_failure: None,
        }
    }

    #[test]
    fn denied_urls_get_a_head_counter_and_a_trailing_section_without_the_rule_key() {
        let mut result = dataset_fixture();
        result.denied = denied_fixture();
        let out = render_crawl_result(&result, CrawlMode::Full, false);
        let text = out["content"][0]["text"].as_str().unwrap();
        let counter = text
            .find("Denied by local DonSeTch rules: 19 URLs, listed at the end.")
            .expect(text);
        let first_page = text.find("## [1]").unwrap();
        let section = text
            .find(
                "## Denied by local DonSeTch rules (not fetched; web_fetch would refuse them too)",
            )
            .expect(text);
        assert!(counter < first_page && first_page < section, "{text}");
        let tail = &text[section..];
        assert!(
            tail.contains(
                "walled: try another source\nNext action: use BladeBrowser for this site\n- https://www.banned.example/publication/1\n- https://www.banned.example/publication/2\n…and 14 more"
            ),
            "{tail}"
        );
        // A group with no listed URLs (crawl_denied_urls_per_rule = 0)
        // still shows its kind, message and count.
        assert!(
            tail.contains(
                "permanent: do not pursue\nNext action: this mirror is gone, do not look for it\n3 URLs"
            ),
            "{tail}"
        );
        assert!(
            !text.contains("gone-mirror.example"),
            "no rule key in the text"
        );

        let denied = &out["structuredContent"]["denied_by_local_rules"];
        assert_eq!(denied["count"], 19);
        assert_eq!(denied["groups"][0]["rule"], "banned.example");
        assert_eq!(denied["groups"][0]["errorKind"], "walled");
        assert_eq!(
            denied["groups"][0]["next_action"],
            "use BladeBrowser for this site"
        );
        assert_eq!(denied["groups"][0]["count"], 16);
        assert_eq!(denied["groups"][0]["urls"].as_array().unwrap().len(), 2);
        assert_eq!(denied["groups"][1]["errorKind"], "permanent");

        let data = render_crawl_result(&result, CrawlMode::Full, true);
        assert_eq!(data["structuredContent"]["denied_by_local_rules"], *denied);
        assert!(
            !data["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Denied by local"),
            "JSON Lines stay rows only"
        );

        // Negative: a crawl that denied nothing carries no trace of it.
        for dataset in [false, true] {
            let clean = render_crawl_result(&dataset_fixture(), CrawlMode::Full, dataset);
            assert!(
                clean["structuredContent"]
                    .get("denied_by_local_rules")
                    .is_none()
            );
            assert!(
                !clean["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("Denied by local")
            );
        }
    }

    #[test]
    fn a_denial_or_a_policy_skip_row_makes_the_crawl_incomplete() {
        let complete = |r: &CrawlResult| {
            render_crawl_result(r, CrawlMode::Full, false)["structuredContent"]["complete"].clone()
        };
        let mut result = dataset_fixture();
        result.skipped.clear();
        assert_eq!(complete(&result), true);
        // Negative: a real duplicate row stays harmless.
        result.skipped = vec![(
            "https://example.com/docs/a?ref=1".into(),
            "duplicate of https://example.com/docs/a".into(),
        )];
        assert_eq!(complete(&result), true);
        // The operator's key holds "duplicate", the row is still a refusal.
        result.skipped = vec![(
            "https://example.com/docs/moved".into(),
            "policy.denied.unspecified: blocked by a local DonSeTch rule `duplicate-finder.example`"
                .into(),
        )];
        assert_eq!(complete(&result), false);
        result.skipped.clear();
        result.denied = denied_fixture();
        assert_eq!(complete(&result), false);
    }

    #[test]
    fn policy_skip_rows_do_not_trip_the_wall_or_404_guidance() {
        let mut result = empty_result();
        result.skipped = vec![
            (
                "https://a.example/x".into(),
                "policy.denied.unspecified: blocked by a local DonSeTch rule `wallpaper.example`"
                    .into(),
            ),
            (
                "https://b.example/y".into(),
                "policy.denied.ip_ban: blocked by a local DonSeTch rule `404news.example`".into(),
            ),
        ];
        let hint = super::compute_crawl_next_action(&result);
        assert!(hint.contains("local DonSeTch rule"), "{hint}");
        assert!(!hint.contains("blocked the crawler"), "{hint}");
        assert!(!hint.contains("404"), "{hint}");
        // Negative: a genuine wall row keeps the wall guidance.
        result.skipped = vec![(
            "https://a.example/x".into(),
            "wall.challenge Blocked".into(),
        )];
        assert!(super::compute_crawl_next_action(&result).contains("blocked the crawler"));
    }

    fn seed_failure(status: u16, verdict: Option<&str>) -> crate::crawl::SeedFailure {
        crate::crawl::SeedFailure {
            requested: "https://example.org/old-docs/".into(),
            landing: None,
            status,
            verdict: verdict.map(String::from),
            error: None,
            robots: None,
            denial: None,
            has_sitemap_phase: true,
        }
    }

    #[test]
    fn a_failed_seed_error_carries_its_cause_kind_and_evidence() {
        use crate::crawl::RobotsRefusal;

        let mut gone = seed_failure(404, Some("SoftNotFound"));
        gone.landing = Some("https://example.org/new-docs/".into());
        let error = super::seed_failure_error(&gone, None);
        let sc = &error["structuredContent"];
        assert_eq!(sc["ok"], false, "{error}");
        // The code web_fetch gives a 404, with its read_status and
        // moved-page query.
        assert_eq!(sc["code"], "content.notfound");
        assert_eq!(sc["read_status"], "notfound");
        assert_eq!(sc["suggested_query"], "site:example.org old docs");
        assert_eq!(error["errorKind"], "permanent");
        assert_eq!(sc["status"], 404);
        assert_eq!(sc["verdict"], "SoftNotFound");
        assert_eq!(sc["requested_url"], "https://example.org/old-docs/");
        assert_eq!(sc["landing_url"], "https://example.org/new-docs/");
        assert!(
            sc["discovery"]
                .as_str()
                .unwrap()
                .contains("no sitemap entries")
        );
        let text = error["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("redirected to https://example.org/new-docs/"),
            "{text}"
        );

        let walled =
            super::seed_failure_error(&seed_failure(403, Some("Challenge(Cloudflare)")), None);
        assert_eq!(walled["errorKind"], "walled");
        assert_eq!(walled["structuredContent"]["code"], "wall.challenge");
        // A 401 whose verdict names no wall still gets a wall code.
        let auth = super::seed_failure_error(&seed_failure(401, Some("ContentOk")), None);
        assert_eq!(auth["structuredContent"]["code"], "wall.auth");

        // A fetch error the classifier names keeps its network code.
        let mut slow = seed_failure(0, Some("Blocked"));
        slow.error = Some("network: timed out".into());
        let slow = super::seed_failure_error(&slow, None);
        assert_eq!(slow["structuredContent"]["code"], "network.timeout");

        let limited = super::seed_failure_error(&seed_failure(429, Some("Blocked")), None);
        assert_eq!(limited["structuredContent"]["code"], "network.ratelimit");
        assert_eq!(limited["errorKind"], "transient", "a 429 is not a wall");
        // Causes web_fetch has no code for get the crawl fallback, never
        // the malformed-seed input code.
        let down = super::seed_failure_error(&seed_failure(503, Some("Blocked")), None);
        assert_eq!(down["structuredContent"]["code"], "crawl.seed_failed");
        assert_eq!(down["errorKind"], "transient");

        // A fetch error arrives as status 0 with a Blocked verdict: it is
        // a network failure, not a wall.
        let mut offline = seed_failure(0, Some("Blocked"));
        offline.error = Some("network: connection refused".into());
        offline.has_sitemap_phase = false;
        let offline = super::seed_failure_error(&offline, None);
        assert_eq!(offline["errorKind"], "transient");
        // The classifier has no code for a refused connection.
        assert_eq!(offline["structuredContent"]["code"], "crawl.seed_failed");
        assert_eq!(
            offline["structuredContent"]["error"],
            "network: connection refused"
        );
        assert!(
            offline["structuredContent"]["discovery"]
                .as_str()
                .unwrap()
                .contains("no sitemap phase")
        );
        assert!(offline["structuredContent"].get("landing_url").is_none());

        let mut disallowed = seed_failure(0, None);
        disallowed.robots = Some(RobotsRefusal::Disallow);
        let disallowed = super::seed_failure_error(&disallowed, None);
        assert_eq!(disallowed["errorKind"], "walled");
        assert_eq!(
            disallowed["structuredContent"]["code"],
            "crawl.robots_disallow"
        );
        assert_eq!(disallowed["structuredContent"]["robots"], "disallow");
        assert!(
            disallowed["structuredContent"]["next_action"]
                .as_str()
                .unwrap()
                .contains("respect_robots=false")
        );

        // Negative: an unreachable robots.txt gets no bypass hint.
        let mut outage = seed_failure(0, None);
        outage.robots = Some(RobotsRefusal::Unreachable);
        let outage = super::seed_failure_error(&outage, None);
        assert_eq!(outage["errorKind"], "transient");
        assert_eq!(
            outage["structuredContent"]["code"],
            "crawl.robots_unreachable"
        );
        assert!(
            !outage["structuredContent"]["next_action"]
                .as_str()
                .unwrap()
                .contains("respect_robots")
        );
    }

    #[test]
    fn a_seed_redirected_into_a_denied_host_fails_with_the_policy_error() {
        let mut failure = seed_failure(0, Some("Blocked"));
        failure.landing = Some("https://www.banned.example/publication/1".into());
        failure.denial = Some(crate::rules::Denial {
            rule: "banned.example".into(),
            message: "ask the human operator to download the file".into(),
            kind: "walled".into(),
            reason: Some("ip_ban".into()),
        });
        let error = super::seed_failure_error(&failure, Some(json!([])));
        let sc = &error["structuredContent"];
        assert_eq!(sc["code"], "policy.denied.ip_ban", "{error}");
        assert_eq!(error["errorKind"], "walled");
        assert_eq!(sc["rule"], "banned.example");
        assert_eq!(
            sc["next_action"],
            "ask the human operator to download the file"
        );
        assert_eq!(sc["requested_url"], "https://example.org/old-docs/");
        assert_eq!(
            sc["landing_url"],
            "https://www.banned.example/publication/1"
        );
    }

    #[test]
    fn the_rendered_seed_harvest_needs_a_content_like_read() {
        use crate::ghost::ops::BrowserOutcome as O;
        assert!(super::rendered_seed_usable(O::Content, Some(200)));
        assert!(super::rendered_seed_usable(O::Content, None));
        assert!(super::rendered_seed_usable(O::Incomplete, Some(200)));
        for refused in [
            O::NotFound,
            O::ManagedChallenge,
            O::HumanRequired,
            O::AuthRequired,
            O::Paywall,
        ] {
            assert!(
                !super::rendered_seed_usable(refused, Some(200)),
                "{refused:?}"
            );
        }
        // A 404 page with a full site header reads as Content.
        assert!(!super::rendered_seed_usable(O::Content, Some(404)));
    }

    /// A map crawl of one seed answering `seed_status` with a page full of
    /// links; every other URL is a 404. The renderer applies the rendered
    /// harvest's gate to a read with the same status.
    async fn map_crawl_of_seed(seed_status: u16) -> CrawlResult {
        use crate::crawl::governor::{Governor, Lane, LaneKind};
        use crate::crawl::{CrawlOptions, Crawler, FetchedPage, PageFetcher, RenderFn};
        use crate::detect::walls::Verdict;
        use futures_util::FutureExt;
        use std::sync::Arc;

        const SEED: &str = "https://example.org/old-docs/";
        const PAGE: &str = "<html><head><title>Docs</title></head><body><main><ul>\
            <li><a href=\"/old-docs/getting-started\">Getting started</a></li>\
            <li><a href=\"/old-docs/configuration\">Configuration</a></li>\
            <li><a href=\"/old-docs/reference\">Reference</a></li>\
            </ul></main></body></html>";
        let fetch: PageFetcher = Arc::new(
            move |url: String,
                  lane: String,
                  _referer: Option<String>,
                  _gate: Option<crate::fetch::client::RedirectGate>| {
                async move {
                    let (status, body, verdict) = if url == SEED {
                        let verdict = if seed_status == 200 {
                            Verdict::ContentOk
                        } else {
                            Verdict::SoftNotFound
                        };
                        (seed_status, PAGE, verdict)
                    } else {
                        (404, "not found", Verdict::SoftNotFound)
                    };
                    FetchedPage {
                        lane,
                        route: Some(crate::transport::request_route::RequestRoute::direct()),
                        url,
                        status,
                        headers: vec![("content-type".into(), "text/html".into())],
                        body: body.as_bytes().to_vec(),
                        verdict,
                        latency: Duration::ZERO,
                        cached: false,
                        error_hint: None,
                        denied: None,
                    }
                }
                .boxed()
            },
        );
        let render: RenderFn = Arc::new(move |url: String| {
            Box::pin(async move {
                super::rendered_seed_usable(
                    crate::ghost::ops::BrowserOutcome::Content,
                    Some(seed_status),
                )
                .then(|| (PAGE.to_string(), url))
            })
        });
        let crawler = Crawler::new(
            fetch,
            Arc::new(Governor::new(vec![Lane {
                id: "direct".into(),
                kind: LaneKind::Direct,
            }])),
        );
        crawler
            .crawl(
                SEED,
                CrawlOptions {
                    mode: CrawlMode::Map,
                    respect_robots: false,
                    deadline: Duration::from_secs(30),
                    render_html: Some(render),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_dead_seed_with_links_fails_in_map_mode_while_a_live_one_keeps_its_map() {
        let dead = map_crawl_of_seed(404).await;
        assert!(dead.map.is_empty(), "{:?}", dead.map);
        let failure = dead
            .seed_failure
            .as_ref()
            .expect("a 404 seed with nothing else to map is a failed seed");
        let error = super::seed_failure_error(failure, None);
        assert_eq!(error["structuredContent"]["ok"], false, "{error}");
        assert_eq!(error["errorKind"], "permanent");
        assert_eq!(error["structuredContent"]["status"], 404);

        // Negative: the same page served 200 keeps its map.
        let live = map_crawl_of_seed(200).await;
        assert!(live.seed_failure.is_none());
        assert!(!live.map.is_empty());
        let rendered = render_crawl_result(&live, CrawlMode::Map, false);
        assert_eq!(rendered["structuredContent"]["complete"], true);
    }

    // Installs a ruleset into the process (crate::config::install) and
    // writes resume tokens into its cache root: depends on nextest's
    // process-per-test.
    #[tokio::test]
    async fn a_resumed_seed_on_a_denied_host_is_refused_and_keeps_its_token() {
        use crate::rules::{RuleAction, UrlRule};
        use std::sync::Arc;

        let mut config = crate::config::DonsetchConfig::default();
        config.proxy.from_environment = false;
        config.rules.url.insert(
            "denied.example".into(),
            UrlRule {
                action: RuleAction::Deny,
                message: Some("ask the human operator for this file".into()),
                reason: Some("ip_ban".into()),
                ..Default::default()
            },
        );
        crate::config::install(config).unwrap();
        let dir = crate::paths::cache_dir().join("crawl-resumes");
        std::fs::create_dir_all(&dir).unwrap();
        // The token file's on-disk shape (`ResumeState`, crawl/mod.rs).
        let write_token = |tok: &str, seed: &str| {
            let path = dir.join(format!("{tok}.json"));
            std::fs::write(
                &path,
                json!({ "seed": seed, "queue": [], "seen": [] }).to_string(),
            )
            .unwrap();
            path
        };
        let denied_token = write_token("cdenied1", "https://denied.example/docs/");
        let allowed_token = write_token("callowed1", "https://allowed.example/docs/");
        let daemon = Arc::new(super::Daemon::new().await.unwrap());

        let refused = super::crawl_tool(&daemon, &json!({ "resume": "cdenied1" }), None).await;
        let sc = &refused["structuredContent"];
        assert_eq!(sc["ok"], false, "{refused}");
        assert_eq!(sc["code"], "policy.denied.ip_ban");
        assert_eq!(sc["errorKind"], "walled");
        assert_eq!(sc["rule"], "denied.example");
        assert_eq!(sc["next_action"], "ask the human operator for this file");
        assert!(
            denied_token.exists(),
            "a refused resumed seed must not consume its token"
        );

        // An explicit seed on the same host takes the same refusal.
        let explicit = super::crawl_tool(
            &daemon,
            &json!({ "url": "https://denied.example/docs/" }),
            None,
        )
        .await;
        assert_eq!(
            explicit["structuredContent"]["code"], "policy.denied.ip_ban",
            "{explicit}"
        );

        // Negative: an allowed resumed seed resumes as before. An empty
        // queue in content mode with robots off makes no request.
        let resumed = super::crawl_tool(
            &daemon,
            &json!({
                "resume": "callowed1",
                "mode": "content",
                "respect_robots": false,
                "deadline_s": 5,
            }),
            None,
        )
        .await;
        assert_eq!(resumed["structuredContent"]["ok"], true, "{resumed}");
        assert!(
            !allowed_token.exists(),
            "a resumed crawl consumes its token"
        );
    }
}
