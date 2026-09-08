//! web_answer (v4 phase 2.2): one call from a factual question to a
//! compact, cited evidence pack.
//!
//! Design: design/v4.md §4b. The tool does NOT synthesize prose; it
//! gathers verbatim, cited passages and leaves the reasoning to the
//! calling agent (no cloud LLM dependency, no fabrication surface).
//!
//! Everything heavy is reused, nothing duplicated:
//! - discovery via Searcher::search (single-flight, BYOK-or-keyless,
//!   rerank; phase 2.1 prewarm parks the top bodies as a side effect)
//! - each page via fetch_single_inner (prewarm take, route memory,
//!   cookie jar, wall escalation, focus extraction) : the exact
//!   web_fetch pipeline, so a cited page reads identically to what
//!   the agent would get calling web_fetch itself
//!
//! Honesty rules (the §8 evidence-pack ledger):
//! - passages are the page's own focused markdown, never rewritten
//! - text is never blended across sources; each section cites one URL
//! - duplicate final URLs (mirrors/redirect twins) are skipped
//! - walled/dead/binary pages are skipped WITH a reason, listed in
//!   structuredContent.skipped : silence is never the answer
//! - zero usable evidence -> empty pack that says so, with what was
//!   tried; no padding with irrelevant content
//! - token budget caps the pack; overflow is marked, not hidden

use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};

use super::fetch_tool::fetch_single_inner;
use super::*;

/// Search fanout: a few extra candidates so dead/walled skips still
/// leave max_pages of evidence.
const SEARCH_RESULTS: usize = 8;
const DEFAULT_BUDGET_TOKENS: usize = 2000;
const MIN_BUDGET_TOKENS: usize = 200;
const MAX_BUDGET_TOKENS: usize = 8000;
const DEFAULT_MAX_PAGES: usize = 3;
const MAX_PAGES_CAP: usize = 5;
/// Safety net per page: a hung origin must not eat the whole pack
/// when the caller gave no deadline_ms. Well under fetch's own
/// transport timeouts.
const PAGE_TIMEOUT_MS: u64 = 20_000;

pub(super) async fn answer_tool(
    daemon: &Arc<Daemon>,
    args: &Value,
    mut ctx: Option<ToolCtx>,
) -> Value {
    // Kill switch (law 6): honest-off state. tools/list hides the
    // tool too when this is set (see tools::list).
    if crate::config::env_flag("DONSETCH_NO_ANSWER_TOOL") {
        return tool_error("answer: web_answer is disabled (DONSETCH_NO_ANSWER_TOOL)");
    }
    daemon.refresh_vault().await;

    let Some(query) = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
    else {
        return tool_error("answer: missing required parameter: query");
    };

    let budget_tokens = args
        .get("budget_tokens")
        .and_then(Value::as_u64)
        .map(|t| (t as usize).clamp(MIN_BUDGET_TOKENS, MAX_BUDGET_TOKENS))
        .unwrap_or(DEFAULT_BUDGET_TOKENS);
    let max_pages = args
        .get("max_pages")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_PAGES_CAP))
        .unwrap_or(DEFAULT_MAX_PAGES);
    let deadline = args
        .get("deadline_ms")
        .and_then(Value::as_u64)
        .map(|ms| std::time::Duration::from_millis(ms.clamp(500, 600_000)));

    let query = query.to_string();
    let query_for_error = query.clone();
    run_with_budget(
        answer_inner(daemon, &query, budget_tokens, max_pages),
        deadline,
        ctx.as_mut(),
        move || {
            let mut trace = Trace::default();
            trace.step("clock", "deadline", "hit", 0);
            tool_error_structured(
                format!("answer: deadline_ms exceeded for \"{query_for_error}\""),
                "transient",
                Some(json!({
                    "query": query_for_error,
                    "escalation": trace.value(),
                    "next_action": "retry with a higher deadline_ms, or smaller max_pages/budget_tokens",
                })),
            )
        },
    )
    .await
}

struct EvidenceSource {
    url: String,
    title: String,
    fresh: String,
    markdown: String,
}

struct SkippedPage {
    url: String,
    reason: String,
}

