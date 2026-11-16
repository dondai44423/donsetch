//! The crawl tool handler: dispatch, map/plan/run/diff/get, and the
//! CrawlResult rendering + next-action guidance for the model.

use serde_json::{Value, json};
use std::borrow::Cow;

use super::*;
#[allow(clippy::field_reassign_with_default)]
pub(super) async fn crawl_tool(daemon: &Arc<Daemon>, args: &Value, ctx: Option<ToolCtx>) -> Value {
    match args.get("mode").and_then(Value::as_str) {
        Some("get") => return crawl_get(args),
        Some("sites") => return crawl_sites(args),
        Some("forget") => return crawl_forget(args),
        _ => {}
    }
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
    let mut opts = crawl_options(args);
    let resume = args.get("resume").and_then(Value::as_str).map(String::from);

    // delta crawl: pages are re-checked and their fresh
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
    // gracefully on cancel (the stop-flag mechanism). A cancelled
    // call gets no response, so it keeps no resume token either.
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

    // Centralized guard (SSRF and local rules) on the seed. A run resume
    // with no URL is loaded by the executor from its job and guards itself.
    if !url.is_empty()
        && let Some(refusal) = seed_guard_error(&url)
    {
        return refusal;
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

fn crawl_sites(_args: &Value) -> Value {
    let sites = crate::crawl::model::dossier::list();
    let mut text = String::from("# Crawl sites\n");
    for s in &sites {
        text.push_str(&format!(
            "- {} · {} · {} URLs · {} families · {} page memories · {}\n",
            s.host, s.seed, s.urls, s.families, s.pages, s.source
        ));
    }
    if sites.is_empty() {
        text.push_str("No dossiers yet.\n");
    }
    json!({
        "content": [{ "type": "text", "text": text.trim_end() }],
        "structuredContent": {"ok": true, "mode": "sites", "sites": sites, "count": sites.len()},
        "_meta": {"com.donsetch/crawl-debug": {"mode": "sites", "count": sites.len()}},
    })
}

fn crawl_forget(args: &Value) -> Value {
    let raw = args
        .get("host")
        .and_then(Value::as_str)
        .or_else(|| args.get("url").and_then(Value::as_str))
        .unwrap_or("")
        .trim();
    if raw.is_empty() {
        return tool_error("crawl forget: host required");
    }
    let host = url::Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_else(|| raw.trim_start_matches("www.").to_ascii_lowercase());
    if crate::crawl::model::dossier::forget(&host) {
        json!({
            "content": [{"type": "text", "text": format!("forgot {host}: dossier removed; fetch cache untouched")}],
            "structuredContent": {"ok": true, "mode": "forget", "host": host, "forgotten": true},
            "_meta": {"com.donsetch/crawl-debug": {"mode": "forget", "host": host}},
        })
    } else {
        tool_error(format!("crawl forget: no dossier for {host}"))
    }
}

/// `crawl get pN`: read a page straight from the durable run job. No
/// crawler, no network, no resume guard: the bytes were already fetched.
fn crawl_get(args: &Value) -> Value {
    use crate::crawl::executor::JobStore;
    use crate::extract::view::View;

    let Some(mut handle) = args
        .get("handle")
        .and_then(Value::as_str)
        .or_else(|| args.get("url").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return tool_error("crawl get: handle required (p1, or job:p1)");
    };
    let mut job_id = args
        .get("job")
        .and_then(Value::as_str)
        .or_else(|| args.get("resume").and_then(Value::as_str))
        .map(String::from);
    if let Some((prefix, rest)) = handle.split_once(':')
        && prefix.starts_with('j')
        && rest.starts_with('p')
    {
        job_id = Some(prefix.to_string());
        handle = rest;
    }
    let store = JobStore::default();
    let id = match job_id {
        Some(id) => id,
        None => match store.latest_id() {
            Ok(id) => id,
            Err(e) => return tool_error(format!("crawl get: {e}")),
        },
    };
    let job = match store.load(&id) {
        Ok(job) => job,
        Err(e) => return tool_error(format!("crawl get: {e}")),
    };
    let page = match job.page_by_handle(handle) {
        Some(page) => page,
        None => {
            return tool_error(format!(
                "crawl get: no {handle} in job {id} ({} handles)",
                job.handle_count()
            ));
        }
    };
    let section = args
        .get("section")
        .and_then(Value::as_str)
        .map(String::from);
    let offset = args
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(1_000_000_000) as usize;
    let view = View {
        section: section.clone(),
        offset,
        max_chars: 100_000,
        ..Default::default()
    };
    let incomplete = page
        .partial
        .as_ref()
        .map(|p| {
            format!(
                "{}: {} of {:?} items",
                p.reason, p.items_found, p.items_total
            )
        })
        .or_else(|| {
            page.next_offset
                .map(|_| "the stored page was capped".to_string())
        });
    let rendered = crate::extract::view::render(&page.markdown, &view, incomplete.as_deref());
    let content_complete = rendered.next_offset.is_none()
        && page.partial.is_none()
        && !page.focus.as_ref().is_some_and(|f| f.omitted);
    let mut text = format!("# [{handle}]");
    if !page.title.is_empty() {
        text.push_str(&format!(" {}", page.title));
    }
    text.push('\n');
    text.push_str(&page.url);
    text.push_str("\n\n");
    if offset > 0 {
        text.push_str(rendered.text.trim_start_matches('\n'));
    } else {
        text.push_str(&rendered.text);
    }
    let mut structured = json!({
        "ok": true,
        "mode": "get",
        "handle": handle,
        "job": id,
        "url": page.url,
        "title": page.title,
        "kind": format!("{:?}", page.kind).to_lowercase(),
        "section": section,
        "offset": offset,
        "next_offset": rendered.next_offset,
        "read_status": rendered.status,
        "content_complete": content_complete,
        "chars": rendered.text.chars().count(),
        "full_chars": page.markdown.chars().count(),
        "lastmod": page.lastmod,
    });
    if let Some(state) = rendered.state.as_object() {
        for (k, v) in state {
            structured[k] = v.clone();
        }
    }
    json!({
        "content": [{ "type": "text", "text": text.trim_end() }],
        "structuredContent": structured,
        "_meta": {"com.donsetch/crawl-debug": {"job": id, "handle": handle, "mode": "get"}},
    })
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
            Some(policy_error_value(
                &denial.into_error(&rewritten),
                seed,
                None,
            ))
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
        let refused = landing.unwrap_or(&failure.requested);
        let mut value =
            policy_error_value(&denial.clone().into_error(refused), refused, escalation);
        value["structuredContent"]["requested_url"] = json!(failure.requested);
        if let Some(landing) = landing {
            value["structuredContent"]["landing_url"] = json!(landing);
        }
        return value;
    }

    let status = failure.status;
    let verdict = failure.verdict.as_deref().unwrap_or("");
    // The verdicts web_fetch calls walled on any status (`verdict_kind`).
    let hard_wall = verdict.starts_with("Challenge") || matches!(verdict, "AuthWall" | "Paywall");
    let is_wall = hard_wall || verdict == "Blocked" || matches!(status, 401 | 403);
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
        Some(RobotsRefusal::Unreachable)
            if failure
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("network.dns:")) =>
        {
            (
                "permanent",
                Cow::Borrowed("network.dns"),
                format!(
                    "DNS resolution failed while checking robots.txt: {}",
                    failure.error.as_deref().unwrap_or("")
                ),
                "check the seed hostname; this name could not be resolved, so no pages were fetched",
            )
        }
        Some(RobotsRefusal::Unreachable) => (
            "transient",
            Cow::Borrowed("crawl.robots_unreachable"),
            format!(
                "the origin's robots.txt could not be read, which counts as disallow-all{}",
                failure
                    .error
                    .as_ref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            ),
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
        None if status == 0
            && failure
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("network.dns:")) =>
        {
            (
                "permanent",
                Cow::Borrowed("network.dns"),
                format!(
                    "DNS resolution failed: {}",
                    failure.error.as_deref().unwrap_or("")
                ),
                "check the seed hostname or use another source",
            )
        }
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
        // (`verdict_kind`), not as a wall. A challenge served as 503 or
        // 429 (Cloudflare, DataDome) stays a wall there, and here.
        None if (status == 429 || status >= 500) && !hard_wall => (
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
            "fetch the seed URL with web_fetch, or use another source",
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

#[allow(clippy::field_reassign_with_default)]
fn crawl_options(args: &Value) -> CrawlOptions {
    let mut opts = CrawlOptions::default();
    opts.focus = args.get("focus").and_then(Value::as_str).map(String::from);
    opts.mode = match args.get("mode").and_then(Value::as_str).unwrap_or("full") {
        "map" => CrawlMode::Map,
        "plan" => CrawlMode::Plan,
        "run" => CrawlMode::Run,
        "get" => CrawlMode::Get,
        "diff" => CrawlMode::Diff,
        "sites" => CrawlMode::Sites,
        "forget" => CrawlMode::Forget,
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
        opts.max_total_chars = (n as usize).clamp(1, 500_000);
    }
    if let Some(n) = args.get("per_page_max").and_then(Value::as_u64) {
        opts.per_page_max = (n as usize).clamp(1, 40_000);
    }
    if let Some(n) = args.get("map_limit").and_then(Value::as_u64) {
        opts.map_cap = (n as usize).clamp(1, 50_000);
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
        opts.deadline = std::time::Duration::from_secs(n.clamp(1, 600));
    }
    if let Some(q) = args.get("min_quality").and_then(Value::as_f64) {
        opts.min_quality = q.clamp(0.0, 1.0) as f32;
    }
    // dataset mode is output-format only.
    if args
        .get("dataset")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        opts.dataset = true;
    }
    opts.plan_explain = args
        .get("explain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if args
        .get("refresh")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        opts.force_fresh_map = true;
    }
    // A run asked to be incremental is the delta mode; the old full
    // path keeps its own since_last semantics until M6.
    if opts.mode == CrawlMode::Run
        && args
            .get("since_last")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        opts.mode = CrawlMode::Diff;
    }
    opts
}

