//! The fetch tool handler: dispatch, URL/handle resolution,
//! multi-fetch batching, single fetch + the full escalation ladder
//! (bypass, ghost, actions, OCR, anticloak, resurrection), page
//! history + link handles, and the result envelope assembly.

use serde_json::{Value, json};

use super::*;
pub(super) async fn fetch_tool(
    daemon: &Arc<Daemon>,
    args: &Value,
    mut ctx: Option<ToolCtx>,
) -> Value {
    daemon.refresh_vault().await;
    let deadline = args
        .get("deadline_ms")
        .and_then(Value::as_u64)
        .map(|ms| std::time::Duration::from_millis(ms.clamp(500, 600_000)));
    // v3: url accepts one URL/handle OR an array of them : batch
    // fetch in a single call, optionally under a shared token
    // budget (budget_tokens) allocated across results.
    let urls: Vec<String> = match args.get("url") {
        Some(Value::String(s)) => vec![s.to_string()],
        Some(Value::Array(a)) => {
            let v: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect();
            if v.is_empty() {
                return tool_error("fetch: url array is empty");
            }
            if v.len() > 12 {
                return tool_error("fetch: max 12 urls per batch call");
            }
            v
        }
        _ => return tool_error("fetch: url must be http(s)"),
    };
    let budget_tokens = args
        .get("budget_tokens")
        .and_then(Value::as_u64)
        .map(|t| (t as usize).clamp(200, 500_000));

    if urls.len() == 1 && budget_tokens.is_none() {
        let url = match resolve_fetch_url(daemon, &urls[0]).await {
            Ok(u) => u,
            Err(e) => return e,
        };
        let result = run_with_budget(
            fetch_single(daemon, args, &url),
            deadline,
            ctx.as_mut(),
            || deadline_error(&url),
        )
        .await;
        memory_ingest_result(&url, &result);
        return result;
    }
    let mut resolved: Vec<String> = Vec::with_capacity(urls.len());
    for u in &urls {
        match resolve_fetch_url(daemon, u).await {
            Ok(r) => resolved.push(r),
            Err(e) => return e,
        }
    }
    // Single resolved URL: keep the single-page response shape, but
    // always run under the deadline + MCP-cancellation wrapper (#164).
    // Previously this branch (reached whenever budget_tokens was set,
    // since the fast path above demands budget_tokens.is_none())
    // called fetch_single bare: an uncancellable, deadline-free fetch
    // on a path that can still spawn a ghost render. budget_tokens
    // also bounds the page now, exactly like the batch path.
    if resolved.len() == 1 {
        let owned_args;
        let effective_args = if let Some(b) = budget_tokens {
            let budget_chars = b.saturating_mul(4).max(800);
            let mut a = args.clone();
            a["max_chars"] = json!(budget_chars);
            owned_args = a;
            &owned_args
        } else {
            args
        };
        let result = run_with_budget(
            fetch_single(daemon, effective_args, &resolved[0]),
            deadline,
            ctx.as_mut(),
            || deadline_error(&resolved[0]),
        )
        .await;
        memory_ingest_result(&resolved[0], &result);
        return result;
    }
    fetch_multi(daemon, args, resolved, budget_tokens, deadline, ctx).await
}

/// Store a successful fetch result page into the local web memory.
/// Additive (v4 law 5): ingest failure never touches the fetch result;
/// its only surface is a stderr receipt.
#[cfg_attr(not(feature = "rerank"), allow(unused_variables))]
fn memory_ingest_result(url: &str, result: &Value) {
    #[cfg(feature = "rerank")]
    if !crate::memory::kill_switch() && result.get("isError").and_then(Value::as_bool) != Some(true)
    {
        let md = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        if md.is_empty() {
            return;
        }
        crate::memory::ingest_async(vec![(
            url.to_string(),
            crate::memory::title_of(md),
            md.to_string(),
        )]);
    }
}

/// Honest deadline error (v3 D1): the tool respects the agent's
/// clock. What was fetched so far is described; nothing pretends.
pub(super) async fn resolve_fetch_url(daemon: &Arc<Daemon>, raw: &str) -> Result<String, Value> {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Ok(raw.to_string());
    }
    // v3 handles: random L/S handles resolve through the
    // handle table (L persisted, S in-memory) before anything else.
    if crate::handles::is_handle(raw) {
        if let Some(resolved) = daemon.handles.lock().await.resolve(raw) {
            if let Ok(parsed) = url::Url::parse(&resolved)
                && matches!(parsed.scheme(), "http" | "https")
            {
                return Ok(resolved);
            }
            return Err(tool_error(format!(
                "fetch: handle {raw} resolved to a non-http(s) URL : refused"
            )));
        }
        return Err(tool_error_structured(
            format!("fetch: handle {raw} is unknown or expired (24h TTL)"),
            "permanent",
            Some(json!({
                "url": raw,
                "next_action": "re-run the search/fetch that produced the handle, or pass the full URL directly",
            })),
        ));
    }
    Err(tool_error(format!(
        "fetch: url must be http(s), got: {raw}"
    )))
}

/// Batch fetch (v3): parallel single-fetches composed into one
/// result under an optional shared token budget. Small pages stay
/// whole; the budget slices proportional to size, never below a
/// floor. All-failed = honest error; partial = composed result
/// with per-URL status.
pub(super) async fn fetch_multi(
    daemon: &Arc<Daemon>,
    args: &Value,
    urls: Vec<String>,
    budget_tokens: Option<usize>,
    deadline: Option<std::time::Duration>,
    ctx: Option<ToolCtx>,
) -> Value {
    // Under a budget, let each fetch run up to the whole budget
    // (slicing happens in composition); without one, defaults rule.
    let mut call_args = args.clone();
    if let Some(b) = budget_tokens {
        call_args["max_chars"] = json!(b * 4);
    }
    let progress_parts = ctx.as_ref().map(|c| c.progress_parts());
    let n_total = urls.len();
    let futs: Vec<_> = urls
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let a = call_args.clone();
            let d = Arc::clone(daemon);
            let url = u.clone();
            let dl = deadline;
            let prog = progress_parts.clone();
            async move {
                let v = run_with_budget(fetch_single(&d, &a, &url), dl, None, || {
                    deadline_error(&url)
                })
                .await;
                if let Some(p) = &prog {
                    emit_progress(
                        p,
                        (i + 1) as u64,
                        Some(n_total as u64),
                        &format!("{}/{} done", i + 1, n_total),
                    );
                }
                v
            }
        })
        .collect();
    let results = futures_util::future::join_all(futs).await;

    let is_err = |v: &Value| v.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let md_of = |v: &Value| {
        v.pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    // Budget slicing: proportional to returned size, floor 300
    // chars, only when the sum overflows.
    let mut markdowns: Vec<Option<String>> = results
        .iter()
        .map(|r| if is_err(r) { None } else { Some(md_of(r)) })
        .collect();
    // Store the successful fetches into the local web memory. The
    // hook gets the FULL body before any budget slicing: the memory
    // keeps the page whole and truncates on its own. One batch =
    // one embed pass + one disk write (issue #178), and it runs on
    // the blocking pool so it cannot hold the response past the
    // deadline.
    #[cfg(feature = "rerank")]
    if !crate::memory::kill_switch() {
        let mut rows = Vec::with_capacity(urls.len());
        for (idx, url) in urls.iter().enumerate() {
            let Some(md) = markdowns[idx].as_deref() else {
                continue;
            };
            rows.push((url.clone(), crate::memory::title_of(md), md.to_string()));
        }
        crate::memory::ingest_async(rows);
    }
    let mut sliced_flags = vec![false; results.len()];
    if let Some(budget_tok) = budget_tokens {
        let budget_chars = budget_tok * 4;
        let lens: Vec<usize> = markdowns
            .iter()
            .map(|m| m.as_ref().map(|s| s.len()).unwrap_or(0))
            .collect();
        let total: usize = lens.iter().sum();
        if total > budget_chars && total > 0 {
            let n_ok = lens.iter().filter(|&&l| l > 0).count().max(1);
            let floor = (budget_chars / n_ok / 4).clamp(300, 4_000);
            let mut alloc: Vec<usize> = lens
                .iter()
                .map(|&l| {
                    if l == 0 {
                        0
                    } else {
                        (budget_chars * l / total).max(floor)
                    }
                })
                .collect();
            // Trim the largest allocations down to fit the budget.
            let mut over: i128 = alloc.iter().sum::<usize>() as i128 - budget_chars as i128;
            while over > 0 {
                let (idx, _) = alloc
                    .iter()
                    .enumerate()
                    .filter(|(_i, a)| **a > floor)
                    .max_by_key(|(i, a)| (**a as i128, std::cmp::Reverse(*i)))
                    .map(|(i, a)| (i, *a))
                    .unwrap_or((0, 0));
                let take = (alloc[idx] - floor).min(over as usize);
                if take == 0 {
                    break;
                }
                alloc[idx] -= take;
                over -= take as i128;
            }
            for (i, m) in markdowns.iter_mut().enumerate() {
                if let Some(md) = m
                    && md.len() > alloc[i]
                {
                    let mut cut = alloc[i];
                    while cut > 0 && !md.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    let truncated = format!(
                        "{}\n\n*[budget-sliced: showing {} of {} chars : refetch this url alone with max_chars for the rest]*",
                        &md[..cut],
                        cut,
                        md.len()
                    );
                    *m = Some(truncated);
                    sliced_flags[i] = true;
                }
            }
        }
    }

    render_fetch_batch(&urls, &results, &markdowns, budget_tokens, &sliced_flags)
}

/// Compose a batch without repeating page evidence or transport telemetry on
/// model-visible surfaces. Kept pure so partial and all-failed contracts are
/// deterministic unit-test inputs.
pub(super) fn render_fetch_batch(
    urls: &[String],
    results: &[Value],
    markdowns: &[Option<String>],
    budget_tokens: Option<usize>,
    sliced_flags: &[bool],
) -> Value {
    debug_assert_eq!(urls.len(), results.len());
    debug_assert_eq!(urls.len(), markdowns.len());
    debug_assert_eq!(urls.len(), sliced_flags.len());

    let is_err = |v: &Value| v.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let title_of = |v: &Value| {
        v.pointer("/_meta/com.donsetch~1fetch-debug/title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    let mut text = String::new();
    let ok_count = markdowns.iter().filter(|m| m.is_some()).count();
    let err_count = results.len() - ok_count;
    for (i, r) in results.iter().enumerate() {
        if let Some(md) = &markdowns[i] {
            let title = title_of(r);
            let head = if title.is_empty() {
                urls[i].as_str()
            } else {
                title.as_str()
            };
            let body = strip_source_frontmatter(
                md,
                &urls[i],
                (!title.is_empty()).then_some(title.as_str()),
            );
            text.push_str(&format!(
                "## [{}] {}\n{}\n\n{}\n\n---\n\n",
                i + 1,
                head,
                urls[i],
                body
            ));
        } else {
            let msg = r
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or("fetch failed");
            text.push_str(&format!(
                "## [{}] {} : ERROR\n{}\n\n---\n\n",
                i + 1,
                urls[i],
                msg
            ));
        }
    }
    let structured_results = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut o = json!({
                "url": urls[i],
                "ok": !is_err(r),
            });
            if !is_err(r) {
                let state = &r["structuredContent"];
                if state.get("content_ok").and_then(Value::as_bool) == Some(false) {
                    o["content_ok"] = json!(false);
                }
                for field in ["next_offset", "archived"] {
                    if let Some(value) = state.get(field)
                        && !value.is_null()
                    {
                        o[field] = value.clone();
                    }
                }
                for field in ["thin", "cloak_suspected"] {
                    if state.get(field).and_then(Value::as_bool).unwrap_or(false) {
                        o[field] = json!(true);
                    }
                }
                if let Some(changed) = state.get("changed").and_then(Value::as_str)
                    && changed != "new"
                {
                    o["changed"] = json!(changed);
                }
            }
            if sliced_flags[i] {
                o["sliced"] = json!(true);
            }
            if is_err(r) {
                o["code"] = r
                    .pointer("/structuredContent/code")
                    .cloned()
                    .unwrap_or_else(|| json!("content.extract"));
            }
            o
        })
        .collect::<Vec<_>>();
    let mut structured = json!({
        "ok": ok_count,
        "errors": err_count,
        "results": structured_results,
    });
    let debug_results = results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let fetch_debug = &result["_meta"]["com.donsetch/fetch-debug"];
            let mut item = json!({"url": urls[index]});
            for field in ["tier", "tokens_est"] {
                if let Some(value) = fetch_debug.get(field)
                    && !value.is_null()
                {
                    item[field] = value.clone();
                }
            }
            item
        })
        .collect::<Vec<_>>();
    let debug = json!({
        "count": results.len(),
        "budget_tokens": budget_tokens,
        "results": debug_results,
    });

    if ok_count == 0 {
        structured["next_action"] =
            json!("inspect the per-URL errors above; retry only transient failures individually");
        let mut error = tool_error_structured(
            format!(
                "fetch: all {} urls failed\n\n{}",
                results.len(),
                text.trim_end()
            ),
            "transient",
            Some(structured),
        );
        error["_meta"]["com.donsetch/fetch-batch-debug"] = debug;
        return error;
    }
    json!({
        "content": [{"type": "text", "text": text.trim_end()}],
        "structuredContent": structured,
        "_meta": {"com.donsetch/fetch-batch-debug": debug},
    })
}