async fn answer_inner(
    daemon: &Arc<Daemon>,
    query: &str,
    budget_tokens: usize,
    max_pages: usize,
) -> Value {
    let t0 = Instant::now();

    // ── 1. Discovery: the SAME pipeline as web_search (BYOK-first
    // with local fallback, single-flight dedup, rerank, dedup vs
    // read history). Phase 2.1 prewarm parks the top bodies as a
    // side effect, so the page reads below usually hit RAM.
    let outcome = match search_outcome(daemon, query, SEARCH_RESULTS, None).await {
        Ok(o) => o,
        Err(f) => return search_error(query, &f.cause, f.byok_tried, f.kind),
    };
    let search_ms = t0.elapsed().as_millis() as u64;

    let mut skipped: Vec<SkippedPage> = Vec::new();

    // ── 2. Page picks: top-ranked http(s) results, deduped by URL
    // (redirect twins collapse later by FINAL url).
    let mut picks: Vec<(String, String)> = Vec::new(); // (url, serp title)
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in outcome.results.iter() {
        if picks.len() >= max_pages {
            break;
        }
        let Ok(u) = url::Url::parse(&r.url) else {
            continue;
        };
        if !matches!(u.scheme(), "http" | "https") {
            continue;
        }
        if !seen.insert(r.url.clone()) {
            continue;
        }
        picks.push((r.url.clone(), r.title.clone()));
    }

    if picks.is_empty() {
        skipped.push(SkippedPage {
            url: String::new(),
            reason: "search returned no fetchable http(s) results".into(),
        });
        return build_pack(
            daemon,
            query,
            budget_tokens,
            Vec::new(),
            skipped,
            search_ms,
            t0,
            outcome.cached,
        )
        .await;
    }

    // ── 3. Page reads: the exact web_fetch pipeline with focus=the
    // question. Prewarm hits skip the network; walls escalate to
    // ghost exactly like a normal fetch.
    let page_chars = (budget_tokens.saturating_mul(4) / picks.len()).max(2000);
    let mut futs = Vec::new();
    for (url, title) in picks {
        let page_args = json!({
            "url": url,
            "focus": query,
            "max_chars": page_chars,
        });
        futs.push(async move {
            let started = Instant::now();
            // Safety net: a hung origin must not eat the whole pack.
            let res = match tokio::time::timeout(
                std::time::Duration::from_millis(PAGE_TIMEOUT_MS),
                fetch_single_inner(daemon, &page_args, &url),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => json!({
                    "isError": true,
                    "structuredContent": { "code": "network.timeout" },
                }),
            };
            (url, title, res, started.elapsed().as_millis() as u64)
        });
    }
    let results = futures_util::future::join_all(futs).await;

    // ── 4. Collect evidence; every drop gets a reason.
    let mut sources: Vec<EvidenceSource> = Vec::new();
    let mut seen_final: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (url, serp_title, res, ms) in results {
        let is_error = res.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let sc = res.get("structuredContent").cloned().unwrap_or(json!({}));
        let dbg = res
            .pointer("/_meta/com.donsetch~1fetch-debug")
            .cloned()
            .unwrap_or(json!({}));
        let final_url = sc
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or(&url)
            .to_string();

        if is_error {
            let code = sc
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("fetch error");
            skipped.push(SkippedPage {
                url,
                reason: code.into(),
            });
            continue;
        }
        let content_ok = sc
            .get("content_ok")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = res
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !content_ok || text.trim().is_empty() {
            let reason = if content_ok {
                "no extractable content".to_string()
            } else {
                format!(
                    "verdict {}",
                    dbg.get("verdict").and_then(Value::as_str).unwrap_or("?")
                )
            };
            skipped.push(SkippedPage { url, reason });
            continue;
        }
        // Redirect twins / mirrors: same final page already cited.
        if !seen_final.insert(final_url.clone()) {
            skipped.push(SkippedPage {
                url,
                reason: "duplicate of an earlier source (same final URL)".into(),
            });
            continue;
        }

        // Freshness: the server's own Last-Modified when it says,
        // else the fetch moment. Labeled, so the agent knows which.
        let fresh = match sc.get("server_modified").and_then(Value::as_str) {
            Some(lm) if !lm.is_empty() => format!("server modified {lm}"),
            _ => format!("fetched {}ms ago this call", ms),
        };
        sources.push(EvidenceSource {
            url: final_url,
            title: serp_title,
            fresh,
            markdown: text.to_string(),
        });
    }

    build_pack(
        daemon,
        query,
        budget_tokens,
        sources,
        skipped,
        search_ms,
        t0,
        outcome.cached,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn build_pack(
    daemon: &Arc<Daemon>,
    query: &str,
    budget_tokens: usize,
    sources: Vec<EvidenceSource>,
    skipped: Vec<SkippedPage>,
    search_ms: u64,
    t0: Instant,
    search_cached: bool,
) -> Value {
    let budget_chars = budget_tokens.saturating_mul(4);

    // ── Empty-evidence honesty: say so, and say what was tried.
    if sources.is_empty() {
        let mut md = format!("# Evidence pack: {query}\n\nNo usable evidence found.\n");
        for s in &skipped {
            if s.url.is_empty() {
                md.push_str(&format!("- {}\n", s.reason));
            } else {
                md.push_str(&format!("- {}: {}\n", s.url, s.reason));
            }
        }
        let structured = json!({
            "query": query,
            "empty": true,
            "truncated": false,
            "tokens_est": md.chars().count() / 4,
            "sources": [],
            "skipped": skipped.iter().map(|s| json!({ "url": s.url, "reason": s.reason })).collect::<Vec<_>>(),
            "cached": search_cached,
        });
        return json!({
            "content": [{ "type": "text", "text": md }],
            "structuredContent": structured,
            "_meta": {
                "com.donsetch/answer-debug": {
                    "search_ms": search_ms,
                    "search_cached": search_cached,
                    "elapsed_ms": t0.elapsed().as_millis() as u64,
                },
            },
        });
    }

    // ── Assemble: one section per source, never blended. The fetched
    // markdown keeps its own "# title / url" citation header verbatim
    // (citation fidelity); we add the pack header, source index, and
    // freshness on top.
    let mut md = String::new();
    md.push_str(&format!("# Evidence pack: {query}\n"));
    md.push_str(&format!(
        "Sources: {} · skipped: {}\n\n",
        sources.len(),
        skipped.len()
    ));

    let mut source_meta: Vec<Value> = Vec::new();
    let mut used_chars = md.chars().count();
    let mut truncated = false;

    for (i, s) in sources.iter().enumerate() {
        let header = format!("## [{}] {}\nFresh: {}\n\n", i + 1, s.title, s.fresh);
        let body = &s.markdown;
        let header_chars = header.chars().count();
        let room = budget_chars.saturating_sub(used_chars + header_chars + 16);
        if room < 200 {
            // No room for another meaningful source.
            truncated = true;
            for rest in &sources[i..] {
                skipped_note_push(&mut source_meta, rest);
            }
            break;
        }
        let (page_md, page_truncated) = if body.chars().count() > room {
            (truncate_at_boundary(body, room), true)
        } else {
            (body.clone(), false)
        };
        md.push_str(&header);
        md.push_str(&page_md);
        md.push_str("\n\n");
        used_chars += header_chars + page_md.chars().count() + 2;
        if page_truncated {
            truncated = true;
        }
        source_meta.push(json!({
            "ref": i + 1,
            "url": s.url,
            "title": s.title,
            "fresh": s.fresh,
            "chars": page_md.chars().count(),
            "truncated": page_truncated,
        }));
        if truncated {
            break;
        }
    }

    // Law 6: served packs are observable in `donsetch status`.
    daemon.state.lock().await.note_answer_served();

    let structured = json!({
        "query": query,
        "empty": false,
        "truncated": truncated,
        "tokens_est": md.chars().count() / 4,
        "sources": source_meta,
        "skipped": skipped.iter().map(|s| json!({ "url": s.url, "reason": s.reason })).collect::<Vec<_>>(),
        "cached": search_cached,
    });

    json!({
        "content": [{ "type": "text", "text": md }],
        "structuredContent": structured,
        "_meta": {
            "com.donsetch/answer-debug": {
                "search_ms": search_ms,
                "search_cached": search_cached,
                "elapsed_ms": t0.elapsed().as_millis() as u64,
            },
        },
    })
}

/// Sources dropped at the budget line are still listed (as zero-char
/// citations with the reason), so the pack never silently hides a
/// source it found.
fn skipped_note_push(source_meta: &mut Vec<Value>, s: &EvidenceSource) {
    source_meta.push(json!({
        "ref": source_meta.len() + 1,
        "url": s.url,
        "title": s.title,
        "fresh": s.fresh,
        "chars": 0,
        "truncated": true,
        "dropped": "token budget",
    }));
}

/// Cut at the nearest paragraph (then whitespace) boundary at or
/// before max_chars, char-safe.
fn truncate_at_boundary(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    // Byte offset of the max_chars-th character.
    let end = text
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let slice = &text[..end];
    if let Some(p) = slice.rfind("\n\n")
        && p >= slice.len() / 2
    {
        return format!("{}\n\n[truncated to budget]", slice[..p].trim_end());
    }
    if let Some(p) = slice.rfind(char::is_whitespace)
        && p >= slice.len() / 2
    {
        return format!("{} [truncated to budget]", slice[..p].trim_end());
    }
    format!("{slice} [truncated to budget]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text_whole() {
        assert_eq!(truncate_at_boundary("short", 100), "short");
    }

    #[test]
    fn truncate_prefers_paragraph_boundary() {
        let text = "para one is here.\n\npara two is longer and should be cut.";
        let cut = truncate_at_boundary(text, 25);
        assert!(cut.starts_with("para one is here."));
        assert!(cut.ends_with("[truncated to budget]"));
        assert!(!cut.contains("para two"));
    }

    #[test]
    fn truncate_falls_back_to_word_boundary() {
        let text = "word1 word2 word3 word4 word5 word6";
        let cut = truncate_at_boundary(text, 12);
        assert!(cut.chars().count() <= 12 + " [truncated to budget]".len());
        assert!(cut.starts_with("word1"));
    }

    #[test]
    fn truncate_is_char_safe_on_multibyte() {
        let text = "héllo wörld ünïcode téxt hére";
        let cut = truncate_at_boundary(text, 8);
        // must not panic and must be valid utf-8 prefix-ish content
        assert!(cut.chars().count() < text.chars().count());
    }
}