/// Queued URLs shown in the debug block. The queue itself holds up to
/// `crawl::frontier::MAX_QUEUE`; the response shows the head and says
/// how many there are.
pub(super) const QUEUED_PREVIEW: usize = 100;

fn queued_preview(queued: &[String]) -> Value {
    json!(queued.iter().take(QUEUED_PREVIEW).collect::<Vec<_>>())
}

/// Depth-cut URLs shown in the debug block, beside their count.
const DEPTH_CUT_SAMPLE: usize = 10;

fn depth_cut_sample(cut: &[String]) -> Value {
    json!(cut.iter().take(DEPTH_CUT_SAMPLE).collect::<Vec<_>>())
}

pub(super) fn render_crawl_result(
    result: &crate::crawl::CrawlResult,
    requested_mode: CrawlMode,
    dataset: bool,
) -> Value {
    if requested_mode == CrawlMode::Plan
        && let Some(plan) = &result.plan
    {
        return render_crawl_plan(result, plan, requested_mode);
    }
    if dataset {
        return render_crawl_dataset(result, requested_mode);
    }
    if matches!(
        requested_mode,
        CrawlMode::Full | CrawlMode::Run | CrawlMode::Diff
    ) {
        return render_crawl_digest(result);
    }
    // One linear evidence document: page identity and body appear exactly once.
    let mut text = String::new();
    text.push_str(&format!("# Crawl\n{}\n\n", result.seed));
    // The count sits at the head as well as in the trailing section, so
    // it survives a client that truncates a long response.
    if !result.denied.is_empty() {
        text.push_str(&format!(
            "Excluded by the operator's local DonSeTch rules: {}, listed at the end.\n\n",
            url_count(result.denied.total())
        ));
    }
    if let Some(run) = &result.run {
        text.push_str(&format!(
            "job: {} · {} pages · {} requests · {} attempts · {} ms · {}\n",
            run.job,
            run.fetched,
            run.requests,
            run.attempts,
            run.elapsed_ms,
            run.stop.as_str(),
        ));
        if let Some(wall) = &run.wall {
            text.push_str(&format!(
                "wall: {} · {} · {} · {}\n",
                wall.class,
                wall.vendor.as_deref().unwrap_or("unknown vendor"),
                wall.status,
                wall.ask,
            ));
        }
    }
    if let Some(model) = &result.model {
        if result.run.is_some() {
            text.push_str(&render_model_brief(model));
        } else {
            text.push_str(&render_model(model));
        }
        if requested_mode == CrawlMode::Map && result.map.len() >= result.inventory.limit {
            text.push_str(&format!(
                "map list capped at {} URLs; the model covers all {} in-scope URLs.\n\n",
                result.inventory.limit, model.urls
            ));
        }
    }
    if result.run.is_some()
        && let Some(plan) = &result.plan
    {
        text.push_str(&plan_summary_line(plan));
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
        "unchanged_pages": result.skipped.iter().filter(|(_, why)| why == "unchanged since last crawl").count(),
        "seed": result.seed,
        "complete": crawl_complete(result, requested_mode),
        "inventory": { "complete": result.inventory.complete, "source": result.inventory.source, "limit": result.inventory.limit },
        "pages": result.pages.iter().filter(|p| !p.duplicate).map(|p| json!({
            "url": p.url,
            "lastmod": p.lastmod,
            "content_complete": p.next_offset.is_none() && p.partial.is_none() && !p.focus.as_ref().is_some_and(|f| f.omitted),
            "focus": p.focus,
            "partial": p.partial,
            "next_offset": p.next_offset,
        })).collect::<Vec<_>>(),
        "stop": format!("{:?}", result.stop),
    });
    if let Some(model) = &result.model {
        structured["model"] = serde_json::to_value(model).unwrap_or(Value::Null);
    }
    if let Some(run) = &result.run {
        structured["mode"] = json!("run");
        structured["run"] = serde_json::to_value(run).unwrap_or(Value::Null);
        structured["job"] = json!(run.job);
        structured["stop"] = json!(run.stop.as_str());
        structured["complete"] = json!(run.complete);
        if let Some(plan) = &result.plan {
            structured["plan"] = serde_json::to_value(plan).unwrap_or(Value::Null);
        }
        if let Some(wall) = &run.wall {
            structured["wall"] = serde_json::to_value(wall).unwrap_or(Value::Null);
        }
    }
    if requested_mode != CrawlMode::Content {
        structured["map"] = json!(result.map);
    } else {
        structured.as_object_mut().unwrap().remove("inventory");
    }
    if structured["unchanged_pages"] == 0 {
        structured
            .as_object_mut()
            .unwrap()
            .remove("unchanged_pages");
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
        "depth_cut": result.depth_cut.len(),
        "depth_cut_sample": depth_cut_sample(&result.depth_cut),
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
        "model": &result.model,
    });
    json!({
        "content": [{"type": "text", "text": text.trim_end()}],
        "structuredContent": structured,
        "_meta": {"com.donsetch/crawl-debug": debug},
    })
}

