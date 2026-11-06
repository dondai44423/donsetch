//! The fetch tool handler: dispatch, URL/handle resolution,
//! multi-fetch batching, single fetch + wall handling (the optional
//! unlocker), OCR, page history + link handles, and the result
//! envelope assembly.

use serde_json::{Value, json};

use super::*;
use crate::egress::request_route::RequestRoute;

/// State selected once for the whole call.
struct FetchCall {
    session_stamp: String,
    route: RequestRoute,
}

async fn fetch_with_budget(
    daemon: &Arc<Daemon>,
    args: &Value,
    url: &str,
    deadline: Option<std::time::Duration>,
    ctx: Option<&mut ToolCtx>,
) -> Value {
    let started = std::time::Instant::now();
    let witness = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut result = FETCH_TRACE
        .scope(
            witness.clone(),
            run_with_budget(
                Box::pin(fetch_single(daemon, args, url)),
                deadline,
                ctx,
                || {
                    let prior = witness
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    let mut result = deadline_error(url);
                    fold_trace_into_result(&mut result, prior);
                    result
                },
            ),
        )
        .await;
    let trace = witness
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    drop(trace);
    if args.get("links").and_then(Value::as_bool) == Some(true) {
        result["structuredContent"]["budget_scope"] =
            json!("rendered markdown, including link markup");
    }
    if let Some(debug) = result.pointer_mut("/_meta/com.donsetch~1fetch-debug") {
        debug["elapsed_ms"] = json!(started.elapsed().as_millis());
    }
    result
}

pub(super) async fn fetch_tool(
    daemon: &Arc<Daemon>,
    args: &Value,
    mut ctx: Option<ToolCtx>,
) -> Value {
    let deadline = args
        .get("deadline_ms")
        .and_then(Value::as_u64)
        .map(|ms| std::time::Duration::from_millis(ms.clamp(1, 600_000)));
    // url is one URL/handle or an array of up to 12: one parallel batch
    // whose max_chars is the total across its pages.
    let urls: Vec<String> = match args.get("url") {
        Some(Value::String(s)) => vec![s.to_string()],
        Some(Value::Array(a)) => {
            if a.iter()
                .any(|value| value.as_str().is_none_or(|s| s.trim().is_empty()))
            {
                return tool_error("fetch: every url array entry must be a non-empty string");
            }
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
    // Argument mistakes answer before any URL is resolved or fetched.
    if let Err(error) = fetch_view(args) {
        return error;
    }
    if urls.len() == 1 {
        let url = match resolve_fetch_url(daemon, &urls[0]).await {
            Ok(u) => u,
            Err(e) => {
                return e;
            }
        };
        return fetch_with_budget(daemon, args, &url, deadline, ctx.as_mut()).await;
    }
    let budget = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .map_or(BATCH_MAX_CHARS_DEFAULT, |n| {
            (n as usize).clamp(MAX_CHARS_MIN, MAX_CHARS_MAX)
        });
    fetch_multi(daemon, args, urls, budget, deadline, ctx).await
}

/// `max_chars` when the caller sets none: one page.
pub(super) const MAX_CHARS_DEFAULT: usize = 16_000;
/// `max_chars` when the caller sets none: the total of a URL batch.
pub(super) const BATCH_MAX_CHARS_DEFAULT: usize = 48_000;
const MAX_CHARS_MIN: usize = 200;
const MAX_CHARS_MAX: usize = 1_048_576;
/// The most document one read extracts before any view applies. A larger
/// document is cut here, and its views say part of it was not read.
const DOCUMENT_MAX_CHARS: usize = 8 * 1_048_576;

#[cfg(test)]
mod fetch_batch_tests {
    // Nextest isolates daemon state. Match the production runtime's 8 MiB
    // stack: the unoptimized fetch future exceeds libtest's 2 MiB default.
    #[test]
    fn report_audit_batch_keeps_each_invalid_url_as_an_individual_result() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let daemon = std::sync::Arc::new(super::Daemon::new().await.unwrap());
                        let result = super::fetch_tool(
                            &daemon,
                            &serde_json::json!({"url":["not-a-url", "also-not-a-url"]}),
                            None,
                        )
                        .await;
                        let rows = result["structuredContent"]["results"].as_array().unwrap();
                        assert_eq!(rows.len(), 2);
                        assert!(
                            rows.iter()
                                .all(|r| r["ok"] == false && r["code"] == "fetch.invalid")
                        );
                        assert_eq!(rows[0]["url"], "not-a-url");
                        assert_eq!(rows[1]["url"], "also-not-a-url");
                    });
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

pub(super) async fn resolve_fetch_url(daemon: &Arc<Daemon>, raw: &str) -> Result<String, Value> {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix('@')
        .filter(|id| crate::handles::is_handle(id))
        .unwrap_or(raw);
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
    Err(tool_error_structured(
        format!("fetch: url must be http(s), got: {raw}"),
        "permanent",
        Some(
            json!({"url": raw, "code": "fetch.invalid", "next_action": "pass a full http(s) URL or a valid fetch handle"}),
        ),
    ))
}

/// Batch fetch: parallel single fetches composed into one result under
/// the shared `budget` of characters. Small pages stay whole; the budget
/// slices proportional to size, never below a floor. All-failed = honest
/// error; partial = composed result with per-URL status.
pub(super) async fn fetch_multi(
    daemon: &Arc<Daemon>,
    args: &Value,
    urls: Vec<String>,
    budget: usize,
    deadline: Option<std::time::Duration>,
    ctx: Option<ToolCtx>,
) -> Value {
    // Each fetch may use the whole budget; composition slices.
    let mut call_args = args.clone();
    call_args["max_chars"] = json!(budget);
    let progress_parts = ctx.as_ref().map(|c| c.progress_parts());
    // Cancellation must reach every sub-fetch: the batch used to
    // drop the ctx entirely, so a cancelled 12-URL batch kept
    // running every escalation (each holding pool slots) while the
    // client had already moved on. Each sub-fetch watches a clone
    // of the same cancel channel.
    let cancel_rx = ctx.as_ref().map(|c| c.cancel_receiver());
    let n_total = urls.len();
    let futs: Vec<_> = urls
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let a = call_args.clone();
            let d = Arc::clone(daemon);
            let requested = u.clone();
            let dl = deadline;
            let prog = progress_parts.clone();
            let mut cancel = cancel_rx.clone();
            async move {
                let url = match resolve_fetch_url(&d, &requested).await {
                    Ok(url) => url,
                    Err(error) => {
                        return (requested, error);
                    }
                };
                let v = match cancel.as_mut() {
                    Some(rx) => tokio::select! {
                        v = fetch_with_budget(&d, &a, &url, dl, None) => v,
                        _ = rx.changed() => tool_error("cancelled"),
                    },
                    None => fetch_with_budget(&d, &a, &url, dl, None).await,
                };
                if let Some(p) = &prog {
                    emit_progress(
                        p,
                        (i + 1) as u64,
                        Some(n_total as u64),
                        &format!("{}/{} done", i + 1, n_total),
                    );
                }
                (url, v)
            }
        })
        .collect();
    let (urls, results): (Vec<_>, Vec<_>) = futures_util::future::join_all(futs)
        .await
        .into_iter()
        .unzip();

    let is_err = is_failure;
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
    let mut sliced_flags = vec![false; results.len()];
    slice_batch_markdowns(&urls, &results, &mut markdowns, budget, &mut sliced_flags);

    render_fetch_batch(&urls, &results, &markdowns, budget, &sliced_flags)
}

/// Compose a batch without repeating page evidence or transport telemetry on
/// model-visible surfaces. Kept pure so partial and all-failed contracts are
/// deterministic unit-test inputs.
pub(super) fn render_fetch_batch(
    urls: &[String],
    results: &[Value],
    markdowns: &[Option<String>],
    budget: usize,
    sliced_flags: &[bool],
) -> Value {
    debug_assert_eq!(urls.len(), results.len());
    debug_assert_eq!(urls.len(), markdowns.len());
    debug_assert_eq!(urls.len(), sliced_flags.len());

    let is_err = is_failure;

    let mut text = String::new();
    let ok_count = markdowns.iter().filter(|m| m.is_some()).count();
    let err_count = results.len() - ok_count;
    for (i, r) in results.iter().enumerate() {
        if let Some(md) = &markdowns[i] {
            let title = batch_title(r);
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
            text.push_str(&batch_ok_header(i, head, &urls[i]));
            text.push_str(&body);
            text.push_str(BATCH_MEMBER_SEP);
        } else {
            let msg = r
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or("fetch failed");
            text.push_str(&batch_error_block(i, &urls[i], msg));
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
                for field in ["budget_scope", "next_offset", "read_status", "content_complete", "partial", "partial_reason", "items_found", "items_total", "must_contain", "focus", "section", "next_page"] {
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
                o["content_complete"] = json!(false);
                o["read_status"] = json!("partial");
                o.as_object_mut().unwrap().remove("next_offset");
                o["next_action"] = json!("refetch this URL alone with the original offset to read content withheld by the batch budget");
            }
            if is_err(r) {
                o["content_ok"] = json!(false);
                o["content_complete"] = json!(false);
                for field in ["read_status", "next_action"] {
                    if let Some(value) = r["structuredContent"].get(field) {
                        o[field] = value.clone();
                    }
                }
                o["code"] = r
                    .pointer("/structuredContent/code")
                    .cloned()
                    .unwrap_or_else(|| json!("content.extract"));
                // A denial's kind is the operator's choice and its rule
                // names what fired; neither is inferable from the row.
                // Policy rows only, so every other row keeps its shape.
                if o["code"]
                    .as_str()
                    .is_some_and(|code| code.starts_with("policy.denied."))
                {
                    for field in ["errorKind", "rule"] {
                        if let Some(value) = r["structuredContent"].get(field) {
                            o[field] = value.clone();
                        }
                    }
                }
            }
            o
        })
        .collect::<Vec<_>>();
    // Envelope-level ok: false only when the whole call failed
    // (the all-failed branch below returns an error envelope);
    // per-URL truth rides results[].ok.
    let mut structured = json!({
        "ok": true,
        "ok_count": ok_count,
        "errors": err_count,
        "results": structured_results,
    });
    let debug_results = results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let fetch_debug = &result["_meta"]["com.donsetch/fetch-debug"];
            let mut item = json!({"url": urls[index]});
            for field in ["fetch", "tokens_est"] {
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
        "max_chars": budget,
        "results": debug_results,
    });

    if ok_count == 0 {
        structured["next_action"] =
            json!("inspect the per-URL errors above; retry only transient failures individually");
        // The batch carries the kind its URLs agree on: an all-permanent
        // batch (12 SSRF-refused URLs) must not advertise "safe to
        // retry immediately", and an all-walled one is walled. Mixed
        // kinds, or no kind anywhere = stay conservative (transient).
        let kinds: Vec<&str> = results
            .iter()
            .filter(|v| is_err(v))
            .filter_map(|v| v.get("errorKind").and_then(Value::as_str))
            .collect();
        let kind = agreed(&kinds).unwrap_or("transient");
        // An agreeing code is set explicitly, so it skips the text
        // classifier, which would read the first keyword anywhere in
        // the joined member texts. Mixed codes keep that derivation.
        let codes: Vec<&str> = results
            .iter()
            .filter(|v| is_err(v))
            .map(|v| {
                v.pointer("/structuredContent/code")
                    .and_then(Value::as_str)
                    .unwrap_or("content.extract")
            })
            .collect();
        if let Some(code) = agreed(&codes) {
            structured["code"] = json!(code);
        }
        let mut error = tool_error_structured(
            format!(
                "fetch: all {} urls failed\n\n{}",
                results.len(),
                text.trim_end()
            ),
            kind,
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

/// The value every item shares; `None` when the slice is empty or
/// the items differ.
fn agreed<'a>(items: &[&'a str]) -> Option<&'a str> {
    let (first, rest) = items.split_first()?;
    rest.iter().all(|item| item == first).then_some(*first)
}

/// Separator between batch members in the composed document.
const BATCH_MEMBER_SEP: &str = "\n\n---\n\n";

/// The composed header for an OK batch member.
fn batch_ok_header(index: usize, head: &str, url: &str) -> String {
    format!("## [{}] {}\n{}\n\n", index + 1, head, url)
}

/// The composed block for a failed batch member (error text verbatim).
fn batch_error_block(index: usize, url: &str, msg: &str) -> String {
    format!("## [{}] {} : ERROR\n{}\n\n---\n\n", index + 1, url, msg)
}