/// Single-URL fetch with resurrection (v3): dead URLs get one
/// honest attempt at the Wayback Machine before the error stands.
pub(super) async fn fetch_single(daemon: &Arc<Daemon>, args: &Value, url: &str) -> Value {
    let archive = match args.get("archive").and_then(Value::as_str) {
        Some("off") => "off",
        Some("only") => "only",
        _ => "auto",
    };
    if archive == "only" {
        let no_live = tool_error(format!("archive=only : skipping live fetch for {url}"));
        return match try_resurrect(daemon, url, &no_live).await {
            Some(v) => v,
            None => tool_error_structured(
                format!("archive: no Wayback snapshot found for {url}"),
                "permanent",
                Some(json!({
                    "url": url,
                    "next_action": "the URL was never archived : try web_search for a live alternative",
                })),
            ),
        };
    }
    let result = fetch_single_inner(daemon, args, url).await;
    if archive == "off" || result.get("isError") != Some(&json!(true)) {
        return result;
    }
    // Resurreactable failures only: dead pages and hard walls.
    // Transient network errors mean "maybe dead", not "dead" : a
    // snapshot would launder an unknown into fake certainty.
    let resurrectable = result
        .pointer("/structuredContent/verdict")
        .and_then(Value::as_str)
        .is_some_and(|v| matches!(v, "SoftNotFound" | "Paywall" | "Challenge" | "AuthWall"))
        || result
            .pointer("/structuredContent/status")
            .and_then(Value::as_u64)
            .is_some_and(|s| s == 404 || s == 410);
    if !resurrectable {
        return result;
    }
    match try_resurrect(daemon, url, &result).await {
        Some(v) => v,
        None => result,
    }
}

/// v3 F3: find the rel=next pagination link (rel may carry other
/// tokens, e.g. rel="next chapter"); resolved against `base`.
pub(super) fn find_rel_next(html: &str, base: &str) -> Option<String> {
    let doc = scraper::Html::parse_document(html);
    let sel = scraper::Selector::parse("link[rel], a[rel]").ok()?;
    let base = url::Url::parse(base).ok()?;
    for el in doc.select(&sel) {
        let rel = el.value().attr("rel").unwrap_or_default();
        if !rel
            .split_whitespace()
            .any(|t| t.eq_ignore_ascii_case("next"))
        {
            continue;
        }
        if let Some(href) = el.value().attr("href")
            && let Ok(joined) = base.join(href)
        {
            return Some(joined.to_string());
        }
    }
    None
}

/// Strip a part's frontmatter (title line, URL line, description
/// line) : stitched parts share the article's chrome, and the
/// `*(part N)*` marker already carries the context.
pub(super) fn strip_part_frontmatter(md: &str) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut start = 0;
    if lines.first().is_some_and(|l| l.starts_with("# ")) {
        start = 1;
    }
    if lines.get(start).is_some_and(|l| l.starts_with("http")) {
        start += 1;
    }
    if lines.get(start).is_some_and(|l| l.starts_with("> ")) {
        start += 1;
    }
    lines[start..].join("\n").trim().to_string()
}