/// Plan mode's evidence document: the plan is the deliverable; the model
/// header says what inventory it came from. Every page row carries its
/// reason, cost and section; reserves are visible before a page fails.
fn render_crawl_plan(
    result: &crate::crawl::CrawlResult,
    plan: &crate::crawl::model::plan::Plan,
    requested_mode: CrawlMode,
) -> Value {
    use crate::crawl::model::plan::PlanStop;

    let mut text = format!("# Crawl plan\n{}\n\n", result.seed);
    if let Some(model) = &result.model {
        text.push_str(&format!(
            "site: {} · {} of {} URLs · {} families · {} sections · {} inventory requests · {}\n",
            model.site_kind.as_str(),
            model.urls,
            model.raw,
            model.families,
            model.sections,
            model.requests,
            if model.complete {
                "complete"
            } else {
                "incomplete"
            },
        ));
        if model.cached {
            text.push_str(&format!("cached map · {}\n", age_secs(model.generated_at)));
        }
        if !model.sources.is_empty() {
            let sources = model
                .sources
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!("sources: {sources}\n"));
        }
    }
    let focus = match &plan.focus {
        Some(q) => format!(
            "\"{q}\": {}/{} topics · {}/{} pages",
            plan.coverage.focus_topics_answered,
            plan.coverage.focus_topics,
            plan.coverage.focus_matches,
            plan.pages.len().max(1)
        ),
        None => "none".to_string(),
    };
    text.push_str(&format!(
        "plan: {} pages · {}/{} sections ({}% of URLs) · {}/{} families · focus {} · {} est tokens · {} ms · {}\n",
        plan.pages.len(),
        plan.coverage.sections_covered,
        plan.coverage.sections_total,
        (plan.coverage.url_weight * 100.0).round() as usize,
        plan.coverage.families_covered,
        plan.coverage.families_total,
        focus,
        plan.estimated_tokens,
        plan.elapsed_ms,
        plan.stop.as_str(),
    ));
    if plan.focus_fallback {
        text.push_str(
            "focus fallback: no URL matched; the plan is breadth-only and focus precision is 0.\n",
        );
    }
    if !plan.coverage.focus_unanswered.is_empty() {
        text.push_str(&format!(
            "focus gaps: {} — no inventory URL carries the term\n",
            plan.coverage.focus_unanswered.join(", ")
        ));
    }
    text.push_str("\n## Pages\n");
    for (index, row) in plan.pages.iter().enumerate() {
        text.push_str(&format!(
            "{}. [{}] {}\n   {} · section {} ({} URLs) · {} est tokens{}\n",
            index + 1,
            row.kind.as_str(),
            row.url,
            row.reason,
            row.section,
            row.section_urls,
            row.est_tokens,
            if row.focus { " · focus" } else { "" },
        ));
    }
    if !plan.reserves.is_empty() {
        text.push_str(&format!("\n## Reserves ({})\n", plan.reserves.len()));
        for row in plan.reserves.iter().take(12) {
            text.push_str(&format!(
                "- [{}] {} — {}\n",
                row.kind.as_str(),
                row.url,
                row.reason
            ));
        }
        if plan.reserves.len() > 12 {
            text.push_str(&format!("- … {} more reserves\n", plan.reserves.len() - 12));
        }
    }
    if !plan.trace.is_empty() {
        text.push_str(&format!(
            "\n## Candidates (top {} of {})\n",
            plan.trace.len().min(20),
            if plan.trace_truncated {
                "more"
            } else {
                "the pool"
            }
        ));
        for row in plan.trace.iter().take(20) {
            text.push_str(&format!(
                "- {}/{} — score {:.3}{} · {}\n",
                row.section,
                row.template,
                row.score,
                if row.focus { " · focus" } else { "" },
                row.status
            ));
        }
        if plan.trace.len() > 20 || plan.trace_truncated {
            text.push_str("- …\n");
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
        "seed": &result.seed,
        "mode": "plan",
        "complete": plan.stop == PlanStop::Covered,
        "inventory": {
            "complete": result.inventory.complete,
            "source": result.inventory.source,
            "limit": result.inventory.limit,
        },
        "stop": plan.stop.as_str(),
        "plan": plan,
    });
    if let Some(model) = &result.model {
        structured["model"] = serde_json::to_value(model).unwrap_or(Value::Null);
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
        "filtered_out": result.filtered_out,
        "elapsed_s": result.elapsed.as_secs_f64(),
        "model": &result.model,
        "plan": &result.plan,
        "run": &result.run,
    });
    json!({
        "content": [{"type": "text", "text": text.trim_end()}],
        "structuredContent": structured,
        "_meta": {"com.donsetch/crawl-debug": debug},
    })
}