/// The member title recorded by its fetch ("" when absent).
fn batch_title(v: &Value) -> String {
    v.pointer("/_meta/com.donsetch~1fetch-debug/title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The in-content marker appended to a budget-sliced member.
fn batch_slice_marker(cut: usize, orig: usize) -> String {
    format!(
        "\n\n*[budget-sliced: showing {cut} of {orig} chars : refetch this url alone with max_chars for the rest]*"
    )
}

/// Slice OK batch markdowns to fit the shared budget. Pure so unit
/// tests pin the accounting (fetch_multi composes with
/// render_fetch_batch, which must agree with these helpers).
pub(super) fn slice_batch_markdowns(
    urls: &[String],
    results: &[Value],
    markdowns: &mut [Option<String>],
    budget_chars: usize,
    sliced_flags: &mut [bool],
) {
    let lens: Vec<usize> = markdowns
        .iter()
        .map(|m| m.as_ref().map(|s| s.len()).unwrap_or(0))
        .collect();
    let total: usize = lens.iter().sum();
    if total > budget_chars && total > 0 {
        // Everything the composer adds outside the member bodies rides
        // the same budget: member headers and separators, error text
        // verbatim, and a reserve for the per-member slicing marker.
        // The previous shape charged none of it, so a real batch ran
        // ~9% over its budget (markers alone: N x ~100 chars).
        const SLICE_MARKER_RESERVE: usize = 120;
        let mut fixed = 0usize;
        for (i, r) in results.iter().enumerate() {
            if lens[i] > 0 {
                fixed +=
                    batch_ok_header(i, &batch_title(r), &urls[i]).len() + BATCH_MEMBER_SEP.len();
                fixed += SLICE_MARKER_RESERVE;
            } else {
                let msg = r
                    .pointer("/content/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("fetch failed");
                fixed += batch_error_block(i, &urls[i], msg).len();
            }
        }
        let pool = budget_chars.saturating_sub(fixed);
        let n_ok = lens.iter().filter(|&&l| l > 0).count().max(1);
        let fair = pool / n_ok;
        let floor = (pool / n_ok / 4).clamp(300, 4_000).min(fair);
        let mut alloc: Vec<usize> = lens
            .iter()
            .map(|&l| {
                if l == 0 {
                    0
                } else {
                    (pool * l / total).max(floor)
                }
            })
            .collect();
        // Trim the largest allocations down to fit the pool.
        let mut over: i128 = alloc.iter().sum::<usize>() as i128 - pool as i128;
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
                let orig = md.len();
                let mut cut = alloc[i];
                // Reserve the marker inside the member's own share so
                // it can never push the batch past its budget.
                for _ in 0..3 {
                    let want = alloc[i].saturating_sub(batch_slice_marker(cut, orig).len());
                    if cut <= want {
                        break;
                    }
                    cut = want;
                }
                while cut > 0 && !md.is_char_boundary(cut) {
                    cut -= 1;
                }
                *m = Some(format!("{}{}", &md[..cut], batch_slice_marker(cut, orig)));
                sliced_flags[i] = true;
            }
        }
    }
}

fn fetch_input(url: &str) -> Result<url::Url, Value> {
    // Full parse up front: an unparseable URL would otherwise flow
    // through the whole pipeline with host="" : poisoning domain
    // profiles and producing confusing late errors.
    let parsed_url = match url::Url::parse(url) {
        Ok(u) => u,
        Err(e) => return Err(tool_error(format!("fetch: invalid URL ({e})"))),
    };
    // Validate the caller's URL before an adapter can remove credentials or
    // rewrite the host. The rewritten endpoint is independently guarded below.
    if let Err(error) = crate::fetch::guards::validate_url_basic(url) {
        // A rule denial is not an SSRF refusal: the explicit code below
        // would outrank the classifier and hide the operator's message.
        if matches!(error, FetchError::Denied { .. }) {
            return Err(policy_error_value(&error, url, None));
        }
        return Err(tool_error_structured(
            error.to_string(),
            "permanent",
            Some(json!({
                "url": url, "code": "guard.ssrf",
                "next_action": "pass a public http(s) URL without embedded credentials",
            })),
        ));
    }
    Ok(parsed_url)
}

/// An argument mistake: permanent, named, before anything is fetched.
fn args_error(message: impl Into<String>, next_action: &str) -> Value {
    tool_error_structured(
        message.into(),
        "permanent",
        Some(json!({"code": "fetch.args", "next_action": next_action})),
    )
}

/// The extraction options and the agent's view, from the call's arguments.
/// Extraction always reads the whole document; the view projects it.
pub(super) fn fetch_view(args: &Value) -> Result<(ExtractOptions, extract::view::View), Value> {
    use extract::view::{MUST_CONTAIN_MAX_TERMS, Term, View};
    let opts = ExtractOptions {
        include_links: args.get("links").and_then(Value::as_bool).unwrap_or(false),
        max_chars: Some(DOCUMENT_MAX_CHARS),
        ..ExtractOptions::default()
    };
    let text = |name: &str| {
        args.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let flag = |name: &str| args.get(name).and_then(Value::as_bool).unwrap_or(false);
    let raw_terms: Vec<String> = match args.get("must_contain") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(many)) => many
            .iter()
            .map(|v| v.as_str().map(String::from))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                args_error(
                    "must_contain: every term must be a string",
                    "pass a string or an array of strings",
                )
            })?,
        Some(_) => {
            return Err(args_error(
                "must_contain must be a string or an array of strings",
                "pass a string or an array of strings",
            ));
        }
    };
    if raw_terms.len() > MUST_CONTAIN_MAX_TERMS {
        return Err(args_error(
            format!("must_contain takes at most {MUST_CONTAIN_MAX_TERMS} terms"),
            "split the terms over several calls",
        ));
    }
    let must_contain = raw_terms
        .iter()
        .map(|t| Term::parse(t))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| args_error(e, "fix the term: a phrase, or a /regex/ with valid syntax"))?;
    let view = View {
        focus: text("focus"),
        section: text("section"),
        toc: flag("toc"),
        must_contain,
        offset: args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(1_000_000_000) as usize,
        max_chars: args
            .get("max_chars")
            .and_then(Value::as_u64)
            .map_or(MAX_CHARS_DEFAULT, |n| {
                (n as usize).clamp(MAX_CHARS_MIN, MAX_CHARS_MAX)
            }),
    };
    // Each view answers one question; a combination that would silently
    // drop one of them is refused instead.
    let probe = !view.must_contain.is_empty();
    let conflict = |a: &str, b: &str| {
        args_error(
            format!("{a} and {b} cannot be combined"),
            &format!("drop {b}, or make a second call for it"),
        )
    };
    if probe {
        for (set, name) in [
            (view.focus.is_some(), "focus"),
            (view.toc, "toc"),
            (view.offset > 0, "offset"),
            (flag("since_last"), "since_last"),
        ] {
            if set {
                return Err(conflict("must_contain", name));
            }
        }
    }
    if view.toc && view.focus.is_some() {
        return Err(conflict("toc", "focus"));
    }
    if flag("since_last") {
        for (set, name) in [
            (view.focus.is_some(), "focus"),
            (view.section.is_some(), "section"),
            (view.toc, "toc"),
            (view.offset > 0, "offset"),
        ] {
            if set {
                return Err(conflict("since_last", name));
            }
        }
    }
    Ok((opts, view))
}