#[allow(clippy::field_reassign_with_default)]
pub(super) async fn fetch_single_inner(daemon: &Arc<Daemon>, args: &Value, url: &str) -> Value {
    let t0 = std::time::Instant::now();
    // Full parse up front: an unparseable URL would otherwise flow
    // through the whole pipeline with host="" : poisoning domain
    // profiles and producing confusing late errors.
    let parsed_url = match url::Url::parse(url) {
        Ok(u) => u,
        Err(e) => return tool_error(format!("fetch: invalid URL ({e})")),
    };
    let url_host = parsed_url.host_str().unwrap_or("").to_string();

    // Domain intelligence (v3): the adapters registry may rewrite
    // the URL to the site's own structured endpoint (reddit .json,
    // npm/PyPI/crates/Go/RubyGems APIs) : one cheap tier-1 request
    // for structured truth. `orig_url` stays the agent-facing
    // identity: history, handles, and display key on it.
    let no_adapter = args
        .get("_no_adapter")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let orig_url = url.to_string();
    let adapter_used: Option<&'static str>;
    let url = match crate::adapters::rewrite(&parsed_url) {
        Some((new_url, name))
            if !no_adapter && args.get("section").and_then(Value::as_str).is_none() =>
        {
            adapter_used = Some(name);
            new_url
        }
        _ => {
            adapter_used = None;
            url.to_string()
        }
    };
    // Adapter endpoints (registry CDNs, reddit .json) are plain
    // GET targets : never route them at the browser.
    let adapter_host = adapter_used.is_some();

    // Centralized SSRF guard (sync part): scheme, credentials, localhost/private literals.
    // DNS-resolved private addresses are checked at transport/browser layers.
    if let Err(e) = crate::fetch::guards::validate_url_basic(&url) {
        return tool_error_structured(
            format!("{e}"),
            "permanent",
            Some(json!({
                "url": url,
                "next_action": "private/loopback targets are blocked by design : use a public URL",
            })),
        );
    }
    let mut opts = ExtractOptions::default();
    opts.focus = args.get("focus").and_then(Value::as_str).map(String::from);
    opts.max_chars = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(200, 1_048_576));
    opts.offset = args
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(1_000_000_000) as usize;
    opts.section = args
        .get("section")
        .and_then(Value::as_str)
        .map(String::from);
    opts.selector = args
        .get("selector")
        .and_then(Value::as_str)
        .map(String::from);
    opts.toc = args.get("toc").and_then(Value::as_bool).unwrap_or(false);
    opts.include_links = args.get("links").and_then(Value::as_bool).unwrap_or(false);
    opts.include_media = args.get("media").and_then(Value::as_bool).unwrap_or(false);
    opts.must_contain = args
        .get("must_contain")
        .and_then(Value::as_str)
        .map(String::from);
    let image_text = args
        .get("image_text")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let since_last = args
        .get("since_last")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stitch = args.get("stitch").and_then(Value::as_bool).unwrap_or(false);
    let tier = args.get("tier").and_then(Value::as_str).unwrap_or("auto");
    let shot = args.get("shot").and_then(Value::as_str);

    // === v2: fetch-actions : browser control INSIDE fetch ===
    // A non-empty `actions` array routes the whole call to the
    // ghost with an action executor: navigate → act → extract.
    // Parsing/validation happens before any browser time is
    // spent; a typo in step 5 must not burn a launch on step 1.
    let actions = match args.get("actions") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => match crate::ghost::actions::parse(v) {
            Ok(a) => a,
            Err(e) => return tool_error(format!("fetch: {e}")),
        },
    };
    if !actions.is_empty() {
        if is_pdf_url_like(&url) {
            return tool_error(
                "fetch: actions cannot run on PDFs : fetch the PDF directly instead",
            );
        }
        if tier == "1" {
            return tool_error(
                "fetch: actions need the browser : use tier=auto (default) or tier=2",
            );
        }
        return fetch_with_actions(daemon, &url, &url_host, &opts, &actions, shot, image_text)
            .await;
    }

    let host = url_host;

    // === PDF early detection ===
    // Ghost can't render PDFs (Chrome's PDF viewer is a JS shell).
    // If the URL looks like a PDF, always fetch raw bytes (tier 1)
    // and route to the DonSheet engine. Never skip tier 1 for PDFs.
    // Uses the SAME helper as the actions guard : covers both the
    // `.pdf` suffix and the `/pdf/` path convention (arXiv serves
    // PDFs at /pdf/1706.03762 with no extension).
    let is_pdf_url = is_pdf_url_like(&url);

    // === Decision: how to route this fetch? ===
    // The self-improving loop: the domain profile decides
    // cold / warm / skip-to-solve / recheck-cold.
    // Adapter endpoints (reddit .json / old.reddit SSR, package
    // registry APIs) are plain-GET structured targets : never
    // need a browser. Force Cold even if a stale profile says
    // SkipToSolve (from a previous Xvfb failure that poisoned
    // the domain).
    // Remember the real origin scheme (v4 phase 0.2: the prober
    // probes the origin, not a guessed https upgrade).
    {
        let scheme = if url.starts_with("http://") {
            "http"
        } else {
            "https"
        };
        let port = url::Url::parse(&url)
            .ok()
            .and_then(|u| u.port_or_known_default())
            .unwrap_or(if scheme == "http" { 80 } else { 443 });
        let mut state = daemon.state.lock().await;
        state.note_origin(&host, scheme, port);
        // Longitudinal identity (v4 phase 0.3): mint or validate
        // the domain persona. Coherence drift or quarantine here
        // re-mints automatically.
        let caps = crate::persona::PersonaCaps::from_profile(daemon.fetcher.profile());
        state.ensure_persona(&host, &caps);
    }
    let route = if tier == "2" && !is_pdf_url && !adapter_host {
        RouteDecision::SkipToSolve
    } else if tier == "1" || is_pdf_url || adapter_host {
        RouteDecision::Cold
    } else {
        daemon.state.lock().await.route_for(&host)
    };

    let warm_cookies: Vec<CookieRecord> = match &route {
        RouteDecision::Warm(c) => c.clone(),
        _ => Vec::new(),
    };
    let is_warm = !warm_cookies.is_empty();
    let is_recheck = matches!(route, RouteDecision::RecheckCold);
    let skip_tier1 = matches!(route, RouteDecision::SkipToSolve);

    let mut tier_used = "1";
    if is_warm {
        daemon.fetcher.import_cookies(&warm_cookies).await;
        tier_used = "1(warm)";
    } else if is_recheck {
        tier_used = "1(recheck)";
    } else if skip_tier1 {
        tier_used = "2-direct";
    }

    let mut trace = Trace::default();
    let route_name = match &route {
        RouteDecision::Cold => "cold",
        RouteDecision::Warm(_) => "warm",
        RouteDecision::SkipToSolve => "skip-to-solve",
        RouteDecision::RecheckCold => "recheck-cold",
        RouteDecision::SolveCooldown(_) => "solve-cooldown",
    };
    trace.step("route", "domain-profile", route_name, 0);

    // Fail-fast memory: the wall survived real-browser passes
    // recently. Honest error, no browser cycle. The cooldown
    // lapses on its own; success memories from other layers
    // (record_cold_ok) clear it on their own cadence.
    if let RouteDecision::SolveCooldown(retry_in) = route {
        return tool_error_structured(
            format!(
                "known wall at {host} : recent browser passes did not clear it on this host; retrying before {retry_in}s from now would waste a solve cycle"
            ),
            "walled",
            Some(json!({
                "url": url,
                "status": 0,
                "retry_in_secs": retry_in,
                "next_action": "wait for the cooldown, or browse the site with an interactive agent browser (your human session clears walls this host cannot)",
                "escalation": trace.value(),
            })),
        );
    }

    // === Fetch (tier 1, unless skipped) ===
    let mut out: Option<crate::fetch::client::FetchOutcome> = None;

    // === v3 F1: search→fetch warm handoff ===
    // Enrichment already fetched the top search results : if
    // this URL is one of them, serve that body, skip the
    // network. One-shot (a second fetch goes to the wire for
    // freshness); the rest of the pipeline (extraction,
    // thin→ghost, history) runs unchanged on the cached body.
    let mut prewarmed = false;
    // Bind the take() result first: a lock guard in the if-let
    // scrutinee would live across the .await below and make the
    // future !Send.
    let prewarm_entry = if !is_pdf_url {
        daemon
            .searcher
            .prewarms()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(&orig_url)
    } else {
        None
    };
    if let Some(entry) = prewarm_entry {
        tier_used = "prewarmed";
        prewarmed = true;
        trace.step("prewarm", "search-handoff", "hit", 0);
        // law 6: make the warm handoff observable in `donsetch status`.
        daemon.state.lock().await.note_prewarm_served();
        out = Some(crate::fetch::client::FetchOutcome {
            url: orig_url.clone(),
            status: 200,
            alpn: "h2".to_string(),
            headers: vec![("content-type".to_string(), entry.content_type)],
            body: entry.body,
            redirects: 0,
            cache: crate::fetch::client::CacheState::None,
            used_pool: true,
            verdict: Verdict::ContentOk,
            elapsed: std::time::Duration::from_millis(0),
        });
    }

    if !skip_tier1 && !prewarmed {
        let t0 = std::time::Instant::now();
        let fetched = match daemon.fetcher.fetch(&url).await {
            Ok(o) => o,
            Err(e) => {
                if adapter_host && !no_adapter {
                    // Transport failure on the adapter endpoint :
                    // try the original URL before giving up.
                    let mut args2 = args.clone();
                    args2["_no_adapter"] = json!(true);
                    return Box::pin(fetch_single_inner(daemon, &args2, &orig_url)).await;
                }
                return tool_error_structured(
                    friendly_fetch_error(&e),
                    fetch_error_kind(&e),
                    Some(json!({
                        "url": url,
                        "status": 0,
                        "next_action": next_action_for(None, 0, fetch_error_kind(&e)),
                        "escalation": trace.value(),
                    })),
                );
            }
        };
        let ms = t0.elapsed().as_millis();
        let verdict_str = format!("{:?}", fetched.verdict);
        trace.step(
            "1",
            "http-fetch",
            &format!("{} status={}", verdict_str, fetched.status),
            ms,
        );
        out = Some(fetched);

        // === Observe the outcome ===
        // Every fetch teaches the domain profile something : but
        // only CHALLENGES say anything about walls. A 404, 429,
        // paywall, or auth wall is an honest terminal answer from
        // the origin; recording it as "walled" used to poison easy
        // domains into permanent skip-to-solve (every later fetch
        // burned a 20s ghost launch on a 404).
        let o = out.as_ref().unwrap();
        {
            let mut state = daemon.state.lock().await;
            // Tier-1 jar flush (v4 phase 1.4): persist the whole
            // cookie store on every completed navigation, inside
            // the same record_* save (one state write per fetch,
            // today's cost class). Browser-true: cookies survive
            // process restarts, so remote sessions see a RETURNING
            // visitor, not a fresh jar every run.
            state.sync_tier1_cookies(&daemon.fetcher.jar_all_snapshot().await);
            match o.verdict {
                Verdict::Challenge(_) => {
                    state.record_failure(&host, crate::ghost::cache::FailClass::Block);
                    if is_warm {
                        // Warm cookies went stale : learn the real lifetime.
                        state.record_warm_stale(&host);
                    } else {
                        // Cold (or recheck) was challenged : domain needs tier 2.
                        let vendor = match &o.verdict {
                            Verdict::Challenge(v) => Some(format!("{v:?}").to_lowercase()),
                            _ => None,
                        };
                        state.record_cold_walled(&host, vendor.as_deref());
                    }
                }
                Verdict::ContentOk => {
                    if is_warm {
                        // Warm succeeded : refresh the cookie vault (write-back).
                        let snap = daemon.fetcher.jar_snapshot(&host);
                        state.record_warm_ok(&host, &snap);
                    } else {
                        // Cold (or recheck) succeeded : if was needs_tier2, wall is gone.
                        state.record_cold_ok(&host);
                    }
                }
                // Everything else (404, rate-limit, paywall, auth,
                // hard block): counters only, no wall inference.
                _ => state.record_fetch(&host),
            }
        }
    }

    // === Verdict gate: everything except ContentOk/Challenge ===
    // is a terminal, legitimate response : clean error, no ghost.
    // Challenge on an explicit tier=1 request is also terminal.
    // Adapter failures (rate-limited .json, registry hiccup)
    // first fall back to the ORIGINAL URL through the generic
    // path : the adapter is an optimization, never a dependency.
    if let Some(o) = &out {
        match o.verdict {
            Verdict::ContentOk => {
                // Page-load realism (v4 phase 1.3): background
                // subresource burst for stealth-relevant hosts.
                crate::fetch::shadow::maybe_shadow(&daemon.fetcher, &daemon.state, &o.url, o).await;
            }
            Verdict::Challenge(_) if tier != "1" => {}
            v => {
                if adapter_host && !no_adapter {
                    let mut args2 = args.clone();
                    args2["_no_adapter"] = json!(true);
                    trace.step(
                        "adapter",
                        "fallback",
                        &format!("{:?} : retrying original URL", v),
                        0,
                    );
                    let mut res = Box::pin(fetch_single_inner(daemon, &args2, &orig_url)).await;
                    // Fold the adapter attempt into the trace so
                    // the agent sees why there are two hops.
                    if let Some(sc) = res.pointer_mut("/structuredContent") {
                        sc["adapter_fallback"] = json!(true);
                    }
                    return res;
                }
                let kind = verdict_kind(v, o.status);
                // v3.4: bypass fetch for hard walls (Challenge/Blocked).
                // Fires on tier != "1" (respect explicit no-escalation).
                // Skip AuthWall/Paywall/SoftNotFound (credentials/money/dead).
                if tier != "1"
                    && matches!(v, Verdict::Challenge(_) | Verdict::Blocked)
                    && let Some(v3) = try_bypass(daemon, &url, &opts, &mut trace).await
                {
                    return v3;
                }
                return tool_error_structured(
                    verdict_error(v, o.status, &o.url),
                    kind,
                    Some(json!({
                        "url": o.url,
                        "status": o.status,
                        "verdict": format!("{:?}", v),
                        "next_action": next_action_for(Some(v), o.status, kind),
                        "escalation": trace.value(),
                    })),
                );
            }
        }
    }

    // === Adapter shape check ===
    // A 200 that isn't JSON on a rewritten endpoint (login walls,
    // HTML error interstitials) bought the adapter nothing : fall
    // back to the original URL through the full generic pipeline
    // (which still escalates to ghost if the page is a shell).
    if adapter_host
        && !no_adapter
        && let Some(o) = &out
        && matches!(o.verdict, Verdict::ContentOk)
        && !matches!(
            o.body.iter().find(|b| !b.is_ascii_whitespace()),
            Some(b'{') | Some(b'[')
        )
    {
        trace.step(
            "adapter",
            "shape-mismatch",
            "200 but not JSON : retrying original URL",
            0,
        );
        let mut args2 = args.clone();
        args2["_no_adapter"] = json!(true);
        let mut res = Box::pin(fetch_single_inner(daemon, &args2, &orig_url)).await;
        if let Some(sc) = res.pointer_mut("/structuredContent") {
            sc["adapter_fallback"] = json!(true);
        }
        return res;
    }

    // === Tier-1 extraction (when we have a body) ===
    let mut final_ex: Option<extract::Extracted> = None;
    let mut final_tier: &str = tier_used;
    let mut final_status: u16 = out.as_ref().map(|o| o.status).unwrap_or(0);
    let mut final_url: String = url.clone();
    let mut final_verdict: String = out
        .as_ref()
        .map(|o| format!("{:?}", o.verdict))
        .unwrap_or_else(|| "ContentOk".to_string());

    if let Some(o) = &out
        && matches!(o.verdict, Verdict::ContentOk)
    {
        let ct = o
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        // Binary content guard: images, video, audio, etc.
        // Don't pass binary bytes to extract (mojibake).
        if crate::fetch::guards::is_binary(&o.body, &ct) {
            let kind = ct.split(';').next().unwrap_or("unknown").trim();
            return tool_error_structured(
                format!(
                    "binary content: {url} returned {kind} ({} bytes) : not text, cannot extract",
                    o.body.len()
                ),
                "permanent",
                Some(json!({
                    "url": url,
                    "next_action": "this URL is a raw file, not a page : if it is a PDF, fetch it directly (DonSeTch parses PDFs); otherwise look for an HTML landing page via web_search",
                })),
            );
        }
        match extract::extract(&o.body, &ct, &o.url, &opts) {
            Ok(e) => {
                final_url = o.url.clone();
                final_ex = Some(e);
            }
            Err(e) => {
                return tool_error_structured(
                    format!("content extraction failed: {e}"),
                    "transient",
                    Some(json!({
                        "url": url,
                        "next_action": "retry with a narrow selector= or focus=; if the page is JS-heavy, tier=2 renders it in a browser",
                    })),
                );
            }
        }
    }

    let ex_thin = final_ex.as_ref().map(|e| e.thin).unwrap_or(false);
    let challenge = out
        .as_ref()
        .map(|o| matches!(o.verdict, Verdict::Challenge(_)))
        .unwrap_or(false);

    // Warm cookies that only buy a SHELL are stale cookies : but
    // the evidence must be a shell, not an extraction gap. A warm
    // ContentOk whose body is big yet nearly invisible-text-free
    // (JS shell) means the clearance bought nothing. A body with
    // rich visible text that extracts thin is a DonSift gap :
    // killing valid cookies for it is the gallery-page bug.
    let shell_warm = is_warm && ex_thin && {
        let o = out.as_ref().unwrap();
        o.body.len() > 20_000
            && (crate::detect::walls::visible_text_count(&o.body) as f64 / o.body.len() as f64)
                < 0.02
    };
    if shell_warm {
        daemon.state.lock().await.record_warm_stale(&host);
    }

    // Tier-1 links fallback: listing/feed pages over plain
    // HTTP (Hacker News, indexes) die in the prose pipeline
    // simply for being link-dense. Try links-keeping
    // extraction before any ghost work.
    if final_ex.as_ref().map(|e| e.thin).unwrap_or(false)
        && !opts.include_links
        && let Some(o) = &out
        && matches!(o.verdict, Verdict::ContentOk)
    {
        let ct = o
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let mut lopts = opts.clone();
        lopts.include_links = true;
        if let Ok(e3) = extract::extract(&o.body, &ct, &o.url, &lopts)
            && !e3.thin
        {
            final_ex = Some(e3);
            final_tier = "1(links)";
            trace.step("1", "links-extract", "ok", 0);
        }
    }

    // === Tier 2 via ghost (unified) ===
    // Triggers: explicit tier 2, profile skip-to-solve, challenge
    // wall, or tier 1 produced only a JS shell on auto tier.
    // (thin recomputed AFTER the tier-1 links fallback.)
    //
    // Exception: very small pages (< 5KB) that came back thin are
    // 404/error pages, not JS shells. JS shells are > 50KB (React
    // apps, SPAs). A 2KB page with no content is a 404 : don't
    // waste 20s launching a browser for it.
    let still_thin = final_ex.as_ref().map(|e| e.thin).unwrap_or(false);
    let page_size = out.as_ref().map(|o| o.body.len()).unwrap_or(0);
    // PDF detection: if the response is a PDF (content-type or magic
    // bytes), never escalate to ghost : Chrome's PDF viewer is a JS
    // shell with no extractable text. PDFs are handled by DonSheet.
    let is_pdf_content = out
        .as_ref()
        .map(|o| {
            let ct = o
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            crate::fetch::guards::is_pdf(&o.body, &ct)
        })
        .unwrap_or(is_pdf_url);
    // Small 404 check: a small thin page is likely a 404/error.
    // But a small PDF is still a PDF : DonSheet handles it.
    let is_small_404 =
        page_size > 0 && page_size < 5_000 && still_thin && !challenge && !is_pdf_content;
    let need_ghost = !is_pdf_content
        && !adapter_host // adapter endpoints (reddit .json, registry APIs) are plain GETs
        && ((challenge && tier != "1" && !is_small_404)
            || skip_tier1
            || (still_thin && tier == "auto" && !is_small_404));

    if need_ghost {
        // Render-cache shortcut: a previously recovered DOM.
        // Verified non-thin AND non-challenge before serving : the
        // cache used to store shells and challenge interstitials,
        // re-serving them forever as ContentOk.
        if ex_thin
            && tier == "auto"
            && let Some(rc) = daemon.state.lock().await.render_for(&final_url).cloned()
            && let Ok(e2) = extract::extract(
                rc.html.as_bytes(),
                extract::charset::GHOST_TEXT_CT,
                &final_url,
                &opts,
            )
            && !e2.thin
        {
            // Defense in depth: even if a challenge page slipped into
            // the cache (pre-fix), don't serve it as ContentOk.
            let cached_verdict = crate::detect::walls::detect_dom_smart(rc.html.as_bytes());
            if !matches!(cached_verdict, crate::detect::walls::Verdict::Challenge(_)) {
                let vstr = format!("{:?}", cached_verdict);
                trace.step("cache", "render-hit", "ok", 0);
                let mut res = finish_result(
                    &e2,
                    "render-cache",
                    final_status,
                    &vstr,
                    &final_url,
                    &trace,
                    t0.elapsed().as_millis(),
                );
                res["_meta"]["ttlMs"] = json!(300_000);
                res["_meta"]["cacheScope"] = json!("session");
                if prewarmed {
                    res["_meta"]["com.donsetch/fetch-debug"]["prewarmed_by_search"] = json!(true);
                }
                apply_link_handles(daemon, &mut res).await;
                return res;
            }
        }

        match ghost_escalate(
            daemon,
            &url,
            &host,
            &opts,
            challenge || shell_warm || skip_tier1,
            shot,
            &mut trace,
        )
        .await
        {
            Ok((e, tier2, status, furl)) => {
                final_ex = Some(e);
                final_tier = tier2;
                final_status = status;
                final_url = furl;
                // Ghost beat the challenge : the verdict should reflect
                // the actual content, not the tier-1 wall that was
                // bypassed. Without this, a successfully rendered page
                // shows "Challenge(DataDome)" in the verdict field.
                final_verdict = "ContentOk".to_string();
            }
            Err((msg, kind)) => {
                // A ghost failure on a warm-routed fetch means the
                // cookies no longer clear the wall : count it as the
                // second warm failure so the vault clears (first was
                // the tier-1 challenge that triggered escalation).
                if is_warm {
                    daemon.state.lock().await.record_warm_stale(&host);
                }
                // v3.4: ghost hit a hard wall (kind == "walled"),
                // try bypass unlocker before giving up.
                if kind == "walled"
                    && let Some(v3) = try_bypass(daemon, &url, &opts, &mut trace).await
                {
                    return v3;
                }
                return tool_error_structured(
                    msg,
                    kind,
                    Some(json!({
                        "url": url,
                        "status": final_status,
                        "verdict": final_verdict,
                        "next_action": next_action_for(out.as_ref().map(|o| o.verdict), final_status, kind),
                        "escalation": trace.value(),
                    })),
                );
            }
        }
    }

    // === v3 F3: article pagination stitching ===
    // rel=next chains walked to a bounded budget: one call returns
    // the whole article with part markers instead of eight calls.
    let mut stitched_parts: usize = 1;
    if stitch {
        const STITCH_MAX_PARTS: usize = 6;
        const STITCH_BUDGET: usize = 48_000;
        let base = out.as_ref().map(|o| {
            let ct = o
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            (crate::extract::charset::decode(&o.body, &ct), o.url.clone())
        });
        if let Some((html, base_url)) = base
            && let Some(mut next) = find_rel_next(&html, &base_url)
            && let Some(ex) = final_ex.as_mut()
        {
            let base_host = url::Url::parse(&base_url)
                .ok()
                .and_then(|u| u.host_str().map(String::from));
            let mut total = ex.markdown.len();
            let mut parts: Vec<String> = Vec::new();
            while parts.len() + 1 < STITCH_MAX_PARTS && total < STITCH_BUDGET {
                // Hijack guard: never follow rel=next off-host.
                let Ok(nu) = url::Url::parse(&next) else {
                    break;
                };
                if nu.host_str().map(String::from) != base_host {
                    break;
                }
                let fetched = match daemon.fetcher.fetch(&next).await {
                    Ok(o2) if matches!(o2.verdict, Verdict::ContentOk) => o2,
                    _ => break,
                };
                trace.step("stitch", "fetch-part", &next, fetched.elapsed.as_millis());
                let ct2 = fetched
                    .headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let html2 = crate::extract::charset::decode(&fetched.body, &ct2);
                let mut popts = opts.clone();
                popts.max_chars = Some(8_000);
                match extract::extract(&fetched.body, &ct2, &fetched.url, &popts) {
                    Ok(pe) => {
                        let md = strip_part_frontmatter(&pe.markdown);
                        total += md.len();
                        parts.push(md);
                        next = match find_rel_next(&html2, &fetched.url) {
                            Some(n) => n,
                            None => break,
                        };
                    }
                    Err(_) => break,
                }
            }
            if !parts.is_empty() {
                stitched_parts = parts.len() + 1;
                for (i, p) in parts.iter().enumerate() {
                    ex.markdown
                        .push_str(&format!("\n\n---\n\n*(part {})*\n\n", i + 2));
                    ex.markdown.push_str(p);
                }
                // One article, one budget: the stitched cap is the
                // larger of the user's max and 48k.
                let cap = opts
                    .max_chars
                    .unwrap_or(16_000)
                    .max(200)
                    .max(STITCH_BUDGET.min(48_000));
                let (slice, next_off) = extract::paginate_public(&ex.markdown, opts.offset, cap);
                ex.markdown = slice;
                ex.next_offset = next_off;
                ex.total_chars = total;
                ex.tokens_est = ex.markdown.len() / 4;
            }
        }
    }

    let Some(ex) = final_ex else {
        return tool_error_structured(
            "all fetch tiers exhausted : no response received",
            "permanent",
            Some(json!({
                "url": url,
                "status": 0,
                "next_action": "retry : if repeated, the site may be down",
                "escalation": trace.value(),
            })),
        );
    };

    // Small 404 page: if we didn't escalate to ghost (is_small_404)
    // and the extraction is still thin/empty, return "not found".
    // This is honest : the page exists (HTTP 200) but has no content.
    // Could be a non-existent product, a deleted page, or a soft 404.
    if is_small_404 {
        return tool_error_structured(
            format!(
                "not found: {url} : page returned no content (may not exist or requires JavaScript)"
            ),
            "permanent",
            Some(json!({
                "url": url,
                "status": final_status,
                "verdict": "SoftNotFound",
                "next_action": next_action_for(Some(Verdict::SoftNotFound), final_status, "permanent"),
                "escalation": trace.value(),
            })),
        );
    }

    // v3 adapters: the agent-facing URL stays the one they asked
    // for : history, handles, and display key on it, not the
    // rewritten API endpoint.
    let display_url = if adapter_used.is_some() {
        orig_url.clone()
    } else {
        final_url.clone()
    };
    let mut res = finish_result(
        &ex,
        final_tier,
        final_status,
        &final_verdict,
        &display_url,
        &trace,
        t0.elapsed().as_millis(),
    );
    if prewarmed {
        res["_meta"]["com.donsetch/fetch-debug"]["prewarmed_by_search"] = json!(true);
    }
    if stitched_parts > 1
        && let Some(sc) = res.pointer_mut("/structuredContent")
    {
        sc["stitched"] = json!(stitched_parts);
    }
    apply_link_handles(daemon, &mut res).await;
    // v3 anti-cloak: a known-walled domain passing tier-1 cold
    // cleanly is suspicious. One equivalence check; a warning is
    // stamped, never a silent pass.
    let mut cloak_warning: Option<String> = None;
    let profile_walled = daemon.state.lock().await.is_known_walled(&host);
    if profile_walled
        && !skip_tier1
        && !is_warm
        && let Some((_sim, note)) = anticloak_check(daemon, &url, &ex.markdown).await
    {
        cloak_warning = Some(note);
    }
    if let Some(note) = &cloak_warning {
        if let Some(cell) = res.pointer_mut("/content/0/text")
            && let Some(md) = cell.as_str().map(String::from)
        {
            *cell = json!(format!("*[cloak_suspected: {note}]*\n\n{md}"));
        }
        if let Some(sc) = res.pointer_mut("/structuredContent") {
            sc["cloak_suspected"] = json!(true);
            sc["cloak_note"] = json!(note);
        }
    }

    // v3 freshness: the server's own Last-Modified, when it
    // deigns to tell the truth about it.
    if let Some(o) = &out
        && matches!(o.verdict, Verdict::ContentOk)
        && let Some(lm) = o
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("last-modified"))
            .map(|(_, v)| v.clone())
        && let Some(sc) = res.pointer_mut("/structuredContent")
    {
        sc["server_modified"] = json!(lm);
    }
    apply_page_history(
        daemon,
        &mut res,
        &display_url,
        PageFacts {
            fingerprint: ex.fingerprint.as_deref(),
            markdown: &ex.markdown,
            title: ex.title.as_deref(),
            complete: ex.next_offset.is_none(),
        },
        since_last,
    );
    if image_text {
        apply_image_ocr(daemon, &mut res, &ex.images).await;
    }
    res
}