/// Run mode's evidence document: a site brief, page cards with stable pN
/// handles, boilerplate removed and focus facts kept. The full page is one
/// `crawl get` away.
fn render_crawl_digest(result: &crate::crawl::CrawlResult) -> Value {
    let d = crate::crawl::digest::build(result);
    let mut text = d.text.clone();
    if let Some(delta) = &result.delta {
        text.push_str(&format!(
            "\n## Delta\n{} new · {} changed · {} unchanged · {} removed · {} page(s) fetched\n",
            delta.new,
            delta.changed,
            delta.unchanged,
            delta.removed.len(),
            delta.pages_fetched
        ));
        for u in delta.removed.iter().take(20) {
            text.push_str(&format!("- removed: {u}\n"));
        }
        if delta.removed.len() > 20 {
            text.push_str(&format!("- … {} more removed\n", delta.removed.len() - 20));
        }
    }
    let next_action = compute_crawl_next_action(result);
    let run = result.run.as_ref();
    let mut structured = json!({
        "ok": true,
        "seed": result.seed,
        "mode": "run",
        "job": d.job,
        "stop": d.stop,
        "complete": run.is_some_and(|r| r.complete),
        "run": run,
        "plan": result.plan,
        "digest_chars": d.digest_chars,
        "read_chars": d.read_chars,
        "boilerplate_removed": d.boilerplate_removed,
        "collapsed": d.collapsed,
        "pages": d.pages.iter().map(|p| json!({
            "handle": p.handle,
            "url": p.url,
            "title": p.title,
            "section": p.section,
            "template": p.template,
            "kind": p.kind,
            "lastmod": p.lastmod,
            "headings": p.headings,
            "chars": p.chars,
            "full_chars": p.full_chars,
            "focus": p.focus,
            "duplicate_of": p.duplicate_of,
        })).collect::<Vec<_>>(),
        "handles": d.pages.iter().map(|p| (p.handle.clone(), json!(p.url))).collect::<serde_json::Map<_, _>>(),
    });
    if let Some(job) = &d.job {
        // `resume` is the documented continuation token; the job id is it.
        structured["resume"] = json!(job);
    }
    if let Some(delta) = &result.delta {
        structured["mode"] = json!("diff");
        structured["delta"] = serde_json::to_value(delta).unwrap_or(Value::Null);
        structured["complete"] = json!(delta.new + delta.changed == 0);
    }
    if let Some(model) = &result.model {
        structured["model"] = serde_json::to_value(model).unwrap_or(Value::Null);
    }
    if let Some(wall) = run.and_then(|r| r.wall.as_ref()) {
        structured["wall"] = serde_json::to_value(wall).unwrap_or(Value::Null);
    }
    if !next_action.is_empty() {
        structured["next_action"] = json!(next_action);
    }
    json!({
        "content": [{ "type": "text", "text": text.trim_end() }],
        "structuredContent": structured,
        "_meta": {"com.donsetch/crawl-debug": {
            "mode": if result.delta.is_some() { "diff" } else { "run" },
            "job": d.job,
            "digest_chars": d.digest_chars,
            "read_chars": d.read_chars,
            "boilerplate_removed": d.boilerplate_removed,
            "collapsed": d.collapsed,
            "pages": d.pages.len(),
        }},
    })
}

