//! The crawl tool handler: dispatch, map/content mode, and the
//! CrawlResult rendering + next-action guidance for the model.

use serde_json::{Value, json};

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
    let resume = args.get("resume").and_then(Value::as_str).map(String::from);

    // v3 delta crawl: skip pages whose fingerprints are on file,
    // and record the fingerprints of everything actually fetched :
    // crawls feed the same memory fetches do.
    if args
        .get("since_last")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let hist = Arc::clone(&daemon.history);
        opts.skip_unchanged = Some(Arc::new(move |url: &str| {
            hist.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .has_recent(url)
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

    // Centralized SSRF guard on the seed.
    if !url.is_empty()
        && let Err(e) = crate::fetch::guards::validate_url_basic(&url)
    {
        return tool_error(format!("{e}"));
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

    let requested_mode = opts.mode;
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
                    "safe to retry immediately; if repeated, lower max_pages or widen deadline_s",
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

    render_crawl_result(&result, requested_mode)
}

pub(super) fn render_crawl_result(
    result: &crate::crawl::CrawlResult,
    requested_mode: CrawlMode,
) -> Value {
    // One linear evidence document: page identity and body appear exactly once.
    let mut text = String::new();
    text.push_str(&format!("# Crawl\n{}\n\n", result.seed));
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
                text.push_str("## Discovered URLs not fetched\n");
                for url in remaining {
                    text.push_str(&format!("- {url}\n"));
                }
            }
        }
    }

    let next_action = compute_crawl_next_action(result);
    let mut structured = json!({
        "seed": result.seed,
        "complete": matches!(result.stop, crate::crawl::StopReason::FrontierEmpty),
        "pages": result.pages.iter().filter(|p| !p.duplicate).map(|p| json!({
            "url": p.url,
            "lastmod": p.lastmod,
        })).collect::<Vec<_>>(),
        "stop": format!("{:?}", result.stop),
    });
    if let Some(resume) = &result.resume {
        structured["resume"] = json!(resume);
    }
    if !next_action.is_empty() {
        structured["next_action"] = json!(next_action);
    }
    let debug = json!({
        "mode": format!("{:?}", requested_mode),
        "map": result.map,
        "queued": result.queued,
        "filtered_out": result.filtered_out,
        "skipped": result.skipped.iter().map(|(u, w)| json!({"url": u, "reason": w})).collect::<Vec<_>>(),
        "pages": result.pages.iter().map(|p| json!({
            "url": p.url,
            "title": p.title,
            "kind": format!("{:?}", p.kind),
            "chars": p.chars,
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
    if result.pages.is_empty() {
        let skip_reasons: Vec<&str> = result.skipped.iter().map(|(_, w)| w.as_str()).collect();
        let all_scope = skip_reasons
            .iter()
            .all(|r| r.contains("out of scope") || r.contains("filtered"));
        let all_blocked = skip_reasons
            .iter()
            .all(|r| r.contains("Challenge") || r.contains("Blocked") || r.contains("wall"));
        let all_404 = skip_reasons
            .iter()
            .all(|r| r.contains("404") || r.contains("NotFound"));
        let has_sitemap = !result.map.is_empty();

        if all_404 {
            return "seed URL returned 404 : check the URL is correct.".into();
        }
        if all_blocked {
            return "the site blocked the crawler. Try respect_robots=false, or fetch the seed URL directly first to check access.".into();
        }
        if all_scope && result.filtered_out > 0 {
            return "all discovered URLs were outside the seed's path scope. Try broader include_paths, or same_host=false to crawl the whole host.".into();
        }
        if !has_sitemap && result.map.is_empty() && result.filtered_out == 0 {
            return "no sitemap found and no links discovered. Try mode=content to BFS from the seed, or check the seed URL is accessible.".into();
        }
        return "crawl returned 0 pages. Try mode=content, broader include_paths, or a different seed URL.".into();
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

#[cfg(test)]
mod crawl_output_contract_tests {
    use super::render_crawl_result;
    use crate::crawl::{CrawlMode, CrawlPage, CrawlResult, StopReason};
    use crate::extract::ContentKind;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    pub(super) fn crawl_renders_page_identity_once_and_keeps_resume_as_state() {
        let page = CrawlPage {
            url: "https://example.com/docs/page".into(),
            title: "Evidence page".into(),
            kind: ContentKind::Article,
            markdown: "# Evidence page\nhttps://example.com/docs/page\n\nUseful evidence.".into(),
            chars: 16,
            quality: 0.93,
            duplicate: false,
            parent: Some("https://example.com/docs/".into()),
            score: 0.88,
            lastmod: Some("2026-09-04".into()),
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
        };
        let output = render_crawl_result(&result, CrawlMode::Full);
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
}