/// Unified tier-2: ghost render + cookie harvest + tier-1 retry,
/// then pick the candidate with the best content yield. Ok ONLY
/// when a candidate extracts as real content : a shell is a
/// failure, never a success. This is the loop the design always
/// promised: escalate, render, hand cookies back to tier 1.
///
/// Anti-bot bypass fetch (v3.4): when ghost fails on a hard wall,
/// hand the URL to Bright Data Web Unlocker API. Opt-in via
/// `donsetch keys add unlocker <key>`. No key = inert, returns None.
/// Only fires on wall verdicts (Challenge/Blocked) and ghost "walled"
/// failures. Respects tier=1 (explicit no-escalation).
pub(super) async fn try_bypass(
    daemon: &Arc<Daemon>,
    url: &str,
    opts: &ExtractOptions,
    trace: &mut Trace,
) -> Option<Value> {
    let cfg = crate::fetch::bypass::BypassConfig::from_env();
    if !cfg.enabled {
        return None;
    }
    let byok = crate::search::byok::store::ByokConfig::load();
    let key = crate::fetch::bypass::active_unlocker_key(&byok)?;
    let cache_dir = crate::paths::cache_dir();
    let t0 = std::time::Instant::now();
    let outcome = match crate::fetch::bypass::unlock(&key, url, &cfg, &cache_dir).await {
        Ok(o) => o,
        Err(e) => {
            crate::fetch::bypass::apply_key_state("unlocker", &key, &e);
            // Every failure class carries its own recovery hint;
            // the trace is the channel agents (and users) can see.
            let msg = format!("{e} [hint: {}]", e.guidance());
            trace.step("bypass", "unlocker", &msg, t0.elapsed().as_millis());
            return None;
        }
    };
    if crate::fetch::guards::is_binary(&outcome.body, &outcome.content_type) {
        trace.step(
            "bypass",
            "unlocker",
            "binary content",
            t0.elapsed().as_millis(),
        );
        return None;
    }
    let ex = match extract::extract(&outcome.body, &outcome.content_type, url, opts) {
        Ok(e) => e,
        Err(e) => {
            trace.step(
                "bypass",
                "extract",
                &format!("{e}"),
                t0.elapsed().as_millis(),
            );
            return None;
        }
    };
    trace.step(
        "bypass",
        "unlocker",
        &format!(
            "{} status={} body={}KB",
            if outcome.cached { "cache hit" } else { "ok" },
            outcome.status,
            outcome.body.len() / 1024
        ),
        t0.elapsed().as_millis(),
    );
    let tier = if outcome.cached {
        "3-cached"
    } else {
        "3-bypass"
    };
    let mut res = finish_result(
        &ex,
        tier,
        outcome.status,
        "ContentOk",
        url,
        trace,
        t0.elapsed().as_millis(),
    );
    res["_meta"]["com.donsetch/fetch-debug"]["bypass"] = json!({
        "provider": "brightdata",
        "tier": if outcome.cached { "cache" } else { "unlocker" },
        "cache": outcome.cached,
    });
    apply_link_handles(daemon, &mut res).await;
    Some(res)
}