/// Dataset mode: one JSON object per fetched page,
/// JSON Lines. Rows are sorted by URL for deterministic output
/// (delta-friendly diffs across recrawls). Duplicate content pages
/// are dropped: a dataset wants one row per page. serde_json does
/// the escaping, so every row is valid JSON by construction.
pub(super) fn render_crawl_dataset(
    result: &crate::crawl::CrawlResult,
    requested_mode: CrawlMode,
) -> Value {
    if result.run.is_some() {
        return render_crawl_run_dataset(result);
    }
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
                "focus": p.focus,
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
        "unchanged_pages": result.skipped.iter().filter(|(_, why)| why == "unchanged since last crawl").count(),
        "seed": result.seed,
        "dataset": true,
        // Schema marker (v4 F). Row shape: url/title/kind/markdown/
        // chars/fetched_at/lastmod/parent. Bump on any row-field change
        // so downstream parsers can fail closed.
        "dataset_version": 2,
        "rows": rows.len(),
        // Summed row chars so CLI/MCP summaries report real volume in
        // dataset mode (the JSON mode's pages[] is absent here).
        "chars": rows
            .iter()
            .filter_map(|r| r.get("chars").and_then(|c| c.as_u64()))
            .sum::<u64>(),
        "complete": crawl_complete(result, requested_mode),
        "inventory": { "complete": result.inventory.complete, "source": result.inventory.source, "limit": result.inventory.limit },
        "stop": format!("{:?}", result.stop),
    });
    if let Some(run) = &result.run {
        structured["mode"] = json!("run");
        structured["run"] = serde_json::to_value(run).unwrap_or(Value::Null);
        structured["job"] = json!(run.job);
        structured["stop"] = json!(run.stop.as_str());
        structured["complete"] = json!(run.complete);
        if let Some(plan) = &result.plan {
            structured["plan"] = serde_json::to_value(plan).unwrap_or(Value::Null);
        }
    }
    let partial_pages = result
        .pages
        .iter()
        .filter(|p| !p.duplicate && (p.next_offset.is_some() || p.partial.is_some() || p.focus.as_ref().is_some_and(|f| f.omitted)))
        .map(|p| json!({"url": p.url, "next_offset": p.next_offset, "partial": p.partial, "focus": p.focus}))
        .collect::<Vec<_>>();
    if !partial_pages.is_empty() {
        structured["partial_pages"] = json!(partial_pages);
    }
    if requested_mode != CrawlMode::Content {
        structured["map"] = json!(result.map);
    } else {
        structured.as_object_mut().unwrap().remove("inventory");
    }
    if structured["unchanged_pages"] == 0 {
        structured
            .as_object_mut()
            .unwrap()
            .remove("unchanged_pages");
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
        "depth_cut": result.depth_cut.len(),
        "depth_cut_sample": depth_cut_sample(&result.depth_cut),
        "filtered_out": result.filtered_out,
        "skipped": result
            .skipped
            .iter()
            .map(|(u, w)| json!({ "url": u, "reason": w }))
            .collect::<Vec<_>>(),
        "crawl_delay": result.crawl_delay,
        "elapsed_s": result.elapsed.as_secs_f64(),
        "model": &result.model,
    });
    json!({
        "content": [{ "type": "text", "text": text.trim_end() }],
        "structuredContent": structured,
        "_meta": { "com.donsetch/crawl-debug": debug },
    })
}