/// Fetch one live URL on a route selected once for the whole call.
pub(super) async fn fetch_single(daemon: &Arc<Daemon>, args: &Value, url: &str) -> Value {
    let parsed_url = match fetch_input(url) {
        Ok(parsed) => parsed,
        Err(error) => return error,
    };
    // Preserve persona binding before the pool pick, then freeze that route
    // for the entire call. A related adapter host cannot bind another lane.
    let route = {
        let host = parsed_url.host_str().unwrap_or_default();
        let mut state = daemon.state.lock().await;
        let caps = crate::persona::PersonaCaps::from_profile(daemon.fetcher.profile());
        state.ensure_persona(host, &caps);
        state.ensure_persona_egress(host);
        daemon.fetcher.route_for_fetch(url)
    };
    let call = FetchCall {
        route,
        session_stamp: crate::state::disk_session_stamp().unwrap_or_else(|| "legacy:0".into()),
    };
    Box::pin(fetch_single_inner(daemon, args, url, &call)).await
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

fn fetch_url_rewrite(url: &url::Url, no_adapter: bool) -> Option<(String, &'static str)> {
    crate::adapters::rewrite(url).filter(|_| !no_adapter)
}

/// Whether a failed verdict means the ADAPTER rewrite bought
/// nothing: every non-content verdict, a challenge included. The
/// adapter is an optimization the caller never asked for, and
/// adapter endpoints never render (a `.json` page in a browser is
/// useless), so a refusal there must retry the page the caller
/// asked for (#287: every reddit fetch died on a single `.json`
/// 403 with no second hop and a one-step trail).
fn adapter_hop_failed(verdict: Verdict, adapter_host: bool, no_adapter: bool) -> bool {
    adapter_host && !no_adapter && !matches!(verdict, Verdict::ContentOk)
}

/// One legacy-host navigation to seed the reddit session (#291
/// follow-through): any old.reddit.com response runs its login
/// flow and seeds the cookies `www.reddit.com` needs before it
/// serves the real SSR page instead of the humanity interstitial
/// or the JS shell. The body is discarded; the side effect is the
/// cookie jar. Returns whether a navigation actually ran.
async fn reddit_session_hop(
    daemon: &Arc<Daemon>,
    u: &url::Url,
    trace: &mut Trace,
    route: &RequestRoute,
) -> bool {
    let Some(oldu) = crate::adapters::reddit_session_url(u) else {
        return false;
    };
    let t0 = std::time::Instant::now();
    let hop = daemon
        .fetcher
        .fetch_persona_on_route(&oldu, Some(route))
        .await;
    trace.step(
        "local",
        "reddit-session",
        &format!("status={}", hop.as_ref().map_or(0, |o| o.status)),
        t0.elapsed().as_millis(),
    );
    true
}

/// Reddit's anonymous identity cookie, `loid`, is the gate for the
/// direct `.json` endpoint (verified live: loid alone serves the
/// listing, absent it is refused). When the jar already carries it
/// the pre-emptive legacy-host hop is a redundant request; without
/// it the hop is what seeds a session. A refusal on a direct
/// attempt still runs the one-shot session fallback, so a stale
/// loid self-heals.
fn reddit_session_live(cookie_names: &[String]) -> bool {
    cookie_names.iter().any(|name| name == "loid")
}

async fn reddit_session_fallback(
    daemon: &Arc<Daemon>,
    args: &Value,
    url: &str,
    trace: &mut Trace,
    call: &FetchCall,
) -> Value {
    let hop_done = match url::Url::parse(url) {
        Ok(pu) => reddit_session_hop(daemon, &pu, trace, &call.route).await,
        Err(_) => false,
    };
    let prior = trace.value().as_array().cloned().unwrap_or_default();
    let mut args2 = args.clone();
    args2["_reddit_session"] = json!(true);
    let mut res = Box::pin(fetch_single_inner(daemon, &args2, url, call)).await;
    if let Some(sc) = res.pointer_mut("/structuredContent") {
        sc["reddit_session"] = json!(hop_done);
    }
    fold_trace_into_result(&mut res, prior);
    res
}

/// A reddit page refusal locally that one session-init retry can
/// fix (the content host serves the humanity page or the JS shell
/// to a sessionless client). Never twice (`_reddit_session`
/// guard), never off the content host.
fn reddit_session_retry_eligible(verdict: &Verdict, host: &str, args: &Value) -> bool {
    matches!(verdict, Verdict::Challenge(_) | Verdict::Blocked)
        && matches!(host, "www.reddit.com" | "reddit.com")
        && !args
            .get("_reddit_session")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// Retry the caller's URL without the adapter rewrite, and fold the
/// adapter hop's trail in front of the retry's so the trail reads
/// as ONE story rather than two unrelated hops. The retry runs the
/// full generic pipeline, which handles walls on its own rules
/// (HTML fetch, cookie retry, the optional unlocker).
#[allow(clippy::too_many_arguments)]
async fn adapter_fallback(
    daemon: &Arc<Daemon>,
    args: &Value,
    orig_url: &str,
    trace: &mut Trace,
    action: &str,
    why: &str,
    session_seeded: bool,
    call: &FetchCall,
) -> Value {
    trace.step("adapter", action, why, 0);
    // Try the public page before spending a session-init request. A blocked
    // or thin Reddit page still gets the existing one-hop session recovery.
    // Legacy-host caller URLs retry on the content host: `old.`/
    // `np.` serve a login wall to anonymous clients, so the retry
    // there could never succeed.
    let retry_url = url::Url::parse(orig_url)
        .ok()
        .and_then(|u| crate::adapters::reddit_content_url(&u))
        .unwrap_or_else(|| orig_url.to_string());
    let prior = match trace.value() {
        Value::Array(a) => a,
        _ => Vec::new(),
    };
    let mut args2 = args.clone();
    args2["_no_adapter"] = json!(true);
    args2["_reddit_session"] = json!(session_seeded);
    let mut res = Box::pin(fetch_single_inner(daemon, &args2, &retry_url, call)).await;
    if let Some(sc) = res.pointer_mut("/structuredContent") {
        sc["adapter_fallback"] = json!(true);
    }
    fold_trace_into_result(&mut res, prior);
    res
}

/// Put the adapter hop's steps in front of the retry's own trail.
/// Error envelopes carry the trail in `structuredContent`; success
/// results carry it under `_meta.com.donsetch/fetch-debug`.
fn fold_trace_into_result(res: &mut Value, prior: Vec<Value>) {
    if prior.is_empty() {
        return;
    }
    if let Some(esc) = res
        .pointer_mut("/structuredContent/escalation")
        .and_then(Value::as_array_mut)
    {
        let mut combined = prior;
        combined.append(esc);
        *esc = combined;
        return;
    }
    if let Some(esc) = res
        .pointer_mut("/_meta/com.donsetch~1fetch-debug/escalation")
        .and_then(Value::as_array_mut)
    {
        let mut combined = prior;
        combined.append(esc);
        *esc = combined;
    }
}

#[allow(clippy::field_reassign_with_default)]
async fn fetch_single_inner(
    daemon: &Arc<Daemon>,
    args: &Value,
    url: &str,
    call: &FetchCall,
) -> Value {
    let t0 = std::time::Instant::now();
    let parsed_url = match fetch_input(url) {
        Ok(parsed) => parsed,
        Err(error) => return error,
    };
    let url_host = parsed_url.host_str().unwrap_or("").to_string();

    // Domain intelligence (v3): the adapters registry may rewrite
    // the URL to the site's own structured endpoint (reddit .json,
    // npm/PyPI/crates/Go/RubyGems APIs) : one cheap local request
    // for structured truth. `orig_url` stays the agent-facing
    // identity: history, handles, and display key on it.
    let no_adapter = args
        .get("_no_adapter")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let orig_url = url.to_string();
    let adapter_used: Option<&'static str>;
    let url = match fetch_url_rewrite(&parsed_url, no_adapter) {
        Some((new_url, name)) => {
            adapter_used = Some(name);
            new_url
        }
        None => {
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
        // A rule on the rewrite target: the SSRF advice below would
        // replace the operator's message.
        if matches!(e, FetchError::Denied { .. }) {
            return policy_error_value(&e, &url, None);
        }
        return tool_error_structured(
            format!("{e}"),
            "permanent",
            Some(json!({
                "url": url,
                "next_action": "private/loopback targets are blocked by design : use a public URL",
            })),
        );
    }
    let (opts, view) = match fetch_view(args) {
        Ok(parsed) => parsed,
        Err(error) => return error,
    };
    let image_text = args
        .get("image_text")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let since_last = args
        .get("since_last")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // v5: `actions` was removed with the browser tier. The spec layer
    // answers it first (like `tier`); this guard is the layer under it.
    if args.get("actions").is_some_and(|v| !v.is_null()) {
        return tool_error(
            "fetch: actions was removed: every web_fetch runs the local engine, with no browser drive",
        );
    }

    let host = url_host;

    // === PDF early detection ===
    // If the URL looks like a PDF, always fetch raw bytes and route
    // to the DonSheet engine. The check covers both the `.pdf`
    // suffix and the `/pdf/` path convention (arXiv serves PDFs at
    // /pdf/1706.03762 with no extension).
    let is_pdf_url = crate::fetch::guards::is_pdf_url_like(&url);
    // v4.7 V03: JSON data endpoints (`reddit .json`, API dumps) are
    // terminal at the local fetch : no further pass can improve a
    // structured payload. Same class PDFs and adapter endpoints
    // already follow.
    let is_json_url = is_json_url_like(&url);

    // === Decision: how to route this fetch? ===
    // The self-improving loop: the domain profile decides
    // cold / warm / skip-to-solve / recheck-cold.
    // Adapter endpoints (reddit .json, package registry APIs) are
    // plain-GET structured targets : they never need a rendered
    // fetch. Force Cold even if a stale profile says SkipToSolve.
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
    let age = daemon
        .state
        .lock()
        .await
        .human_wall_age(&host, &url, &call.route.id());
    if let Some(age) = age {
        let mut trace = Trace::default();
        trace.step("route", "human-wall-cache", "recent observed refusal", 0);
        return tool_error_structured(
            "this exact URL recently required human verification",
            "walled",
            Some(
                json!({"url":url,"code":"wall.captcha","cached":true,"observed_ago_s":age,
                    "next_action":"complete this verification in an interactive browser session; a new login or route invalidates this short-lived refusal",
                    "escalation":trace.value()}),
            ),
        );
    }
    let mut trace = Trace::default();

    // === The local fetch ===
    let mut out: Option<crate::fetch::client::FetchOutcome> = None;

    // === v3 F1: search→fetch warm handoff ===
    // Enrichment already fetched the top search results : if
    // this URL is one of them, serve that body, skip the
    // network. One-shot (a second fetch goes to the wire for
    // freshness); the rest of the pipeline (extraction, history)
    // runs unchanged on the cached body.
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
            .filter(|entry| entry.outcome.route == call.route)
    } else {
        None
    };
    if let Some(entry) = prewarm_entry {
        prewarmed = true;
        trace.step("prewarm", "search-handoff", "hit", 0);
        // law 6: make the warm handoff observable in `donsetch status`.
        daemon.state.lock().await.note_prewarm_served();
        out = Some(entry.outcome);
    }

    let mut adapter_session_seeded = false;
    if !prewarmed {
        let t0 = std::time::Instant::now();
        let response = {
            let request = daemon
                .fetcher
                .fetch_persona_on_route(&url, Some(&call.route));
            let needs_seed = adapter_used == Some("adapter:reddit-json")
                && !reddit_session_live(&daemon.fetcher.cookie_names(&host));
            if needs_seed {
                // The structured endpoint and session initialization do not
                // depend on each other's response. Overlap their network waits.
                // A usable JSON reply wins immediately; failures still wait
                // for the session before reading the public SSR page. A jar
                // that already holds the session skips the hop entirely.
                let seed = reddit_session_hop(daemon, &parsed_url, &mut trace, &call.route);
                tokio::pin!(request, seed);
                tokio::select! {
                    reply = &mut request => {
                        if !matches!(&reply, Ok(out) if out.verdict == Verdict::ContentOk) {
                            adapter_session_seeded = seed.await;
                        }
                        reply
                    },
                    seeded = &mut seed => {
                        adapter_session_seeded = seeded;
                        request.await
                    },
                }
            } else {
                request.await
            }
        };
        let fetched = match response {
            Ok(o) => o,
            Err(e) => {
                // A rule refused a hop (a redirect into a denied host).
                // Above the adapter branch: falling back to the original
                // URL would lose the policy error.
                if matches!(e, FetchError::Denied { .. }) {
                    return policy_error_value(&e, &url, Some(trace.value()));
                }
                if adapter_host && !no_adapter && !matches!(e, FetchError::ProxyConfig(_)) {
                    // Transport failure on the adapter endpoint :
                    // retry the caller's URL before giving up.
                    return adapter_fallback(
                        daemon,
                        args,
                        &orig_url,
                        &mut trace,
                        "fallback",
                        "transport error : retrying original URL",
                        adapter_session_seeded,
                        call,
                    )
                    .await;
                }
                let kind = fetch_error_kind(&e);
                let failures = if transport_failure_evidence(&e) {
                    let mut state = daemon.state.lock().await;
                    state.record_failure(&host, crate::state::FailClass::Network);
                    state.recent_network_failures(&host)
                } else {
                    0
                };
                let next_action = if failures >= 3 {
                    "repeated transport failures from this egress in the last 10 minutes; check the host from another network or choose another source before retrying".into()
                } else {
                    transport_next_action(&e, kind)
                };
                return tool_error_structured(
                    friendly_fetch_error(&e),
                    kind,
                    Some(json!({
                        "url": url,
                        "status": 0,
                        // The error's OWN code outranks the text
                        // classifier: the guard knows a resolution failure
                        // is a DNS failure (#248).
                        "code": fetch_error_code(&e),
                        "fetch_error": transport_class(&e),
                        "next_action": next_action,
                        "recent_network_failures": failures,
                        "escalation": trace.value(),
                    })),
                );
            }
        };
        let ms = t0.elapsed().as_millis();
        let verdict_str = format!("{:?}", fetched.verdict);
        trace.step(
            "local",
            "http-fetch",
            &format!("{} status={}", verdict_str, fetched.status),
            ms,
        );
        out = Some(fetched);

        // === Observe the outcome ===
        // Counters for `donsetch status`: only CHALLENGES say anything
        // about walls. A 404, 429, paywall or auth wall is an honest
        // terminal answer from the origin. The fetch layer keeps its own
        // per-origin memory; these never route a fetch.
        let o = out.as_ref().unwrap();
        {
            let mut state = daemon.state.lock().await;
            match o.verdict {
                // An adapter endpoint's refusal is not evidence about
                // the PAGE (reddit's protected `.json` is the known
                // case, #291): recording it as a domain wall
                // mis-routed the fallback retry and every later fetch.
                // The fallback below re-probes the
                // caller's URL itself; its own verdict is what
                // records a wall. v4.7 V03: the same holds for any
                // JSON data endpoint : a `.json` challenge says
                // nothing about the human-facing pages.
                Verdict::Challenge(_) if adapter_host || is_json_url || is_json_endpoint(o) => {}
                Verdict::Challenge(vendor) => {
                    state.record_failure(&host, crate::state::FailClass::Block);
                    let vendor = format!("{vendor:?}").to_lowercase();
                    state.record_cold_walled(&host, Some(&vendor));
                }
                // API access does not prove the original website wall
                // cleared (v4.7 V03: nor does a JSON endpoint's success :
                // the data lane teaches counters, not page routing).
                Verdict::ContentOk if adapter_host || is_json_url || is_json_endpoint(o) => {
                    state.record_fetch(&host)
                }
                Verdict::ContentOk => state.record_cold_ok(&host),
                // Everything else (404, rate-limit, paywall, auth,
                // hard block): counters only, no wall inference.
                _ => state.record_fetch(&host),
            }
        }
    }

    // v4.7 V03: JSON data endpoints are terminal at the local fetch.
    // The `.json` name or a JSON content type means a structured
    // payload : nothing downstream can improve it, and it must not
    // gate the verdict.
    let json_like = is_json_url || out.as_ref().is_some_and(is_json_endpoint);

    // === Verdict gate ===
    // Everything except ContentOk is terminal at the local fetch :
    // a clean error; there is no second engine. Wall-class verdicts
    // (Challenge/Blocked) ask the optional unlocker first.
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
            // A refusal on an ADAPTER endpoint (a challenge
            // included) is a failed adapter hop, never the end:
            // retry the caller's URL through the generic pipeline
            // (#287: reddit's `.json` 403 used to end every
            // fetch at exactly this point).
            _ if adapter_hop_failed(o.verdict, adapter_host, no_adapter) => {
                let why = format!("{:?} : retrying original URL", o.verdict);
                return adapter_fallback(
                    daemon,
                    args,
                    &orig_url,
                    &mut trace,
                    "fallback",
                    &why,
                    adapter_session_seeded,
                    call,
                )
                .await;
            }
            // A reddit page (thread, listing, about, wiki) refused
            // without a session: the humanity page or the
            // shell. One legacy-host navigation seeds the session,
            // the retry gets the real SSR page, and the SSR adapter
            // turns that into the card. Cheap enough to try before
            // any wall handling.
            _ if reddit_session_retry_eligible(&o.verdict, &host, args) => {
                return reddit_session_fallback(daemon, args, &url, &mut trace, call).await;
            }
            v => {
                let kind = verdict_kind(v, o.status);
                // The optional unlocker for hard walls (Challenge/
                // Blocked); skipped for AuthWall/Paywall/SoftNotFound
                // (credentials/money/dead).
                if matches!(v, Verdict::Challenge(_) | Verdict::Blocked)
                    && let Some(v3) = try_bypass(daemon, &url, &opts, &mut trace).await
                {
                    return v3;
                }
                let human_wall = crate::detect::walls::human_verification_wall(&o.body, v);
                if human_wall {
                    daemon.state.lock().await.record_human_wall(
                        &host,
                        &url,
                        &call.route.id(),
                        &call.session_stamp,
                    );
                }
                return tool_error_structured(
                    if human_wall {
                        format!("interactive verification required at {}", o.url)
                    } else {
                        verdict_error(v, o.status, &o.url)
                    },
                    kind,
                    Some(json!({
                        "url": o.url,
                        "status": o.status,
                        "verdict": format!("{:?}", v),
                        "next_action": if human_wall {
                            "complete this verification in an interactive browser session; no automated solver can pass it".to_string()
                        } else { next_action_for(Some(v), o.status, kind) },
                        "escalation": trace.value(),
                    })),
                );
            }
        }
    }

    if adapter_used == Some("adapter:stackexchange-api")
        && let Some(o) = &out
    {
        crate::adapters::stackexchange::record_api_backoff(&o.body);
    }

    // === Adapter shape check ===
    // A 200 that isn't JSON on a rewritten endpoint (login walls,
    // HTML error interstitials) bought the adapter nothing : fall
    // back to the original URL through the full generic pipeline
    // (which still treats a shell honestly).
    if adapter_host
        && !no_adapter
        && let Some(o) = &out
        && matches!(o.verdict, Verdict::ContentOk)
        && (!matches!(
            o.body.iter().find(|b| !b.is_ascii_whitespace()),
            Some(b'{') | Some(b'[')
        ) || (adapter_used == Some("adapter:stackexchange-api")
            && !crate::adapters::stackexchange::api_payload_valid(&o.body, &o.url)))
    {
        return adapter_fallback(
            daemon,
            args,
            &orig_url,
            &mut trace,
            "shape-mismatch",
            "structured payload unavailable : retrying original URL",
            adapter_session_seeded,
            call,
        )
        .await;
    }

    // === Local extraction (when we have a body) ===
    let mut final_ex: Option<extract::Extracted> = None;
    let final_fetch: &str = "local";
    let final_status: u16 = out.as_ref().map(|o| o.status).unwrap_or(0);
    let mut final_url: String = url.clone();
    let mut final_verdict: String = out
        .as_ref()
        .map(|o| format!("{:?}", o.verdict))
        .unwrap_or_else(|| "ContentOk".to_string());
    let mut extraction_wall = None;

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
        let read_opts = extract::ExtractOptions {
            verified: o.verified,
            ..opts.clone()
        };
        match extract::extract_off_worker(&o.body, &ct, &o.url, &read_opts).await {
            Ok(e) => {
                final_url = o.url.clone();
                final_ex = Some(e);
            }
            Err(extract::ExtractError::Wall(v)) => {
                extraction_wall = Some(v);
                final_verdict = format!("{v:?}");
            }
            Err(e) => {
                return tool_error_structured(
                    format!("content extraction failed: {e}"),
                    "transient",
                    Some(json!({
                        "url": url,
                        "next_action": "retry once; if it fails again, the page is too large or malformed to extract: choose another source",
                    })),
                );
            }
        }
    }

    let challenge = out
        .as_ref()
        .map(|o| crate::detect::walls::browser_recovery(o.status, &o.headers, &o.body, o.verdict))
        .unwrap_or(false)
        || extraction_wall.is_some();

    // Links fallback: listing/feed pages (Hacker News, indexes)
    // die in the prose pipeline simply for being link-dense. Try
    // links-keeping extraction before failing.
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
        if let Ok(e3) = extract::extract_off_worker(&o.body, &ct, &o.url, &lopts).await
            && !e3.thin
        {
            final_ex = Some(e3);
            trace.step("local", "links-extract", "ok", 0);
        }
    }

    // === Walled and shell outcomes ===
    // v5: one local system, no second engine to escalate to. A
    // wall-class outcome reaches the optional unlocker once, then
    // fails honestly; a shell that never hydrated locally fails
    // honestly too, and a shell is never served as content.
    // (thin recomputed AFTER the local links fallback.)
    //
    // Exception: very small pages (< 5KB) that came back thin are
    // 404/error pages, not JS shells. JS shells are > 50KB (React
    // apps, SPAs). A 2KB page with no content is a 404, never a
    // shell class.
    let still_thin = final_ex.as_ref().map(|e| e.thin).unwrap_or(false);
    let page_size = out.as_ref().map(|o| o.body.len()).unwrap_or(0);
    // PDF detection: if the response is a PDF (content-type or magic
    // bytes), never render it : a browser PDF viewer is a JS shell
    // with no extractable text. PDFs are handled by DonSheet.
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
    // But a small PDF is still a PDF : DonSheet handles it. And
    // it is only a 404 when the STATUS says so: a 2xx that comes
    // back with no content is a wall or a spinner shell, not a
    // 404 (live case: tiktok's "Please wait..." 12-token fetch
    // served as ok). A real error status is never a shell class.
    let status_2xx = out
        .as_ref()
        .map(|o| (200..300).contains(&o.status))
        .unwrap_or(false);
    let is_small_404 = page_size > 0
        && page_size < 5_000
        && still_thin
        && !challenge
        && !is_pdf_content
        && !status_2xx;
    // Framework-junk shells (React server dumps leak identifiers
    // as DOM text nodes): the extracted "content" is things like
    // `# is_latency_sensitive_broadcast`, `FDSTooltipDef`,
    // `[["low","normal"]]` with no prose (live case: facebook's
    // local fetch served that as a successful result). Short,
    // sentence-free, identifier-dominated text = a shell, not
    // content: fail instead of serving garbage.
    let shell_text =
        status_2xx && !is_pdf_content && final_ex.as_ref().map(is_framework_shell).unwrap_or(false);
    // #282: "the extraction is non-empty" is not a success test. A
    // challenge interstitial or a chrome-only shell must fail
    // honestly, not serve.
    let verified = out.as_ref().is_some_and(|o| o.verified);
    let challenge_text = status_2xx
        && !verified
        && final_ex
            .as_ref()
            .map(|e| crate::detect::walls::challenge_text(&e.markdown))
            .unwrap_or(false);
    let chrome_text = status_2xx
        && !is_pdf_content
        && final_ex
            .as_ref()
            .map(|e| crate::extract::quality::chrome_only(&e.markdown))
            .unwrap_or(false);
    // Reddit can answer 200 with a logged-out JS shell. The same cheap
    // session initialization used for explicit walls belongs before render.
    if status_2xx
        && (still_thin || shell_text || challenge_text || chrome_text)
        && reddit_session_retry_eligible(&Verdict::Blocked, &host, args)
    {
        return reddit_session_fallback(daemon, args, &url, &mut trace, call).await;
    }
    // Wall-class: a challenge body, an extraction wall, or an
    // interstitial behind a 2xx. Shell-class: a client-rendered page
    // that never hydrated locally. Adapters, JSON payloads and PDFs
    // are excluded (they are terminal at the local fetch already).
    let sink_eligible = !is_pdf_content && !adapter_host && !json_like;
    // Wave W / D5: a scripted micro-document with near-empty extraction is a
    // throttle/block shell (bloomberg's 502-byte ok:true body), never ok.
    let micro_shell = sink_eligible
        && status_2xx
        && out
            .as_ref()
            .map(|o| {
                crate::detect::walls::js_micro_shell(
                    &o.body,
                    final_ex.as_ref().map(|e| e.markdown.len()).unwrap_or(0),
                )
            })
            .unwrap_or(false);
    let wall_like = sink_eligible && (challenge || challenge_text);
    // A body the fetch layer verified as content is served as extracted, short
    // or not: its own contract already ruled out a shell.
    let shell_like = sink_eligible
        && !wall_like
        && !(verified
            && final_ex
                .as_ref()
                .is_some_and(|e| !e.markdown.trim().is_empty()))
        && (shell_text || chrome_text || micro_shell || (still_thin && !is_small_404));
    if wall_like {
        let v = extraction_wall.unwrap_or(Verdict::Challenge(Vendor::Generic));
        // The optional unlocker: a paid opt-in reached after a local
        // failure, never a tier. No key = inert.
        if matches!(v, Verdict::Challenge(_) | Verdict::Blocked)
            && let Some(v3) = try_bypass(daemon, &url, &opts, &mut trace).await
        {
            return v3;
        }
        let kind = verdict_kind(v, final_status);
        let msg = match v {
            Verdict::Challenge(Vendor::Generic) => format!(
                "blocked at {url} : the page serves an anti-bot challenge that did not clear"
            ),
            _ => verdict_error(v, final_status, &url),
        };
        return tool_error_structured(
            msg,
            kind,
            Some(json!({
                "url": url,
                "status": final_status,
                "verdict": format!("{v:?}"),
                "next_action": next_action_for(Some(v), final_status, kind),
                "escalation": trace.value(),
            })),
        );
    }
    if shell_like {
        // Wave W / D4: a thin shell whose visible content is auth CTAs is an
        // authentication gate, not a hydration failure — the honest answer a
        // browser would give (measured: x.com's logged-out shell).
        if out
            .as_ref()
            .map(|o| crate::detect::walls::auth_shell(&o.body))
            .unwrap_or(false)
        {
            return tool_error_structured(
                format!(
                    "login required at {url} : the page is an authentication gate and served no public content"
                ),
                "permanent",
                Some(json!({
                    "url": url,
                    "status": final_status,
                    "verdict": "AuthWall",
                    "next_action": "the page needs a signed-in session, which fetch does not carry; choose a public source for this content",
                    "escalation": trace.value(),
                })),
            );
        }
        return tool_error_structured(
            format!(
                "no readable content at {url} : the page renders client-side and did not hydrate"
            ),
            "transient",
            Some(json!({
                "url": url,
                "status": final_status,
                "verdict": "Incomplete",
                "next_action": "retry later, or choose another source for this page",
                "escalation": trace.value(),
            })),
        );
    }

    let Some(ex) = final_ex else {
        return tool_error_structured(
            "fetch produced no usable response",
            "permanent",
            Some(json!({
                "url": url,
                "status": 0,
                "next_action": "retry : if repeated, the site may be down",
                "escalation": trace.value(),
            })),
        );
    };

    // #282: a challenge interstitial or a chrome-only shell is not
    // content, whatever produced it. The failure is "walled" so the
    // optional unlocker can engage instead of the agent trusting
    // navigation boilerplate.
    if let Some((msg, kind, action)) = content_fail(&ex.markdown, &url, final_status, verified) {
        return tool_error_structured(
            msg,
            kind,
            Some(json!({
                "url": url,
                "status": final_status,
                "verdict": "Challenge(Generic)",
                "next_action": action,
                "escalation": trace.value(),
            })),
        );
    }

    // Final shell gate: a framework dump (React server renders)
    // must never ship as successful content regardless of what
    // produced it (live case: facebook returned React internals
    // as markdown with a clean verdict). Honest fail with the
    // repair hint instead of garbage "success".
    if is_framework_shell(&ex) {
        return tool_error_structured(
            format!(
                "blocked at {url} : the site renders an app shell without real content for non-interactive clients"
            ),
            "permanent",
            Some(json!({
                "url": url,
                "verdict": "Challenge(Generic)",
                "next_action": "open the page in an interactive browser session; retry later",
                "escalation": trace.value(),
            })),
        );
    }

    // Small 404 page: a small error-status page with no content and
    // no wall class is "not found", not a shell. The page may not
    // exist, be deleted, or require JavaScript.
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
    // The document is whole here; the view projects it. OCR text joins the
    // document first, so every view (a probe, focus) searches it too.
    let mut ex = ex;
    let document = ex.markdown.clone();
    let ocr_state = if image_text {
        let (section, state) = image_ocr(daemon, &ex.images).await;
        ex.markdown.push_str(&section);
        Some(state)
    } else {
        None
    };
    let incomplete = ex
        .partial
        .as_ref()
        .map(|p| match p.items_total {
            Some(total) => format!("{}: {} of {total} items loaded", p.reason, p.items_found),
            None => format!("{}: {} items loaded", p.reason, p.items_found),
        })
        .or_else(|| {
            ex.next_offset.map(|_| {
                format!("the document is longer than the {DOCUMENT_MAX_CHARS}-char read limit")
            })
        });
    let rendered = extract::view::render(&ex.markdown, &view, incomplete.as_deref());
    ex.markdown = rendered.text;
    ex.next_offset = rendered.next_offset;
    ex.tokens_est = ex.markdown.len() / 4;

    let mut res = finish_result(
        &ex,
        final_fetch,
        final_status,
        &final_verdict,
        &display_url,
        &trace,
        t0.elapsed().as_millis(),
    );
    if let Some(sc) = res.pointer_mut("/structuredContent") {
        if !ex.thin {
            sc["read_status"] = json!(match (rendered.status, ex.partial.is_some()) {
                ("content", true) => "partial",
                (status, _) => status,
            });
            sc["content_complete"] = json!(rendered.complete && ex.partial.is_none());
        }
        if let Some(state) = rendered.state.as_object() {
            for (key, value) in state {
                sc[key] = value.clone();
            }
        }
        if rendered.verdict.is_some()
            && let Some(object) = sc.as_object_mut()
        {
            object.remove("content_kind");
        }
        if let Some(state) = ocr_state {
            sc["image_text"] = state;
        }
        if let Some(next) = out
            .as_ref()
            .filter(|o| o.verdict == Verdict::ContentOk && adapter_used.is_none())
            .and_then(|o| next_page(&o.body, &o.url))
        {
            sc["next_page"] = json!(next);
        }
    }
    maybe_record_agent_outcome(daemon, &host, &view, &ex, &final_verdict, rendered.verdict);
    if daemon.state.lock().await.is_flaky(&host) {
        res["structuredContent"]["stability"] = json!("flaky");
    }
    if prewarmed {
        res["_meta"]["com.donsetch/fetch-debug"]["prewarmed_by_search"] = json!(true);
    }
    apply_link_handles(daemon, &mut res).await;

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
    // Every read records the page for since_last; only a since_last call
    // replaces its output with the change report.
    apply_page_history(
        daemon,
        &mut res,
        &display_url,
        PageFacts {
            fingerprint: ex.fingerprint.as_deref(),
            markdown: &document,
            title: ex.title.as_deref(),
            complete: incomplete.is_none() && !ex.thin,
        },
        since_last,
    );
    res
}