/// `learn` = this escalation was WALL-DRIVEN (challenge seen, warm
/// cookies bought a shell, or the profile routed skip-to-solve).
/// A wall-driven success records the solve so the next fetch can
/// ride warm tier 1 : with `replay_ok` set from the tier-1 retry's
/// actual outcome. A pure SPA render (thin content, no wall) never
/// touches the domain profile: the site isn't walled, it's JS-only.
pub(super) async fn ghost_escalate(
    daemon: &Arc<Daemon>,
    url: &str,
    host: &str,
    opts: &ExtractOptions,
    learn: bool,
    shot: Option<&str>,
    trace: &mut Trace,
) -> Result<(extract::Extracted, &'static str, u16, String), (String, &'static str)> {
    let t0 = std::time::Instant::now();
    let mut g = daemon
        .ghost_mgr
        .acquire_for(&daemon.profile, Some(host))
        .await
        .map_err(|e| (format!("browser launch failed: {e}"), "permanent"))?;
    trace.step("2", "browser-launch", "ok", t0.elapsed().as_millis());
    let t1 = std::time::Instant::now();
    let mut page = match ops::ghost_fetch(&mut g, url, std::time::Duration::from_secs(20)).await {
        Ok(p) => p,
        Err(e) => {
            // CDP timeouts on first attempt are transient : the
            // browser was still warming up. Retry once before
            // conceding a permanent failure.
            if std::env::var_os("DONGHOST_DEBUG").is_some() {
                eprintln!("[ghost_escalate] first attempt failed: {e}, retrying...");
            }
            ops::ghost_fetch(&mut g, url, std::time::Duration::from_secs(20))
                .await
                .map_err(|e| (format!("browser automation error: {e}"), "permanent"))?
        }
    };
    trace.step(
        "2",
        "ghost-render",
        &format!("captcha={} dom={}KB", page.captcha, page.html.len() / 1024),
        t1.elapsed().as_millis(),
    );
    if std::env::var_os("DONGHOST_DEBUG").is_some() {
        let safe: String = host
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let dir = crate::paths::cache_dir().join("ghost-debug");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(format!("dom-{safe}.html"));
        let _ = std::fs::write(&p, &page.html);
        eprintln!(
            "[ghost_escalate] dom={}B dumped to {}",
            page.html.len(),
            p.display()
        );
    }
    if page.captcha {
        // Solve-grade second pass: some vendors (Akamai) run the
        // sensor on the first load and only clear on a follow-up
        // navigation once their first-party state is planted. The
        // browser is warm now: one bounded re-render, then a
        // settle re-check. Never more: two passes is the ceiling,
        // an honest captcha stays an honest captcha.
        let t1b = std::time::Instant::now();
        let page2 = ops::ghost_fetch(&mut g, url, std::time::Duration::from_secs(20))
            .await
            .ok();
        match page2 {
            Some(p2) if !p2.captcha => {
                trace.step(
                    "2",
                    "solve-pass2",
                    &format!(
                        "cleared: captcha={} dom={}KB",
                        p2.captcha,
                        p2.html.len() / 1024
                    ),
                    t1b.elapsed().as_millis(),
                );
                // Fall through into the normal harvest/retry flow.
                page = p2;
            }
            Some(p2) if p2.captcha => {
                if let Some(p) = shot {
                    let _ = g.screenshot(p).await;
                }
                // The wall survived BOTH passes in a real browser:
                // this is wall-persisting evidence, recorded.
                daemon.state.lock().await.record_wall_failed(host);
                return Err((
                    format!(
                        "blocked at {url} : interactive captcha or challenge could not be solved automatically. Use an Agent browser to browse sites like these"
                    ),
                    "walled",
                ));
            }
            // ghost_fetch errored on the retry (automation failure,
            // not a wall): no wall memory recorded.
            None => {
                if let Some(p) = shot {
                    let _ = g.screenshot(p).await;
                }
                return Err((
                    format!(
                        "blocked at {url} : interactive captcha or challenge could not be solved automatically. Use an Agent browser to browse sites like these"
                    ),
                    "walled",
                ));
            }
            _ => unreachable!(),
        }
    }
    if !page.captcha {
        // Solve-grade pass for invisible walls: Akamai-class vendors
        // render a "still checking" page that settles (no captcha
        // form) but whose DOM classifies as a wall. The sensor fires
        // during pass 1 and plants first-party state; a warm re-render
        // right after is the pass that gets the real page. One
        // attempt only, then fall into the normal flow regardless.
        let dom_verdict = crate::detect::walls::detect_dom_smart(page.html.as_bytes());
        // Gate: a stuck wall burns the FULL 20s in pass 1; a second
        // 20s pass2b on top = 40s of dead air. Only re-render when
        // pass 1 returned fast (the wall cleared mid-flight and the
        // re-render catches the real page).
        if matches!(
            dom_verdict,
            crate::detect::walls::Verdict::Challenge(_) | crate::detect::walls::Verdict::Blocked
        ) && page.took < std::time::Duration::from_secs(12)
        {
            let t1b = std::time::Instant::now();
            if let Ok(p2) = ops::ghost_fetch(&mut g, url, std::time::Duration::from_secs(20)).await
            {
                let v2 = crate::detect::walls::detect_dom_smart(p2.html.as_bytes());
                if !p2.captcha
                    && !matches!(
                        v2,
                        crate::detect::walls::Verdict::Challenge(_)
                            | crate::detect::walls::Verdict::Blocked
                    )
                {
                    trace.step(
                        "2",
                        "solve-pass2",
                        &format!("cleared after re-render: {}KB", p2.html.len() / 1024),
                        t1b.elapsed().as_millis(),
                    );
                    page = p2;
                }
            }
        }
    }
    if !page.cookies.is_empty() {
        daemon.fetcher.import_cookies(&page.cookies).await;
        crate::ghost::cache::store_session_cookies(&page.cookies);
    }
    // Retry tier 1 with fresh cookies : the cheap path back to
    // normal HTTP when the gate was cookie-driven.
    let t2 = std::time::Instant::now();
    let retry = if !page.cookies.is_empty() {
        let r = daemon.fetcher.fetch(url).await.ok();
        trace.step(
            "1",
            "http-retry-with-ghost-cookies",
            &format!(
                "cookies={} status={}",
                page.cookies.len(),
                r.as_ref().map(|o| o.status).unwrap_or(0)
            ),
            t2.elapsed().as_millis(),
        );
        r
    } else {
        None
    };

    // Replay verification: cookies are only "warm-worthy" when the
    // tier-1 retry returned real content with them. A walled or
    // shell retry means the vendor binds clearance to the browser
    // fingerprint : record replay_ok=false so route_for never
    // serves a doomed Warm roundtrip again.
    let mut replay_content_ok = false;

    // The retry is the oracle of record for TERMINAL verdicts: a
    // 404/paywall on tier 1 means the ghost spent its time
    // rendering a dead page (browsers render 404s too). The ghost's
    // pretty DOM must never launder a dead URL into ContentOk.
    //
    // AuthWall is deliberately excluded: an auth wall on the
    // retry means the HTTP path can't authenticate, but the
    // browser may have (Chromium handles userinfo/cookies/JS
    // auth natively). Discarding the ghost's content because the
    // tier-1 retry hit a wall the browser already cleared is
    // the core tier-2 regression in issue #15.
    if let Some(r) = &retry
        && matches!(r.verdict, Verdict::SoftNotFound | Verdict::Paywall)
    {
        let kind = verdict_kind(r.verdict, r.status);
        return Err((verdict_error(r.verdict, r.status, &r.url), kind));
    }

    // Candidates: retry bytes (cheap path) and the ghost's own
    // rendered DOM. Non-thin always beats thin; within a class,
    // bigger yield wins. The old code always preferred the retry
    // and discarded the browser's work : the core tier-2 bug.
    let mut best: Option<(bool, extract::Extracted, &'static str, u16, String)> = None;

    if let Some(r) = &retry
        && matches!(r.verdict, Verdict::ContentOk)
    {
        let ct = r
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        if !crate::fetch::guards::is_binary(&r.body, &ct)
            && let Ok(e) = extract::extract(&r.body, &ct, &r.url, opts)
        {
            let thin = e.thin;
            replay_content_ok = !thin;
            let better = match &best {
                None => true,
                Some((bt, be, ..)) => {
                    (!thin && *bt) || (thin == *bt && e.total_chars > be.total_chars)
                }
            };
            if better {
                best = Some((thin, e, "1+ghost-solve", r.status, r.url.clone()));
            }
        }
    }
    if let Ok(e2) = extract::extract(
        page.html.as_bytes(),
        extract::charset::GHOST_TEXT_CT,
        url,
        opts,
    ) {
        let thin = e2.thin;
        let better = match &best {
            None => true,
            Some((bt, be, ..)) => {
                (!thin && *bt) || (thin == *bt && e2.total_chars > be.total_chars)
            }
        };
        if better {
            best = Some((
                thin,
                e2,
                "ghost-dom",
                retry.as_ref().map(|r| r.status).unwrap_or(200),
                url.to_string(),
            ));
        }
    }

    // Links fallback: listing/feed pages (marketplaces, SERPs,
    // thread indexes) are link-dense by nature : the prose-tuned
    // pipeline kills them. Re-extract with links kept as a last
    // candidate before conceding.
    if best.as_ref().map(|(thin, ..)| *thin).unwrap_or(true) {
        let mut lopts = opts.clone();
        lopts.include_links = true;
        if let Ok(e3) = extract::extract(
            page.html.as_bytes(),
            extract::charset::GHOST_TEXT_CT,
            url,
            &lopts,
        ) {
            let thin = e3.thin;
            let better = match &best {
                None => true,
                Some((bt, be, ..)) => {
                    (!thin && *bt) || (thin == *bt && e3.total_chars > be.total_chars)
                }
            };
            if better {
                best = Some((
                    thin,
                    e3,
                    "ghost-dom(links)",
                    retry.as_ref().map(|r| r.status).unwrap_or(200),
                    url.to_string(),
                ));
            }
        }
    }

    if let Some((thin, e, t, s, u)) = best
        && !thin
    {
        // Learning is gated on WALL-DRIVEN escalation AND gated on
        // CONTENT : success is "we got content", not "we got HTTP
        // 200". The replay probe (or its absence) sets replay_ok.
        if learn {
            daemon.state.lock().await.record_solved(
                host,
                &page.cookies,
                page.vendor.as_deref(),
                replay_content_ok,
            );
            crate::ghost::cache::store_session_cookies(&page.cookies);
        }
        // Don't cache challenge/wall DOMs : defense in depth alongside
        // the ghost_fetch timeout check. A challenge page that has
        // enough block structure to pass !thin would otherwise be
        // cached and re-served as ContentOk forever.
        let dom_verdict = crate::detect::walls::detect_dom_smart(page.html.as_bytes());
        if !matches!(dom_verdict, crate::detect::walls::Verdict::Challenge(_)) {
            daemon.state.lock().await.record_render(&u, &page.html);
        }
        return Ok((e, t, s, u));
    }

    // JSON endpoints recovered through the ghost (reddit .json,
    // registry APIs): Chrome wraps raw JSON in a <pre>; unwrap it
    // and run the adapter over the true bytes instead of
    // misclassifying a JSON page as a wall or a thin shell. This
    // closes the adapter-after-solve gap: a reddit listing walled
    // on tier-1 re-parses cleanly here after ghost recovery.
    let pre = scraper::Html::parse_document(&page.html)
        .select(&scraper::Selector::parse("pre").unwrap_or(scraper::Selector::parse("*").unwrap()))
        .next()
        .map(|n| n.text().collect::<String>());
    let candidate = pre.as_deref().unwrap_or(&page.html);
    let trimmed = candidate.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Some(ext) =
            crate::adapters::extract_json(trimmed.as_bytes(), "application/json", url, opts)
        {
            return Ok((
                ext,
                "ghost-json",
                retry.as_ref().map(|r| r.status).unwrap_or(200),
                url.to_string(),
            ));
        }
        // No adapter: DonSift's generic pass for the raw body.
        if let Ok(ext) = extract::extract(trimmed.as_bytes(), "application/json", url, opts) {
            return Ok((
                ext,
                "ghost-json",
                retry.as_ref().map(|r| r.status).unwrap_or(200),
                url.to_string(),
            ));
        }
    }

    // Last resort: raw text fallback. If the ghost DOM has real
    // visible text but DonSift's block extraction couldn't parse
    // it (complex DOM, non-standard structure), strip tags and
    // return the visible text. This makes "found DOM but failed
    // to extract content" IMPOSSIBLE when the DOM has real text.
    //
    // BUT: only return Ok when the fallback is non-thin (>= 800
    // chars of visible text). A captcha/challenge page with 300
    // chars of "Please verify you are a human" must NOT be
    // returned as ContentOk : the agent would trust it.
    if !page.captcha {
        let doc = scraper::Html::parse_document(&page.html);
        let meta = crate::extract::metadata::metadata(&doc);
        let max_chars = opts.max_chars.unwrap_or(16_000).max(200);
        if let Some(fb) =
            crate::extract::fallback::text_fallback(&page.html, &meta, url, opts, max_chars)
            && !fb.thin
        {
            return Ok((fb, "ghost-text", 200, url.to_string()));
        }
    }

    // Differentiate: small DOM with no content = not found / blocked.
    // Large DOM with no extractable content = genuine extraction failure.
    // A challenge page (captcha, bot wall) must ALWAYS return "blocked"
    // with kind="walled" (exit 3), regardless of DOM size : never "not
    // found" (exit 1). This fixes the Medium URL that gave different
    // verdicts across runs: sometimes the challenge page was < 5KB
    // (→ "not found"), sometimes larger (→ "blocked").
    let dom_verdict = crate::detect::walls::detect_dom_smart(page.html.as_bytes());
    if matches!(dom_verdict, Verdict::Challenge(_)) {
        daemon.state.lock().await.record_wall_failed(host);
        return Err((
            format!(
                "blocked at {url} : interactive captcha or challenge could not be solved automatically. Use an Agent browser to browse sites like these"
            ),
            "walled",
        ));
    }
    if page.html.len() < 5_000 {
        return Err((
            format!(
                "not found: {url} : page returned no extractable content (may not exist, is an empty JS shell, or the site served an anti-bot interstitial too small for the wall detector)"
            ),
            "permanent",
        ));
    }
    daemon.state.lock().await.record_wall_failed(host);
    Err((
        format!(
            "blocked at {url} : tier 2 rendered a {}KB DOM but no real content was extractable. Use an Agent browser to browse sites like these",
            page.html.len() / 1024
        ),
        "walled",
    ))
}