/// Run dataset v3: one row per digest card with the handle, headings,
/// section/template and full stored markdown. `crawl get` is still the
/// lossless path; the row is the machine-readable card.
fn render_crawl_run_dataset(result: &crate::crawl::CrawlResult) -> Value {
    let d = crate::crawl::digest::build(result);
    let mut rows: Vec<Value> = d
        .pages
        .iter()
        .filter_map(|p| {
            let page = result.pages.iter().find(|page| page.url == p.url)?;
            Some(json!({
                "schema": "donsetch.crawl.page",
                "schema_version": 3,
                "handle": p.handle,
                "url": p.url,
                "title": p.title,
                "section": p.section,
                "template": p.template,
                "kind": p.kind,
                "lastmod": p.lastmod,
                "headings": p.headings,
                "card": p.card,
                "markdown": page.markdown,
                "chars": page.chars,
                "digest_chars": p.chars,
                "full_chars": p.full_chars,
                "focus": p.focus,
                "duplicate_of": p.duplicate_of,
                "fetched_at": page.fetched_at,
                "parent": page.parent,
                "content_complete": page.next_offset.is_none()
                    && page.partial.is_none()
                    && !page.focus.as_ref().is_some_and(|f| f.omitted),
            }))
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
        "mode": "run",
        "dataset": true,
        "schema": "donsetch.crawl.page",
        "dataset_version": 3,
        "rows": rows.len(),
        "chars": d.digest_chars,
        "digest_chars": d.digest_chars,
        "complete": result.run.as_ref().is_some_and(|r| r.complete),
        "stop": d.stop,
        "job": d.job,
        "run": result.run,
        "plan": result.plan,
        "handles": d.pages.iter().map(|p| (p.handle.clone(), json!(p.url))).collect::<serde_json::Map<_, _>>(),
    });
    if let Some(job) = &d.job {
        structured["resume"] = json!(job);
    }
    if !next_action.is_empty() {
        structured["next_action"] = json!(next_action);
    }
    json!({
        "content": [{ "type": "text", "text": text.trim_end() }],
        "structuredContent": structured,
        "_meta": {"com.donsetch/crawl-debug": {"mode": "run", "rows": rows.len(), "dataset_version": 3}},
    })
}