/// The same-host page an HTML document names as its next one
/// (`rel="next"`), for reading a paginated article part by part.
fn next_page(body: &[u8], base: &str) -> Option<String> {
    static REL_NEXT: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r#"(?i)<(?:link|a)\b[^>]*\brel\s*=\s*["']?[^"'>]*\bnext\b"#)
            .expect("static regex")
    });
    if !REL_NEXT.is_match(body) {
        return None;
    }
    let html = String::from_utf8_lossy(body);
    let next = find_rel_next(&html, base)?;
    let host = |u: &str| {
        url::Url::parse(u)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
    };
    (next != base && host(&next).is_some() && host(&next) == host(base)).then_some(next)
}

/// The optional unlocker (Bright Data Web Unlocker): when a local
/// fetch hits a hard wall (Challenge/Blocked), hand the URL to the
/// paid API once. Opt-in via `donsetch keys add unlocker <key>`;
/// no key = inert, returns None. A challenge or empty shell that
/// comes back stays a failed fetch.
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
    let ex =
        match extract::extract_off_worker(&outcome.body, &outcome.content_type, url, opts).await {
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
    let failure = if ex.blocks_total == 0 || ex.markdown.trim().is_empty() {
        Some("unlocker returned no extracted content".to_string())
    } else {
        content_fail(&ex.markdown, url, outcome.status, false).map(|(msg, _, _)| msg)
    };
    if let Some(msg) = failure {
        crate::fetch::bypass::invalidate_cached(&cache_dir, url, cfg.render);
        trace.step("bypass", "unlocker", &msg, t0.elapsed().as_millis());
        return None;
    }
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
    let mut res = finish_result(
        &ex,
        "unlocker",
        outcome.status,
        "ContentOk",
        url,
        trace,
        t0.elapsed().as_millis(),
    );
    res["_meta"]["com.donsetch/fetch-debug"]["bypass"] = json!({
        "provider": "brightdata",
        "cache": outcome.cached,
    });
    apply_link_handles(daemon, &mut res).await;
    Some(res)
}