/// PDF-shaped URL check for the actions guard (before the main
/// flow computes its own is_pdf_url). Covers both the .pdf
/// suffix convention and the /pdf/ path convention (arXiv:
/// arxiv.org/pdf/1706.03762 serves a PDF with no extension).
pub(super) fn is_pdf_url_like(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or(url).to_lowercase();
    if path.ends_with(".pdf") {
        return true;
    }
    // Path-segment "/pdf/" or trailing "/pdf" (arXiv, IACR,
    // many journal endpoints).
    let no_scheme = path
        .strip_prefix("https://")
        .or_else(|| path.strip_prefix("http://"))
        .unwrap_or(&path);
    let path_part = no_scheme.split_once('/').map(|(_, p)| p).unwrap_or("");
    let segs: Vec<&str> = path_part.split('/').filter(|s| !s.is_empty()).collect();
    segs.contains(&"pdf") || path_part.ends_with("/pdf")
}

/// v2: fetch with an action script : navigate, act (click /
/// type / press / scroll / wait), then run the NORMAL DonSift
/// extraction over the final DOM. focus/section/toc all work
/// on the interacted-with page. One call replaces hound's
/// navigate→act→act→read round-trips.
pub(super) async fn fetch_with_actions(
    daemon: &Arc<Daemon>,
    url: &str,
    host: &str,
    opts: &ExtractOptions,
    actions: &[crate::ghost::actions::Action],
    shot: Option<&str>,
    image_text: bool,
) -> Value {
    let mut trace = Trace::default();
    trace.step("route", "actions", "browser-script", 0);

    let t0 = std::time::Instant::now();
    let mut g = match daemon
        .ghost_mgr
        .acquire_for(&daemon.profile, Some(host))
        .await
    {
        Ok(g) => g,
        Err(e) => {
            return tool_error_structured(
                format!("browser launch failed: {e}"),
                "permanent",
                Some(json!({
                    "url": url,
                    "status": 0,
                    "next_action": "run `donsetch doctor` : the browser path is broken on this machine",
                    "escalation": trace.value(),
                })),
            );
        }
    };
    trace.step("2", "browser-launch", "ok", t0.elapsed().as_millis());

    // Initial render through the standard ghost oracle: navigate,
    // settle, challenge handling, content checks.
    let t1 = std::time::Instant::now();
    let page = match ops::ghost_fetch(&mut g, url, std::time::Duration::from_secs(25)).await {
        Ok(p) => p,
        Err(e) => {
            // One transient retry, same as ghost_escalate.
            match ops::ghost_fetch(&mut g, url, std::time::Duration::from_secs(25)).await {
                Ok(p) => p,
                Err(e2) => {
                    return tool_error_structured(
                        format!("browser automation error: {e} / {e2}"),
                        "permanent",
                        Some(json!({
                            "url": url,
                            "status": 0,
                            "escalation": trace.value(),
                        })),
                    );
                }
            }
        }
    };
    trace.step(
        "2",
        "ghost-render",
        &format!("captcha={} dom={}KB", page.captcha, page.html.len() / 1024),
        t1.elapsed().as_millis(),
    );
    if page.captcha {
        if let Some(p) = shot {
            let _ = g.screenshot(p).await;
        }
        return tool_error_structured(
            format!(
                "blocked at {url} : interactive captcha before actions could run. Use an Agent browser to browse sites like these"
            ),
            "walled",
            Some(json!({
                "url": url,
                "status": 200,
                "verdict": "Challenge",
                "next_action": next_action_for(Some(Verdict::Challenge(Vendor::Generic)), 200, "walled"),
                "escalation": trace.value(),
            })),
        );
    }

    // Run the script.
    let t2 = std::time::Instant::now();
    let outcomes = match crate::ghost::actions::run(&mut g, actions).await {
        Ok(o) => {
            trace.step(
                "2",
                "actions",
                &format!("{} steps ok", o.len()),
                t2.elapsed().as_millis(),
            );
            o
        }
        Err((step, reason, partial)) => {
            for o in &partial {
                trace.step("2", &format!("action[{}]", o.step), &o.outcome, o.ms);
            }
            if let Some(p) = shot {
                let _ = g.screenshot(p).await;
            }
            let steps_json: Vec<Value> = partial
                .iter()
                .map(|o| json!({"step": o.step, "action": o.action, "outcome": o.outcome, "ms": o.ms}))
                .collect();
            return tool_error_structured(
                format!(
                    "actions[{step}] failed: {reason} : steps before it succeeded (see structuredContent.actions); fix the step and re-run"
                ),
                "permanent",
                Some(json!({
                    "url": url,
                    "status": 200,
                    "actions": steps_json,
                    "escalation": trace.value(),
                    "next_action": "inspect the page with a plain fetch (no actions), correct the failing step's selector/text, re-run",
                })),
            );
        }
    };

    // Post-action DOM + optional screenshot for visual debugging.
    let html = match g.outer_html().await {
        Ok(h) => h,
        Err(e) => {
            return tool_error_structured(
                format!("post-action DOM read failed: {e}"),
                "transient",
                Some(json!({
                    "url": url,
                    "status": 200,
                    "escalation": trace.value(),
                })),
            );
        }
    };
    // Post-action navigation guard: actions like click can cause the
    // browser to navigate to a new URL (href, form submit). Re-check
    // the current URL via the centralized SSRF gate (async DNS,
    // fail-closed for browser tier).
    if let Ok(cur) = g.current_url().await
        && !cur.is_empty()
        && !cur.starts_with("about:")
        && let Err(e) = crate::fetch::guards::ensure_url_safe(&cur).await
    {
        return tool_error_structured(
            format!("blocked after action navigation: {e}"),
            "permanent",
            Some(json!({
                "url": cur,
                "escalation": trace.value(),
                "next_action": "action caused navigation to a private/loopback URL : blocked",
            })),
        );
    }
    if let Some(p) = shot {
        let _ = g.screenshot(p).await;
    }

    // Cookie write-back : same discipline as ghost_escalate:
    // the browser's clearance cookies flow to tier 1 for future
    // plain-HTTP fetches of this domain. record_solved ONLY when
    // a challenge was actually cleared (page.vendor set) :
    // marking a never-walled domain needs_tier2 would poison its
    // route to skip-to-solve forever (the v1.1 reddit-poisoning
    // bug class). Replay is unverified in the actions flow (no
    // tier-1 retry happens) : false until the fetch path proves it.
    if let Ok(Ok(cookies)) =
        tokio::time::timeout(std::time::Duration::from_secs(3), g.cookies()).await
        && !cookies.is_empty()
    {
        daemon.fetcher.import_cookies(&cookies).await;
        crate::ghost::cache::store_session_cookies(&cookies);
        if page.vendor.is_some() {
            daemon
                .state
                .lock()
                .await
                .record_solved(host, &cookies, page.vendor.as_deref(), false);
        }
    }

    // Standard extraction over the final DOM, with the same
    // candidate ladder as ghost_escalate: prose → links-keeping
    // → raw text. A shell after actions is still a shell.
    let mut best: Option<extract::Extracted> = None;
    if let Ok(e) = extract::extract(html.as_bytes(), extract::charset::GHOST_TEXT_CT, url, opts)
        && !e.thin
    {
        best = Some(e);
    }
    if best.is_none() {
        let mut lopts = opts.clone();
        lopts.include_links = true;
        if let Ok(e2) = extract::extract(
            html.as_bytes(),
            extract::charset::GHOST_TEXT_CT,
            url,
            &lopts,
        ) && !e2.thin
        {
            best = Some(e2);
        }
    }
    let Some(ex) = best else {
        return tool_error_structured(
            format!(
                "actions succeeded but the resulting page yielded no extractable content ({}KB DOM) : the site may still be loading; add a wait step and re-run",
                html.len() / 1024
            ),
            "walled",
            Some(json!({
                "url": url,
                "status": 200,
                "escalation": trace.value(),
                "next_action": "add {\"do\":\"wait_text\",\"text\":\"<expected>\"} or {\"do\":\"wait\",\"ms\":2000} before extraction",
            })),
        );
    };

    // Cache the action-recovered DOM for future plain fetches.
    let dom_verdict = crate::detect::walls::detect_dom_smart(html.as_bytes());
    if !matches!(dom_verdict, crate::detect::walls::Verdict::Challenge(_)) {
        daemon.state.lock().await.record_render(url, &html);
    }

    let steps_json: Vec<Value> = outcomes
        .iter()
        .map(|o| json!({"step": o.step, "action": o.action, "outcome": o.outcome, "ms": o.ms}))
        .collect();
    let mut res = finish_result(
        &ex,
        "2-actions",
        200,
        "ContentOk",
        url,
        &trace,
        t0.elapsed().as_millis(),
    );
    res["structuredContent"]["actions"] = Value::Array(steps_json);
    apply_link_handles(daemon, &mut res).await;
    if image_text {
        apply_image_ocr(daemon, &mut res, &ex.images).await;
    }
    res
}