/// The model header a run uses: the site line and provenance, not the
/// whole section outline (the pages are the evidence).
fn render_model_brief(m: &crate::crawl::model::ModelSummary) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "site: {} · {} of {} URLs · {} families · {} sections · {} inventory requests · {}\n",
        m.site_kind.as_str(),
        m.urls,
        m.raw,
        m.families,
        m.sections,
        m.requests,
        if m.complete { "complete" } else { "incomplete" },
    ));
    if m.cached {
        out.push_str(&format!("cached map · {}\n", age_secs(m.generated_at)));
    }
    if !m.sources.is_empty() {
        let sources = m
            .sources
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("sources: {sources}\n"));
    }
    let f = &m.filtered;
    if f.total() > 0 {
        let mut parts = Vec::new();
        for (name, n) in [
            ("robots", f.robots),
            ("rules", f.rules),
            ("scope", f.scope),
            ("locale", f.locale),
            ("assets", f.assets),
            ("other", f.other),
        ] {
            if n > 0 {
                parts.push(format!("{name} {n}"));
            }
        }
        out.push_str(&format!("filtered: {}\n", parts.join(", ")));
    }
    if let Some(prev) = &m.previous {
        out.push_str(&format!(
            "previous map: {} · {}\n",
            age_secs(prev.generated_at),
            if prev.changed { "changed" } else { "unchanged" }
        ));
    }
    if !m.apis.is_empty() {
        out.push_str(&format!(
            "site interfaces: {}\n",
            m.apis.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    out
}

fn plan_summary_line(plan: &crate::crawl::model::plan::Plan) -> String {
    let focus = match &plan.focus {
        Some(q) => format!(
            "\"{q}\": {}/{} topics · {}/{} pages",
            plan.coverage.focus_topics_answered,
            plan.coverage.focus_topics,
            plan.coverage.focus_matches,
            plan.pages.len().max(1)
        ),
        None => "none".to_string(),
    };
    format!(
        "plan: {} pages · {}/{} sections ({}% of URLs) · {}/{} families · focus {} · {} est tokens · {}\n",
        plan.pages.len(),
        plan.coverage.sections_covered,
        plan.coverage.sections_total,
        (plan.coverage.url_weight * 100.0).round() as usize,
        plan.coverage.families_covered,
        plan.coverage.families_total,
        focus,
        plan.estimated_tokens,
        plan.stop.as_str(),
    )
}

/// The site model a map built: the outline an agent reads before it
/// spends a page. Compact by design; the flat URL list follows.
fn render_model(m: &crate::crawl::model::ModelSummary) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "site: {} · {} of {} URLs · {} families · {} sections · {} inventory requests · {}\n",
        m.site_kind.as_str(),
        m.urls,
        m.raw,
        m.families,
        m.sections,
        m.requests,
        if m.complete { "complete" } else { "incomplete" },
    ));
    if m.cached {
        out.push_str(&format!("cached map · {}\n", age_secs(m.generated_at)));
    }
    if !m.sources.is_empty() {
        let sources = m
            .sources
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("sources: {sources}\n"));
    }
    let f = &m.filtered;
    if f.total() > 0 {
        let mut parts = Vec::new();
        for (name, n) in [
            ("robots", f.robots),
            ("rules", f.rules),
            ("scope", f.scope),
            ("locale", f.locale),
            ("assets", f.assets),
            ("other", f.other),
        ] {
            if n > 0 {
                parts.push(format!("{name} {n}"));
            }
        }
        out.push_str(&format!("filtered: {}\n", parts.join(", ")));
    }
    if let Some(prev) = &m.previous {
        out.push_str(&format!(
            "previous map: {} · {}\n",
            age_secs(prev.generated_at),
            if prev.changed { "changed" } else { "unchanged" }
        ));
    }
    if !m.apis.is_empty() {
        out.push_str(&format!(
            "site interfaces: {}\n",
            m.apis.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !m.top_sections.is_empty() {
        out.push_str(&format!("\n## Sections ({} shown)\n", m.top_sections.len()));
        for s in &m.top_sections {
            out.push_str(&format!(
                "- {} — {} URLs, {} families",
                s.path, s.urls, s.families
            ));
            if let Some(sample) = s.sample.first() {
                out.push_str(&format!(" · {sample}"));
            }
            out.push('\n');
        }
    }
    if !m.top_families.is_empty() {
        out.push_str(&format!("\n## Families ({} shown)\n", m.top_families.len()));
        for f in &m.top_families {
            out.push_str(&format!(
                "- {} · {} — {} URLs",
                f.section, f.template, f.size
            ));
            if let Some(sample) = f.sample.first() {
                out.push_str(&format!(" · {sample}"));
            }
            out.push('\n');
        }
    }
    out.push('\n');
    out
}

fn age_secs(ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let d = now.saturating_sub(ts);
    if d < 90 {
        "just now".to_string()
    } else if d < 5400 {
        format!("{}m ago", d / 60)
    } else if d < 172_800 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

fn url_count(n: usize) -> String {
    if n == 1 {
        "1 URL".into()
    } else {
        format!("{n} URLs")
    }
}

/// The trailing "Excluded by the operator's local DonSeTch rules" section: one group per
/// rule with its kind phrase, the operator's message as `Next action:`
/// and the group's capped URLs. The rule key never appears in it.
fn denied_section(denied: &crate::crawl::DeniedLog) -> String {
    let mut out = String::from(
        "## Excluded by the operator's local DonSeTch rules (not fetched; web_fetch returns the same)\n",
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

    if let Some(delta) = &result.delta {
        if delta.new + delta.changed == 0 {
            return if delta.removed.is_empty() {
                String::new()
            } else {
                format!(
                    "{} page(s) removed; the delta lists them.",
                    delta.removed.len()
                )
            };
        }
        return "diff fetched the changed/new pages; use crawl get for a full page.".into();
    }

    if let Some(run) = &result.run {
        return match run.stop {
            crate::crawl::executor::RunStop::Covered => String::new(),
            crate::crawl::executor::RunStop::Walled => {
                let wall = run
                    .wall
                    .as_ref()
                    .map(|w| format!("{} {}", w.class, w.vendor.as_deref().unwrap_or("")))
                    .unwrap_or_else(|| "a classified wall".into());
                format!(
                    "the host is declared walled ({wall}); use another egress or a real browser driver (bladebro), or resume={} later.",
                    run.job
                )
            }
            crate::crawl::executor::RunStop::Deadline
            | crate::crawl::executor::RunStop::Cancelled
            | crate::crawl::executor::RunStop::Throttled => {
                format!("resume={} to continue the job.", run.job)
            }
            crate::crawl::executor::RunStop::Budget if run.complete => String::new(),
            crate::crawl::executor::RunStop::Budget => {
                format!(
                    "page budget spent; raise max_pages or resume={} to continue.",
                    run.job
                )
            }
            crate::crawl::executor::RunStop::Empty => {
                "the plan selected no pages; widen include_paths or remove the focus.".into()
            }
        };
    }

    if let Some(plan) = &result.plan {
        return match plan.stop {
            crate::crawl::model::plan::PlanStop::Budget => format!(
                "page budget {} spent; raise max_pages or narrow focus/include_paths for another plan.",
                plan.budget_pages
            ),
            crate::crawl::model::plan::PlanStop::Empty => {
                "no selectable pages in scope; widen include_paths or drop the focus.".into()
            }
            crate::crawl::model::plan::PlanStop::Covered => String::new(),
        };
    }

    // Resume available : always suggest it first.
    if let Some(tok) = &result.resume {
        // A resume made while the host is still in the penalty box
        // can stop ThrottledOut again without fetching a queued URL.
        if result.stop == StopReason::ThrottledOut {
            return format!(
                "the host throttled the crawler. Wait a few minutes, then resume={tok} to continue crawling."
            );
        }
        return format!(
            "resume={tok} to continue crawling (stopped: {:?}).",
            result.stop
        );
    }

    if result.pages.is_empty()
        && !result.map.is_empty()
        && !result.inventory.complete
        && result.stop == StopReason::FrontierEmpty
    {
        return if result.inventory.source == "seed-links" {
            "inventory contains only links observed on the seed page; site-wide completeness is unknown. Use mode=content to follow the reachable scope.".into()
        } else {
            "published inventory is partial because discovery hit a safety bound or an unavailable sitemap; narrow the scope or use mode=content.".into()
        };
    }
    if result.pages.is_empty()
        && !result.skipped.is_empty()
        && result
            .skipped
            .iter()
            .all(|(_, why)| why == "unchanged since last crawl" || why.contains("duplicate"))
    {
        return "no source changes found in the pages rechecked; unchanged pages were omitted"
            .into();
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
            return "every URL this crawl reached is excluded by the operator's local DonSeTch rules, so none of them was fetched here; another source may have this content, and the denied list's Next action says which one the operator wants.".into();
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
            "crawl cancelled : it kept no resume token; start a new crawl to read the rest.".into()
        }
        StopReason::ThrottledOut => {
            "the host throttled the crawler. Wait a few minutes and resume.".into()
        }
        StopReason::Walled => {
            "a classified wall stopped this host; the wall report names its class. Use another egress or a real browser driver (bladebro) for that host.".into()
        }
        StopReason::DepthLimit => {
            "crawl hit the depth limit. Increase max_depth to discover more pages.".into()
        }
        StopReason::InventoryLimit => "URL inventory was capped; increase map_limit or narrow focus/include_paths. Discovery has a 100000-URL and 32-sitemap safety bound.".into(),
        StopReason::FrontierEmpty => String::new(), // normal completion
    }
}

fn crawl_complete(result: &crate::crawl::CrawlResult, mode: CrawlMode) -> bool {
    if matches!(mode, CrawlMode::Full | CrawlMode::Run) {
        return result.run.as_ref().is_some_and(|r| r.complete);
    }
    if mode == CrawlMode::Diff {
        return result
            .delta
            .as_ref()
            .is_some_and(|d| d.new + d.changed == 0);
    }
    if mode == CrawlMode::Plan {
        return result
            .plan
            .as_ref()
            .is_some_and(|p| p.stop == crate::crawl::model::plan::PlanStop::Covered);
    }
    matches!(result.stop, crate::crawl::StopReason::FrontierEmpty)
        && if mode == CrawlMode::Map {
            !result.map.is_empty() && result.inventory.complete
        } else {
            (mode == CrawlMode::Content || result.inventory.complete || result.inventory.source == "seed-links")
                && (!result.pages.is_empty() || !result.skipped.is_empty())
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