/// Short, sentence-free, identifier-dominated extraction = a
/// framework shell (React server dumps leak identifiers as DOM
/// text nodes). Real prose has sentences.
fn is_framework_shell(ex: &extract::Extracted) -> bool {
    ex.via.is_none() && looks_like_shell_text(&ex.markdown)
}

fn looks_like_shell_text(markdown: &str) -> bool {
    if markdown.len() > 1500 {
        return false;
    }
    let sentences = markdown
        .split(['.', '!', '?', '\n'])
        .filter(|s| s.split(' ').filter(|w| w.chars().count() > 3).count() >= 6)
        .count();
    if sentences >= 2 {
        return false;
    }
    let tokens: Vec<&str> = markdown.split_whitespace().collect();
    if tokens.len() < 8 {
        // A tiny fragment is a shell only when it is a spinner
        // prompt; a short real profile card ("cristiano / 679M
        // followers") must not be killed by bare smallness.
        let lower = markdown.to_ascii_lowercase();
        return lower.contains("please wait") || lower.contains("one moment");
    }
    let ident = tokens
        .iter()
        .filter(|t| {
            // A prose word = plain letters. Everything else (version
            // strings, paths, bracket arrays, snake_case, camelCase,
            // scheme:// tokens) is framework residue.
            let t = t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            !t.is_empty()
                && (t.contains('_')
                    || t.contains('/')
                    || t.contains('[')
                    || t.contains(':')
                    || (t.chars().any(|c| c.is_ascii_digit())
                        && t.chars().any(|c| !c.is_alphanumeric()))
                    || t.char_indices().skip(1).any(|(i, ch)| {
                        ch.is_uppercase() && t[..i].chars().last().is_some_and(|c| c.is_lowercase())
                    }))
        })
        .count();
    ident * 100 / tokens.len() >= 40
}

/// Positive content tests (#282): a challenge interstitial or a
/// chrome-only shell that made it through as "some text" is not
/// content. Returns the honest failure for the extracted shape, or
/// None when the text is real content. A `verified` body (the fetch layer's
/// contract found content) is not failed on bot-wall words alone.
fn content_fail(
    markdown: &str,
    url: &str,
    status: u16,
    verified: bool,
) -> Option<(String, &'static str, &'static str)> {
    if let Some(wall) = crate::detect::walls::text_wall(markdown)
        && !(verified && matches!(wall, Verdict::Challenge(_) | Verdict::Blocked))
    {
        if wall == Verdict::AuthWall {
            return Some((
                format!("login required at {url}"),
                "walled",
                "the page needs a signed-in session, which fetch does not carry; use an accessible source",
            ));
        }
        return Some((
            format!(
                "blocked at {url} : the page is an anti-bot challenge that did not clear (the extracted text is the interstitial, not content)"
            ),
            "walled",
            "retry later : the challenge may clear; a configured unlocker can try this class",
        ));
    }
    if (200..300).contains(&status) && crate::extract::quality::chrome_only(markdown) {
        return Some((
            format!(
                "blocked at {url} : the page rendered only navigation and login chrome (an empty shell or a login wall), no content"
            ),
            "walled",
            "use an interactive browser session for sites like these : the page may be a client-rendered shell or a login wall",
        ));
    }
    None
}

/// v4.7 V03: JSON data-endpoint URL check : the path names a `.json`
/// document (reddit `.json`, API dumps; query and fragment stripped,
/// case-insensitive). A structured payload is terminal at the local
/// fetch : a rendered document cannot improve it.
pub(super) fn is_json_url_like(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_lowercase();
    path.ends_with(".json")
}

/// A response whose content type declares JSON (application/json,
/// application/ld+json, application/problem+json, ...).
fn ct_is_json(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type") && value.to_lowercase().contains("json")
    })
}

/// A fetched outcome that is a JSON data endpoint : its URL names a
/// `.json` document or the response itself is JSON.
fn is_json_endpoint(o: &crate::fetch::client::FetchOutcome) -> bool {
    is_json_url_like(&o.url) || ct_is_json(&o.headers)
}