/// v3 image OCR: fetch + OCR the page's content images (up to 4,
/// 5MB each, SSRF-guarded) and append an `## image text` section
/// to the result. On-demand only : OCR models are heavy and most
/// pages never need it.
pub(super) async fn apply_image_ocr(
    daemon: &Arc<Daemon>,
    res: &mut Value,
    images: &[(String, String)],
) {
    #[cfg(not(feature = "ocr"))]
    {
        let _ = (daemon, images);
        if let Some(sc) = res.pointer_mut("/structuredContent") {
            sc["image_text"] = json!("unavailable : this build lacks the ocr feature");
        }
    }
    #[cfg(feature = "ocr")]
    {
        const MAX_IMAGES: usize = 4;
        const MAX_BYTES: usize = 5 * 1024 * 1024;
        if images.is_empty() {
            return;
        }
        let mut section = String::from("\n## image text (OCR)\n");
        let mut ocred = 0usize;
        for (alt, src) in images.iter().take(MAX_IMAGES) {
            if !src.starts_with("http://") && !src.starts_with("https://") {
                continue; // data:/relative URIs have no fetch path
            }
            // SSRF guard : image URLs are attacker-controllable.
            match url::Url::parse(src) {
                Ok(u) => match u.host_str() {
                    Some(h) if !crate::fetch::guards::is_ssrf_host(h) => {}
                    _ => continue,
                },
                Err(_) => continue,
            }
            let bytes = match tokio::time::timeout(
                std::time::Duration::from_secs(12),
                daemon.fetcher.fetch(src),
            )
            .await
            {
                Ok(Ok(o))
                    if matches!(o.verdict, Verdict::ContentOk) && o.body.len() <= MAX_BYTES =>
                {
                    o.body
                }
                _ => {
                    section.push_str(&format!("- {src}: [unavailable]\n"));
                    continue;
                }
            };
            let ocr_result = tokio::task::spawn_blocking(move || {
                let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
                let rgba = img.into_rgba8();
                let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                let bitmap = crate::pdf::pixels::PageBitmap {
                    w,
                    h,
                    buf: rgba.into_raw(),
                    page_w_pt: w as f32,
                    page_h_pt: h as f32,
                };
                let (lines, _kind) = crate::pdf::ocr::ocr_page(&bitmap, "auto")?;
                let text: String = lines
                    .iter()
                    .map(|l| l.text.trim())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join("  ");
                Ok::<String, String>(text)
            })
            .await;
            match ocr_result {
                Ok(Ok(text)) if !text.trim().is_empty() => {
                    ocred += 1;
                    let t: String = text.chars().take(400).collect();
                    if alt.is_empty() {
                        section.push_str(&format!("- {src}: {t}\n"));
                    } else {
                        section.push_str(&format!("- {alt} ({src}): {t}\n"));
                    }
                }
                _ => {
                    section.push_str(&format!("- {src}: [no text detected]\n"));
                }
            }
        }
        if let Some(cell) = res.pointer_mut("/content/0/text")
            && let Some(md) = cell.as_str().map(String::from)
        {
            *cell = json!(md + &section);
        }
        if let Some(sc) = res.pointer_mut("/structuredContent") {
            sc["image_text"] = json!({ "images": images.len().min(MAX_IMAGES), "ocred": ocred });
        }
    }
}

/// v3 anti-cloak: a domain KNOWN to be walled (needs_tier2 in the
/// profile) suddenly serving clean tier-1 content is suspicious :
/// bot walls sometimes serve benign-looking bait to suspected
/// bots. Render the same URL in the real browser and compare word
/// sets. Material divergence → `cloak_suspected` with a trust
/// recommendation. Cost: one browser render, only on suspicion.
pub(super) async fn anticloak_check(
    daemon: &Arc<Daemon>,
    url: &str,
    tier1_markdown: &str,
) -> Option<(f64, String)> {
    let host = crate::search::rank::host_of(url);
    let mut g = daemon
        .ghost_mgr
        .acquire_for(&daemon.profile, Some(host.as_str()))
        .await
        .ok()?;
    let page = ops::ghost_fetch(&mut g, url, std::time::Duration::from_secs(20))
        .await
        .ok()?;
    if page.captcha {
        return Some((
            0.0,
            "browser sees a challenge where HTTP saw content".to_string(),
        ));
    }
    let ex = extract::extract(
        page.html.as_bytes(),
        extract::charset::GHOST_TEXT_CT,
        url,
        &ExtractOptions::default(),
    )
    .ok()?;
    pub(super) fn words(s: &str) -> std::collections::HashSet<&str> {
        s.split_whitespace().collect()
    }
    let a = words(tier1_markdown);
    let b = words(&ex.markdown);
    if b.is_empty() {
        return None; // browser got nothing : inconclusive, not bait
    }
    let inter = a.intersection(&b).count();
    let union = a.union(&b).count();
    let sim = if union == 0 {
        1.0
    } else {
        inter as f64 / union as f64
    };
    if sim < 0.55 {
        Some((
            sim,
            format!(
                "HTTP and browser content diverge (similarity {sim:.2}) : the HTTP copy may be bot-bait; browser tier text is the one to trust"
            ),
        ))
    } else {
        None
    }
}

/// v3 resurrection fetch: when a URL is truly dead (404, paywall,
/// unsolvable wall) consult the keyless Wayback Machine and serve
/// the nearest snapshot : labeled ruthlessly so archived content
/// can never masquerade as live. `archive: auto` (default) only on
/// dead-end failures; `only` skips the live attempt; `off` never.
pub(super) async fn try_resurrect(
    daemon: &Arc<Daemon>,
    url: &str,
    live_error: &Value,
) -> Option<Value> {
    // 1. Availability lookup (keyless, public API).
    let avail_url = format!(
        "https://archive.org/wayback/available?url={}",
        encode_query_value(url)
    );
    let avail = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        daemon.fetcher.fetch(&avail_url),
    )
    .await
    .ok()?
    .ok()?;
    let v: Value = serde_json::from_slice(&avail.body).ok()?;
    let closest = v.pointer("/archived_snapshots/closest")?.clone();
    let snap_url = closest.get("url")?.as_str()?.to_string();
    let ts = closest
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("");
    // The API returns status as a STRING ("200"); accept both.
    let snap_status = closest
        .get("status")
        .map(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                .unwrap_or(0)
        })
        .unwrap_or(0);
    if snap_status != 200 {
        return None;
    }

    // 2. Fetch the snapshot : wayback is plain HTTP-friendly.
    let snap = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        daemon.fetcher.fetch(&snap_url),
    )
    .await
    .ok()?
    .ok()?;
    if !matches!(snap.verdict, Verdict::ContentOk) {
        return None;
    }
    let ct = snap
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    if crate::fetch::guards::is_binary(&snap.body, &ct) {
        return None;
    }
    let opts = ExtractOptions::default();
    let mut ex = extract::extract(&snap.body, &ct, &snap_url, &opts).ok()?;
    // Wayback serves the ORIGINAL server-rendered HTML : thinness
    // here usually means a genuinely small page, not a JS shell.
    // Only truly empty extractions are useless.
    if ex.total_chars < 50 {
        return None;
    }

    // 3. Label everything: banner in content, fields in structure.
    let date = wayback_date(ts);
    let age_days = wayback_age_days(ts);
    let live_reason = live_error
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("live fetch failed")
        .lines()
        .next()
        .unwrap_or("live fetch failed")
        .to_string();
    let staleness = if age_days > 730 {
        format!(
            " : WARNING: {} years old, treat as historical",
            age_days / 365
        )
    } else {
        String::new()
    };
    let banner = format!(
        "*[ARCHIVED COPY : Wayback snapshot {date} ({age_days}d old){staleness}. Live fetch failed: {live_reason}]*\n\n"
    );
    ex.markdown = format!("{banner}{}", ex.markdown);

    let tokens = ex.markdown.len() / 4;
    let mut trace = Trace::default();
    trace.step("archive", "wayback", &format!("snapshot {ts}"), 0);
    let structured = json!({
        "content_ok": !ex.thin,
        "url": url,
        "snapshot_url": snap_url,
        "archived": { "snapshot": ts, "date": date, "age_days": age_days },
    });
    let debug = json!({
        "status": snap.status,
        "tier": "1(wayback)",
        "verdict": "Archived",
        "thin": ex.thin,
        "title": ex.title,
        "total_chars": ex.total_chars,
        "tokens_est": tokens,
        "live_error": live_reason,
        "escalation": trace.value(),
    });
    Some(json!({
        "content": [{"type": "text", "text": format_fetch_markdown(&ex, &snap_url, url)}],
        "structuredContent": structured,
        "_meta": {"com.donsetch/fetch-debug": debug},
    }))
}

/// Percent-encode a value for a query string: everything outside
/// the unreserved set (plus ':', '/' which wayback tolerates raw).
pub(super) fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Wayback timestamp (YYYYMMDDhhmmss) → "YYYY-MM-DD".
pub(super) fn wayback_date(ts: &str) -> String {
    if ts.len() >= 8 && ts[..8].chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &ts[0..4], &ts[4..6], &ts[6..8])
    } else {
        ts.to_string()
    }
}

pub(super) fn wayback_age_days(ts: &str) -> u64 {
    let y: u64 = ts.get(0..4).and_then(|s| s.parse().ok()).unwrap_or(2015);
    let m: u64 = ts.get(4..6).and_then(|s| s.parse().ok()).unwrap_or(1);
    let d: u64 = ts.get(6..8).and_then(|s| s.parse().ok()).unwrap_or(1);
    // Approximation good enough for staleness warnings (30-day
    // months; the warning threshold is 2 years).
    let snap_days = y.saturating_sub(1970) * 365 + (m.saturating_sub(1)) * 30 + d;
    let now_days = now_unix() / 86_400;
    now_days.saturating_sub(snap_days)
}

/// v3 page history: record the fingerprint, compare with the
/// previous fetch, and stamp the change verdict into the result.
/// With `since_last`, collapse the output to the delta (or the
/// unchanged verdict) instead of the full content.
/// What the extractor learned about one fetched page : the
/// page-history record input.
pub(super) struct PageFacts<'a> {
    fingerprint: Option<&'a str>,
    markdown: &'a str,
    title: Option<&'a str>,
    /// Full page was rendered (not cut by pagination).
    complete: bool,
}

pub(super) fn apply_page_history(
    daemon: &Arc<Daemon>,
    res: &mut Value,
    url: &str,
    facts: PageFacts<'_>,
    since_last: bool,
) {
    let (ex_fingerprint, ex_markdown, ex_title, complete) = (
        facts.fingerprint,
        facts.markdown,
        facts.title,
        facts.complete,
    );
    let Some(fp) = ex_fingerprint else {
        return;
    };
    let mut hist = daemon
        .history
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prev = hist.record(
        url,
        fp,
        ex_markdown.len(),
        ex_title,
        if complete { ex_markdown } else { "" },
    );
    hist.flush();
    drop(hist);

    let (changed, delta, ago) = match &prev {
        Some(p) if p.fingerprint == fp => (
            "unchanged".to_string(),
            None,
            now_unix().saturating_sub(p.at),
        ),
        Some(p) => {
            let old = p.text.as_deref().unwrap_or("");
            let kind = crate::pages::history::classify_change(old, ex_markdown);
            let delta = crate::pages::history::section_delta_report(old, ex_markdown);
            (
                kind.label().to_string(),
                Some(delta),
                now_unix().saturating_sub(p.at),
            )
        }
        None => ("new".to_string(), None, 0),
    };

    // since_last: collapse the payload to the verdict.
    if since_last {
        let title_line = ex_title.map(|t| format!("# {t}\n")).unwrap_or_default();
        let body = match (changed.as_str(), &delta) {
            ("unchanged", _) => {
                format!("{title_line}{url}\n\n*unchanged since last fetch ({ago}s ago)*\n")
            }
            (_, Some(d)) => format!(
                "{title_line}{url}\n\n*changed since last fetch ({changed}, {ago}s ago):*\n\n- {d}\n\n*(full content: refetch without since_last)*\n"
            ),
            _ => format!(
                "{title_line}{url}\n\n*{changed} since last fetch ({ago}s ago) : refetch without since_last for full content*\n"
            ),
        };
        if let Some(cell) = res.pointer_mut("/content/0/text") {
            *cell = json!(body);
        }
    } else if changed != "new" {
        // Note in the content on change (first contact with the
        // delta is valuable; unchanged stays silent).
        if let Some(d) = &delta
            && let Some(cell) = res.pointer_mut("/content/0/text")
            && let Some(md) = cell.as_str().map(String::from)
        {
            *cell = json!(format!(
                "*[changed since last fetch ({}): {}]*\n\n{md}",
                changed, d
            ));
        }
    }

    // Change state affects the next model decision. Opaque fingerprints and
    // observation age are client diagnostics.
    if let Some(sc) = res.pointer_mut("/structuredContent") {
        sc["changed"] = json!(changed);
        if let Some(d) = &delta {
            sc["changed_sections"] = json!(d);
        }
    }
    res["_meta"]["com.donsetch/fetch-debug"]["fingerprint"] = json!(fp);
    if ago > 0 {
        res["_meta"]["com.donsetch/fetch-debug"]["history_age_s"] = json!(ago);
    }
}

pub(super) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// v3 reference handles: rewrite markdown links in a fetch result
/// to `L{n}` handles and expose the count as compact machine state.
/// Mutates `res` in place; no-op when links aren't in the output.
pub(super) async fn apply_link_handles(daemon: &Arc<Daemon>, res: &mut Value) {
    // When handles are disabled, links keep their hrefs.
    if !crate::handles::handles_enabled() {
        return;
    }
    let Some(text) = res
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .map(String::from)
    else {
        return;
    };
    let mut ht = daemon.handles.lock().await;
    let (new_md, n) = ht.replace_link_urls(&text);
    if n == 0 {
        return;
    }
    ht.flush();
    if let Some(cell) = res.pointer_mut("/content/0/text") {
        *cell = json!(new_md);
    }
    if let Some(sc) = res.pointer_mut("/structuredContent") {
        sc["link_handles"] = json!(n);
    }
}

/// Remove only frontmatter represented by the wrapper's canonical source
/// header. Byline, publication date and summaries remain evidence.
pub(super) fn strip_source_frontmatter(markdown: &str, url: &str, title: Option<&str>) -> String {
    const FRONTMATTER_LINES: usize = 8;
    let mut dropped_title = false;
    let mut dropped_url = false;
    let mut lines = Vec::new();
    for (index, line) in markdown.lines().enumerate() {
        let is_same_title = title.is_some_and(|title| {
            line.strip_prefix("# ")
                .is_some_and(|candidate| candidate.trim() == title.trim())
        });
        if index < FRONTMATTER_LINES && !dropped_title && is_same_title {
            dropped_title = true;
            continue;
        }
        if index < FRONTMATTER_LINES && !dropped_url && same_fetch_url(line.trim(), url) {
            dropped_url = true;
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

pub(super) fn same_fetch_url(candidate: &str, expected: &str) -> bool {
    match (url::Url::parse(candidate), url::Url::parse(expected)) {
        (Ok(candidate), Ok(expected)) => candidate == expected,
        _ => candidate == expected,
    }
}

/// Present one canonical title and source URL followed by the evidence body.
pub(super) fn format_fetch_markdown(
    ex: &extract::Extracted,
    source_url: &str,
    display_url: &str,
) -> String {
    let body = strip_source_frontmatter(&ex.markdown, source_url, ex.title.as_deref());
    let mut markdown = String::new();
    if let Some(title) = &ex.title {
        markdown.push_str(&format!("# {title}\n"));
    }
    markdown.push_str(display_url);
    if !body.is_empty() {
        markdown.push_str("\n\n");
        markdown.push_str(&body);
    }
    markdown
}

pub(super) fn finish_result(
    ex: &extract::Extracted,
    tier: &str,
    status: u16,
    verdict: &str,
    url: &str,
    trace: &Trace,
    elapsed_ms: u128,
) -> Value {
    // PDF per-page stats: chars, ocr flag, per-page confidence.
    // Cap at 50 pages to avoid blowing up the response on large
    // PDFs (a 1000-page PDF produces 60K of per-page JSON alone).
    // The summary (total pages, ocr pages, mean confidence) is
    // always included; per_page detail is capped.
    let pdf = ex.pdf_pages.as_ref().map(|pages| {
        let ocr_pages = pages.iter().filter(|p| p.ocr).count();
        let mean_conf = if pages.is_empty() {
            0.0
        } else {
            pages.iter().map(|p| p.confidence).sum::<f32>() / pages.len() as f32
        };
        let capped: Vec<_> = pages.iter().take(50).collect();
        json!({
            "pages": pages.len(),
            "ocr_pages": ocr_pages,
            "mean_confidence": mean_conf,
            "per_page": capped,
            "per_page_capped": pages.len() > 50,
        })
    });
    // The model-facing object contains only state that can alter its next
    // action. Evidence itself appears once in the text block below.
    let mut structured = json!({
        "url": url,
        "content_ok": !ex.thin && verdict == "ContentOk",
        "content_kind": format!("{:?}", ex.content_kind),
    });
    if ex.thin {
        structured["thin"] = json!(true);
    }
    if !matches!(ex.lang.as_str(), "" | "und" | "unknown") {
        structured["lang"] = json!(ex.lang);
    }
    if let Some(next_offset) = ex.next_offset {
        structured["next_offset"] = json!(next_offset);
    }
    if let Some(pdf) = &pdf {
        structured["pdf"] = json!({
            "pages": pdf["pages"],
            "ocr_pages": pdf["ocr_pages"],
        });
    }

    // Transport and extraction telemetry remains available to MCP clients but
    // no longer competes with source evidence in model context.
    let debug = json!({
        "status": status,
        "tier": tier,
        "verdict": verdict,
        "quality": ex.quality,
        "title": ex.title,
        "byline": ex.byline,
        "published": ex.published,
        "site": ex.site,
        "blocks_shown": ex.blocks_shown,
        "blocks_total": ex.blocks_total,
        "total_chars": ex.total_chars,
        "tokens_est": ex.tokens_est,
        "elapsed_ms": elapsed_ms,
        "escalation": trace.value(),
        "via": ex.via,
        "pdf": pdf,
    });
    json!({
        "content": [{"type": "text", "text": format_fetch_markdown(ex, url, url)}],
        "structuredContent": structured,
        "_meta": {"com.donsetch/fetch-debug": debug},
    })
}

/// v3 F2: per-result route hints from the self-improving store :
/// domains that consistently need the browser carry the cost in
/// the open so the agent can budget or pick a faster source.
pub(super) async fn route_hints(
    daemon: &Arc<Daemon>,
    out: &crate::search::SearchOutcome,
) -> Vec<Option<String>> {
    let state = daemon.state.lock().await;
    out.results
        .iter()
        .map(|r| {
            let host = crate::search::rank::host_of(&r.url);
            state
                .is_known_walled(&host)
                .then(|| "· ⚠ needs browser (~+6s)".to_string())
        })
        .collect()
}

pub(super) async fn bind_search_handles(
    daemon: &Arc<Daemon>,
    out: &crate::search::SearchOutcome,
) -> Vec<String> {
    let urls: Vec<String> = out.results.iter().map(|r| r.url.clone()).collect();
    bind_search_urls(daemon, &urls).await
}

pub(super) async fn bind_search_urls(daemon: &Arc<Daemon>, urls: &[String]) -> Vec<String> {
    // When handles are disabled (DONSETCH_URL_HANDLES=off), return
    // empty vec : search results show raw URLs instead.
    if !crate::handles::handles_enabled() {
        return Vec::new();
    }
    let mut ht = daemon.handles.lock().await;
    let hs = ht.set_search_results(urls);
    // Search handles are in-memory only : no flush.
    hs
}

#[cfg(test)]
mod fetch_output_contract_tests {
    use super::{Trace, finish_result, format_fetch_markdown};
    use crate::extract::{ContentKind, Extracted};

    pub(super) fn extracted(markdown: &str) -> Extracted {
        Extracted {
            markdown: markdown.into(),
            title: Some("Example".into()),
            byline: Some("A. Author".into()),
            published: Some("2026-09-04".into()),
            site: Some("Example Site".into()),
            total_chars: markdown.len(),
            next_offset: None,
            blocks_total: 4,
            blocks_shown: 3,
            tokens_est: markdown.len() / 4,
            thin: false,
            content_kind: ContentKind::Article,
            lang: "en".into(),
            quality: 0.91,
            pdf_pages: None,
            images: Vec::new(),
            fingerprint: Some("opaque".into()),
            via: None,
        }
    }

    #[test]
    pub(super) fn fetch_renders_identity_once_and_hides_diagnostics_from_model_state() {
        let mut trace = Trace::default();
        trace.step("1", "http-fetch", "ok", 12);
        let output = finish_result(
            &extracted("https://example.com/page\n\n# Example\n\nEvidence."),
            "1",
            200,
            "ContentOk",
            "https://example.com/page",
            &trace,
            14,
        );

        assert_eq!(output["content"].as_array().unwrap().len(), 1);
        let text = output["content"][0]["text"].as_str().unwrap();
        assert_eq!(text.matches("# Example").count(), 1);
        assert_eq!(text.matches("https://example.com/page").count(), 1);
        assert!(!text.starts_with("[meta]"));
        let state = output["structuredContent"].as_object().unwrap();
        for absent in [
            "title",
            "tier",
            "quality",
            "tokens_est",
            "escalation",
            "status",
        ] {
            assert!(
                !state.contains_key(absent),
                "{absent} leaked to model state"
            );
        }
        assert_eq!(state["url"], "https://example.com/page");
        assert_eq!(output["_meta"]["com.donsetch/fetch-debug"]["tier"], "1");

        let unrelated = extracted("# Actual first section\n\nEvidence.");
        assert!(
            format_fetch_markdown(
                &unrelated,
                "https://example.com/page",
                "https://example.com/page"
            )
            .contains("# Actual first section")
        );
    }
}

#[cfg(test)]
mod batch_output_contract_tests {
    use super::fetch_output_contract_tests::extracted;
    use super::{Trace, finish_result, render_fetch_batch};
    use serde_json::json;

    #[test]
    pub(super) fn batch_keeps_evidence_order_and_per_url_failure_codes() {
        let urls: Vec<String> = vec![
            "https://example.com/a".into(),
            "https://example.com/b".into(),
        ];
        let success = finish_result(
            &extracted("# Example\nhttps://example.com/a\n\nAlpha."),
            "1",
            200,
            "ContentOk",
            &urls[0],
            &Trace::default(),
            1,
        );
        let failure = json!({
            "content": [{"type": "text", "text": "request timed out"}],
            "structuredContent": {"code": "network.timeout"},
            "isError": true,
        });
        let results = vec![success, failure];
        let markdowns = vec![
            Some(
                results[0]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            ),
            None,
        ];
        let output = render_fetch_batch(&urls, &results, &markdowns, Some(2_000), &[false, false]);
        assert_eq!(output["content"].as_array().unwrap().len(), 1);
        let text = output["content"][0]["text"].as_str().unwrap();
        assert!(text.find("Alpha.").unwrap() < text.find("request timed out").unwrap());
        assert_eq!(
            output["structuredContent"]["results"][1]["code"],
            "network.timeout"
        );
        let first = &output["structuredContent"]["results"][0];
        assert!(first.get("content_ok").is_none());
        assert!(first.get("content_kind").is_none());
        assert!(first.get("thin").is_none());
        assert!(first.get("changed").is_none());
        assert!(first.get("tier").is_none());
        let first_debug = &output["_meta"]["com.donsetch/fetch-batch-debug"]["results"][0];
        assert_eq!(first_debug["tier"], "1");
        assert!(first_debug.get("tokens_est").is_some());
        assert!(first_debug.get("quality").is_none());
        assert!(first_debug.get("escalation").is_none());

        let flagged = json!({
            "content": [{"type": "text", "text": "# Thin\nhttps://example.com/a"}],
            "structuredContent": {
                "content_ok": false,
                "content_kind": "Page",
                "thin": true,
                "changed": "major",
                "next_offset": 16000,
                "cloak_suspected": true,
                "archived": {"date": "2026-09-01", "age_days": 3}
            }
        });
        let flagged_output = render_fetch_batch(
            &urls[..1],
            &[flagged],
            &[Some("# Thin\nhttps://example.com/a".into())],
            None,
            &[false],
        );
        let flagged_state = &flagged_output["structuredContent"]["results"][0];
        assert_eq!(flagged_state["content_ok"], false);
        assert!(flagged_state.get("content_kind").is_none());
        assert_eq!(flagged_state["thin"], true);
        assert_eq!(flagged_state["changed"], "major");
        assert_eq!(flagged_state["next_offset"], 16000);
        assert_eq!(flagged_state["cloak_suspected"], true);
        assert_eq!(flagged_state["archived"]["age_days"], 3);
    }
}