/// OCR the page's first content images (up to 4, 5 MB each,
/// SSRF-guarded): the `image text` section to append to the document,
/// and the state for the structured result. Runs only when asked.
pub(super) async fn image_ocr(
    daemon: &Arc<Daemon>,
    images: &[(String, String)],
) -> (String, Value) {
    #[cfg(not(feature = "ocr"))]
    {
        let _ = (daemon, images);
        (
            String::new(),
            json!("unavailable : this build lacks the ocr feature"),
        )
    }
    #[cfg(feature = "ocr")]
    {
        const MAX_IMAGES: usize = 4;
        const MAX_BYTES: usize = 5 * 1024 * 1024;
        if images.is_empty() {
            return (String::new(), json!({"images": 0, "ocred": 0}));
        }
        let mut section = String::from("\n## image text (OCR)\n");
        let mut ocred = 0usize;
        use futures_util::{StreamExt, stream};
        let candidates = images.iter().take(MAX_IMAGES).cloned().collect::<Vec<_>>();
        let downloads = stream::iter(candidates.into_iter().map(|(alt, src)| {
            let fetcher = Arc::clone(&daemon.fetcher);
            async move {
                if !src.starts_with("http://") && !src.starts_with("https://") {
                    return (alt, src, None);
                }
                // The fetcher's DNS/redirect guards also apply after this fast guard.
                if url::Url::parse(&src)
                    .ok()
                    .and_then(|u| u.host_str().map(String::from))
                    .is_none_or(|h| crate::fetch::guards::is_ssrf_host(&h))
                {
                    return (alt, src, None);
                }
                let bytes = match tokio::time::timeout(
                    std::time::Duration::from_secs(12),
                    fetcher.fetch(&src),
                )
                .await
                {
                    Ok(Ok(o))
                        if matches!(o.verdict, Verdict::ContentOk) && o.body.len() <= MAX_BYTES =>
                    {
                        Some(o.body)
                    }
                    _ => None,
                };
                (alt, src, bytes)
            }
        }))
        .buffered(2)
        .collect::<Vec<_>>()
        .await;
        // Keep document order and serialize inference through the existing
        // engine; only independent network waits overlap.
        for (alt, src, bytes) in downloads {
            let Some(bytes) = bytes else {
                section.push_str(&format!("- {src}: [unavailable]\n"));
                continue;
            };
            let ocr_result = tokio::task::spawn_blocking(move || {
                let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
                    .with_guessed_format()
                    .map_err(|e| e.to_string())?;
                let mut limits = image::Limits::default();
                limits.max_alloc = Some(128 * 1024 * 1024);
                limits.max_image_width = Some(16_384);
                limits.max_image_height = Some(16_384);
                reader.limits(limits);
                let img = reader.decode().map_err(|e| e.to_string())?;
                let rgba = img.into_rgba8();
                let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                let bitmap = crate::pdf::pixels::PageBitmap {
                    w,
                    h,
                    // PDF's bitmap contract is BGRA; image decoders return RGBA.
                    buf: {
                        let mut pixels = rgba.into_raw();
                        for pixel in pixels.as_chunks_mut::<4>().0 {
                            pixel.swap(0, 2);
                        }
                        pixels
                    },
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
                Ok(Ok(_)) => section.push_str(&format!("- {src}: [no text detected]\n")),
                Ok(Err(error)) => section.push_str(&format!("- {src}: [OCR failed: {error}]\n")),
                Err(error) => section.push_str(&format!("- {src}: [OCR task failed: {error}]\n")),
            }
        }
        (
            section,
            json!({ "images": images.len().min(MAX_IMAGES), "ocred": ocred }),
        )
    }
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
        // No fingerprint: nothing about this read is change-trackable
        // (adapter/fallback/probe paths). Say so in the verdict field
        // instead of omitting it; a since_last check additionally gets
        // the note, since a full page would otherwise read as if the
        // check had run.
        if let Some(sc) = res.pointer_mut("/structuredContent") {
            sc["changed"] = json!("no_baseline");
        }
        if since_last
            && let Some(cell) = res.pointer_mut("/content/0/text")
            && let Some(md) = cell.as_str().map(String::from)
        {
            *cell = json!(format!(
                "*[since_last: no prior snapshot (change tracking is unavailable for this read) : returning full content]*\n\n{md}"
            ));
        }
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
        Some(p) if complete && p.text.is_some() => {
            let old = p.text.as_deref().unwrap_or("");
            let kind = crate::pages::history::classify_change(old, ex_markdown);
            let delta = crate::pages::history::section_delta_report(old, ex_markdown);
            (
                kind.label().to_string(),
                Some(delta),
                now_unix().saturating_sub(p.at),
            )
        }
        Some(p) => ("changed".to_string(), None, now_unix().saturating_sub(p.at)),
        None => ("new".to_string(), None, 0),
    };

    // since_last: collapse the payload to the verdict.
    if since_last {
        let title_line = ex_title.map(|t| format!("# {t}\n")).unwrap_or_default();
        let body = match (changed.as_str(), &delta) {
            ("unchanged", _) => {
                format!("{title_line}{url}\n\n*unchanged since last fetch ({ago}s ago)*\n")
            }
            ("new", _) => format!(
                "{title_line}{url}\n\n*no prior snapshot for this URL : refetch without since_last for full content*\n"
            ),
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
    } else if changed != "new" && !ex_markdown.starts_with("probe: ") {
        // A change note in the content: ONE line, never the delta
        // body. The section-level diff belongs to `since_last` (and
        // to structuredContent.changed_sections); the unasked prepend
        // repeated every extracted fragment into the header and was
        // 34% of a product-page answer (#289).
        if delta.is_some()
            && let Some(cell) = res.pointer_mut("/content/0/text")
            && let Some(md) = cell.as_str().map(String::from)
        {
            let ago_part = if ago > 0 {
                format!(", {ago}s ago")
            } else {
                String::new()
            };
            *cell = json!(format!(
                "*[changed since last fetch ({changed}{ago_part}) : since_last=true returns just the delta]*\n\n{md}"
            ));
        }
    }

    // Change state affects the next model decision. Opaque fingerprints and
    // observation age are client diagnostics.
    if let Some(sc) = res.pointer_mut("/structuredContent") {
        if since_last || !ex_markdown.starts_with("probe: ") {
            sc["changed"] = json!(changed);
        }
        if let Some(d) = &delta
            && (since_last || !ex_markdown.starts_with("probe: "))
        {
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

/// B4 agent-outcome feedback (default off). Soft-demote the
/// (class, host) when the agent's own verification failed:
/// must_contain returned NO MATCH, or the page was unreadable
/// (thin / SoftNotFound). Walls never count: a challenge is a
/// routing fact, not a quality fact. Never fetches anything.
fn maybe_record_agent_outcome(
    daemon: &Daemon,
    host: &str,
    view: &extract::view::View,
    ex: &extract::Extracted,
    verdict: &str,
    probe: Option<extract::view::Verdict>,
) {
    if !crate::config::cfg().search.outcome_feedback {
        return;
    }
    // Wall family (Debug format carries the vendor): routing, not
    // quality. SoftNotFound is a real content failure and falls through.
    if verdict.starts_with("Challenge") || matches!(verdict, "AuthWall" | "Paywall" | "Blocked") {
        return;
    }
    let class = crate::search::outcome_class(
        !view.must_contain.is_empty(),
        view.focus.is_some() || view.toc || view.section.is_some(),
    );
    let probe_miss = probe == Some(extract::view::Verdict::NoMatch);
    let unreadable = ex.thin || verdict == "SoftNotFound";
    if probe_miss || unreadable {
        daemon.searcher.observe_outcome_miss(class, host);
    }
}

pub(super) fn finish_result(
    ex: &extract::Extracted,
    fetch: &str,
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
        "ok": true,
        "url": url,
        "content_ok": !ex.thin && verdict == "ContentOk",
        "content_kind": format!("{:?}", ex.content_kind),
        "read_status": if ex.thin { "thin" } else if ex.next_offset.is_some() || ex.markdown.len() < ex.total_chars { "partial" } else { "content" },
        "content_complete": !ex.thin && ex.next_offset.is_none() && ex.markdown.len() == ex.total_chars && ex.blocks_shown == ex.blocks_total && !ex.focus_omitted,
    });
    if let Some(partial) = &ex.partial {
        structured["partial"] = json!(true);
        structured["read_status"] = json!("partial");
        structured["content_complete"] = json!(false);
        structured["partial_reason"] = json!(partial.reason);
        structured["items_found"] = json!(partial.items_found);
        if let Some(total) = partial.items_total {
            structured["items_total"] = json!(total);
        }
    }
    if ex.thin {
        structured["thin"] = json!(true);
    }
    // #292: repeated blocks/sections were omitted and marked in
    // place; the count makes the omission auditable without reading
    // the whole page.
    let omitted = ex
        .markdown
        .matches(crate::extract::render::REPEATED_MARKER_PREFIX)
        .count();
    if omitted > 0 {
        structured["omitted_repeats"] = json!(omitted);
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
        "status": (status != 0).then_some(status),
        "fetch": fetch,
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
/// domains that consistently wall automated fetches carry the cost
/// in the open so the agent can budget or pick a faster source.
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
                .then(|| "· ⚠ often walled; the fetch may fail".to_string())
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
    if !daemon.search_handles || !crate::handles::handles_enabled() {
        return Vec::new();
    }
    let mut ht = daemon.handles.lock().await;
    let hs = ht.set_search_results(urls);
    // Search handles are in-memory only : no flush.
    hs
}

#[cfg(test)]
mod bypass_content_tests {
    use super::*;
    use crate::search::byok::store::{ByokConfig, KeyEntry, KeyState, ProviderConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Nextest isolates the process-wide key store and daemon state.
    #[tokio::test]
    async fn unlocker_rejects_and_evicts_shells_but_returns_real_content() {
        let cache = crate::paths::cache_dir();
        ByokConfig {
            default: "local".into(),
            providers: vec![ProviderConfig {
                name: "unlocker".into(),
                keys: vec![KeyEntry {
                    key: "fixture-token::fixture-zone".into(),
                    state: KeyState::Active,
                    ts: 0,
                }],
            }],
        }
        .save();
        let daemon = Arc::new(Daemon::new().await.unwrap());
        for (name, html, accepted) in [
            (
                "shell",
                "<html><body><nav>Home Login Sign up Menu Privacy Terms</nav></body></html>",
                false,
            ),
            (
                "article",
                "<html><body><article><h1>Reliable delivery</h1><p>A delivery system records each attempt before contacting the provider. Its deadline covers the complete operation and cancellation releases resources immediately. The next request can then continue without inheriting a stale pending operation.</p></article></body></html>",
                true,
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/request", listener.local_addr().unwrap());
            let body =
                json!({"status_code":200,"headers":{"content-type":"text/html"},"body":html})
                    .to_string();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let cfg = crate::fetch::bypass::BypassConfig {
                endpoint,
                ..Default::default()
            };
            let url = format!("https://fixture.example/{name}");
            crate::fetch::bypass::unlock("fixture-token::fixture-zone", &url, &cfg, &cache)
                .await
                .unwrap();
            server.await.unwrap();
            assert!(
                crate::fetch::bypass::unlock("fixture-token::fixture-zone", &url, &cfg, &cache)
                    .await
                    .unwrap()
                    .cached
            );
            let mut trace = Trace::default();
            let result = try_bypass(&daemon, &url, &ExtractOptions::default(), &mut trace).await;
            assert_eq!(
                result.is_some(),
                accepted,
                "{name}: unlocker content contract: {result:?}"
            );
            if accepted {
                assert!(
                    result.unwrap()["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("Reliable delivery")
                );
            } else {
                // The fixture listener is gone: a stale cache would return Ok.
                assert!(
                    crate::fetch::bypass::unlock("fixture-token::fixture-zone", &url, &cfg, &cache)
                        .await
                        .is_err(),
                    "rejected shell remained cached"
                );
            }
        }
    }
}

#[cfg(test)]
mod page_history_signal_tests {
    use super::*;

    // V08: a read with no fingerprint (adapter/fallback/probe paths)
    // can never answer a since_last check; it must say so instead of
    // silently returning a full page with no verdict and no signal.
    #[tokio::test]
    async fn since_last_without_a_fingerprint_signals_no_baseline() {
        let daemon = Arc::new(Daemon::new().await.unwrap());
        let mut res = json!({
            "content": [{"type": "text", "text": "# T\nhttps://x.test/\n\nbody"}],
            "structuredContent": {"ok": true},
        });
        apply_page_history(
            &daemon,
            &mut res,
            "https://x.test/no-fingerprint",
            PageFacts {
                fingerprint: None,
                markdown: "body",
                title: Some("T"),
                complete: true,
            },
            true,
        );
        assert_eq!(res["structuredContent"]["changed"], "no_baseline");
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("no prior snapshot"), "got: {text}");
    }

    // V08: a first-seen URL (fingerprint present, no prior record) is
    // labeled, not told "new since last fetch (0s ago)".
    #[tokio::test]
    async fn since_last_on_a_first_seen_url_says_no_prior_snapshot() {
        let daemon = Arc::new(Daemon::new().await.unwrap());
        let mut res = json!({
            "content": [{"type": "text", "text": "# T\nhttps://x.test/\n\nbody"}],
            "structuredContent": {"ok": true},
        });
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        apply_page_history(
            &daemon,
            &mut res,
            &format!("https://x.test/first-seen-{unique}"),
            PageFacts {
                fingerprint: Some("deadbeef"),
                markdown: "body",
                title: Some("T"),
                complete: true,
            },
            true,
        );
        assert_eq!(res["structuredContent"]["changed"], "new");
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("no prior snapshot"), "got: {text}");
    }
}

#[cfg(test)]
mod batch_budget_tests {
    use super::{render_fetch_batch, slice_batch_markdowns};
    use serde_json::{Value, json};

    // V09: the composed batch never exceeds the shared budget. The
    // old accounting left member headers and the slicing marker
    // uncounted (measured ~9% over on a real batch).
    #[test]
    fn a_batch_stays_within_its_max_chars() {
        let urls: Vec<String> = (0..6).map(|i| format!("https://ex.test/{i}")).collect();
        let big = "x".repeat(9_000);
        let results: Vec<Value> = urls
            .iter()
            .map(|_| {
                json!({
                    "content": [{"type": "text", "text": big.clone()}],
                    "_meta": {"com.donsetch/fetch-debug": {"title": "T"}},
                    "structuredContent": {"ok": true},
                })
            })
            .collect();
        let mut markdowns: Vec<Option<String>> =
            results.iter().map(|_| Some(big.clone())).collect();
        let mut flags = vec![false; urls.len()];
        slice_batch_markdowns(&urls, &results, &mut markdowns, 8_000, &mut flags);
        assert!(
            flags.iter().all(|f| *f),
            "every over-share member gets sliced"
        );
        let out = render_fetch_batch(&urls, &results, &markdowns, 8_000, &flags);
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(
            text.len() <= 8_000,
            "batch output {} bytes exceeds its 8000-char budget",
            text.len()
        );
    }
}

#[cfg(test)]
mod fetch_output_contract_tests {
    use super::{Trace, finish_result, format_fetch_markdown};
    use crate::extract::{ContentKind, Extracted};

    #[test]
    fn stealth_v3_unavailable_browser_status_is_null_not_success_200() {
        let output = finish_result(
            &extracted("# Owned document\n\nActual evidence."),
            "ghost-dom",
            0,
            "ContentOk",
            "https://owned.test/final",
            &Trace::default(),
            0,
        );
        assert_eq!(
            output["structuredContent"]["url"],
            "https://owned.test/final"
        );
        assert!(output["_meta"]["com.donsetch/fetch-debug"]["status"].is_null());
        assert!(
            output["_meta"]["com.donsetch/fetch-debug"]
                .as_object()
                .unwrap()
                .contains_key("status")
        );
        let observed = finish_result(
            &extracted("# Owned document\n\nActual evidence."),
            "ghost-dom",
            201,
            "ContentOk",
            "https://owned.test/final",
            &Trace::default(),
            0,
        );
        assert_eq!(observed["_meta"]["com.donsetch/fetch-debug"]["status"], 201);
    }

    #[test]
    fn focused_rows_do_not_claim_the_whole_source_is_complete() {
        let mut ex = extracted("| Rule | Details |\n| --- | --- |\n| Audit | Seven years |\n");
        ex.blocks_shown = ex.blocks_total;
        ex.focus_omitted = true;
        let output = finish_result(
            &ex,
            "1",
            200,
            "ContentOk",
            "https://owned.test/table",
            &Trace::default(),
            0,
        );
        assert_eq!(output["structuredContent"]["content_complete"], false);
        ex.focus_omitted = false;
        let output = finish_result(
            &ex,
            "1",
            200,
            "ContentOk",
            "https://owned.test/table",
            &Trace::default(),
            0,
        );
        assert_eq!(output["structuredContent"]["content_complete"], true);
    }

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
            focus_omitted: false,
            tokens_est: markdown.len() / 4,
            thin: false,
            content_kind: ContentKind::Article,
            lang: "en".into(),
            quality: 0.91,
            pdf_pages: None,
            images: Vec::new(),
            fingerprint: Some("opaque".into()),
            via: None,
            partial: None,
        }
    }

    #[test]
    pub(super) fn fetch_renders_identity_once_and_hides_diagnostics_from_model_state() {
        let mut trace = Trace::default();
        trace.step("local", "http-fetch", "ok", 12);
        let output = finish_result(
            &extracted("https://example.com/page\n\n# Example\n\nEvidence."),
            "local",
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
            "fetch",
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
        assert_eq!(
            output["_meta"]["com.donsetch/fetch-debug"]["fetch"],
            "local"
        );

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

    // Framework-junk shells must never ship as successful content:
    // the classifier that gates the shell class (live case:
    // facebook's markdown was React internals).
    #[test]
    fn shell_text_classifier_rejects_react_dumps_and_flags_spinners() {
        let fb = "# is_latency_sensitive_broadcast\n# Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36\nFDSTooltipDef\n\
[[\"low\",\"normal\"]]\n__isReceiptEnd\nfdi_dispatch\npage_logging_framework";
        assert!(
            super::looks_like_shell_text(fb),
            "identifier-dominated junk must classify as a shell"
        );
        assert!(
            super::looks_like_shell_text("Please wait..."),
            "a spinner fragment must classify as a shell"
        );
        let real = "Cristiano Ronaldo is a Portuguese footballer who has won the \
Champions League five times. He remains the most followed athlete \
across every major social platform today.";
        assert!(
            !super::looks_like_shell_text(real),
            "real prose must never classify as a shell"
        );
        let hn = "1. Rust 2.0 released (crates.io)\n2. Show HN: my parser (github.com)\n";
        assert!(
            !super::looks_like_shell_text(hn),
            "short legit listings must not classify as a shell"
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
            "local",
            200,
            "ContentOk",
            &urls[0],
            &Trace::default(),
            1,
        );
        let failure = json!({
            "content": [{"type": "text", "text": "request timed out"}],
            "structuredContent": {"ok": false, "code": "network.timeout"},
            "isError": false,
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
        let output = render_fetch_batch(&urls, &results, &markdowns, 8_000, &[false, false]);
        assert_eq!(output["content"].as_array().unwrap().len(), 1);
        let text = output["content"][0]["text"].as_str().unwrap();
        assert!(text.find("Alpha.").unwrap() < text.find("request timed out").unwrap());
        assert_eq!(
            output["structuredContent"]["results"][1]["code"],
            "network.timeout"
        );
        let first = &output["structuredContent"]["results"][0];
        assert_eq!(first["ok"], true);
        assert!(first.get("content_ok").is_none());
        assert!(first.get("content_kind").is_none());
        assert!(first.get("thin").is_none());
        assert!(first.get("changed").is_none());
        assert!(first.get("fetch").is_none());
        let first_debug = &output["_meta"]["com.donsetch/fetch-batch-debug"]["results"][0];
        assert_eq!(first_debug["fetch"], "local");
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
                "cloak_suspected": true
            }
        });
        let flagged_output = render_fetch_batch(
            &urls[..1],
            &[flagged],
            &[Some("# Thin\nhttps://example.com/a".into())],
            48_000,
            &[false],
        );
        let flagged_state = &flagged_output["structuredContent"]["results"][0];
        assert_eq!(flagged_state["content_ok"], false);
        assert!(flagged_state.get("content_kind").is_none());
        assert_eq!(flagged_state["thin"], true);
        assert_eq!(flagged_state["changed"], "major");
        assert_eq!(flagged_state["next_offset"], 16000);
        assert_eq!(flagged_state["cloak_suspected"], true);
    }
}

#[cfg(test)]
mod budget_tests {

    #[test]
    fn wave450_registry_card_is_not_a_framework_dump() {
        let payload = json!({"info":{"name":"requests","version":"2.34.2","summary":"Python HTTP for Humans.","license":"Apache-2.0", "requires_dist":["charset-normalizer>=2","idna>=2.5","urllib3>=1.21","certifi>=2017.4.17"]},"releases":{
            "2.34.2":[], "2.34.1":[], "2.33.0":[], "2.32.5":[], "2.32.4":[], "2.32.3":[], "2.32.2":[], "2.32.1":[], "2.32.0":[], "2.31.0":[]
        }});
        let ex = extract::extract(
            payload.to_string().as_bytes(),
            "application/json",
            "https://pypi.org/pypi/requests/json",
            &ExtractOptions::default(),
        )
        .unwrap();
        assert_eq!(ex.via, Some("adapter:pypi-json"));
        assert!(ex.markdown.contains("requests 2.34.2"));
        assert!(!is_framework_shell(&ex), "{}", ex.markdown);
    }

    #[test]
    fn wave450_batch_preserves_partial_state_and_never_skips_sliced_content() {
        let url = "https://example.org/".to_string();
        let result = json!({"content":[{"type":"text","text":"evidence"}], "structuredContent":{"content_ok":true,"partial":true,"content_complete":false,"read_status":"partial","items_found":3,"next_offset":16000}});
        let batch = render_fetch_batch(&[url], &[result], &[Some("evidence".into())], 800, &[true]);
        let sc = &batch["structuredContent"]["results"][0];
        assert_eq!(sc["partial"], true);
        assert_eq!(sc["items_found"], 3);
        assert_eq!(sc["content_complete"], false);
        assert!(
            sc.get("next_offset").is_none(),
            "the original offset skips newly budget-sliced evidence"
        );
        assert!(sc["next_action"].as_str().unwrap().contains("refetch"));
    }

    #[test]
    fn wave450_reddit_partial_listing_is_machine_readable() {
        let html = "<html><body><shreddit-post post-title='A real post' subreddit-prefixed-name='r/rust'></shreddit-post><faceplate-partial src='/svc/shreddit/community-more-posts/top/'></faceplate-partial></body></html>";
        let ex = crate::adapters::reddit_html::extract(
            html,
            "https://www.reddit.com/r/rust/",
            &ExtractOptions::default(),
        )
        .unwrap();
        let result = finish_result(
            &ex,
            "1",
            200,
            "ContentOk",
            "https://www.reddit.com/r/rust/",
            &Trace::default(),
            5,
        );
        let sc = &result["structuredContent"];
        assert_eq!(
            sc["ok"], true,
            "success envelopes carry the machine ok flag"
        );
        assert_eq!(sc["content_ok"], true);
        assert_eq!(sc["content_complete"], false);
        assert_eq!(sc["partial"], true);
        assert_eq!(sc["items_found"], 1);
        assert!(
            sc["items_total"].is_null(),
            "do not invent an unknown feed size"
        );
    }
    use super::*;

    #[tokio::test]
    async fn wave450_deadline_retains_observed_escalation() {
        let witness = Arc::new(std::sync::Mutex::new(Vec::new()));
        let work = async {
            let mut trace = Trace::default();
            trace.step("1", "http-fetch", "Challenge(Cloudflare)", 7);
            trace.step("2", "browser-launch", "started", 0);
            std::future::pending::<Value>().await
        };
        let result = FETCH_TRACE
            .scope(
                witness.clone(),
                run_with_budget(
                    work,
                    Some(std::time::Duration::from_millis(10)),
                    None,
                    || {
                        let prior = witness.lock().unwrap().clone();
                        let mut result = deadline_error("https://wall.example/");
                        fold_trace_into_result(&mut result, prior);
                        result
                    },
                ),
            )
            .await;
        assert_eq!(result["structuredContent"]["code"], "deadline.hit");
        let steps = result["structuredContent"]["escalation"]
            .as_array()
            .unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0]["outcome"], "Challenge(Cloudflare)");
        assert_eq!(steps[1]["action"], "browser-launch");
        assert_eq!(steps[2]["action"], "deadline");
    }
}

#[cfg(test)]
mod adapter_hop_tests {
    use super::*;
    use crate::detect::walls::Vendor;

    // Nextest owns config/state; match the production runtime's 8 MiB stack.
    #[test]
    fn stealth_v3_adapter_fallback_keeps_the_callers_selected_lane() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all().build().unwrap();
                runtime.block_on(async {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    unsafe {
                        std::env::set_var("DONSETCH_FETCH_ROTATE", "1");
                        std::env::remove_var("DONSETCH_NO_FETCH_ROTATE");
                        std::env::set_var("DONSETCH_NO_ENV_PROXY", "1");
                        std::env::set_var("DONSETCH_ALLOW_PRIVATE_EGRESS", "1");
                        std::env::set_var("DONSETCH_NO_ROUTE_PROBES", "1");
                    }
                    let a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let proxies = [&a, &b].map(|listener| {
                        crate::egress::proxy::Proxy::parse(&format!(
                            "http://{}", listener.local_addr().unwrap()
                        )).unwrap()
                    });
                    let pool = Arc::new(crate::search::egress::EgressPool::new(proxies.to_vec()));
                    crate::search::egress::install_global(Arc::clone(&pool));
                    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
                    let (stop, stopped) = tokio::sync::watch::channel(false);
                    let mut servers = Vec::new();
                    for (lane, listener) in [("A", a), ("B", b)] {
                        let events = Arc::clone(&events);
                        let mut stopped = stopped.clone();
                        servers.push(tokio::spawn(async move {
                            loop {
                                let mut socket = tokio::select! {
                                    _ = stopped.changed() => break,
                                    accepted = listener.accept() => accepted.unwrap().0,
                                };
                                let mut head = Vec::new();
                                while !head.ends_with(b"\r\n\r\n") {
                                    assert!(head.len() < 16384);
                                    head.push(socket.read_u8().await.unwrap());
                                }
                                let request = String::from_utf8(head).unwrap();
                                let target = request.split_whitespace().nth(1).unwrap().to_owned();
                                events.lock().unwrap().push((lane, target.clone()));
                                let (status, body) = if target.contains(".json") {
                                    (429, "<html><body>Too many requests</body></html>".to_string())
                                } else {
                                    (200, format!("<article><h1>Owned fallback lane {lane}</h1><p>{}</p></article>",
                                        "This complete research document must retain the proxy selected for the original call. ".repeat(40)))
                                };
                                socket.write_all(format!("HTTP/1.1 {status} Owned\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                            }
                        }));
                    }
                    let mut daemon = Daemon::new().await.unwrap();
                    daemon.fetcher = Arc::new(Fetcher::new(daemon.fetcher.profile().clone()).unwrap().with_egress(Arc::clone(&pool)));
                    let daemon = Arc::new(daemon);
                    let result = tokio::time::timeout(std::time::Duration::from_secs(8),
                        fetch_single(&daemon, &json!({}), "http://old.reddit.com/r/owned/")
                    ).await.unwrap();
                    stop.send(true).unwrap();
                    for server in servers { tokio::time::timeout(std::time::Duration::from_secs(2), server).await.unwrap().unwrap(); }
                    assert_eq!(result["structuredContent"]["ok"], true, "{result}");
                    let events = events.lock().unwrap();
                    // The adapter read is one encounter (v5 may ask it as more
                    // than one identity); the fallback is one read after it.
                    assert!(events[0].1.contains(".json"), "{events:?}");
                    let last = events.last().unwrap();
                    assert!(last.1.ends_with("/r/owned/"), "{events:?}");
                    assert_eq!(events.iter().filter(|(_, target)| target.ends_with("/r/owned/")).count(), 1,
                        "one fallback read: {events:?}");
                    assert!(events.iter().all(|(lane, _)| *lane == "A"),
                        "a related fallback must not repick after the adapter burns its lane: {events:?}");
                    assert!(result["content"][0]["text"].as_str().unwrap().contains("Owned fallback lane A"), "{result}");
                    assert!(!pool.is_dead(&proxies[0].id()),
                        "an origin 429 must not globally bench a reachable proxy");
                });
            }).unwrap().join().unwrap();
    }

    #[test]
    fn the_adapter_rewrite_runs_unless_the_read_is_its_own_fallback() {
        // Views apply to the adapter's markdown like any other, so no view
        // argument turns the rewrite off; only the adapter fallback does.
        let url = url::Url::parse("https://stackoverflow.com/questions/42917566/example").unwrap();
        assert!(fetch_url_rewrite(&url, false).is_some());
        assert!(fetch_url_rewrite(&url, true).is_none());
    }

    #[test]
    fn json_endpoints_are_detected_by_url_and_content_type() {
        assert!(is_json_url_like("https://api.example.com/items.json"));
        assert!(is_json_url_like("https://www.reddit.com/r/rust.json"));
        assert!(is_json_url_like("https://api.example.com/items.JSON?v=2"));
        assert!(is_json_url_like("https://api.example.com/items.json#frag"));
        assert!(!is_json_url_like("https://api.example.com/json"));
        assert!(!is_json_url_like("https://api.example.com/items.jsonl"));
        assert!(!is_json_url_like("https://api.example.com/docs.json/page"));
        let json_ct = vec![(
            "content-type".to_string(),
            "application/json; charset=utf-8".to_string(),
        )];
        assert!(ct_is_json(&json_ct));
        let problem = vec![(
            "Content-Type".to_string(),
            "application/problem+json".to_string(),
        )];
        assert!(ct_is_json(&problem));
        let html = vec![("content-type".to_string(), "text/html".to_string())];
        assert!(!ct_is_json(&html));
        assert!(!ct_is_json(&[]));
    }

    // #287: a challenge (or any non-content verdict) on an adapter
    // endpoint must retry the caller's URL. Before this, the
    // adapter's refusal ended the whole fetch after one HTTP
    // request.
    #[test]
    fn a_refusal_on_an_adapter_endpoint_falls_back() {
        assert!(adapter_hop_failed(
            Verdict::Challenge(Vendor::Generic),
            true,
            false
        ));
        assert!(adapter_hop_failed(Verdict::Blocked, true, false));
        assert!(adapter_hop_failed(Verdict::AuthWall, true, false));
        // Served JSON is a good adapter hop: no fallback.
        assert!(!adapter_hop_failed(Verdict::ContentOk, true, false));
        // Non-adapter fetches keep their own escalation rules.
        assert!(!adapter_hop_failed(
            Verdict::Challenge(Vendor::Generic),
            false,
            false
        ));
        // The retry itself must not bounce (`_no_adapter`).
        assert!(!adapter_hop_failed(Verdict::Blocked, true, true));
    }

    // V02: `loid` is the gateway cookie for the direct `.json`
    // endpoint. When the jar holds it, the pre-emptive hop is
    // skipped; the refusal ladder still covers a revoked session.
    #[test]
    fn a_live_loid_gates_the_preemptive_reddit_hop() {
        let cookie = |name: &str| name.to_string();
        assert!(reddit_session_live(&[cookie("loid")]));
        assert!(reddit_session_live(&[
            cookie("session_tracker"),
            cookie("loid")
        ]));
        assert!(!reddit_session_live(&[cookie("session_tracker")]));
        assert!(!reddit_session_live(&[]));
    }

    // The session retry: reddit page refusals get one session-init
    // attempt; never twice, never off the content host.
    #[test]
    fn reddit_session_retry_eligibility() {
        let args = serde_json::json!({});
        assert!(reddit_session_retry_eligible(
            &Verdict::Challenge(Vendor::Generic),
            "www.reddit.com",
            &args
        ));
        assert!(reddit_session_retry_eligible(
            &Verdict::Blocked,
            "reddit.com",
            &args
        ));
        // Other hosts keep their own rules.
        assert!(!reddit_session_retry_eligible(
            &Verdict::Blocked,
            "registry.npmjs.org",
            &args
        ));
        // Served content is content.
        assert!(!reddit_session_retry_eligible(
            &Verdict::ContentOk,
            "www.reddit.com",
            &args
        ));
        // The retry is one-shot.
        let done = serde_json::json!({"_reddit_session": true});
        assert!(!reddit_session_retry_eligible(
            &Verdict::Blocked,
            "www.reddit.com",
            &done
        ));
    }

    // The adapter hop's steps fold in FRONT of the retry's own trail
    // so the escalation reads as one ladder.
    #[test]
    fn adapter_steps_are_folded_in_front_of_the_retry_trail() {
        let mut res = serde_json::json!({
            "structuredContent": {"escalation": [{"action": "route"}]}
        });
        fold_trace_into_result(&mut res, vec![serde_json::json!({"action": "http-fetch"})]);
        let esc = res.pointer("/structuredContent/escalation").unwrap();
        assert_eq!(esc[0]["action"], "http-fetch");
        assert_eq!(esc[1]["action"], "route");
    }

    // Success results carry the trail under _meta; the fold reaches
    // it there too.
    #[test]
    fn fold_reaches_the_success_meta_trail() {
        let mut res = serde_json::json!({
            "_meta": {"com.donsetch/fetch-debug": {"escalation": [{"action": "route"}]}}
        });
        fold_trace_into_result(&mut res, vec![serde_json::json!({"action": "adapter"})]);
        let esc = res
            .pointer("/_meta/com.donsetch~1fetch-debug/escalation")
            .unwrap();
        assert_eq!(esc[0]["action"], "adapter");
        assert_eq!(esc[1]["action"], "route");
    }
}

#[cfg(test)]
mod rules_tests {
    use super::*;
    use crate::rules::{RuleAction, RulesSection, UrlRule};

    const MESSAGE: &str = "ask the human operator to download the file";

    fn section(rules: &[(&str, UrlRule)]) -> RulesSection {
        let mut section = RulesSection::default();
        for (key, rule) in rules {
            section.url.insert((*key).to_string(), rule.clone());
        }
        section
    }

    fn deny(reason: Option<&str>) -> UrlRule {
        UrlRule {
            action: RuleAction::Deny,
            message: Some(MESSAGE.into()),
            reason: reason.map(String::from),
            ..UrlRule::default()
        }
    }

    fn denial(kind: &'static str) -> FetchError {
        FetchError::Denied {
            rule: "banned.example".into(),
            message: MESSAGE.into(),
            kind,
            reason: Some("ip_ban".into()),
            url: "https://www.banned.example/".into(),
        }
    }

    #[test]
    fn batch_rows_carry_the_rule_and_kind_of_a_policy_error_only() {
        let urls = vec![
            "https://example.com/ok".to_string(),
            "https://www.banned.example/x".to_string(),
            "https://example.com/slow".to_string(),
        ];
        let results = vec![
            json!({
                "content": [{"type": "text", "text": "# Ok\nhttps://example.com/ok\n\nBody."}],
                "structuredContent": {"ok": true},
            }),
            policy_error_value(&denial("permanent"), &urls[1], None),
            tool_error_structured(
                "request timed out",
                "transient",
                Some(json!({"url": urls[2], "code": "network.timeout"})),
            ),
        ];
        let markdowns: Vec<Option<String>> = vec![
            Some("# Ok\nhttps://example.com/ok\n\nBody.".into()),
            None,
            None,
        ];
        let out = render_fetch_batch(&urls, &results, &markdowns, 48_000, &[false; 3]);
        let rows = &out["structuredContent"]["results"];
        assert_eq!(rows[1]["code"], "policy.denied.ip_ban");
        assert_eq!(rows[1]["errorKind"], "permanent");
        assert_eq!(rows[1]["rule"], "banned.example");
        assert_eq!(rows[1]["next_action"], MESSAGE);
        // Every other failed row keeps its shape.
        assert_eq!(rows[2]["code"], "network.timeout");
        assert!(rows[2].get("errorKind").is_none(), "{}", rows[2]);
        assert!(rows[2].get("rule").is_none(), "{}", rows[2]);
        assert!(rows[0].get("errorKind").is_none(), "{}", rows[0]);
    }

    #[test]
    fn an_all_failed_batch_carries_the_code_and_kind_its_urls_agree_on() {
        let urls: Vec<String> = (0..3)
            .map(|i| format!("https://www.banned.example/{i}"))
            .collect();
        let denied: Vec<Value> = urls
            .iter()
            .map(|u| policy_error_value(&denial("walled"), u, None))
            .collect();
        let out = render_fetch_batch(&urls, &denied, &[None, None, None], 48_000, &[false; 3]);
        assert_eq!(out["structuredContent"]["ok"], false);
        assert_eq!(out["structuredContent"]["code"], "policy.denied.ip_ban");
        assert_eq!(out["errorKind"], "walled");

        // No rule involved: three walls are a walled batch with the wall code.
        let walled: Vec<Value> = urls
            .iter()
            .map(|u| {
                tool_error_structured(
                    "blocked at the site",
                    "walled",
                    Some(json!({"url": u, "code": "wall.blocked"})),
                )
            })
            .collect();
        let out = render_fetch_batch(&urls, &walled, &[None, None, None], 48_000, &[false; 3]);
        assert_eq!(out["structuredContent"]["code"], "wall.blocked");
        assert_eq!(out["errorKind"], "walled");

        // Disagreeing members: no explicit code, and the kind stays transient.
        let mixed = vec![
            denied[0].clone(),
            tool_error_structured(
                "page not found",
                "permanent",
                Some(json!({"url": urls[1], "code": "content.notfound"})),
            ),
            walled[2].clone(),
        ];
        let out = render_fetch_batch(&urls, &mixed, &[None, None, None], 48_000, &[false; 3]);
        assert_ne!(out["structuredContent"]["code"], "policy.denied.ip_ban");
        assert_eq!(out["errorKind"], "transient");
        let rows = &out["structuredContent"]["results"];
        assert_eq!(
            rows[0]["code"], "policy.denied.ip_ban",
            "the rows stay right"
        );
        assert_eq!(rows[1]["code"], "content.notfound");
    }

    /// Installs `rules`, then runs `fetch_single` once per call through two
    /// owned proxy lanes. Returns each result and every request target the
    /// lanes saw. A target containing `/hop` answers with a redirect to
    /// `http://www.banned.example/publication/2`, a `.json` target with
    /// 429, anything else with a complete article.
    fn fetch_through_owned_lanes(
        rules: RulesSection,
        calls: Vec<(Value, &'static str)>,
    ) -> (Vec<Value>, Vec<String>) {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut config = crate::config::DonsetchConfig::default();
                    config.proxy.from_environment = false;
                    config.proxy.fetch_rotate = true;
                    config.fetch.allow_private_egress = true;
                    config.browser.route_probes = false;
                    config.rules = rules;
                    crate::config::install(config).unwrap();
                    let a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let proxies = [&a, &b].map(|listener| {
                        crate::egress::proxy::Proxy::parse(&format!(
                            "http://{}",
                            listener.local_addr().unwrap()
                        ))
                        .unwrap()
                    });
                    let pool = Arc::new(crate::search::egress::EgressPool::new(proxies.to_vec()));
                    crate::search::egress::install_global(Arc::clone(&pool));
                    let targets = Arc::new(std::sync::Mutex::new(Vec::new()));
                    let (stop, stopped) = tokio::sync::watch::channel(false);
                    let mut servers = Vec::new();
                    for listener in [a, b] {
                        let targets = Arc::clone(&targets);
                        let mut stopped = stopped.clone();
                        servers.push(tokio::spawn(async move {
                            loop {
                                let mut socket = tokio::select! {
                                    _ = stopped.changed() => break,
                                    accepted = listener.accept() => accepted.unwrap().0,
                                };
                                let mut head = Vec::new();
                                while !head.ends_with(b"\r\n\r\n") {
                                    assert!(head.len() < 16384);
                                    head.push(socket.read_u8().await.unwrap());
                                }
                                let request = String::from_utf8(head).unwrap();
                                let target = request.split_whitespace().nth(1).unwrap().to_owned();
                                targets.lock().unwrap().push(target.clone());
                                let response = if target.contains("/hop") {
                                    "HTTP/1.1 302 Found\r\nLocation: http://www.banned.example/publication/2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                                } else {
                                    let (status, body) = if target.contains(".json") {
                                        (429, "<html><body>Too many requests</body></html>".to_string())
                                    } else {
                                        (200, format!(
                                            "<article><h1>Owned page</h1><p>{}</p></article>",
                                            "This complete research document is served by an owned test lane. ".repeat(40)
                                        ))
                                    };
                                    format!(
                                        "HTTP/1.1 {status} Owned\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                        body.len()
                                    )
                                };
                                socket.write_all(response.as_bytes()).await.unwrap();
                            }
                        }));
                    }
                    let mut daemon = Daemon::new().await.unwrap();
                    daemon.fetcher = Arc::new(
                        Fetcher::new(daemon.fetcher.profile().clone())
                            .unwrap()
                            .with_egress(Arc::clone(&pool)),
                    );
                    let daemon = Arc::new(daemon);
                    let mut results = Vec::new();
                    for (args, target) in calls {
                        results.push(
                            tokio::time::timeout(
                                std::time::Duration::from_secs(8),
                                fetch_single(&daemon, &args, target),
                            )
                            .await
                            .unwrap(),
                        );
                    }
                    stop.send(true).unwrap();
                    for server in servers {
                        tokio::time::timeout(std::time::Duration::from_secs(2), server)
                            .await
                            .unwrap()
                            .unwrap();
                    }
                    let targets = targets.lock().unwrap().clone();
                    (results, targets)
                })
            })
            .unwrap()
            .join()
            .unwrap()
    }

    // Installs a ruleset into the process config: depends on nextest's
    // process-per-test.
    #[test]
    fn a_denied_url_returns_its_policy_error_and_sends_nothing() {
        let (results, targets) = fetch_through_owned_lanes(
            section(&[("banned.example", deny(Some("ip_ban")))]),
            vec![
                (json!({}), "http://www.banned.example/publication/1"),
                (json!({}), "http://allowed.example/doc"),
                (json!({}), "http://allowed.example/hop"),
            ],
        );
        let denied = &results[0]["structuredContent"];
        assert_eq!(denied["ok"], false, "{}", results[0]);
        assert_eq!(
            denied["code"], "policy.denied.ip_ban",
            "not guard.ssrf: {}",
            results[0]
        );
        assert_eq!(denied["errorKind"], "walled");
        assert_eq!(denied["rule"], "banned.example");
        assert_eq!(denied["next_action"], MESSAGE);
        // The allowed host passes unchanged (the negative case).
        assert_eq!(
            results[1]["structuredContent"]["ok"], true,
            "{}",
            results[1]
        );
        // A redirect from an allowed URL into the denied host: the rule's
        // error, with the trail that shows how the call got there.
        let hop = &results[2]["structuredContent"];
        assert_eq!(hop["code"], "policy.denied.ip_ban", "{}", results[2]);
        assert_eq!(hop["rule"], "banned.example");
        assert!(hop["escalation"].is_array(), "{}", results[2]);
        assert!(
            targets.iter().all(|t| !t.contains("banned.example")),
            "nothing reaches the denied host: {targets:?}"
        );
        assert!(
            targets.iter().any(|t| t.contains("allowed.example/doc")),
            "{targets:?}"
        );
    }

    // Installs a ruleset into the process config: depends on nextest's
    // process-per-test.
    #[test]
    fn a_reddit_rewrite_into_a_denied_host_sends_nothing() {
        let (results, targets) = fetch_through_owned_lanes(
            section(&[("www.reddit.com", deny(None))]),
            vec![
                // The adapter rewrites old. onto www. for its JSON read, and
                // its fallback retries on www. as well.
                (json!({}), "http://old.reddit.com/r/owned/"),
                // A wiki page has no JSON shape: it stays on the allowed host.
                (json!({}), "http://old.reddit.com/r/owned/wiki/books"),
            ],
        );
        let denied = &results[0]["structuredContent"];
        assert_eq!(
            denied["code"], "policy.denied.unspecified",
            "{}",
            results[0]
        );
        assert_eq!(denied["rule"], "www.reddit.com");
        assert_eq!(denied["next_action"], MESSAGE);
        assert_eq!(
            results[1]["structuredContent"]["ok"], true,
            "{}",
            results[1]
        );
        assert!(
            targets.iter().all(|t| !t.contains("www.reddit.com")),
            "nothing reaches the denied host: {targets:?}"
        );
        assert!(
            targets.iter().any(|t| t.contains("old.reddit.com")),
            "{targets:?}"
        );
    }
}
